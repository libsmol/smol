use crate::agent::{
    logged_tokens, Agent, Asking, Delegating, Event, HelperView, Helpers, Nudging, Place, Queue,
    Sink, Status, Task, Turn, Unseen,
};
use crate::prompt;
use crate::spawn::SHOWN_EVERY;
use crate::steps::{
    spent, AgentMemory, CallError, DoneStep, Ending, Helper, MemoryStep, Returned, StepError,
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
use tokio::sync::{watch, Notify, Semaphore};
use tokio::task::{self, AbortHandle};
use tokio::time;
use tracing::{error_span, info, warn, Instrument};

pub const MAX_PARALLEL_AGENTS: usize = 8;
const MAX_PREVIEW_LINES: usize = 4;
const MAX_PREVIEW_CHARS: usize = 800;
const MAX_LINE_CHARS: usize = 100;
/// Agents get this long to stop by themselves when the turn ends,
/// and a worker of theirs may take `STOP_WAIT`
const HELPERS_WAIT: Duration = Duration::from_secs(5);
/// Idle agents keep their Python, at most this many, the ones used last
const MAX_IDLE_WORKERS: usize = 4;
/// An idle agent's Python ends after this long unused
const IDLE_WAIT: Duration = match cfg!(test) {
    true => Duration::from_secs(2),
    false => Duration::from_secs(30 * 60),
};

/// The records of the tasks the code of one call sent, in the order their views opened,
/// which numbers them on replay too
pub(crate) struct Records {
    pub(crate) helpers: Mutex<Vec<Helper>>,
    /// Where the call's step lands in memory,
    /// which only grows at the end until the turn has filed its tasks' records
    step: usize,
    call: usize,
}

impl Records {
    pub(crate) fn new(step: usize, call: usize) -> Self {
        Self {
            helpers: Mutex::default(),
            step,
            call,
        }
    }
}

/// Where the record of a task sits in memory
#[derive(Clone, Copy)]
struct Seat {
    step: usize,
    call: usize,
    agent: usize,
}

/// What a task at work shares with the team
#[derive(Default)]
struct Activity {
    status: Status,
    /// How it ends when `agent.stop()` or the end of the turn rather than the user stopped it
    stopped: Option<Ending>,
}

/// What the code sends an agent at work, which takes it as `[Task update]` after its current call
#[derive(Default)]
struct Inbox {
    sent: Mutex<Vec<String>>,
    arrived: Notify,
}

impl Queue for Inbox {
    fn queued(&self) -> Asking<'_> {
        let mut inbox = self.sent.lock().expect("inbox");
        let sent = mem::take(&mut *inbox).into_iter();
        let tasks = sent.map(|text| Task {
            text,
            mentions: Vec::new(),
        });
        Box::pin(future::ready(tasks.collect()))
    }

    fn enqueued(&self) -> Nudging<'_> {
        Box::pin(self.arrived.notified())
    }
}

/// Shows the client which agents the code waits for until it is dropped
struct Awaiting(Arc<dyn Helpers>);

impl Drop for Awaiting {
    fn drop(&mut self) {
        self.0.waiting(&[]);
    }
}

