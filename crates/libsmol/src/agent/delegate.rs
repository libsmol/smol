use crate::agent::{
    logged_tokens, Agent, Delegating, Event, HelperView, Helpers, Place, Sink, Status, Task, Turn,
    Unseen,
};
use crate::prompt;
use crate::spawn::SHOWN_EVERY;
use crate::steps::{AgentMemory, Ending, Helper, MemoryStep};
use crate::utils::{Interrupt, Reply};
use crate::worker::host::{Added, Called, Calling, Worker};
use crate::worker::AgentCall;
use serde_json::{json, Value};
use std::future::{self, Future};
use std::mem;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::select;
use tokio::sync::{watch, Semaphore};
use tokio::task::{self, AbortHandle, JoinSet};
use tokio::time;
use tracing::{error_span, info, warn, Instrument};

pub const MAX_PARALLEL_AGENTS: usize = 8;
const MAX_PREVIEW_LINES: usize = 4;
const MAX_PREVIEW_CHARS: usize = 800;
const MAX_LINE_CHARS: usize = 100;
/// Agents get this long to stop by themselves on an interrupt, and a worker of theirs may take `STOP_WAIT`
const HELPERS_WAIT: Duration = Duration::from_secs(5);

/// The records of the agents the code of one call started, in the order their views opened,
/// which numbers them on replay too
pub(crate) struct Records {
    pub(crate) helpers: Mutex<Vec<Helper>>,
    /// Where the call's step lands in memory,
    /// which only grows at the end until the turn has filed its agents' records
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

/// Where the record of an agent started in the background sits in memory
#[derive(Clone, Copy)]
struct Seat {
    step: usize,
    call: usize,
    agent: usize,
}

/// What an agent at work shares with its handle
#[derive(Default)]
struct Activity {
    status: Status,
    /// How it ends when `stop()` or the end of the turn rather than the user stopped it
    stopped: Option<Ending>,
}

impl Agent {
    /// Runs the calls that the code of one call makes to `agent()` and to the handles it returned,
    /// and keeps the record of each agent they start in `records`
    pub(crate) fn delegate<'a>(
        &'a self,
        servers: &'a [&'a Added],
        views: Box<dyn Helpers>,
        records: &Arc<Records>,
    ) -> Box<Delegating<'a>> {
        let Place::Main { roles, team, .. } = &self.place else {
            let refused = |_: AgentCall| -> Calling {
                let reply = Err(prompt::NESTED.to_owned());
                let called = Called::from(reply);
                Box::pin(future::ready(called))
            };
            return Box::new(refused);
        };

        let views: Arc<dyn Helpers> = Arc::from(views);
        let records = Arc::clone(records);
        let delegate = move |call: AgentCall| -> Calling {
            let (asked, background) = match call {
                AgentCall::Ask { agents } => (agents, false),
                AgentCall::Start { agents } => (agents, true),
                AgentCall::Find { name } => {
                    let members = team.members.lock().expect("members");
                    let mut latest = members.iter().enumerate().rev();
                    let found =
                        latest.find(|(_, member)| member.helper.name.as_deref() == Some(&name));
                    let reply = match found {
                        Some((id, member)) => handle(id, &member.helper),
                        None => Value::Null,
                    };
                    let called = Called::from(Ok(reply));
                    return Box::pin(future::ready(called));
                }
                AgentCall::Wait { id, timeout } => {
                    let team = Arc::clone(team);
                    return Box::pin(async move {
                        let waited = team.wait(id, timeout).await;
                        Called::from(waited)
                    });
                }
                AgentCall::Status { id } => {
                    let members = team.members.lock().expect("members");
                    let reply = match members.get(id) {
                        Some(member) => {
                            let live = member.live.lock().expect("live");
                            let text = prompt::agent_status(&live.status, &member.helper.end);
                            Ok(Value::String(text))
                        }
                        None => Err(prompt::NO_AGENT.to_owned()),
                    };
                    let called = Called::from(reply);
                    return Box::pin(future::ready(called));
                }
                AgentCall::Stop { id } => {
                    let mut members = team.members.lock().expect("members");
                    let running = members.get_mut(id).filter(|member| member.helper.running());
                    if let Some(member) = running {
                        member.live.lock().expect("live").stopped = Some(Ending::Stopped {});
                        member.stop.cancel(u64::MAX);
                    }
                    drop(members);

                    let team = Arc::clone(team);
                    return Box::pin(async move {
                        let waited = team.wait(id, None).await;
                        let stopped = waited.map(|_| Value::Null);
                        Called::from(stopped)
                    });
                }
            };

            let end = match background {
                true => Ending::Running {},
                false => Ending::Unfinished {},
            };
            let mut assigned = Vec::new();
            for asked in asked {
                let mut agent = Agent {
                    place: Place::Delegated(None),
                    ..self.clone()
                };
                if let Some(wanted) = &asked.role {
                    let Some(found) = roles.iter().find(|role| role.name == *wanted) else {
                        let reply = Err(prompt::unknown_role(wanted, roles));
                        let called = Called::from(reply);
                        return Box::pin(future::ready(called));
                    };
                    if let Some(model) = &found.model {
                        agent.model = Arc::clone(model);
                    }
                    agent.place = Place::Delegated(Some(found.clone()));
                }
                let record = Helper {
                    task: asked.task,
                    role: asked.role,
                    name: asked.name,
                    end: end.clone(),
                    memory: AgentMemory::default(),
                };
                assigned.push((record, agent));
            }

            let servers: Arc<[Added]> = servers.iter().map(|added| (*added).clone()).collect();
            let launcher = Launcher {
                views: Arc::clone(&views),
                records: Arc::clone(&records),
                servers,
                team: Arc::clone(team),
            };
            if background {
                let mut members = team.members.lock().expect("members");
                for (record, _) in &assigned {
                    let named = |member: &Member| {
                        member.helper.running() && member.helper.name == record.name
                    };
                    if members.iter().any(named) {
                        let name = record.name.as_deref().unwrap_or_default();
                        let refused = prompt::name_running(name);
                        let called = Called::from(Err(refused));
                        return Box::pin(future::ready(called));
                    }
                }

                let mut handles = Vec::new();
                for (record, mut agent) in assigned {
                    let id = members.len();
                    let entry = handle(id, &record);
                    handles.push(entry);
                    // Its own interrupt stops it alone, and the session's is passed on at once
                    let session = mem::take(&mut agent.interrupt);
                    let stop = agent.interrupt.clone();
                    let (index, helping, live) = launcher.launch(agent, record.clone());
                    let mut helping = task::spawn(helping);
                    let abort = helping.abort_handle();

                    // One that panics or is aborted ends with the record its dropped view showed
                    let unfinished = Helper {
                        end: Ending::Unfinished {},
                        ..record.clone()
                    };
                    let passed = stop.clone();
                    let team = Arc::clone(team);
                    let finishing = async move {
                        let joined = select! {
                            joined = &mut helping => joined,
                            () = session.wait() => {
                                passed.cancel(u64::MAX);
                                helping.await
                            }
                        };
                        let helper = joined.unwrap_or(unfinished);

                        let mut members = team.members.lock().expect("members");
                        members[id].helper = helper;
                        drop(members);
                        team.ended.send_modify(|count| *count += 1);
                    };
                    task::spawn(finishing);

                    let seat = Seat {
                        step: records.step,
                        call: records.call,
                        agent: index,
                    };
                    members.push(Member {
                        helper: record,
                        live,
                        stop,
                        taken: false,
                        seat: Some(seat),
                        abort,
                    });
                }

                let called = Called::from(Ok(Value::Array(handles)));
                return Box::pin(future::ready(called));
            }

            // Views open in task order before any agent waits for a free slot, the order replay uses
            let mut launched = Vec::new();
            for (record, agent) in assigned {
                let (index, helping, _) = launcher.launch(agent, record);
                launched.push((index, helping));
            }
            let records = launcher.records;
            let interrupt = self.interrupt.clone();
            Box::pin(async move {
                let mut running = JoinSet::new();
                let mut indices = Vec::new();
                for (index, helping) in launched {
                    running.spawn(async move { (index, helping.await) });
                    indices.push(index);
                }

                {
                    let joining = async {
                        while let Some(joined) = running.join_next().await {
                            // One that panicked keeps the record its dropped view showed
                            if let Ok((index, helper)) = joined {
                                records.helpers.lock().expect("records")[index] = helper;
                            }
                        }
                    };
                    let stopping = async {
                        interrupt.wait().await;
                        time::sleep(HELPERS_WAIT).await;
                    };
                    select! {
                        () = joining => {}
                        () = stopping => {}
                    }
                }
                // Those still running keep the records their dropped views showed
                running.abort_all();

                let helpers = records.helpers.lock().expect("records");
                let results = indices.iter().map(|index| handed(&helpers[*index].end));
                let results = results.collect();
                Called::from(Ok(Value::Array(results)))
            })
        };
        Box::new(delegate)
    }

    /// Fills in how the agent `asked` describes ended, and what it did
    async fn help(
        self,
        asked: Helper,
        mut watched: Watched,
        servers: Arc<[Added]>,
        slots: Arc<Semaphore>,
    ) -> Helper {
        let started = Instant::now();
        let mut memory = AgentMemory::default();

        // One still waiting for a slot stops on an interrupt without starting a worker
        let answered = select! {
            biased;
            () = self.interrupt.wait() => Ok(None),
            slot = slots.acquire() => {
                let _slot = slot.expect("slots");
                let mut keys = Vec::new();
                if let Some(name) = &asked.name {
                    keys.push(format!("name={name}"));
                }
                if let Some(role) = &asked.role {
                    keys.push(format!("role={role}"));
                }
                match keys.is_empty() {
                    true => info!("agent: started"),
                    false => info!("agent: started [{}]", keys.join(", ")),
                }
                {
                    let mut live = watched.live.lock().expect("live");
                    live.status.since = Some(Instant::now());
                    live.status.doing = prompt::STARTING.to_owned();
                }
                watched.show(false);

                let workplace = Arc::clone(&self.workplace);
                let installed: Vec<&Added> = servers.iter().collect();
                let delegation = self.place.delegation();
                let restarted = false;
                let starting =
                    Worker::new(&self.interrupt, workplace, &installed, delegation, restarted);
                match starting.await {
                    Ok(mut worker) => {
                        let task = Task {
                            text: asked.task.clone(),
                            mentions: Vec::new(),
                        };
                        let running =
                            self.run(&mut worker, &installed, &mut memory, task, &mut watched);
                        running.await
                    }
                    Err(error) => Err(error),
                }
            }
        };

        let (usage, steps) = memory.spent();
        let seconds = started.elapsed().as_secs_f64();
        let tokens = logged_tokens(&usage);
        let counted = format!("[steps={steps}, tokens={tokens}, elapsed={seconds:.1}s]");
        let end = match answered {
            Ok(Some(answer)) => {
                info!("agent: answered {counted}");
                Ending::Answer(answer)
            }
            // Its own interrupt can't tell the user from `stop()` or the turn ending
            Ok(None) => {
                let stopped = watched.live.lock().expect("live").stopped.clone();
                match stopped {
                    Some(end) => {
                        info!("agent: stopped {counted}");
                        end
                    }
                    None => {
                        info!("agent: interrupted {counted}");
                        Ending::Interrupted {}
                    }
                }
            }
            Err(error) => {
                warn!("agent: failed, {error} {counted}");
                Ending::Error(error.to_string())
            }
        };

        let helper = Helper {
            end,
            memory,
            ..asked
        };
        watched.view.close(&helper);
        helper
    }
}

