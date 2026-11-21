use crate::agent::{
    logged_tokens, Agent, Asking, Delegating, Event, Nudging, Place, Queue, Sink, Status, Task,
    TaskView, TaskViews, Turn, Unseen,
};
use crate::prompt;
use crate::spawn::SHOWN_EVERY;
use crate::steps::{
    spent, AgentMemory, AgentRecord, AgentTask, CallError, DoneStep, Ending, MemoryStep, Returned,
    Sent, StepError,
};
use crate::utils::{Interrupt, Reply};
use crate::worker::host::{Added, Called, Calling, Worker};
use crate::worker::{AgentCall, Asked};
use serde_json::{json, Value};
use std::future;
use std::mem;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::select;
use tokio::sync::{watch, Semaphore};
use tokio::task::{self, AbortHandle};
use tokio::time;
use tracing::{error_span, info, warn, Instrument};

pub(crate) const MAX_PARALLEL_AGENTS: usize = 8;
const MAX_PREVIEW_LINES: usize = 4;
const MAX_PREVIEW_CHARS: usize = 800;
const MAX_LINE_CHARS: usize = 100;
/// Agents get this long to stop by themselves when the turn ends,
/// and a worker of theirs may take `STOP_WAIT`
const AGENTS_WAIT: Duration = Duration::from_secs(5);
/// Idle agents keep their Python, at most this many, the ones used last
const MAX_IDLE_WORKERS: usize = 4;
/// An idle agent's Python ends after this long unused
const IDLE_WAIT: Duration = match cfg!(test) {
    true => Duration::from_secs(2),
    false => Duration::from_secs(30 * 60),
};

/// The tasks the code of one call sent, in the order their views opened,
/// which numbers them on replay too
pub(crate) type Records = Mutex<Vec<Sent>>;

/// What a task at work shares with the team
#[derive(Default)]
struct Activity {
    status: Status,
    /// How it ends when `agent.stop()` or the end of the turn rather than the user stopped it
    stopped: Option<Ending>,
}

/// What the code sends an agent at work, which takes it after its current call
#[derive(Default)]
struct Inbox {
    sent: Mutex<Vec<String>>,
}

impl Queue for Inbox {
    fn queued(&self) -> Asking<'_> {
        let mut inbox = self.sent.lock().expect("inbox");
        let mut tasks = Vec::new();
        for text in mem::take(&mut *inbox) {
            let task = Task {
                text,
                mentions: Vec::new(),
            };
            tasks.push(task);
        }
        Box::pin(future::ready(tasks))
    }

    /// Only the session's own agent waits for messages to arrive
    fn enqueued(&self) -> Nudging<'_> {
        Box::pin(future::pending())
    }
}

/// Shows the client which agents the code waits for until it is dropped
struct Awaiting(Arc<dyn TaskViews>);

impl Drop for Awaiting {
    fn drop(&mut self) {
        self.0.waiting(&[]);
    }
}