impl Agent {
    /// Runs the calls that the code of one call makes to `agent()` and its functions,
    /// and keeps the record of each task it sends in `records`
    pub(crate) fn delegate<'a>(
        &'a self,
        servers: &'a [&'a Added],
        views: Box<dyn Helpers>,
        records: &Arc<Records>,
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
            records: Arc::clone(records),
            servers,
            team: Arc::clone(team),
        };
        let delegate = move |call: AgentCall| -> Calling {
            match call {
                AgentCall::Send(asked) => {
                    let sent = self.send(&launcher, asked);
                    let called = Called::from(sent);
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
                        Called::from(waited)
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
    fn send(&self, launcher: &Launcher, asked: Asked) -> Reply {
        let Asked { name, task } = asked;
        let mut members = launcher.team.members.lock().expect("members");
        let found = members.iter().position(|member| member.name() == name);
        if let Some(id) = found {
            let member = &members[id];
            if member.helper.running() {
                member.inbox.sent.lock().expect("inbox").push(task);
                member.inbox.arrived.notify_one();
                return Ok(Value::Null);
            }
        }

        let record = Helper {
            task,
            name: Some(name),
            end: Ending::Running {},
            memory: AgentMemory::default(),
            resendable: true,
        };
        let id = match found {
            Some(id) => id,
            None => {
                let member = Member::new(record.clone());
                members.push(member);
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
        launcher.launch(agent, member, id, record, &self.interrupt);
        Ok(Value::Null)
    }

    /// Works on the task `asked` records in the agent's conversation `memory`, on the Python its
    /// last task kept if that has the same MCP servers, and returns the task's final record,
    /// the conversation and the Python to keep, none once the task leaves programs running
    async fn help(
        self,
        asked: Helper,
        mut memory: AgentMemory,
        kept: Option<Kept>,
        mut watched: Watched,
        servers: Arc<[Added]>,
        slots: Arc<Semaphore>,
    ) -> (Helper, AgentMemory, Option<Kept>) {
        let started = Instant::now();
        // A resent task that failed before its first step leaves the conversation here rather than
        // in the run, so the task's steps start at `start`
        memory.withdraw(&asked.task);
        let start = memory.steps.len();
        watched.start = start;

        // One still waiting for a slot stops on an interrupt without starting a worker
        let slot = select! {
            biased;
            () = self.interrupt.wait() => None,
            slot = slots.acquire() => Some(slot.expect("slots")),
        };
        let (answered, kept) = match slot {
            None => (Ok(None), kept),
            Some(_slot) => {
                let name = asked.name.as_deref().unwrap_or_default();
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
                        let delegation = self.place.delegation();
                        let restarted = memory.ran_code();
                        let interrupt = &self.interrupt;
                        Worker::new(interrupt, workplace, &installed, delegation, restarted).await
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
                    if call.error == Some(CallError::Interrupted) {
                        call.error = Some(CallError::Stopped);
                    }
                }
            }
        }

        let added = AgentMemory {
            steps: memory.steps[start..].to_vec(),
        };
        let (usage, steps) = spent(&added.steps);
        let seconds = started.elapsed().as_secs_f64();
        let tokens = logged_tokens(&usage);
        let counted = format!("[steps={steps}, tokens={tokens}, elapsed={seconds:.1}s]");
        match &end {
            Ending::Answer(_) => info!("agent: answered {counted}"),
            Ending::Error(error) => warn!("agent: failed, {error} {counted}"),
            Ending::Interrupted {} => info!("agent: interrupted {counted}"),
            _ => info!("agent: stopped {counted}"),
        }

        let helper = Helper {
            end,
            memory: added,
            ..asked
        };
        watched.view.close(&helper);
        // Programs a task leaves running end with its Python, as no turn of the agent follows them
        let kept = kept.filter(|kept| kept.worker.running().is_empty());
        (helper, memory, kept)
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
    views: Arc<dyn Helpers>,
    records: Arc<Records>,
    servers: &'a [&'a Added],
    team: Arc<Team>,
}

impl Launcher<'_> {
    /// Keeps the record of the task and opens its view, numbered by the record's place, and starts
    /// it, with `id` its agent's place in the team, which `session` interrupts too
    fn launch(
        &self,
        agent: Agent,
        member: &mut Member,
        id: usize,
        record: Helper,
        session: &Interrupt,
    ) {
        let index = {
            let mut helpers = self.records.helpers.lock().expect("records");
            helpers.push(record.clone());
            helpers.len() - 1
        };
        let view = self.views.open(index + 1, &record);
        let watched = Watched::new(view);

        // The task before keeps its record and announcement until the turn files them
        if let Some(seat) = member.seat.take() {
            let returned = member.returned();
            let earlier = (seat, member.helper.clone(), returned);
            self.team.earlier.lock().expect("earlier").push(earlier);
        }
        member.tasks += 1;
        member.stop.begin(member.tasks);
        member.live = Arc::clone(&watched.live);
        member.helper = record.clone();
        member.taken = false;
        member.seat = Some(Seat {
            step: self.records.step,
            call: self.records.call,
            agent: index,
        });

        let memory = mem::take(&mut member.memory);
        let kept = member.kept.take();
        let servers: Arc<[Added]> = self.servers.iter().map(|added| (*added).clone()).collect();
        let slots = Arc::clone(&self.team.slots);
        let launched = self.team.launched.fetch_add(1, Ordering::Relaxed) + 1;
        let span = error_span!("", agent = launched);
        let helping = agent.help(record.clone(), memory, kept, watched, servers, slots);
        let mut helping = task::spawn(helping.instrument(span));
        member.abort = Some(helping.abort_handle());

        // One that panics or is aborted ends with the record its dropped view showed,
        // and its agent starts over
        let unfinished = Helper {
            end: Ending::Unfinished {},
            ..record
        };
        let session = session.clone();
        let stop = member.stop.clone();
        let turn = member.tasks;
        let team = Arc::clone(&self.team);
        let finishing = async move {
            let joined = select! {
                joined = &mut helping => joined,
                () = session.wait() => {
                    stop.cancel(turn);
                    helping.await
                }
            };
            let (helper, memory, kept) =
                joined.unwrap_or((unfinished, AgentMemory::default(), None));
            let keeps = kept.is_some();

            let mut members = team.members.lock().expect("members");
            let member = &mut members[id];
            member.helper = helper;
            member.memory = memory;
            member.kept = kept;
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
                    let unused = match member.tasks == turn {
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
    /// Tasks whose agents took another before the turn filed their records,
    /// with how they are announced
    earlier: Mutex<Vec<(Seat, Helper, Option<Returned>)>>,
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

/// An agent the code started, named by its records
struct Member {
    /// The record of its latest task, which says it is running until the task ends
    helper: Helper,
    /// Its conversation, which its task holds while it works
    memory: AgentMemory,
    /// The Python its last task left without programs, while it is among the `MAX_IDLE_WORKERS`
    /// used last and for at most `IDLE_WAIT`
    kept: Option<Kept>,
    inbox: Arc<Inbox>,
    live: Arc<Mutex<Activity>>,
    /// Interrupts its tasks alone, each a turn of its own numbered by `tasks`
    stop: Interrupt,
    tasks: u64,
    abort: Option<AbortHandle>,
    /// The code took the answer of its latest task, or it was announced or stopped
    taken: bool,
    /// Where the turn files the record of its latest task, taken once it has
    seat: Option<Seat>,
}

impl Member {
    fn new(helper: Helper) -> Self {
        Self {
            helper,
            memory: AgentMemory::default(),
            kept: None,
            inbox: Arc::default(),
            live: Arc::default(),
            stop: Interrupt::default(),
            tasks: 0,
            abort: None,
            taken: true,
            seat: None,
        }
    }

    fn name(&self) -> &str {
        self.helper.name.as_deref().unwrap_or_default()
    }

    /// How its latest task is announced, which it isn't once the code took the answer
    /// or the user interrupted it
    fn returned(&self) -> Option<Returned> {
        let interrupted = matches!(self.helper.end, Ending::Interrupted {});
        if self.taken || interrupted {
            return None;
        }

        let name = self.name().to_owned();
        let end = self.helper.end.clone();
        Some(Returned::Ended { name, end })
    }

    fn unfiled(&self) -> bool {
        !self.helper.running() && self.seat.is_some()
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
    /// The agents whose tasks the session's conversation `history` records, each idle with its
    /// conversation, except those saved before agents kept their conversations
    pub(crate) fn restored(history: &[MemoryStep]) -> Self {
        let mut members: Vec<Member> = Vec::new();
        for step in history {
            let MemoryStep::Action(action) = step else {
                continue;
            };
            let results = action.calls.iter().filter_map(|call| call.result.as_ref());
            for helper in results.flat_map(|result| &result.agents) {
                let Some(name) = helper.name.as_deref().filter(|_| helper.resendable) else {
                    continue;
                };
                let found = members.iter().position(|member| member.name() == name);
                let id = match found {
                    Some(id) => id,
                    None => {
                        let member = Member::new(helper.clone());
                        members.push(member);
                        members.len() - 1
                    }
                };

                let member = &mut members[id];
                // A resent task left the conversation as it did when the agent took the new one
                if let Some(MemoryStep::Task(task)) = helper.memory.steps.first() {
                    member.memory.withdraw(&task.text);
                }
                let steps = helper.memory.steps.iter().cloned();
                member.memory.steps.extend(steps);
                // A task that was running when libsmol exited never ends
                let end = match &helper.end {
                    Ending::Running {} => Ending::Unfinished {},
                    end => end.clone(),
                };
                member.helper = Helper {
                    end,
                    memory: AgentMemory::default(),
                    ..helper.clone()
                };
            }
        }

        Self {
            members: Mutex::new(members),
            ..Self::default()
        }
    }

    /// The agents `names` asks for, or without names those at work or with an answer not yet taken
    fn awaited(&self, names: Vec<String>) -> Result<Vec<String>, String> {
        let members = self.members.lock().expect("members");
        if names.is_empty() {
            let outstanding = members
                .iter()
                .filter(|member| member.helper.running() || !member.taken);
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
    async fn wait(&self, names: &[String], timeout: Option<f64>, enqueued: Nudging<'_>) -> Reply {
        let working = |member: &Member| {
            let named = names.iter().any(|name| name == member.name());
            named && member.helper.running()
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
            if member.helper.running() {
                let live = member.live.lock().expect("live");
                let status = prompt::agent_status(&live.status);
                statuses.push(json!([name, status]));
                continue;
            }
            member.taken = true;
            let handed = handed(&member.helper.end);
            answers.push(json!([name, handed]));
        }
        Ok(json!({ "answers": answers, "working": statuses }))
    }

    /// Stops the task of the agent `name` and returns once it has stopped
    async fn stop(&self, name: &str) -> Reply {
        {
            let mut members = self.members.lock().expect("members");
            let Some(member) = members.iter_mut().find(|member| member.name() == name) else {
                return Err(prompt::no_agent(name));
            };
            if member.helper.running() {
                member.live.lock().expect("live").stopped = Some(Ending::Stopped {});
                member.stop.cancel(member.tasks);
                member.taken = true;
            }
        }

        let stopped = |members: &[Member]| {
            !members
                .iter()
                .any(|member| member.name() == name && member.helper.running())
        };
        self.until(stopped).await;
        Ok(Value::Null)
    }

    pub(crate) fn running(&self) -> bool {
        let members = self.members.lock().expect("members");
        members.iter().any(|member| member.helper.running())
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
    /// Saves the records of tasks that ended, and returns the agents whose answers the code
    /// hasn't taken and the user didn't interrupt, which count as taken from then on
    pub(crate) fn ended(&mut self) -> Vec<Returned> {
        let Some(team) = self.agent.place.team() else {
            return Vec::new();
        };

        let mut records = Vec::new();
        let mut untaken = Vec::new();
        let earlier = mem::take(&mut *team.earlier.lock().expect("earlier"));
        for (seat, record, returned) in earlier {
            untaken.extend(returned);
            records.push((seat, record));
        }
        let mut members = team.members.lock().expect("members");
        for member in members.iter_mut().filter(|member| !member.helper.running()) {
            let Some(seat) = member.seat.take() else {
                continue;
            };
            // The user's interrupt reaches them at once,
            // so some end before `stopped()` stops the rest
            untaken.extend(member.returned());
            member.taken = true;
            let memory = mem::take(&mut member.helper.memory);
            let record = Helper {
                memory,
                ..member.helper.clone()
            };
            records.push((seat, record));
        }
        drop(members);
        if records.is_empty() {
            return untaken;
        }

        for (seat, record) in records {
            let Some(MemoryStep::Action(step)) = self.memory.steps.get_mut(seat.step) else {
                continue;
            };
            let Some(call) = step.calls.get_mut(seat.call) else {
                continue;
            };
            let Some(result) = &mut call.result else {
                continue;
            };
            if let Some(kept) = result.agents.get_mut(seat.agent) {
                *kept = record;
            }
        }
        self.sink.saved(self.memory);

        untaken
    }

    /// Waits for a task or a program the code left running to end, an interrupt
    /// or the client to queue a message, and returns the messages queued
    pub(crate) async fn news(&mut self) -> Vec<Task> {
        let Place::Main { team, queue, .. } = &self.agent.place else {
            return Vec::new();
        };

        let unfiled = |members: &[Member]| members.iter().any(Member::unfiled);
        let ended = team.until(unfiled);
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

    /// Announces the tasks that ended, then stops the rest and saves their records, aborting those
    /// that take longer than `HELPERS_WAIT`
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

        if let Some(team) = self.agent.place.team() {
            let user = self.agent.interrupt.triggered();
            {
                let members = team.members.lock().expect("members");
                for member in members.iter().filter(|member| member.helper.running()) {
                    let mut live = member.live.lock().expect("live");
                    if !user && live.stopped.is_none() {
                        live.stopped = Some(Ending::Unfinished {});
                    }
                    drop(live);
                    member.stop.cancel(member.tasks);
                }
            }

            let settled =
                |members: &[Member]| !members.iter().any(|member| member.helper.running());
            let stopping = team.until(settled);
            if time::timeout(HELPERS_WAIT, stopping).await.is_err() {
                {
                    let members = team.members.lock().expect("members");
                    for member in members.iter().filter(|member| member.helper.running()) {
                        if let Some(abort) = &member.abort {
                            abort.abort();
                        }
                    }
                }
                // Only the task waiting on each one writes its record,
                // an answer that came first included
                team.until(settled).await;
            }
        }

        self.ended();
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
    view: Box<dyn HelperView>,
}

impl Watched {
    fn new(view: Box<dyn HelperView>) -> Self {
        Self {
            live: Arc::default(),
            start: 0,
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

    /// The task is saved with the call that sent it, and its steps and tokens so far
    /// show as soon as a step or compaction is saved
    fn saved(&mut self, memory: &AgentMemory) {
        let (usage, steps) = spent(&memory.steps[self.start..]);
        {
            let mut live = self.live.lock().expect("live");
            live.status.usage = usage;
            live.status.steps = steps;
        }
        self.show(false);
    }

    fn helpers(&mut self, _: usize) -> Box<dyn Helpers> {
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
    use crate::agent::delegate::{
        first_line, tail, Records, Team, Watched, IDLE_WAIT, MAX_IDLE_WORKERS, MAX_LINE_CHARS,
        MAX_PREVIEW_CHARS,
    };
    use crate::agent::tests::{scripted, streamed};
    use crate::agent::{Event, HelperView, Helpers, Sink, Status, Task, Unseen};
    use crate::config::Kind;
    use crate::model::{
        ChatMessage, Finish, Finished, Generating, Model, ModelError, Purpose, StreamDelta,
        TokenUsage,
    };
    use crate::prompt::RESTARTED;
    use crate::steps::tests::metered;
    use crate::steps::{
        spent, AgentMemory, AgentStep, Call, DoneStep, Ending, Helper, MemoryStep, Returned,
    };
    use crate::tests::{cancel_soon, content};
    use crate::tools::mcp::tests::fake;
    use crate::utils::{block_on, Interrupt};
    use crate::worker::host::Added;
    use crate::worker::{AgentCall, Asked};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::Semaphore;
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
                let sealed = StreamDelta::Sealed {
                    provider: "mock",
                    kind: Kind::Responses,
                    item: &item,
                };
                on_delta(sealed);
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
        let tasks: Vec<&str> = result.agents.iter().map(|helper| &*helper.task).collect();
        let asked = [
            "Count the cafés",
            "Crash",
            "Spell naïve",
            "Start another agent",
        ];
        assert_eq!(tasks, asked);
        // The crashed agent keeps the record its dropped view showed in its place
        assert_eq!(result.agents[1].end, Ending::Unfinished {});
        // The records hold the conversations of their agents, which go back to the provider
        for index in [0, 2, 3] {
            let helper = &result.agents[index];
            assert!(matches!(helper.end, Ending::Answer(_)), "{helper:?}");
            assert!(helper.resendable, "{helper:?}");
            let (usage, _) = spent(&helper.memory.steps);
            assert!(usage.tokens.is_some(), "{helper:?}");
            for step in &helper.memory.steps {
                if let MemoryStep::Action(action) = step {
                    assert!(action.sealed.is_some(), "{action:?}");
                }
            }
        }

        // Agents started by agent() don't see it, and answer the code
        let requests = requests.lock().expect("requests");
        let main = content(&requests[0][0]);
        let listed = "\nagent(name: str, task: str) -> None\n    # Sends task";
        assert!(main.contains(listed), "{main}");
        assert!(main.contains("\nagent.wait(*names: str"), "{main}");
        assert!(main.contains("[Task update]"), "{main}");
        let helped = requests.iter().map(|request| content(&request[0]));
        let helper = helped
            .into_iter()
            .find(|text| *text != main)
            .expect("helper");
        assert!(!helper.contains("agent("), "{helper}");
        let returned =
            "your answer and ends the task.\n- Only your answer is returned to the agent that assigned this task";
        assert!(helper.contains(returned), "{helper}");
    }

    /// Agents get the MCP servers of the code that sent them their task
    #[test]
    fn servers_shared() {
        let mut tested = scripted(Teamed(Requests::default()), Box::new(|_| {}));
        let adding = tested.worker.add_server(fake(), &[]);
        let added = block_on(adding).expect("add server");

        let records = Arc::new(Records::new(0, 0));
        let servers = [&added];
        let mut delegate = tested.agent.delegate(&servers, Box::new(Unseen), &records);
        let asked = Asked {
            name: "adder".to_owned(),
            task: "Add with the server".to_owned(),
        };
        let names = vec!["adder".to_owned()];
        let waiting = async {
            delegate(AgentCall::Send(asked)).await;
            let wait = AgentCall::Wait {
                names,
                timeout: None,
            };
            delegate(wait).await
        };
        let waited = block_on(waiting);

        let answered = json!({ "answers": [["adder", { "answer": "5" }]], "working": [] });
        assert_eq!(waited.result, Ok(answered));
    }

    /// One still waiting for a slot ends on an interrupt without starting a worker
    #[test]
    fn queued_agent_stops() {
        let model = Teamed(Requests::default());
        let tested = scripted(model, Box::new(|_| {}));
        let agent = tested.agent.clone();
        agent.interrupt.cancel(0);

        let asked = Helper {
            task: "Count the cafés".to_owned(),
            name: Some("cafés".to_owned()),
            end: Ending::Running {},
            memory: AgentMemory::default(),
            resendable: true,
        };
        let memory = AgentMemory::default();
        let watched = Watched::new(Box::new(Unseen));
        let servers: Arc<[Added]> = Arc::new([]);
        let slots = Arc::new(Semaphore::new(0));
        let helping = agent.help(asked, memory, None, watched, servers, slots);
        let waiting = async { time::timeout(Duration::from_secs(5), helping).await };
        let (helper, memory, worker) = block_on(waiting).expect("stopped");
        assert_eq!(helper.end, Ending::Interrupted {});
        assert!(helper.memory.steps.is_empty(), "{helper:?}");
        assert!(memory.steps.is_empty(), "{memory:?}");
        assert!(worker.is_none());
    }

    /// The main agent runs each of the codes in turn, split into calls at lines `#---`, and with
    /// an empty one or none answers with the number of its reply, while an agent answers with the
    /// last line it was sent that isn't in brackets, after the seconds that follow "after" in it,
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
            let sent = lines.into_iter().find(|line| !line.starts_with('['));
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

    fn ends(call: &Call) -> Vec<(Option<&str>, &Ending)> {
        let result = call.result.as_ref().expect("ran");
        let agents = result.agents.iter();
        agents
            .map(|helper| (helper.name.as_deref(), &helper.end))
            .collect()
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
        assert_eq!(
            ends(ran(&memory, 1)),
            [(Some("cafés"), &cafés), (Some("naïve"), &naïve)]
        );
        let returned = Returned::Ended {
            name: "naïve".to_owned(),
            end: naïve,
        };
        let done = DoneStep {
            agents: vec![returned],
            ..DoneStep::default()
        };
        assert_eq!(memory.steps[3], MemoryStep::Done(done));
        assert_eq!(memory.steps.len(), 5, "{:?}", memory.steps);

        let requests = requests.lock().expect("requests");
        let last = requests.last().and_then(|request| request.last());
        let told = last.map(content).unwrap_or_default();
        let announced =
            "[Agent done]\n<answer agent=\"naïve\">\nAnswer: Spell naïve after 0.5\n</answer>";
        assert_eq!(told, announced);
    }

    /// A program the code left running that ends during the turn comes with the next step,
    /// while one still running when the model answers doesn't keep the turn open
    #[test]
    fn programs_announced() {
        let codes = vec![
            "import subprocess, sys\n\
             quick = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(0.2)'])\n\
             slow = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])\n\
             print(quick.pid)",
            "import time\ntime.sleep(1)",
        ];
        let requests = Requests::default();
        let model = Background(codes, Arc::clone(&requests));
        let mut agent = scripted(model, Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "Run the cafés").expect("run");
        assert_eq!(answer.as_deref(), Some("Answer 2"));
        assert_eq!(memory.steps.len(), 5, "{:?}", memory.steps);

        let quick: u32 = printed(&memory, 1).trim().parse().expect("pid");
        let MemoryStep::Done(done) = &memory.steps[3] else {
            panic!("{:?}", memory.steps);
        };
        let pids: Vec<u32> = done.programs.iter().map(|program| program.pid).collect();
        assert_eq!(pids, [quick]);

        let requests = requests.lock().expect("requests");
        let last = requests.last().and_then(|request| request.last());
        let told = last.map(content).unwrap_or_default();
        let announced = format!("[Program done]\npid {quick}: ");
        assert!(told.starts_with(&announced), "{told}");
    }

    /// The final record of a task goes to the call that sent it
    #[test]
    fn seat_kept() {
        let code = "print('first')\n#---\nagent('naïve', 'Spell naïve after 0.5')";
        let model = Background(vec![code], Requests::default());
        let mut agent = scripted(model, Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "Spell it").expect("run");
        assert_eq!(answer.as_deref(), Some("Answer 2"));

        let MemoryStep::Action(step) = &memory.steps[1] else {
            panic!("{:?}", memory.steps);
        };
        assert_eq!(ends(&step.calls[0]), []);
        let naïve = Ending::Answer("Answer: Spell naïve after 0.5".to_owned());
        assert_eq!(ends(&step.calls[1]), [(Some("naïve"), &naïve)]);
    }

    /// While the turn waits for an agent, a message the client queues arrives as a task update
    #[test]
    fn update_while_waiting() {
        let code = "agent('naïve', 'Spell naïve after 1')";
        let model = Background(vec![code], Requests::default());
        let mut agent = scripted(model, Box::new(|_| {}));
        *agent.queue.nudge.lock().expect("nudge") = Some(Duration::from_millis(200));
        let mut pulls = 0;
        let take = move || {
            pulls += 1;
            let update = Task {
                text: "Also spell café".to_owned(),
                mentions: Vec::new(),
            };
            match pulls {
                2 => vec![update],
                _ => Vec::new(),
            }
        };
        *agent.queue.updates.lock().expect("updates") = Some(Box::new(take));

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "Spell it").expect("run");
        assert_eq!(answer.as_deref(), Some("Answer 3"));
        let mut kinds = Vec::new();
        for step in &memory.steps {
            let kind = match step {
                MemoryStep::Task(_) => "task",
                MemoryStep::Update(_) => "update",
                MemoryStep::Action(_) => "action",
                MemoryStep::Done(_) => "done",
                _ => "other",
            };
            kinds.push(kind);
        }
        let expected = [
            "task", "action", "action", "update", "action", "done", "action",
        ];
        assert_eq!(kinds, expected);
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
                "[Task update]\nSpell café too",
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
        assert_eq!(ends(ran(&memory, 1)), [(Some("slow"), &Ending::Stopped {})]);
        let again = Ending::Answer("Answer: Spell naïve".to_owned());
        assert_eq!(ends(ran(&memory, 2)), [(Some("slow"), &again)]);
        let done = |step: &MemoryStep| matches!(step, MemoryStep::Done(_));
        assert!(!memory.steps.iter().any(done), "{:?}", memory.steps);

        let told = sent(&requests).pop().expect("requests");
        let expected = [
            "Wait after 30",
            "[The agent that assigned your task interrupted you]",
            "Spell naïve",
        ];
        assert_eq!(told, expected);
    }

    /// An agent keeps its Python for its next task, also in a later turn,
    /// until it goes unused for `IDLE_WAIT`
    #[test]
    fn python_kept() {
        let code = "agent('calc', 'run print(x)')\nprint(agent.wait('calc'))";
        let codes = vec![
            "agent('calc', 'run x = 41')\nprint(agent.wait('calc'))",
            "agent('calc', 'run print(x + 1)')\nprint(agent.wait('calc'))",
            "",
            code,
            "",
            code,
        ];
        let requests = Requests::default();
        let model = Background(codes, Arc::clone(&requests));
        let mut agent = scripted(model, Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        agent.turn(&mut memory, "Count on").expect("run");
        assert_eq!(printed(&memory, 1), "{'calc': '[no output]'}");
        assert_eq!(printed(&memory, 2), "{'calc': '42'}");
        agent.turn(&mut memory, "Count again").expect("run");
        assert_eq!(printed(&memory, 5), "{'calc': '41'}");
        // Agent-side tasks only run inside `block_on`
        let idling = async { time::sleep(IDLE_WAIT + Duration::from_secs(1)).await };
        block_on(idling);
        agent.turn(&mut memory, "Count later").expect("run");
        let gone = "NameError: name 'x' is not defined";
        assert!(printed(&memory, 8).contains(gone), "{:?}", memory.steps);

        let told = sent(&requests);
        let restarts: Vec<bool> = told
            .iter()
            .map(|request| request.iter().any(|text| text == RESTARTED))
            .collect();
        let expected = [false, false, false, false, false, false, true, true];
        assert_eq!(restarts, expected);
    }

    /// Only the `MAX_IDLE_WORKERS` Pythons used last stay
    #[test]
    fn idle_limited() {
        let code = "for n in range(5):\n    agent(f'calc{n}', f'run x = {n}')\nagent.wait()";
        let model = Background(vec![code], Requests::default());
        let mut agent = scripted(model, Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        agent.turn(&mut memory, "Count").expect("run");

        let team = agent.agent.place.team().expect("team");
        let members = team.members.lock().expect("members");
        let kept = members.iter().filter(|member| member.kept.is_some());
        assert_eq!(members.len(), 5);
        assert_eq!(kept.count(), MAX_IDLE_WORKERS);
    }

    /// A Python with other MCP servers than the call that sends the next task starts over
    #[test]
    fn servers_changed() {
        let mut tested = scripted(
            Background(Vec::new(), Requests::default()),
            Box::new(|_| {}),
        );
        let adding = tested.worker.add_server(fake(), &[]);
        let added = block_on(adding).expect("add server");

        let asked = |task: &str| {
            let asked = Asked {
                name: "calc".to_owned(),
                task: task.to_owned(),
            };
            AgentCall::Send(asked)
        };
        let wait = || AgentCall::Wait {
            names: vec!["calc".to_owned()],
            timeout: None,
        };
        let mut answers = Vec::new();
        for (servers, task) in [
            (Vec::new(), "run x = 41"),
            (Vec::new(), "run print(x)"),
            (vec![&added], "run print(x)"),
        ] {
            let records = Arc::new(Records::new(0, 0));
            let mut delegate = tested.agent.delegate(&servers, Box::new(Unseen), &records);
            let waiting = async {
                delegate(asked(task)).await;
                delegate(wait()).await
            };
            let waited = block_on(waiting).result.expect("waited");
            let answer = waited["answers"][0][1]["answer"]
                .as_str()
                .unwrap_or_default();
            answers.push(answer.to_owned());
        }

        assert_eq!(answers[1], "41");
        let gone = "NameError: name 'x' is not defined";
        assert!(answers[2].contains(gone), "{answers:?}");
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
        let live = team.members.lock().expect("members");
        let restored = Team::restored(&memory.steps);
        let restored = restored.members.lock().expect("members");
        let names: Vec<&str> = restored.iter().map(|member| member.name()).collect();
        assert_eq!(names, ["copy", "calc"]);
        for (live, restored) in live.iter().zip(restored.iter()) {
            assert_eq!(restored.memory, live.memory);
            assert_eq!(restored.helper.end, live.helper.end);
        }
        drop(live);

        // A task saved as running, as when libsmol exits, comes back ended
        let MemoryStep::Action(step) = &mut memory.steps[1] else {
            panic!("{:?}", memory.steps);
        };
        let result = step.calls[0].result.as_mut().expect("ran");
        result.agents[0].end = Ending::Running {};
        let restored = Team::restored(&memory.steps);
        assert!(!restored.running());
    }

    /// A name finds its agent again after Python restarts
    #[test]
    fn names_survive_restart() {
        let codes = vec![
            "agent('naïve', 'Spell naïve after 2')",
            "import os\nos._exit(1)",
            "print(agent.wait('naïve', timeout=10))",
        ];
        let model = Background(codes, Requests::default());
        let mut agent = scripted(model, Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "Spell it").expect("run");
        assert_eq!(answer.as_deref(), Some("Answer 3"));
        assert_eq!(memory.steps[3], MemoryStep::Restarted);
        assert_eq!(
            printed(&memory, 4),
            "{'naïve': 'Answer: Spell naïve after 2'}"
        );
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
        assert_eq!(
            ends(ran(&memory, 1)),
            [(Some("slow"), &Ending::Interrupted {})]
        );
        let done = |step: &MemoryStep| matches!(step, MemoryStep::Done(_));
        assert!(!memory.steps.iter().any(done), "{:?}", memory.steps);
    }

    /// An agent still at work when a model error ends the turn ends unfinished,
    /// as the user didn't interrupt it
    #[test]
    fn failed_turn_unfinished() {
        let codes = vec!["agent('slow', 'Wait after 30')", "refused"];
        let model = Background(codes, Requests::default());
        let mut agent = scripted(model, Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        let failed = agent.turn(&mut memory, "Wait for it").expect_err("refused");
        assert!(failed.to_string().contains("bad request"), "{failed}");
        assert_eq!(
            ends(ran(&memory, 1)),
            [(Some("slow"), &Ending::Unfinished {})]
        );
    }

    /// An agent that answered during a step the user interrupts is still announced
    #[test]
    fn done_before_interrupt() {
        let code = "import time\n\
            agent('cafés', 'Count the cafés')\n\
            time.sleep(1)\n\
            print('answered', flush=True)\n\
            time.sleep(30)";
        let model = Background(vec![code], Requests::default());
        let stopper = Arc::<Mutex<Option<Interrupt>>>::default();
        let held = Arc::clone(&stopper);
        let events = move |event: Event| {
            let Event::Output(_, _, output) = event else {
                return;
            };
            if let Some(interrupt) = held.lock().expect("interrupt").as_ref() {
                if output.contains("answered") {
                    interrupt.cancel(u64::MAX);
                }
            }
        };
        let mut agent = scripted(model, Box::new(events));
        *stopper.lock().expect("interrupt") = Some(agent.agent.interrupt.clone());

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "Count them").expect("run");
        assert_eq!(answer, None);
        let returned = Returned::Ended {
            name: "cafés".to_owned(),
            end: Ending::Answer("Answer: Count the cafés".to_owned()),
        };
        let done = DoneStep {
            agents: vec![returned],
            ..DoneStep::default()
        };
        let done = MemoryStep::Done(done);
        assert_eq!(memory.steps.last(), Some(&done), "{:?}", memory.steps);
    }

    /// Records the end each view of a task closes with, and the agents the code waits for
    #[derive(Clone, Default)]
    struct Followed {
        ends: Arc<Mutex<Vec<Ending>>>,
        waited: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl Helpers for Followed {
        fn open(&self, _: usize, _: &Helper) -> Box<dyn HelperView> {
            let ends = Arc::clone(&self.ends);
            Box::new(Closed(ends))
        }

        fn waiting(&self, names: &[String]) {
            self.waited.lock().expect("waited").push(names.to_vec());
        }
    }

    struct Closed(Arc<Mutex<Vec<Ending>>>);

    impl HelperView for Closed {
        fn shown(&mut self, _: &Status) {}

        fn close(&mut self, helper: &Helper) {
            let end = helper.end.clone();
            self.0.lock().expect("ends").push(end);
        }
    }

    /// The view of a task the code stopped closes with the end its record keeps, and the call
    /// shows whom it waits for while it does
    #[test]
    fn views_follow() {
        let model = Background(Vec::new(), Requests::default());
        let tested = scripted(model, Box::new(|_| {}));
        let followed = Followed::default();
        let views = Box::new(followed.clone());
        let records = Arc::new(Records::new(0, 0));
        let mut delegate = tested.agent.delegate(&[], views, &records);

        let asked = Asked {
            name: "slow".to_owned(),
            task: "Wait after 30".to_owned(),
        };
        let names = vec!["slow".to_owned()];
        let stopping = async {
            delegate(AgentCall::Send(asked)).await;
            let wait = AgentCall::Wait {
                names,
                timeout: Some(0.2),
            };
            let waited = delegate(wait).await;
            let stop = AgentCall::Stop {
                name: "slow".to_owned(),
            };
            delegate(stop).await;
            waited
        };
        let waited = block_on(stopping).result.expect("waited");

        assert_eq!(waited["answers"], json!([]));
        let status = waited["working"][0][1].as_str().unwrap_or_default();
        assert!(status.starts_with("running for 0s, step 1: "), "{status}");
        assert_eq!(*followed.ends.lock().expect("ends"), [Ending::Stopped {}]);
        let waited = followed.waited.lock().expect("waited");
        assert_eq!(*waited, [vec!["slow".to_owned()], Vec::new()]);
    }

    struct Recorded(Arc<Mutex<Vec<Status>>>);

    impl HelperView for Recorded {
        fn shown(&mut self, status: &Status) {
            self.0.lock().expect("shown").push(status.clone());
        }

        fn close(&mut self, _: &Helper) {}
    }

    #[test]
    fn status_throttled() {
        let shown = Arc::<Mutex<Vec<Status>>>::default();
        let view = Recorded(Arc::clone(&shown));
        let mut watched = Watched::new(Box::new(view));
        watched.event(Event::Thought(streamed("Reading the menu")));
        watched.event(Event::Code(0, streamed("\nprint(len(cafés))\n")));
        let call = Call {
            code: "print(len(cafés))".to_owned(),
            ..Call::default()
        };
        watched.event(Event::Run(0, &call));
        watched.event(Event::Output(0, &call, "3\n"));

        // A saved step shows the tokens so far, and one without any shows nothing new
        let counted = AgentStep {
            usage: metered(100, 20),
            ..AgentStep::default()
        };
        let mut memory = AgentMemory {
            steps: vec![MemoryStep::Action(Box::new(counted))],
        };
        watched.event(Event::StepEnd(&AgentStep::default()));
        watched.saved(&memory);
        let unmetered = AgentStep::default();
        memory.steps.push(MemoryStep::Action(Box::new(unmetered)));
        watched.saved(&memory);

        // Code and output that stream too soon after are held back, but not the start of a run
        let shown = shown.lock().expect("shown");
        let seen: Vec<(&str, &str)> = shown
            .iter()
            .map(|status| (&*status.doing, &*status.preview))
            .collect();
        let expected = [
            ("thinking", "Reading the menu"),
            ("running print(len(cafés))", "print(len(cafés))"),
            ("running print(len(cafés))", "3"),
        ];
        assert_eq!(seen, expected);
        let tokens = shown.last().and_then(|status| status.usage.tokens);
        assert_eq!(tokens, Some(TokenUsage::new(100, 20)));
        assert_eq!(watched.live.lock().expect("live").status.steps, 2);
        drop(shown);

        // A later task of the agent counts only its own steps
        watched.start = 1;
        watched.saved(&memory);
        let live = watched.live.lock().expect("live");
        assert_eq!((live.status.steps, live.status.usage.tokens), (1, None));
    }

    #[test]
    fn preview_tail() {
        assert_eq!(tail("\nprint('café')\n"), "print('café')");
        assert_eq!(tail("a\nb\n\nc\nd\n  \nnaïve\n\n"), "b\nc\nd\nnaïve");

        let long = "résumé ".repeat(200);
        let kept = tail(&long);
        assert_eq!(kept.chars().count(), MAX_PREVIEW_CHARS);
        assert!(long.ends_with(&kept), "{kept}");
    }

    #[test]
    fn line_clipped() {
        let long = "café = 1; ".repeat(20);
        let kept: String = long.chars().take(MAX_LINE_CHARS).collect();
        assert_eq!(first_line(&long), format!("{kept}…"));
        assert_eq!(first_line("\n  print('naïve')\nx = 1"), "print('naïve')");
    }
}