/// What the code gets for an agent that ended
fn handed(end: &Ending) -> Value {
    match end.outcome() {
        Ok(answer) => json!({ "answer": answer }),
        Err(error) => json!({ "error": error }),
    }
}

/// What `Agent` handles are made of
fn handle(id: usize, helper: &Helper) -> Value {
    json!({ "id": id, "name": helper.name, "task": helper.task, "role": helper.role })
}

/// What the agents that the code of one call starts share
struct Launcher {
    views: Arc<dyn Helpers>,
    records: Arc<Records>,
    servers: Arc<[Added]>,
    team: Arc<Team>,
}

impl Launcher {
    /// Keeps the record of the agent and opens its view, numbered by the record's place,
    /// and returns that place, the agent at work and what it shares
    fn launch(
        &self,
        agent: Agent,
        record: Helper,
    ) -> (
        usize,
        impl Future<Output = Helper> + Send + 'static,
        Arc<Mutex<Activity>>,
    ) {
        let index = {
            let mut helpers = self.records.helpers.lock().expect("records");
            helpers.push(record.clone());
            helpers.len() - 1
        };
        let number = index + 1;
        let view = self.views.open(number, &record);
        let watched = Watched::new(view);
        let live = Arc::clone(&watched.live);

        let servers = Arc::clone(&self.servers);
        let slots = Arc::clone(&self.team.slots);
        let id = self.team.launched.fetch_add(1, Ordering::Relaxed) + 1;
        let span = error_span!("", agent = id);
        let helping = agent.help(record, watched, servers, slots);
        (index, helping.instrument(span), live)
    }
}