impl Agent {
    /// Runs the calls that the code of one call makes to `agent()` and its functions,
    /// and lists each task it sends in `records`
    pub(crate) fn delegate<'a>(
        &'a self,
        servers: &'a [&'a Added],
        views: Box<dyn TaskViews>,
        records: &'a Records,
    ) -> Box<Delegating<'a>> {
        let Place::Main { team, queue } = &self.place else {
            let refused = |_: AgentCall| -> Calling {
                let reply = Err(prompt::NESTED.to_owned());
                let called = Called::from(reply);
                Box::pin(future::ready(called))
            };
            return Box::new(refused);
        };

        let launcher = Launcher {
            views: Arc::from(views),
            records,
            servers,
            team: Arc::clone(team),
        };
        let delegate = move |call: AgentCall| -> Calling {
            match call {
                AgentCall::Send(asked) => {
                    self.send(&launcher, asked);
                    let called = Called::from(Ok(Value::Null));
                    Box::pin(future::ready(called))
                }
                AgentCall::Wait { names, timeout } => {
                    let team = Arc::clone(&launcher.team);
                    let views = Arc::clone(&launcher.views);
                    let queue = Arc::clone(queue);
                    Box::pin(async move {
                        let names = match team.awaited(names) {
                            Ok(names) => names,
                            Err(error) => return Called::from(Err(error)),
                        };
                        views.waiting(&names);
                        let _awaiting = Awaiting(views);
                        let enqueued = queue.enqueued();
                        let waited = team.wait(&names, timeout, enqueued).await;
                        Called::from(Ok(waited))
                    })
                }
                AgentCall::Stop { name } => {
                    let team = Arc::clone(&launcher.team);
                    Box::pin(async move {
                        let stopped = team.stop(&name).await;
                        Called::from(stopped)
                    })
                }
            }
        };
        Box::new(delegate)
    }

    /// Starts a task for the agent `asked` names, or hands it to the task that agent is at work on
    fn send(&self, launcher: &Launcher, asked: Asked) {
        let mut members = launcher.team.members.lock().expect("members");
        let found = members
            .iter()
            .position(|member| member.name() == asked.name);
        if let Some(id) = found {
            let member = &members[id];
            if member.running() {
                member.inbox.sent.lock().expect("inbox").push(asked.task);
                return;
            }
        }

        let id = match found {
            Some(id) => id,
            None => {
                let record = AgentRecord {
                    name: asked.name.clone(),
                    ..AgentRecord::default()
                };
                members.push(Member::new(record));
                members.len() - 1
            }
        };

        let member = &mut members[id];
        let agent = Agent {
            place: Place::Delegated {
                queue: member.inbox.clone(),
            },
            interrupt: member.stop.clone(),
            ..self.clone()
        };
        launcher.launch(agent, member, id, asked, &self.interrupt);
    }

    /// Works on the task `asked` in the agent's conversation `memory`, on the Python its last task
    /// kept if that has the same MCP servers, and returns how the task ended, the conversation
    /// and the Python to keep, none once the task leaves programs running
    async fn work(
        self,
        asked: Asked,
        mut memory: AgentMemory,
        kept: Option<Kept>,
        mut watched: Watched,
        servers: Arc<[Added]>,
        slots: Arc<Semaphore>,
    ) -> (Ending, AgentMemory, Option<Kept>) {
        let started = Instant::now();
        let start = watched.start;

        // One still waiting for a slot stops on an interrupt without starting a worker
        let slot = select! {
            biased;
            () = self.interrupt.wait() => None,
            slot = slots.acquire() => Some(slot.expect("slots")),
        };
        let (answered, kept) = match slot {
            None => (Ok(None), kept),
            Some(_slot) => {
                let name = &asked.name;
                info!("agent: started [name={name}]");
                {
                    let mut live = watched.live.lock().expect("live");
                    live.status.since = Some(Instant::now());
                    live.status.doing = prompt::STARTING.to_owned();
                }
                watched.show(false);

                let installed: Vec<&Added> = servers.iter().collect();
                let names: Vec<String> = servers.iter().map(|added| added.name.clone()).collect();
                let reused = kept.filter(|kept| kept.servers == names);
                let starting = match reused {
                    Some(kept) => Ok(kept.worker),
                    None => {
                        let workplace = Arc::clone(&self.workplace);
                        let rank = self.place.rank();
                        let restarted = memory.ran_code();
                        let interrupt = &self.interrupt;
                        Worker::new(interrupt, workplace, &installed, rank, restarted).await
                    }
                };
                match starting {
                    Ok(mut worker) => {
                        let task = Task {
                            text: asked.task.clone(),
                            mentions: Vec::new(),
                        };
                        let mut task = Some(task);
                        let answered = loop {
                            let sink = &mut watched;
                            let running =
                                self.run(&mut worker, &installed, &mut memory, task.take(), sink);
                            let answered = running.await;
                            // What the code sent as it answered keeps the task going
                            let sent = self.place.queue().queued().await;
                            if sent.is_empty() || !matches!(answered, Ok(Some(_))) {
                                break answered;
                            }
                            for update in sent {
                                let update = self.attached(update).await;
                                memory.steps.push(MemoryStep::Update(update));
                            }
                        };
                        let kept = Kept {
                            worker,
                            servers: names,
                            used: Instant::now(),
                        };
                        (answered, Some(kept))
                    }
                    Err(error) => (Err(error), None),
                }
            }
        };

        let stopped = watched.live.lock().expect("live").stopped.clone();
        let end = match answered {
            Ok(Some(answer)) => Ending::Answer(answer),
            // Its own interrupt can't tell the user from `agent.stop()` or the turn ending
            Ok(None) => stopped.unwrap_or(Ending::Interrupted {}),
            Err(error) => Ending::Error(error.to_string()),
        };

        // The agent sees again who stopped it, as the record says
        if end == (Ending::Stopped {}) {
            for step in &mut memory.steps[start..] {
                let MemoryStep::Action(action) = step else {
                    continue;
                };
                if action.error == Some(StepError::Interrupted) {
                    action.error = Some(StepError::Stopped);
                }
                for call in &mut action.calls {
                    call.error = match call.error.take() {
                        Some(CallError::Interrupted(at)) => Some(CallError::Stopped(at)),
                        other => other,
                    };
                }
            }
        }

        let added = &memory.steps[start..];
        let (usage, steps) = spent(added);
        let seconds = started.elapsed().as_secs_f64();
        let tokens = logged_tokens(&usage);
        let counted = format!("[steps={steps}, tokens={tokens}, elapsed={seconds:.1}s]");
        match &end {
            Ending::Answer(_) => info!("agent: answered {counted}"),
            Ending::Error(error) => warn!("agent: failed, {error} {counted}"),
            Ending::Interrupted {} => info!("agent: interrupted {counted}"),
            _ => info!("agent: stopped {counted}"),
        }

        watched.view.close(&end, added);
        // Programs a task leaves running end with its Python, as no turn of the agent follows them
        let kept = kept.filter(|kept| kept.worker.running().is_empty());
        (end, memory, kept)
    }
}

/// What the code gets for a task that ended
fn handed(end: &Ending) -> Value {
    match end.outcome() {
        Ok(answer) => json!({ "answer": answer }),
        Err(error) => json!({ "error": error }),
    }
}

/// What the tasks that the code of one call sends share
struct Launcher<'a> {
    views: Arc<dyn TaskViews>,
    records: &'a Records,
    servers: &'a [&'a Added],
    team: Arc<Team>,
}

impl Launcher<'_> {
    /// Adds the task to the agent's record and the call's, opens its view, numbered by its place in
    /// the call, and starts it, with `id` its agent's place in the team, which `session` interrupts
    fn launch(
        &self,
        agent: Agent,
        member: &mut Member,
        id: usize,
        asked: Asked,
        session: &Interrupt,
    ) {
        // The answer the task before left is announced when the turn next takes in what ended
        if let Some(returned) = member.returned() {
            self.team.earlier.lock().expect("earlier").push(returned);
        }

        let record = &mut member.record;
        // A resent task that failed before its first step leaves the conversation,
        // so its steps start where those of the new one do
        record.memory.withdraw(&asked.task);
        let start = record.memory.steps.len();
        let task = AgentTask {
            task: asked.task.clone(),
            start,
            end: Ending::Running {},
        };
        record.tasks.push(task);
        let number = record.tasks.len() - 1;

        let index = {
            let mut sent = self.records.lock().expect("records");
            let name = asked.name.clone();
            sent.push(Sent { name, number });
            sent.len() - 1
        };
        let view = self.views.open(index + 1, &asked.name, &asked.task);
        let watched = Watched::new(view, start);

        let turn = member.turn();
        member.stop.begin(turn);
        member.live = Arc::clone(&watched.live);
        member.taken = false;

        // The task works on a copy, so the session saves the conversation as it was before the task
        // until the task ends
        let memory = member.record.memory.clone();
        let kept = member.kept.take();
        let servers: Arc<[Added]> = self.servers.iter().map(|added| (*added).clone()).collect();
        let slots = Arc::clone(&self.team.slots);
        let launched = self.team.launched.fetch_add(1, Ordering::Relaxed) + 1;
        let span = error_span!("", agent = launched);
        let working = agent.work(asked, memory, kept, watched, servers, slots);
        let working = working.instrument(span);
        let mut working = task::spawn(working);
        member.abort = Some(working.abort_handle());

        let session = session.clone();
        let stop = member.stop.clone();
        let team = Arc::clone(&self.team);
        let finishing = async move {
            let joined = select! {
                joined = &mut working => joined,
                () = session.wait() => {
                    stop.cancel(turn);
                    working.await
                }
            };

            let mut members = team.members.lock().expect("members");
            let member = &mut members[id];
            // One that panics or is aborted ends with what its dropped view showed,
            // and its agent keeps the conversation it had before the task
            let end = match joined {
                Ok((end, memory, kept)) => {
                    member.record.memory = memory;
                    member.kept = kept;
                    end
                }
                Err(_) => Ending::Unfinished {},
            };
            member.record.tasks[number].end = end;
            let keeps = member.kept.is_some();
            // Beyond the limit the Python used least recently ends
            let idle = members
                .iter()
                .filter(|member| member.kept.is_some())
                .count();
            let mut evicted = None;
            if idle > MAX_IDLE_WORKERS {
                let used = |member: &&mut Member| member.kept.as_ref().map(|kept| kept.used);
                let holding = members.iter_mut().filter(|member| member.kept.is_some());
                let oldest = holding.min_by_key(used);
                evicted = oldest.and_then(|member| member.kept.take());
            }
            drop(members);
            drop(evicted);
            team.ended.send_modify(|count| *count += 1);

            if keeps {
                // A closed session ends its agents' Python at once rather than when this wakes
                let team = Arc::downgrade(&team);
                let idling = async move {
                    time::sleep(IDLE_WAIT).await;
                    let Some(team) = team.upgrade() else {
                        return;
                    };
                    let mut members = team.members.lock().expect("members");
                    let member = &mut members[id];
                    // A later task took the Python, and what it kept has its own wait
                    let unused = match member.turn() == turn {
                        true => member.kept.take(),
                        false => None,
                    };
                    drop(members);
                    drop(unused);
                };
                task::spawn(idling);
            }
        };
        task::spawn(finishing);
    }
}

/// The agents the session's code started, which keep their conversations for the session,
/// with their places as ids
pub(crate) struct Team {
    members: Mutex<Vec<Member>>,
    /// Answers of tasks whose agents took another before the turn took in that they ended
    earlier: Mutex<Vec<Returned>>,
    /// Counts the tasks that ended, which wakes everyone waiting for one
    ended: watch::Sender<usize>,
    /// Shared by all agents the session's code starts
    slots: Arc<Semaphore>,
    /// Counts all tasks the session's code sent, which numbers them in the log
    launched: AtomicUsize,
}

/// The Python an agent's last task left, for its next task to go on with
struct Kept {
    worker: Worker,
    /// The MCP servers it has, as named
    servers: Vec<String>,
    /// When its last task ended
    used: Instant,
}

/// An agent the code started
struct Member {
    /// Its conversation as of the end of its last task, and its tasks, the latest of which says it
    /// is running until it ends
    record: AgentRecord,
    /// The Python its last task left without programs, while it is among the `MAX_IDLE_WORKERS`
    /// used last and for at most `IDLE_WAIT`
    kept: Option<Kept>,
    inbox: Arc<Inbox>,
    live: Arc<Mutex<Activity>>,
    /// Interrupts its tasks alone, each a turn of its own numbered by `turn()`
    stop: Interrupt,
    abort: Option<AbortHandle>,
    /// The code took the answer of its latest task, or it was announced or stopped
    taken: bool,
}

impl Member {
    /// A restored agent has at least one task, and a new one gets its first as it starts
    fn new(record: AgentRecord) -> Self {
        Self {
            record,
            kept: None,
            inbox: Arc::default(),
            live: Arc::default(),
            stop: Interrupt::default(),
            abort: None,
            taken: true,
        }
    }

    fn name(&self) -> &str {
        &self.record.name
    }

    fn turn(&self) -> u64 {
        self.record.tasks.len() as u64
    }

    fn end(&self) -> &Ending {
        let latest = self.record.tasks.last().expect("task");
        &latest.end
    }

    fn running(&self) -> bool {
        *self.end() == (Ending::Running {})
    }

    /// How its latest task is announced, which it isn't once the code took the answer
    /// or the user interrupted it
    fn returned(&self) -> Option<Returned> {
        if self.taken {
            return None;
        }
        let end = self.end();
        if matches!(end, Ending::Interrupted {}) {
            return None;
        }

        let name = self.name().to_owned();
        let end = end.clone();
        Some(Returned { name, end })
    }
}

impl Default for Team {
    fn default() -> Self {
        let (ended, _) = watch::channel(0);
        Self {
            members: Mutex::default(),
            earlier: Mutex::default(),
            ended,
            slots: Arc::new(Semaphore::new(MAX_PARALLEL_AGENTS)),
            launched: AtomicUsize::default(),
        }
    }
}

impl Team {
    /// The agents of a saved session, each idle with its conversation
    pub(crate) fn restored(agents: Vec<AgentRecord>) -> Self {
        let mut members = Vec::new();
        for mut record in agents {
            if record.tasks.is_empty() {
                continue;
            }
            // A task that was running when libsmol exited never ends
            for task in record.tasks.iter_mut().filter(|task| task.running()) {
                task.end = Ending::Unfinished {};
            }
            members.push(Member::new(record));
        }

        Self {
            members: Mutex::new(members),
            ..Self::default()
        }
    }