/// The agents that the session's code started in the background, with their ids as indices
pub(crate) struct Team {
    members: Mutex<Vec<Member>>,
    /// Counts the agents that ended, which wakes everyone waiting for one
    ended: watch::Sender<usize>,
    /// Shared by all agents the session's code starts
    slots: Arc<Semaphore>,
    /// Counts all agents the session's code started, which numbers them in the log
    launched: AtomicUsize,
}

struct Member {
    /// Its record, which says it is running until it ends
    helper: Helper,
    live: Arc<Mutex<Activity>>,
    /// Stops it alone
    stop: Interrupt,
    /// The code got its result or stopped it, so it is not announced
    taken: bool,
    /// Where the turn files its final record, taken once it has
    seat: Option<Seat>,
    abort: AbortHandle,
}

impl Member {
    fn unfiled(&self) -> bool {
        !self.helper.running() && self.seat.is_some()
    }
}

impl Default for Team {
    fn default() -> Self {
        let (ended, _) = watch::channel(0);
        Self {
            members: Mutex::default(),
            ended,
            slots: Arc::new(Semaphore::new(MAX_PARALLEL_AGENTS)),
            launched: AtomicUsize::default(),
        }
    }
}

impl Team {
    /// Takes the result of the agent once it ends, or answers null after `timeout` seconds
    async fn wait(&self, id: usize, timeout: Option<f64>) -> Reply {
        let ended = |members: &[Member]| {
            members
                .get(id)
                .is_none_or(|member| !member.helper.running())
        };
        let waiting = self.until(ended);

        // The worker refuses negative timeouts, and one too large for a duration waits forever
        let limit = timeout.and_then(|seconds| Duration::try_from_secs_f64(seconds).ok());
        let finished = match limit {
            Some(limit) => time::timeout(limit, waiting).await.is_ok(),
            None => {
                waiting.await;
                true
            }
        };
        if !finished {
            return Ok(Value::Null);
        }

        let mut members = self.members.lock().expect("members");
        let Some(member) = members.get_mut(id) else {
            return Err(prompt::NO_AGENT.to_owned());
        };
        member.taken = true;
        Ok(handed(&member.helper.end))
    }