    /// The agents as the session saves them
    pub(crate) fn records(&self) -> Vec<AgentRecord> {
        let members = self.members.lock().expect("members");
        let records = members.iter().map(|member| member.record.clone());
        records.collect()
    }

    /// The agents `names` asks for, or without names those at work or with an answer not yet taken
    fn awaited(&self, names: Vec<String>) -> Result<Vec<String>, String> {
        let members = self.members.lock().expect("members");
        if names.is_empty() {
            let outstanding = members
                .iter()
                .filter(|member| member.running() || !member.taken);
            let names = outstanding.map(|member| member.name().to_owned());
            return Ok(names.collect());
        }

        for name in &names {
            if !members.iter().any(|member| member.name() == name) {
                return Err(prompt::no_agent(name));
            }
        }
        Ok(names)
    }

    /// Takes the answers of the agents `names` once all have ended, or after `timeout` seconds
    /// or once the user queues a message those that have, with the status of the rest
    async fn wait(&self, names: &[String], timeout: Option<f64>, enqueued: Nudging<'_>) -> Value {
        let working = |member: &Member| {
            let named = names.iter().any(|name| name == member.name());
            named && member.running()
        };
        let ended = |members: &[Member]| !members.iter().any(working);
        let waiting = self.until(ended);

        // The worker refuses negative timeouts, and one too large for a duration waits forever
        let limit = timeout.and_then(|seconds| Duration::try_from_secs_f64(seconds).ok());
        let timed = async {
            match limit {
                Some(limit) => {
                    time::timeout(limit, waiting).await.ok();
                }
                None => waiting.await,
            }
        };
        // The message reaches the model once the call ends
        select! {
            () = timed => {}
            () = enqueued => {}
        }

        let mut members = self.members.lock().expect("members");
        let mut answers = Vec::new();
        let mut statuses = Vec::new();
        for name in names {
            let Some(member) = members.iter_mut().find(|member| member.name() == name) else {
                continue;
            };
            if member.running() {
                let live = member.live.lock().expect("live");
                let status = prompt::agent_status(&live.status);
                statuses.push(json!([name, status]));
                continue;
            }
            member.taken = true;
            let handed = handed(member.end());
            answers.push(json!([name, handed]));
        }
        json!({ "answers": answers, "working": statuses })
    }

    /// Stops the task of the agent `name` and returns once it has stopped
    async fn stop(&self, name: &str) -> Reply {
        {
            let mut members = self.members.lock().expect("members");
            let Some(member) = members.iter_mut().find(|member| member.name() == name) else {
                return Err(prompt::no_agent(name));
            };
            if member.running() {
                member.live.lock().expect("live").stopped = Some(Ending::Stopped {});
                member.stop.cancel(member.turn());
                member.taken = true;
            }
        }

        let stopped = |members: &[Member]| {
            !members
                .iter()
                .any(|member| member.name() == name && member.running())
        };
        self.until(stopped).await;
        Ok(Value::Null)
    }

    pub(crate) fn running(&self) -> bool {
        let members = self.members.lock().expect("members");
        members.iter().any(Member::running)
    }

    /// Resolves once `done` holds for the members, checking again whenever a task ends
    async fn until(&self, done: impl Fn(&[Member]) -> bool) {
        let mut ended = self.ended.subscribe();
        loop {
            let finished = {
                let members = self.members.lock().expect("members");
                done(&members)
            };
            if finished {
                return;
            }
            // The sender lives in the team and can't close first
            ended.changed().await.ok();
        }
    }
}

impl Turn<'_> {
    /// The agents whose tasks ended with answers the code hasn't taken and the user didn't interrupt,
    /// which count as taken from then on
    pub(crate) fn ended(&self) -> Vec<Returned> {
        let Some(team) = self.agent.place.team() else {
            return Vec::new();
        };

        let mut untaken = mem::take(&mut *team.earlier.lock().expect("earlier"));
        let mut members = team.members.lock().expect("members");
        for member in members.iter_mut().filter(|member| !member.running()) {
            // The user's interrupt reaches them at once,
            // so some end before `stopped()` stops the rest
            untaken.extend(member.returned());
            member.taken = true;
        }

        untaken
    }

    /// Waits for a task or a program the code left running to end, an interrupt
    /// or the client to queue a message, and returns the messages queued
    pub(crate) async fn news(&mut self) -> Vec<Task> {
        let Place::Main { team, queue, .. } = &self.agent.place else {
            return Vec::new();
        };

        let untaken = |members: &[Member]| {
            let untaken = |member: &Member| !member.running() && !member.taken;
            members.iter().any(untaken)
        };
        let ended = team.until(untaken);
        let exited = self.worker.ending();
        let enqueued = queue.enqueued();
        let queued = select! {
            biased;
            () = self.agent.interrupt.wait() => false,
            () = ended => false,
            () = exited => false,
            () = enqueued => true,
        };

        match queued {
            true => self.queued().await,
            false => Vec::new(),
        }
    }

    /// Announces the tasks that ended, then stops the rest, aborting those that take longer than
    /// `AGENTS_WAIT`, and saves the conversations they leave
    pub(crate) async fn stopped(&mut self) {
        // Taken before the rest are stopped, which aren't announced
        let done = self.ended();
        if !done.is_empty() {
            let step = DoneStep {
                agents: done,
                ..DoneStep::default()
            };
            self.memory.steps.push(MemoryStep::Done(step));
            self.sink.saved(self.memory);
        }

        let Some(team) = self.agent.place.team() else {
            return;
        };
        if !team.running() {
            return;
        }

        let user = self.agent.interrupt.triggered();
        {
            let members = team.members.lock().expect("members");
            for member in members.iter().filter(|member| member.running()) {
                let mut live = member.live.lock().expect("live");
                if !user && live.stopped.is_none() {
                    live.stopped = Some(Ending::Unfinished {});
                }
                drop(live);
                member.stop.cancel(member.turn());
            }
        }

        let settled = |members: &[Member]| !members.iter().any(Member::running);
        let stopping = team.until(settled);
        if time::timeout(AGENTS_WAIT, stopping).await.is_err() {
            {
                let members = team.members.lock().expect("members");
                for member in members.iter().filter(|member| member.running()) {
                    if let Some(abort) = &member.abort {
                        abort.abort();
                    }
                }
            }
            // Only the task waiting on each one writes how it ended, an answer that came first included
            team.until(settled).await;
        }

        self.ended();
        self.sink.saved(self.memory);
    }
}

/// Follows the events of an agent `agent()` started and shows its status,
/// at most every `SHOWN_EVERY` while text streams
struct Watched {
    live: Arc<Mutex<Activity>>,
    /// Where the steps of its task start in the agent's conversation
    start: usize,
    /// The status last shown and when
    shown: Option<(Status, Instant)>,
    view: Box<dyn TaskView>,
}

impl Watched {
    fn new(view: Box<dyn TaskView>, start: usize) -> Self {
        Self {
            live: Arc::default(),
            start,
            shown: None,
            view,
        }
    }

    /// Only streams are throttled, as an update such as starting to run may be the last for a while
    fn show(&mut self, streamed: bool) {
        let live = self.live.lock().expect("live");
        let status = &live.status;
        if let Some((last, at)) = &self.shown {
            let early = streamed && at.elapsed() < SHOWN_EVERY;
            let same = last.doing == status.doing
                && last.preview == status.preview
                && last.usage.tokens == status.usage.tokens;
            if early || same {
                return;
            }
        }
        self.view.shown(status);
        self.shown = Some((status.clone(), Instant::now()));
    }
}

impl Sink for Watched {
    fn event(&mut self, event: Event) {
        let streamed = matches!(
            event,
            Event::Thought(_) | Event::Text(_) | Event::Code(..) | Event::Output(..)
        );
        let mut live = self.live.lock().expect("live");
        let status = &mut live.status;
        status.preview = match event {
            Event::Thought(text) => {
                status.doing = prompt::THINKING.to_owned();
                tail(text.whole)
            }
            Event::Text(text) => {
                status.doing = prompt::WRITING.to_owned();
                tail(text.whole)
            }
            Event::Code(_, code) => {
                let line = first_line(code.whole);
                status.doing = format!("{} {line}", prompt::WRITING);
                tail(code.whole)
            }
            Event::Run(_, call) => {
                let line = first_line(&call.code);
                status.doing = format!("{} {line}", prompt::RUNNING);
                tail(&call.code)
            }
            Event::Output(_, _, output) => tail(output),
            Event::Restarted => {
                status.doing = prompt::PYTHON_RESTARTED.to_owned();
                String::new()
            }
            Event::Retry { reason, .. } => {
                status.doing = prompt::retrying(reason);
                String::new()
            }
            Event::Compact => {
                status.doing = prompt::COMPACTING.to_owned();
                String::new()
            }
            Event::Unread(..)
            | Event::RunEnd(..)
            | Event::StepEnd(_)
            | Event::StepFailed(_)
            | Event::CompactEnd(_)
            | Event::Programs(_) => return,
        };
        drop(live);

        self.show(streamed);
    }

    /// Its steps and tokens so far show as soon as a step or compaction is saved
    fn saved(&mut self, memory: &AgentMemory) {
        let (usage, steps) = spent(&memory.steps[self.start..]);
        {
            let mut live = self.live.lock().expect("live");
            live.status.usage = usage;
            live.status.steps = steps;
        }
        self.show(false);
    }

    fn tasks(&mut self, _: usize) -> Box<dyn TaskViews> {
        Box::new(Unseen)
    }
}

fn first_line(code: &str) -> String {
    let mut lines = code.lines().map(str::trim);
    let line = lines.find(|line| !line.is_empty()).unwrap_or_default();
    let Some((cut, _)) = line.char_indices().nth(MAX_LINE_CHARS) else {
        return line.to_owned();
    };
    format!("{}…", &line[..cut])
}