    pub(crate) fn running(&self) -> bool {
        let members = self.members.lock().expect("members");
        members.iter().any(|member| member.helper.running())
    }

    /// Resolves once `done` holds for the members, checking again whenever an agent ends
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
    /// Saves the records of agents started in the background that ended,
    /// and returns the names of those whose results the code hasn't taken and the user didn't interrupt
    pub(crate) fn ended(&mut self) -> Vec<String> {
        let Some(team) = self.agent.place.team() else {
            return Vec::new();
        };

        let mut records = Vec::new();
        let mut untaken = Vec::new();
        let mut members = team.members.lock().expect("members");
        for member in members.iter_mut().filter(|member| !member.helper.running()) {
            let Some(seat) = member.seat.take() else {
                continue;
            };
            let memory = mem::take(&mut member.helper.memory);
            let record = Helper {
                memory,
                ..member.helper.clone()
            };
            // The user's interrupt reaches them at once, so some end before `stopped()` stops the rest
            let interrupted = matches!(record.end, Ending::Interrupted {});
            if !member.taken && !interrupted {
                let name = record.name.clone().unwrap_or_default();
                untaken.push(name);
            }
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

    /// Waits for an agent started in the background to end, an interrupt or the client
    /// to queue a message, and returns the messages queued
    pub(crate) async fn news(&mut self) -> Vec<Task> {
        let Place::Main { team, queue, .. } = &self.agent.place else {
            return Vec::new();
        };

        let unfiled = |members: &[Member]| members.iter().any(Member::unfiled);
        let ended = team.until(unfiled);
        let enqueued = queue.enqueued();
        let queued = select! {
            biased;
            () = self.agent.interrupt.wait() => false,
            () = ended => false,
            () = enqueued => true,
        };

        match queued {
            true => self.queued().await,
            false => Vec::new(),
        }
    }

    /// Announces the agents started in the background that ended, then stops the rest
    /// and saves their records, aborting those that take longer than `HELPERS_WAIT`
    pub(crate) async fn stopped(&mut self) {
        // Taken before the rest are stopped, which aren't announced
        let done = self.ended();
        if !done.is_empty() {
            let ended = MemoryStep::Done { agents: done };
            self.memory.steps.push(ended);
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
                    member.stop.cancel(u64::MAX);
                }
            }

            let settled =
                |members: &[Member]| !members.iter().any(|member| member.helper.running());
            let stopping = team.until(settled);
            if time::timeout(HELPERS_WAIT, stopping).await.is_err() {
                {
                    let members = team.members.lock().expect("members");
                    for member in members.iter().filter(|member| member.helper.running()) {
                        member.abort.abort();
                    }
                }
                // Only the task waiting on each one writes its record, an answer that came first included
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
    /// The status last shown and when
    shown: Option<(Status, Instant)>,
    view: Box<dyn HelperView>,
}

impl Watched {
    fn new(view: Box<dyn HelperView>) -> Self {
        Self {
            live: Arc::default(),
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
            | Event::CompactEnd(_) => return,
        };
        drop(live);

        self.show(streamed);
    }

    /// The agent is saved with the call that started it, and its steps and tokens so far
    /// show as soon as a step or compaction is saved
    fn saved(&mut self, memory: &AgentMemory) {
        let (usage, steps) = memory.spent();
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
        first_line, tail, Records, Watched, MAX_LINE_CHARS, MAX_PREVIEW_CHARS,
    };
    use crate::agent::tests::{scripted, streamed, with_roles};
    use crate::agent::{Event, HelperView, Helpers, Sink, Status, Task, Unseen};
    use crate::config::Role;
    use crate::model::{
        ChatMessage, Finish, Finished, Generating, Model, ModelError, Purpose, StreamDelta,
        TokenUsage,
    };
    use crate::steps::tests::metered;
    use crate::steps::{AgentMemory, AgentStep, Call, Ending, Helper, MemoryStep};
    use crate::tests::{cancel_soon, content};
    use crate::tools::mcp::tests::fake;
    use crate::utils::{block_on, Interrupt};
    use crate::worker::host::Added;
    use crate::worker::{AgentCall, Asked};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::Semaphore;
    use tokio::time;

    type Requests = Arc<Mutex<Vec<Vec<ChatMessage>>>>;

    /// The main agent starts four agents, which answer with their task, except one that crashes,
    /// and agents that start an agent or add with the MCP server answer with what happened
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
                    (String::new(), Some("print(agent('deeper'))"))
                }
                (true, true) if task == "Add with the server" => {
                    (String::new(), Some("print(fake.add(a=2, b=3))"))
                }
                (true, true) if task == "Crash" => panic!("{task}"),
                (true, true) => (format!("Answer: {task}"), None),
                (true, false) => (last, None),
                (false, true) => {
                    let code = "print(agent(['Count the cafés', 'Crash', 'Spell naïve', 'Start another agent']))";
                    (String::new(), Some(code))
                }
                (false, false) => ("All three answered.".to_owned(), None),
            };

            Box::pin(async move {
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
        let nested = "NameError: name 'agent' is not defined\n  line 1: print(agent('deeper'))";
        let crashed = "RuntimeError('agent: stopped before it answered')";
        let printed =
            format!("['Answer: Count the cafés', {crashed}, 'Answer: Spell naïve', {nested:?}]");
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
        for index in [0, 2, 3] {
            let helper = &result.agents[index];
            assert!(matches!(helper.end, Ending::Answer(_)), "{helper:?}");
            let (usage, _) = helper.memory.spent();
            assert!(usage.tokens.is_some(), "{helper:?}");
        }

        // Agents started by agent() don't see it or task updates, and answer the code
        let requests = requests.lock().expect("requests");
        let main = content(&requests[0][0]);
        let listed = "\nagent(task: str | list[str], *, name: str | list[str] | None = None) -> str | list[str | RuntimeError] | Agent | list[Agent]\n    # A new agent";
        assert!(main.contains(listed), "{main}");
        assert!(main.contains("\nAgent.wait(timeout"), "{main}");
        assert!(main.contains("[Task update]"), "{main}");
        let helped = requests.iter().map(|request| content(&request[0]));
        let helper = helped
            .into_iter()
            .find(|text| *text != main)
            .expect("helper");
        assert!(!helper.contains("agent("), "{helper}");
        assert!(!helper.contains("Agent."), "{helper}");
        assert!(!helper.contains("while you were working"), "{helper}");
        let returned =
            "your answer and ends the task.\n- Only your answer is returned to the agent that assigned this task";
        assert!(helper.contains(returned), "{helper}");
    }

    /// The main agent gives tasks to roles, and agents answer with their role and model
    struct Cast(&'static str);

    impl Model for Cast {
        fn generate<'a>(
            &'a self,
            conversation: &'a [ChatMessage],
            _: Purpose,
            on_delta: &'a mut (dyn FnMut(StreamDelta) + Send),
        ) -> Generating<'a> {
            let system = content(&conversation[0]);
            let own = system.split("# Role\n\n").nth(1);
            let own = own.and_then(|rest| rest.lines().next());
            let main = !system.contains("returned to the agent");
            let first = conversation.len() == 2;
            let (text, code) = match (main, first) {
                (true, true) => {
                    let code = "print(agent('Check café.py', role='reviewer'))\n\
                        print(agent(['Check café.py', 'Spell naïve'], role=['reviewer', 'tester']))\n\
                        agent('Spell naïve', role='critic')";
                    (String::new(), Some(code))
                }
                (true, false) => ("Both answered.".to_owned(), None),
                (false, _) => {
                    let own = own.unwrap_or("no role");
                    (format!("{own} on {}", self.0), None)
                }
            };

            Box::pin(async move {
                match code {
                    Some(code) => {
                        on_delta(StreamDelta::CallCode(code));
                        on_delta(StreamDelta::CallEnd(Ok(())));
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

    #[test]
    fn roles_played() {
        let role = |name: &str, description: &str, model: Option<Arc<dyn Model>>| Role {
            name: name.to_owned(),
            description: description.to_owned(),
            model,
        };
        let roles = vec![
            role(
                "reviewer",
                "Reviews the café's code",
                Some(Arc::new(Cast("its own model"))),
            ),
            role("tester", "Writes naïve tests", None),
        ];
        let mut agent = with_roles(Cast("the session's model"), Box::new(|_| {}), roles);

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "Check the code").expect("run");
        assert_eq!(answer.as_deref(), Some("Both answered."));

        let MemoryStep::Action(step) = &memory.steps[1] else {
            panic!("{:?}", memory.steps);
        };
        let call = &step.calls[0];
        let result = call.result.as_ref().expect("ran");
        let printed = "Your role is `reviewer`: Reviews the café's code on its own model\n\
            [\"Your role is `reviewer`: Reviews the café's code on its own model\", \
            \"Your role is `tester`: Writes naïve tests on the session's model\"]";
        assert_eq!(result.printed.as_deref(), Some(printed));
        let unknown = "ValueError: agent: role: critic: unknown (roles: reviewer, tester)";
        let error = call.error.as_ref().expect("error").to_string();
        assert!(error.starts_with(unknown), "{error}");
        let played: Vec<Option<&str>> = result
            .agents
            .iter()
            .map(|helper| helper.role.as_deref())
            .collect();
        assert_eq!(played, [Some("reviewer"), Some("reviewer"), Some("tester")]);
    }

    /// Agents get the MCP servers of the code that started them
    #[test]
    fn servers_shared() {
        let mut tested = scripted(Teamed(Requests::default()), Box::new(|_| {}));
        let adding = tested.worker.add_server(fake(), &[]);
        let added = block_on(adding).expect("add server");

        let records = Arc::new(Records::new(0, 0));
        let servers = [&added];
        let mut delegate = tested.agent.delegate(&servers, Box::new(Unseen), &records);
        let asked = Asked {
            task: "Add with the server".to_owned(),
            role: None,
            name: None,
        };
        let call = AgentCall::Ask {
            agents: vec![asked],
        };
        let calling = delegate(call);
        block_on(calling);

        let helpers = records.helpers.lock().expect("records");
        assert_eq!(helpers[0].end, Ending::Answer("5".to_owned()));
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
            role: None,
            name: None,
            end: Ending::Running {},
            memory: AgentMemory::default(),
        };
        let watched = Watched::new(Box::new(Unseen));
        let servers: Arc<[Added]> = Arc::new([]);
        let slots = Arc::new(Semaphore::new(0));
        let helping = agent.help(asked, watched, servers, slots);
        let waiting = async { time::timeout(Duration::from_secs(5), helping).await };
        let helper = block_on(waiting).expect("stopped");
        assert_eq!(helper.end, Ending::Interrupted {});
        assert!(helper.memory.steps.is_empty(), "{helper:?}");
    }

    /// The main agent runs each of the codes in turn, split into calls at lines `#---`,
    /// and then answers with the number of its reply,
    /// and agents answer with their task after the seconds that follow "after" in it
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
            let task = content(&conversation[1]);
            let main = !system.contains("returned to the agent");
            let replied = |message: &&ChatMessage| matches!(message, ChatMessage::Assistant { .. });
            let replies = conversation.iter().filter(replied).count();
            let code = self.0.get(replies).copied().filter(|_| main);
            let text = match main {
                true => format!("Answer {replies}"),
                false => format!("Answer: {task}"),
            };
            let after = task.split(" after ").nth(1);
            let delay = after.and_then(|seconds| seconds.parse().ok());

            Box::pin(async move {
                if let Some(seconds) = delay.filter(|_| !main) {
                    time::sleep(Duration::from_secs_f64(seconds)).await;
                }
                match code {
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

    fn ends(call: &Call) -> Vec<(Option<&str>, &Ending)> {
        let result = call.result.as_ref().expect("ran");
        let agents = result.agents.iter();
        agents
            .map(|helper| (helper.name.as_deref(), &helper.end))
            .collect()
    }

    /// An answer doesn't end the turn while an agent started in the background runs,
    /// and one whose result the code didn't take is announced once it ends
    #[test]
    fn background_announced() {
        let code = "cafés = agent('Count the cafés', name='cafés')\n\
            naïve = agent('Spell naïve after 0.5', name='naïve')\n\
            print(cafés.wait(10), naïve.wait(0), naïve.status().startswith('running for'))";
        let requests = Requests::default();
        let model = Background(vec![code], Arc::clone(&requests));
        let mut agent = scripted(model, Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "Split the work").expect("run");
        assert_eq!(answer.as_deref(), Some("Answer 2"));

        let call = ran(&memory, 1);
        let result = call.result.as_ref().expect("ran");
        let printed = "Answer: Count the cafés None True";
        assert_eq!(result.printed.as_deref(), Some(printed));
        let cafés = Ending::Answer("Answer: Count the cafés".to_owned());
        let naïve = Ending::Answer("Answer: Spell naïve after 0.5".to_owned());
        assert_eq!(
            ends(call),
            [(Some("cafés"), &cafés), (Some("naïve"), &naïve)]
        );
        let done = MemoryStep::Done {
            agents: vec!["naïve".to_owned()],
        };
        assert_eq!(memory.steps[3], done);
        assert_eq!(memory.steps.len(), 5, "{:?}", memory.steps);

        let requests = requests.lock().expect("requests");
        let last = requests.last().and_then(|request| request.last());
        let told = last.map(content).unwrap_or_default();
        assert_eq!(told, "[Agent done]\nnaïve");
    }

    /// The final record of an agent started in the background goes to the call that started it
    #[test]
    fn seat_kept() {
        let code = "print('first')\n#---\nagent('Spell naïve after 0.5', name='naïve')";
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
        let code = "agent('Spell naïve after 1', name='naïve')";
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
                MemoryStep::Done { .. } => "done",
                _ => "other",
            };
            kinds.push(kind);
        }
        let expected = [
            "task", "action", "action", "update", "action", "done", "action",
        ];
        assert_eq!(kinds, expected);
    }

    /// `stop()` ends one agent without announcing it and frees its name
    #[test]
    fn stopped_by_code() {
        let codes = vec![
            "slow = agent('Wait after 30', name='slow')\n\
             agent('Spell naïve', name='slow')",
            "slow.stop()\n\
             print(slow.status())\n\
             again = agent('Spell naïve', name='slow')\n\
             print(again.wait(10))\n\
             try:\n    slow.wait()\nexcept RuntimeError as error:\n    print(error)",
        ];
        let model = Background(codes, Requests::default());
        let mut agent = scripted(model, Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "Spell it").expect("run");
        assert_eq!(answer.as_deref(), Some("Answer 2"));

        let taken = ran(&memory, 1).error.as_ref().expect("error").to_string();
        let running = "ValueError: agent: name: slow: already running";
        assert!(taken.starts_with(running), "{taken}");
        let call = ran(&memory, 2);
        let result = call.result.as_ref().expect("ran");
        let printed = "stopped by stop()\nAnswer: Spell naïve\nAgent.wait: stopped by stop()";
        assert_eq!(result.printed.as_deref(), Some(printed));
        assert_eq!(ends(ran(&memory, 1)), [(Some("slow"), &Ending::Stopped {})]);
        let again = Ending::Answer("Answer: Spell naïve".to_owned());
        assert_eq!(ends(call), [(Some("slow"), &again)]);
        let done = |step: &MemoryStep| matches!(step, MemoryStep::Done { .. });
        assert!(!memory.steps.iter().any(done), "{:?}", memory.steps);
    }

    /// Records keep the order the `agent()` calls started in, which numbers their agents
    #[test]
    fn agents_in_order() {
        let code = "import threading, time\n\
            first = threading.Thread(target=agent, args=('Count the cafés after 1',))\n\
            first.start()\n\
            time.sleep(0.2)\n\
            agent('Spell naïve')\n\
            first.join()";
        let model = Background(vec![code], Requests::default());
        let mut agent = scripted(model, Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        agent.turn(&mut memory, "Split the work").expect("run");

        let call = ran(&memory, 1);
        let cafés = Ending::Answer("Answer: Count the cafés after 1".to_owned());
        let naïve = Ending::Answer("Answer: Spell naïve".to_owned());
        assert_eq!(ends(call), [(None, &cafés), (None, &naïve)]);
    }

    /// An agent the code waits for stops by itself on an interrupt and keeps its record,
    /// and one its code leaves running keeps the record its dropped view showed
    #[test]
    fn helpers_kept() {
        let code = "agent('Wait after 30')";
        let model = Background(vec![code], Requests::default());
        let mut agent = scripted(model, Box::new(|_| {}));
        cancel_soon(&agent.agent.interrupt, 0);

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "Wait for it").expect("run");
        assert_eq!(answer, None);
        assert_eq!(ends(ran(&memory, 1)), [(None, &Ending::Interrupted {})]);

        let code = "import threading, time\n\
            threading.Thread(target=agent, args=('Wait after 30',), daemon=True).start()\n\
            time.sleep(0.5)";
        let model = Background(vec![code], Requests::default());
        let mut agent = scripted(model, Box::new(|_| {}));
        let mut memory = AgentMemory::default();
        agent.turn(&mut memory, "Leave it").expect("run");
        assert_eq!(ends(ran(&memory, 1)), [(None, &Ending::Unfinished {})]);
    }

    /// An interrupt ends the turn and the agents running in the background with it
    #[test]
    fn interrupt_stops_all() {
        // The code outlasts the agent, which the interrupt reaches at once
        let code = "import time\n\
            agent('Wait after 30', name='slow')\n\
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
        let done = |step: &MemoryStep| matches!(step, MemoryStep::Done { .. });
        assert!(!memory.steps.iter().any(done), "{:?}", memory.steps);
    }

    /// An agent still running when a model error ends the turn ends unfinished,
    /// as the user didn't interrupt it
    #[test]
    fn failed_turn_unfinished() {
        let codes = vec!["agent('Wait after 30', name='slow')", "refused"];
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
            cafés = agent('Count the cafés', name='cafés')\n\
            for _ in range(100):\n    if cafés.status() == 'answered':\n        break\n    time.sleep(0.1)\n\
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
        let done = MemoryStep::Done {
            agents: vec!["cafés".to_owned()],
        };
        assert_eq!(memory.steps.last(), Some(&done), "{:?}", memory.steps);
    }

    /// Records the end each view of an agent closes with
    struct Closed(Arc<Mutex<Vec<Ending>>>);

    impl Helpers for Closed {
        fn open(&self, _: usize, _: &Helper) -> Box<dyn HelperView> {
            let ends = Arc::clone(&self.0);
            Box::new(Closed(ends))
        }
    }

    impl HelperView for Closed {
        fn shown(&mut self, _: &Status) {}

        fn close(&mut self, helper: &Helper) {
            let end = helper.end.clone();
            self.0.lock().expect("ends").push(end);
        }
    }

    /// The view of an agent the code stopped closes with the end its record keeps
    #[test]
    fn stop_shown() {
        let model = Background(Vec::new(), Requests::default());
        let tested = scripted(model, Box::new(|_| {}));
        let ends = Arc::<Mutex<Vec<Ending>>>::default();
        let views = Box::new(Closed(Arc::clone(&ends)));
        let records = Arc::new(Records::new(0, 0));
        let mut delegate = tested.agent.delegate(&[], views, &records);

        let asked = Asked {
            task: "Wait after 30".to_owned(),
            role: None,
            name: Some("slow".to_owned()),
        };
        let stopping = async {
            delegate(AgentCall::Start {
                agents: vec![asked],
            })
            .await;
            delegate(AgentCall::Stop { id: 0 }).await;
        };
        block_on(stopping);

        assert_eq!(*ends.lock().expect("ends"), [Ending::Stopped {}]);
    }

    /// A handle is found again by name after Python restarts
    #[test]
    fn handle_found_again() {
        let codes = vec![
            "agent('Spell naïve after 2', name='naïve')",
            "import os\nos._exit(1)",
            "print(Agent.get('naïve').wait(10), Agent.get('menu'))",
        ];
        let model = Background(codes, Requests::default());
        let mut agent = scripted(model, Box::new(|_| {}));

        let mut memory = AgentMemory::default();
        let answer = agent.turn(&mut memory, "Spell it").expect("run");
        assert_eq!(answer.as_deref(), Some("Answer 3"));
        assert_eq!(memory.steps[3], MemoryStep::Restarted);
        let result = ran(&memory, 4).result.as_ref().expect("ran");
        let printed = "Answer: Spell naïve after 2 None";
        assert_eq!(result.printed.as_deref(), Some(printed));
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
        let live = watched.live.lock().expect("live");
        assert_eq!(live.status.steps, 2);
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