/// The last lines of `text` that aren't blank, read from its end since a stream can grow long
fn tail(text: &str) -> String {
    let mut chars = text.char_indices().rev();
    let cut = chars.nth(MAX_PREVIEW_CHARS - 1).map_or(0, |(at, _)| at);
    let kept = &text[cut..];

    let lines = kept.lines().rev().filter(|line| !line.trim().is_empty());
    let mut lines: Vec<&str> = lines.take(MAX_PREVIEW_LINES).collect();
    lines.reverse();
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use crate::agent::delegate::Team;
    use crate::agent::tests::scripted;
    use crate::agent::{Agent, Task};
    use crate::config::Kind;
    use crate::model::{
        ChatMessage, Finish, Finished, Generating, Model, ModelError, Purpose, Sealed, StreamDelta,
        TokenUsage,
    };
    use crate::prompt;
    use crate::steps::{
        spent, AgentMemory, Call, DoneStep, Ending, MemoryStep, Returned, MAX_OBSERVATION_CHARS,
    };
    use crate::tests::{cancel_soon, content};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::time;

    type Requests = Arc<Mutex<Vec<Vec<ChatMessage>>>>;

    /// The main agent sends four agents a task each, which answer with their task, except one
    /// that crashes, and agents that start an agent or add with the MCP server answer with what
    /// happened, each reply with a reasoning item
    struct Teamed(Requests);

    impl Model for Teamed {
        fn generate<'a>(
            &'a self,
            conversation: &'a [ChatMessage],
            _: Purpose,
            on_delta: &'a mut (dyn FnMut(StreamDelta) + Send),
        ) -> Generating<'a> {
            self.0.lock().expect("requests").push(conversation.to_vec());
            let system = content(&conversation[0]);
            let task = content(&conversation[1]);
            let last = conversation.last().map(content).unwrap_or_default();
            let first = conversation.len() == 2;
            let (text, code) = match (system.contains("returned to the agent"), first) {
                (true, true) if task == "Start another agent" => {
                    (String::new(), Some("print(agent('deeper', 'Go'))"))
                }
                (true, true) if task == "Add with the server" => {
                    (String::new(), Some("print(fake.add(a=2, b=3))"))
                }
                (true, true) if task == "Crash" => panic!("{task}"),
                (true, true) => (format!("Answer: {task}"), None),
                (true, false) => (last, None),
                (false, true) => {
                    let code = "for name, task in [('cafés', 'Count the cafés'), ('crash', 'Crash'), ('naïve', 'Spell naïve'), ('deeper', 'Start another agent')]:\n    agent(name, task)\nprint(agent.wait())";
                    (String::new(), Some(code))
                }
                (false, false) => ("All three answered.".to_owned(), None),
            };

            Box::pin(async move {
                let item =
                    json!({ "type": "reasoning", "id": "rs_1", "encrypted_content": "gAAAA-café" });
                let sealed = Sealed {
                    provider: "mock".to_owned(),
                    kind: Kind::Responses,
                    items: vec![item],
                    ends: vec![0],
                };
                on_delta(StreamDelta::Sealed(&sealed));
                match code {
                    Some(code) => {
                        on_delta(StreamDelta::CallCode(code));
                        on_delta(StreamDelta::CallEnd(Ok(())));
                    }
                    None => on_delta(StreamDelta::Content(&text)),
                }
                let usage = Some(TokenUsage::new(10, 5));
                Ok(Finished {
                    finish: Finish::Stop,
                    usage,
                })
            })
        }
    }

    #[test]
    fn helpers_answer() {
        let requests = Requests::default();
        let model = Teamed(Arc::clone(&requests));
        let mut agent = scripted(model, Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "Split the work").expect("run");
        assert_eq!(answer.as_deref(), Some("All three answered."));

        let MemoryStep::Action(step) = &memory.steps[1] else {
            panic!("{:?}", memory.steps);
        };
        let result = step.calls[0].result.as_ref().expect("ran");
        let nested =
            "NameError: name 'agent' is not defined\n  line 1: print(agent('deeper', 'Go'))";
        let crashed = "RuntimeError('agent.wait: stopped before it answered')";
        let printed = format!(
            "{{'cafés': 'Answer: Count the cafés', 'crash': {crashed}, \
             'naïve': 'Answer: Spell naïve', 'deeper': {nested:?}}}"
        );
        assert_eq!(result.printed.as_deref(), Some(printed.as_str()));
        let sent: Vec<(&str, usize)> = result
            .agents
            .iter()
            .map(|sent| (sent.name.as_str(), sent.number))
            .collect();
        assert_eq!(
            sent,
            [("cafés", 0), ("crash", 0), ("naïve", 0), ("deeper", 0)]
        );
        let team = agent.agent.place.team().expect("team");
        let records = team.records();
        let tasks: Vec<&str> = records
            .iter()
            .map(|record| &*record.tasks[0].task)
            .collect();
        let asked = [
            "Count the cafés",
            "Crash",
            "Spell naïve",
            "Start another agent",
        ];
        assert_eq!(tasks, asked);
        // The crashed agent ends as its dropped view showed
        assert_eq!(records[1].tasks[0].end, Ending::Unfinished {});
        // The records hold the conversations of their agents, which go back to the provider
        for record in [&records[0], &records[2], &records[3]] {
            assert!(
                matches!(record.tasks[0].end, Ending::Answer(_)),
                "{record:?}"
            );
            let (usage, _) = spent(&record.memory.steps);
            assert!(usage.tokens.is_some(), "{record:?}");
            for step in &record.memory.steps {
                if let MemoryStep::Action(action) = step {
                    assert!(action.sealed.is_some(), "{action:?}");
                }
            }
        }

        // Agents started by agent() don't see it
        let requests = requests.lock().expect("requests");
        let main = content(&requests[0][0]);
        assert!(main.contains("\nagent("), "{main}");
        let helped = requests.iter().map(|request| content(&request[0]));
        let helper = helped
            .into_iter()
            .find(|text| *text != main)
            .expect("helper");
        assert!(!helper.contains("agent("), "{helper}");
    }

    /// The main agent runs each of the codes in turn, split into calls at lines `#---`, and with
    /// an empty one or none answers with the number of its reply, while an agent answers with the
    /// last line it was sent that isn't empty or in brackets, after the seconds that follow "after" in it,
    /// or runs the code that follows "run " and answers with what that printed
    struct Background(Vec<&'static str>, Requests);

    impl Model for Background {
        fn generate<'a>(
            &'a self,
            conversation: &'a [ChatMessage],
            _: Purpose,
            on_delta: &'a mut (dyn FnMut(StreamDelta) + Send),
        ) -> Generating<'a> {
            self.1.lock().expect("requests").push(conversation.to_vec());
            let system = content(&conversation[0]);
            let main = !system.contains("returned to the agent");
            let replied = |message: &&ChatMessage| matches!(message, ChatMessage::Assistant { .. });
            let replies = conversation.iter().filter(replied).count();
            let asked = |message: &&ChatMessage| matches!(message, ChatMessage::User(_));
            let texts: Vec<String> = conversation.iter().filter(asked).map(content).collect();
            let lines = texts.iter().rev().flat_map(|text| text.lines().rev());
            let sent = lines
                .into_iter()
                .find(|line| !line.is_empty() && !line.starts_with('['));
            let sent = sent.unwrap_or_default().to_owned();
            let ran = matches!(conversation.last(), Some(ChatMessage::Tool { .. }));

            let (text, code) = match main {
                true => {
                    let code = self.0.get(replies).filter(|code| !code.is_empty());
                    (
                        format!("Answer {replies}"),
                        code.map(|code| code.to_string()),
                    )
                }
                false if ran => (conversation.last().map(content).unwrap_or_default(), None),
                false => match sent.strip_prefix("run ") {
                    Some(code) => (String::new(), Some(code.to_owned())),
                    None => (format!("Answer: {sent}"), None),
                },
            };
            let after = sent.split(" after ").nth(1);
            let delay = after.and_then(|seconds| seconds.parse().ok());

            Box::pin(async move {
                if let Some(seconds) = delay.filter(|_| !main) {
                    time::sleep(Duration::from_secs_f64(seconds)).await;
                }
                match code.as_deref() {
                    // Stands for a request the server refuses
                    Some("refused") => return Err(ModelError::new("http 400: bad request")),
                    Some(code) => {
                        for part in code.split("\n#---\n") {
                            on_delta(StreamDelta::CallCode(part));
                            on_delta(StreamDelta::CallEnd(Ok(())));
                        }
                    }
                    None => on_delta(StreamDelta::Content(&text)),
                }
                Ok(Finished {
                    finish: Finish::Stop,
                    usage: None,
                })
            })
        }
    }

    fn ran(memory: &AgentMemory, index: usize) -> &Call {
        let MemoryStep::Action(step) = &memory.steps[index] else {
            panic!("{:?}", memory.steps);
        };
        &step.calls[0]
    }

    fn printed(memory: &AgentMemory, index: usize) -> &str {
        let result = ran(memory, index).result.as_ref().expect("ran");
        result.printed.as_deref().unwrap_or_default()
    }

    /// The agent each task the call sent went to, with how the task ended
    fn ends(agent: &Agent, call: &Call) -> Vec<(String, Ending)> {
        let result = call.result.as_ref().expect("ran");
        let records = agent.place.team().expect("team").records();
        let mut ends = Vec::new();
        for sent in &result.agents {
            let found = records.iter().find(|record| record.name == sent.name);
            let record = found.expect("agent");
            let end = record.tasks[sent.number].end.clone();
            ends.push((sent.name.clone(), end));
        }
        ends
    }

    /// What each request an agent made was sent after its system prompt, in order
    fn sent(requests: &Requests) -> Vec<Vec<String>> {
        let requests = requests.lock().expect("requests");
        let helped = requests
            .iter()
            .filter(|request| content(&request[0]).contains("returned to the agent"));
        let told = helped.map(|request| request[1..].iter().map(content).collect());
        told.collect()
    }

    /// An answer doesn't end the turn while an agent works, and one the code didn't take
    /// comes with its answer once it ends, while one it took comes again by name
    #[test]
    fn answers_announced() {
        let code = "agent('cafés', 'Count the cafés')\n\
            agent('naïve', 'Spell naïve after 0.5')\n\
            print(agent.wait('cafés', timeout=10))\n\
            print(agent.wait('cafés'))";
        let requests = Requests::default();
        let model = Background(vec![code], Arc::clone(&requests));
        let mut agent = scripted(model, Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "Split the work").expect("run");
        assert_eq!(answer.as_deref(), Some("Answer 2"));

        let taken = "{'cafés': 'Answer: Count the cafés'}";
        assert_eq!(printed(&memory, 1), format!("{taken}\n{taken}"));
        let cafés = Ending::Answer("Answer: Count the cafés".to_owned());
        let naïve = Ending::Answer("Answer: Spell naïve after 0.5".to_owned());
        let sent = [
            ("cafés".to_owned(), cafés),
            ("naïve".to_owned(), naïve.clone()),
        ];
        assert_eq!(ends(&agent.agent, ran(&memory, 1)), sent);
        let returned = Returned {
            name: "naïve".to_owned(),
            end: naïve,
        };
        let done = DoneStep {
            agents: vec![returned],
            ..DoneStep::default()
        };
        assert_eq!(memory.steps[3], MemoryStep::Done(done.clone()));
        assert_eq!(memory.steps.len(), 5, "{:?}", memory.steps);

        let requests = requests.lock().expect("requests");
        let last = requests.last().and_then(|request| request.last());
        let told = last.map(content).unwrap_or_default();
        assert_eq!(told, prompt::done(&done));
    }

    /// A long answer the code didn't take comes cut, and whole from `agent.wait()` by name
    #[test]
    fn long_answer_cut() {
        let codes = vec![
            "agent('scribe', 'Copy ' + 'café ' * 4000 + 'after 0.5')\nprint('sent')",
            "",
            "answer = agent.wait('scribe')['scribe']\nprint(len(answer))",
        ];
        let requests = Requests::default();
        let model = Background(codes, Arc::clone(&requests));
        let mut agent = scripted(model, Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        agent.turn(&mut memory, "Copy the cafés").expect("run");

        let MemoryStep::Done(done) = &memory.steps[3] else {
            panic!("{:?}", memory.steps);
        };
        let answer = done.agents[0].end.outcome().expect("answer");
        let whole = answer.chars().count();
        assert!(whole > MAX_OBSERVATION_CHARS, "{whole}");
        assert_eq!(printed(&memory, 4), whole.to_string());

        let requests = requests.lock().expect("requests");
        let mut main = requests
            .iter()
            .filter(|request| !content(&request[0]).contains("returned to the agent"));
        let after = main.nth(2).expect("request");
        let told = content(after.last().expect("message"));
        assert_eq!(told, prompt::done(done));
        assert!(told.chars().count() < whole, "{told}");
    }

    /// A message the user queues ends the wait early, and reaches the model after the call
    #[test]
    fn wait_cut_short() {
        let codes = vec![
            "agent('slow', 'Wait after 30')\nprint(agent.wait('slow'))",
            "agent.stop('slow')",
        ];
        let requests = Requests::default();
        let model = Background(codes, Arc::clone(&requests));
        let mut agent = scripted(model, Box::new(|_| {}));
        *agent.queue.nudge.lock().expect("nudge") = Some(Duration::from_millis(300));
        let mut pulls = 0;
        let take = move || {
            pulls += 1;
            let update = Task {
                text: "Stop waiting for the café".to_owned(),
                mentions: Vec::new(),
            };
            match pulls {
                1 => vec![update],
                _ => Vec::new(),
            }
        };
        *agent.queue.updates.lock().expect("updates") = Some(Box::new(take));

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "Wait for it").expect("run");
        assert_eq!(answer.as_deref(), Some("Answer 2"));

        let shown = printed(&memory, 1);
        assert!(
            shown.starts_with("slow: running for 0s, step 1: "),
            "{shown}"
        );
        assert!(shown.ends_with("\n{}"), "{shown}");
        let MemoryStep::Update(update) = &memory.steps[2] else {
            panic!("{:?}", memory.steps);
        };
        assert_eq!(update.text, "Stop waiting for the café");
    }

    /// What the code sends an agent at work joins its task, also after it answered
    #[test]
    fn updates_joined() {
        let code = "import time\n\
            agent('naïve', 'Spell naïve after 1')\n\
            time.sleep(0.3)\n\
            agent('naïve', 'Spell café too')\n\
            print(agent.wait('naïve'))";
        let requests = Requests::default();
        let model = Background(vec![code], Arc::clone(&requests));
        let mut agent = scripted(model, Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        agent.turn(&mut memory, "Spell them").expect("run");
        assert_eq!(printed(&memory, 1), "{'naïve': 'Answer: Spell café too'}");
        let result = ran(&memory, 1).result.as_ref().expect("ran");
        assert_eq!(result.agents.len(), 1, "{result:?}");

        let expected = [
            vec!["Spell naïve after 1"],
            vec![
                "Spell naïve after 1",
                "Answer: Spell naïve after 1",
                "Spell café too",
            ],
        ];
        assert_eq!(sent(&requests), expected);
    }

    /// `agent.stop()` ends a task without announcing it, and the agent takes the next one
    /// knowing who stopped it
    #[test]
    fn stopped_by_code() {
        let codes = vec![
            "agent('slow', 'Wait after 30')",
            "agent.stop('slow')\n\
             agent('slow', 'Spell naïve')\n\
             print(agent.wait('slow', timeout=10))\n\
             try:\n    agent.stop('menu')\nexcept ValueError as error:\n    print(error)",
        ];
        let requests = Requests::default();
        let model = Background(codes, Arc::clone(&requests));
        let mut agent = scripted(model, Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "Spell it").expect("run");
        assert_eq!(answer.as_deref(), Some("Answer 2"));

        let shown = "{'slow': 'Answer: Spell naïve'}\nagent.stop: name: menu: no such agent";
        assert_eq!(printed(&memory, 2), shown);
        let stopped = [("slow".to_owned(), Ending::Stopped {})];
        assert_eq!(ends(&agent.agent, ran(&memory, 1)), stopped);
        let again = Ending::Answer("Answer: Spell naïve".to_owned());
        assert_eq!(
            ends(&agent.agent, ran(&memory, 2)),
            [("slow".to_owned(), again)]
        );
        let done = |step: &MemoryStep| matches!(step, MemoryStep::Done(_));
        assert!(!memory.steps.iter().any(done), "{:?}", memory.steps);

        let told = sent(&requests).pop().expect("requests");
        let interrupted = format!("[{}]", prompt::REPLY_STOPPED);
        let expected = ["Wait after 30", interrupted.as_str(), "Spell naïve"];
        assert_eq!(told, expected);
    }

    /// The agents of a saved conversation come back with their conversations as they were
    #[test]
    fn agents_restored() {
        let codes = vec![
            "agent('copy', 'Repeat the task')\nagent('calc', 'run x = 41')\nagent.wait()",
            "agent('calc', 'run print(x + 1)')\nagent.wait()",
        ];
        let model = Background(codes, Requests::default());
        let mut agent = scripted(model, Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        agent.turn(&mut memory, "Copy and count").expect("run");

        let team = agent.agent.place.team().expect("team");
        let mut saved = team.records();
        let names: Vec<&str> = saved.iter().map(|record| record.name.as_str()).collect();
        assert_eq!(names, ["copy", "calc"]);
        let restored = Team::restored(saved.clone());
        assert_eq!(restored.records(), saved);

        // A task saved as running, as when libsmol exits, comes back unfinished
        saved[1].tasks[1].end = Ending::Running {};
        let restored = Team::restored(saved);
        assert!(!restored.running());
        let records = restored.records();
        assert_eq!(records[1].tasks[1].end, Ending::Unfinished {});
    }

    /// An interrupt ends the turn and the agents at work with it
    #[test]
    fn interrupt_stops_all() {
        // The code outlasts the agent, which the interrupt reaches at once
        let code = "import time\n\
            agent('slow', 'Wait after 30')\n\
            try:\n    time.sleep(30)\nexcept KeyboardInterrupt:\n    time.sleep(1)";
        let model = Background(vec![code], Requests::default());
        let mut agent = scripted(model, Box::new(|_| {}));
        cancel_soon(&agent.agent.interrupt, 0);

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "Wait for it").expect("run");
        assert_eq!(answer, None);
        let interrupted = [("slow".to_owned(), Ending::Interrupted {})];
        assert_eq!(ends(&agent.agent, ran(&memory, 1)), interrupted);
        let done = |step: &MemoryStep| matches!(step, MemoryStep::Done(_));
        assert!(!memory.steps.iter().any(done), "{:?}", memory.steps);
    }
}
