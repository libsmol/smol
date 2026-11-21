use crate::model::Media;
use crate::prompt::{self, ADDING_SERVER, LEFT_BEHIND, LISTING_GLOBALS, NO_SERVER};
use crate::spawn::{Clip, Output, Process, SHOWN_EVERY};
use crate::steps::{CallError, DoneStep, MAX_OBSERVATION_CHARS};
use crate::tools::mcp::Server;
use crate::utils::{cut_inline, new_id, Interrupt, Reply};
use crate::worker::workplace::Workplace;
use crate::worker::{
    control, end_marker, greeted, greeting, FromWorker, Function, Globals, HostCall, HostNote,
    Message, Service, ToWorker, WorkerCall, WorkerNote, CONTROL_ENV, COPIES_ENV, MAIN_ENV,
    WORKER_ARG,
};
use crate::{Error, Result};
use reaper::wire::Program;
use reaper::{cause, Member};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::collections::HashMap;
use std::future::{self, Future};
use std::io;
use std::mem;
use std::path::Component;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::TcpListener;
use tokio::select;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::{self, AbortHandle, JoinSet};
use tokio::time::{self, Instant};
use tracing::{info, warn};

const STOP_WAIT: Duration = Duration::from_secs(3);
/// Code that swallows exceptions may have caught the previous interrupt
const CANCEL_EVERY: Duration = Duration::from_millis(500);
/// Code that closes the worker's own copy of the pipe never lets the end marker through
const OUTPUT_WAIT: Duration = Duration::from_secs(2);
const START_WAIT: Duration = Duration::from_secs(30);
const MAX_COMMAND_CHARS: usize = 300;
/// How often the programs that calls left running are checked
const PROGRAMS_EVERY: Duration = match cfg!(test) {
    true => Duration::from_millis(50),
    false => Duration::from_secs(1),
};
/// In the command line of the helpers multiprocessing starts, which run as long as the worker
const MULTIPROCESSING: &str = "from multiprocessing.";

pub(crate) type Calling = Pin<Box<dyn Future<Output = Called> + Send>>;
pub(crate) type Serving<'a> = dyn FnMut(Service) -> Calling + Send + 'a;

/// The result of work done for the worker, with the images and videos it produced
pub(crate) struct Called {
    pub(crate) result: Reply,
    pub(crate) media: Vec<(String, Media)>,
}

impl From<Reply> for Called {
    fn from(result: Reply) -> Self {
        Self {
            result,
            media: Vec::new(),
        }
    }
}

pub(crate) struct Evaluated {
    pub(crate) printed: Clip,
    pub(crate) error: Option<CallError>,
    pub(crate) media: Vec<(String, Media)>,
    /// The code actually run, when stray tags had to be stripped
    pub(crate) repaired: Option<String>,
}

#[derive(Clone)]
pub(crate) struct Added {
    /// The name used in code
    pub(crate) name: String,
    pub(crate) server: Server,
    /// Each function's signature as the system prompt lists it
    pub(crate) stubs: Vec<String>,
}

/// The session's own worker, which offers `agent()`, `remember()` and `forget()`,
/// or one of an agent that `agent()` started
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Rank {
    Main,
    Delegated,
}

/// Callers keep track of the installed MCP servers and pass them in on every call
pub(crate) struct Worker {
    /// Dropped before the workplace, which removes the worker's temporary folder and lets go of the venv
    live: Option<Live>,
    /// Shared with the workers of agents the code starts
    workplace: Arc<Workplace>,
    interrupt: Interrupt,
    rank: Rank,
    functions: Vec<Function>,
    restarted: bool,
}

/// Dropping it kills the worker
struct Live {
    process: Process,
    lines: Lines<BufReader<OwnedReadHalf>>,
    control: UnboundedSender<String>,
    /// Holds what the code printed between snippets for the next one
    output: Output,
    next_id: u64,
    calls: JoinSet<(u64, Called)>,
    /// Every call not yet answered, which a thread the code started may still wait for
    running: HashMap<u64, AbortHandle>,
    media: Vec<(String, Media)>,
    followed: Followed,
}

/// The programs that calls left running and their descendants, as last seen
#[derive(Default)]
struct Followed {
    left: Vec<Member>,
    /// The worker's own processes, such as its console host on Windows
    own: Vec<Member>,
}

/// An interrupt sent to the running code, which is sent again at `resend` until it stops or
/// `give_up` passes
struct Stopping {
    resend: Instant,
    give_up: Instant,
}

enum Event {
    Call(u64, WorkerCall),
    Answer(u64, Reply),
    Ready(Vec<Function>),
    /// What came before the end marker the worker writes after each snippet
    End(Clip),
    Gone,
}

enum Outcome {
    Replied(Result<Ran, String>),
    Gone,
    Stuck,
}

/// Fields match the dict returned by `_run`
#[derive(Deserialize)]
struct Ran {
    error: Option<String>,
    repaired: Option<String>,
}

/// Matches the dict returned by `_server`
#[derive(Deserialize)]
#[serde(untagged)]
enum Installed {
    /// `holder` is the configured name of another server that holds the name
    Taken {
        name: String,
        holder: String,
    },
    Added {
        name: String,
        stubs: Vec<String>,
    },
}

impl Worker {
    /// `restarted` means earlier code in the session defined names this new worker lacks
    pub(crate) async fn new(
        interrupt: &Interrupt,
        workplace: Arc<Workplace>,
        servers: &[&Added],
        rank: Rank,
        restarted: bool,
    ) -> Result<Self> {
        let mut worker = Self {
            workplace,
            interrupt: interrupt.clone(),
            rank,
            functions: Vec::new(),
            restarted,
            live: None,
        };
        worker.connect(servers).await.map_err(Error::Python)?;
        Ok(worker)
    }

    pub(crate) fn functions(&self) -> &[Function] {
        &self.functions
    }

    /// Turns true again only when the worker is replaced
    pub(crate) fn take_restarted(&mut self) -> bool {
        self.reap();
        mem::take(&mut self.restarted)
    }

    /// Checks only between steps because a worker lost while running code is reported by that step
    fn reap(&mut self) {
        let gone = self.live.as_ref().is_some_and(|live| live.process.reaped());
        if gone {
            self.live = None;
            self.restarted = true;
        }
    }

    /// A newly started worker gets `servers` installed first
    async fn connect(&mut self, servers: &[&Added]) -> Result<(), String> {
        if self.live.is_some() {
            return Ok(());
        }

        // The worker is libsmol's own executable, which the code can't write, unlike the venv's python
        let Workplace {
            cwd,
            copies,
            venv,
            sandbox,
            program,
            ..
        } = &*self.workplace;
        let mut command = sandbox.command(program, cwd);
        // Process lists would show it as `/proc/self/exe`
        #[cfg(target_os = "linux")]
        {
            let exe = worker::executable();
            command.arg0(exe);
        }
        command.arg(WORKER_ARG).arg(&venv.path);

        let failed = |e: io::Error| {
            let reason = cause(&e);
            prompt::unstarted(&reason)
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(failed)?;
        let address = listener.local_addr().map_err(failed)?;
        let token = new_id();
        let address = address.to_string();
        let control = control(&address, &token);

        command
            .env(CONTROL_ENV, control)
            .env(COPIES_ENV, copies)
            .stdin(Stdio::null());
        if self.rank == Rank::Main {
            command.env(MAIN_ENV, "on");
        }
        let marker = end_marker(&token);
        let (mut process, output) = Process::start(command, Some(marker)).map_err(failed)?;

        let accepting = async {
            let (stream, _) = listener.accept().await?;
            let (reader, writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let greeting = lines.next_line().await?;
            io::Result::Ok((lines, writer, greeting))
        };
        let timed = time::timeout(START_WAIT, accepting);
        // A worker that exits before connecting back doesn't make us wait out `START_WAIT`
        let accepted = select! {
            biased;
            accepted = timed => accepted.ok(),
            _ = process.wait() => None,
        };
        let expected = greeting(&token);
        let (lines, mut writer) = match accepted {
            Some(Ok((lines, writer, Some(said)))) if said == expected => (lines, writer),
            // An update replaced the executable while the session ran, which `/proc/self/exe` avoids on Linux
            Some(Ok((_, _, Some(said)))) if greeted(&said, &token).is_some() => {
                let version = greeted(&said, &token).unwrap_or_default();
                let reason = prompt::replaced(version);
                return Err(prompt::unstarted(&reason));
            }
            _ => {
                let reason = start_error(process, output).await;
                return Err(reason);
            }
        };

        let (control, mut sending) = mpsc::unbounded_channel::<String>();
        task::spawn(async move {
            while let Some(line) = sending.recv().await {
                if writer.write_all(line.as_bytes()).await.is_err() {
                    return;
                }
            }
        });

        let mut live = Live {
            process,
            lines,
            control,
            output,
            next_id: 0,
            calls: JoinSet::new(),
            running: HashMap::new(),
            media: Vec::new(),
            followed: Followed::default(),
        };

        // Startup output runs up to the end marker and doesn't belong to the first snippet
        let deadline = Instant::now() + START_WAIT;
        let mut ready = None;
        let mut ended = false;
        let functions = loop {
            let next = live.next();
            let event = time::timeout_at(deadline, next).await;
            match event {
                Ok(Event::Ready(functions)) => ready = Some(functions),
                Ok(Event::End(_)) => ended = true,
                Ok(Event::Gone) | Err(_) => {
                    let Live {
                        process, output, ..
                    } = live;
                    let reason = start_error(process, output).await;
                    return Err(reason);
                }
                Ok(_) => {}
            }
            if ended {
                if let Some(functions) = ready.take() {
                    break functions;
                }
            }
        };

        let own = live.process.members().await;
        live.followed.own = own.unwrap_or_default();

        for added in servers {
            live.install(&added.server, Some(&added.name)).await?;
        }
        self.functions = functions;
        self.live = Some(live);

        Ok(())
    }

    /// `servers` are already installed, and a replacement for a lost worker gets them back first
    pub(crate) async fn add_server(&mut self, server: Server, servers: &[&Added]) -> Result<Added> {
        self.reap();
        self.connect(servers).await.map_err(Error::Python)?;
        let live = self.live.as_mut().expect("connected");
        let installed = live.install(&server, None).await.map_err(Error::Python)?;

        match installed {
            Installed::Added { name, stubs } => Ok(Added {
                name,
                server,
                stubs,
            }),
            Installed::Taken { name, holder } => {
                let message = format!("{name}: name already used by server {holder}");
                Err(Error::Mcp(message))
            }
        }
    }

    /// Empty from a worker that is gone, which isn't restarted just for this
    pub(crate) async fn globals(&mut self) -> Globals {
        self.reap();
        let Some(live) = self.live.as_mut() else {
            return Globals::default();
        };

        let listing = live.request(HostCall::Globals, LISTING_GLOBALS);
        let listed = select! {
            biased;
            () = self.interrupt.wait() => return Globals::default(),
            listed = listing => listed,
        };
        match listed {
            Ok(globals) => globals,
            Err(reason) => {
                warn!("worker: globals not listed, {reason}");
                Globals::default()
            }
        }
    }

    /// Doesn't restart a lost worker just for this
    pub(crate) fn remove_server(&self, name: &str) {
        if let Some(live) = &self.live {
            let name = name.to_owned();
            let note = HostNote::RemoveServer { name };
            live.note(note);
        }
    }

    /// `shown` gets the clipped output so far now and then, `serve` answers what the code asks
    /// of the agent, and stopping before the code is sent returns Err
    pub(crate) async fn evaluate(
        &mut self,
        code: &str,
        servers: &[&Added],
        shown: &mut (dyn FnMut(&str) + Send),
        serve: &mut Serving<'_>,
    ) -> Result<Evaluated, CallError> {
        let interrupt = self.interrupt.clone();
        let failed = |reason: String| {
            let error = prompt::python(&reason);
            CallError::Execution(error)
        };

        // A worker lost while the model wrote the code never ran it
        if let Some(live) = self.live.as_mut().filter(|live| live.process.reaped()) {
            let (end, said) = live.process.remains(&live.output).await;
            let said = said.shown(MAX_OBSERVATION_CHARS / 2);
            self.reap();
            let reason = prompt::lost(end.as_ref(), said.trim());
            return Err(failed(reason));
        }

        let connected = select! {
            biased;
            connected = self.connect(servers) => connected.map_err(failed),
            () = interrupt.wait() => Err(CallError::Interrupted(None)),
        };
        connected?;

        let live = self.live.as_mut().expect("connected");
        // Views that arrived while a server was being added don't belong to this snippet
        live.media.clear();

        let run = HostCall::Run {
            code: code.to_owned(),
        };
        let id = live.send(run);

        let mut stopping: Option<Stopping> = None;
        let mut show_at = Instant::now() + SHOWN_EVERY;
        // The reply, and when to stop waiting for the output still on the way
        let mut replied: Option<(Result<Ran, String>, Instant)> = None;
        let mut ended: Option<Clip> = None;
        let outcome = loop {
            let now = Instant::now();
            let resend_at = stopping.as_ref().map_or(now, |stopping| stopping.resend);
            let give_up_at = stopping.as_ref().map_or(now, |stopping| stopping.give_up);
            let drained_at = replied.as_ref().map_or(now, |(_, at)| *at);
            let event = select! {
                biased;
                event = live.next() => event,
                () = interrupt.wait(), if stopping.is_none() => {
                    live.note(HostNote::Cancel { id });
                    let now = Instant::now();
                    let started = Stopping {
                        resend: now + CANCEL_EVERY,
                        give_up: now + STOP_WAIT,
                    };
                    stopping = Some(started);
                    continue;
                }
                () = time::sleep_until(resend_at), if stopping.is_some() => {
                    live.note(HostNote::Cancel { id });
                    if let Some(stopping) = &mut stopping {
                        stopping.resend += CANCEL_EVERY;
                    }
                    continue;
                }
                // Once the reply is in, the code has stopped and only its output is still on the way
                () = time::sleep_until(give_up_at), if stopping.is_some() && replied.is_none() => {
                    break Outcome::Stuck;
                }
                () = time::sleep_until(drained_at), if replied.is_some() => {
                    match replied.take() {
                        Some((ran, _)) => break Outcome::Replied(ran),
                        None => continue,
                    }
                }
                () = time::sleep_until(show_at) => {
                    show_at = Instant::now() + SHOWN_EVERY;
                    // What follows the end marker belongs to the next snippet
                    let fresh = match ended {
                        Some(_) => None,
                        None => live.output.fresh(MAX_OBSERVATION_CHARS),
                    };
                    if let Some(text) = fresh {
                        shown(&text);
                    }
                    continue;
                }
            };

            let (number, call) = match event {
                Event::Answer(number, reply) if number == id => {
                    let ran = match reply {
                        Ok(finished) => serde_json::from_value(finished).map_err(|e| e.to_string()),
                        Err(reason) => Err(reason),
                    };
                    if ended.is_some() {
                        break Outcome::Replied(ran);
                    }
                    replied = Some((ran, Instant::now() + OUTPUT_WAIT));
                    continue;
                }
                Event::End(part) => {
                    ended = Some(part);
                    match replied.take() {
                        Some((ran, _)) => break Outcome::Replied(ran),
                        None => continue,
                    }
                }
                Event::Gone => break Outcome::Gone,
                Event::Answer(..) | Event::Ready(_) => continue,
                Event::Call(number, call) => (number, call),
            };

            let calling = match call {
                WorkerCall::Tool {
                    server,
                    tool,
                    args,
                    called,
                } => {
                    let found = servers.iter().find(|added| added.name == server);
                    match found {
                        Some(added) => (added.server.call)(tool, args, called),
                        None => {
                            let reply = Err(NO_SERVER.to_owned());
                            let called = Called::from(reply);
                            let ready = future::ready(called);
                            Box::pin(ready)
                        }
                    }
                }
                WorkerCall::Service(service) => serve(service),
            };
            let abort = live.calls.spawn(async move { (number, calling.await) });
            live.running.insert(number, abort);
        };

        // Calls the snippet left behind end with it
        // Calls that finished meanwhile still answer with their result
        while let Some(joined) = live.calls.try_join_next() {
            if let Ok((number, called)) = joined {
                live.answer(number, called);
            }
        }
        let left = mem::take(&mut live.running);
        for (number, abort) in left {
            abort.abort();
            let refused = Err(LEFT_BEHIND.to_owned());
            let reply = Message::reply(number, refused);
            live.deliver(&reply);
        }
        while live.calls.join_next().await.is_some() {}

        let (end, rest) = match outcome {
            Outcome::Gone => {
                let (end, rest) = live.process.remains(&live.output).await;
                (end, Some(rest))
            }
            _ => (None, None),
        };
        let printed = match (ended, rest) {
            (Some(part), _) => part,
            (None, Some(rest)) => rest,
            (None, None) => live.output.take(),
        };
        let mut output = Evaluated {
            printed,
            error: None,
            media: mem::take(&mut live.media),
            repaired: None,
        };

        let failure = match outcome {
            Outcome::Replied(Ok(ran)) => {
                // Ones that ended since `ended` last looked stay until it reports them
                let members = live.members().await.unwrap_or_default();
                let left = &mut live.followed.left;
                for member in members {
                    match left.iter_mut().find(|known| known.same(&member)) {
                        // One in the middle of exec shows its name alone until it is seen again
                        Some(known) => *known = member,
                        None => left.push(member),
                    }
                }

                let judged = |error: String| match interrupt.triggered() {
                    true => CallError::Interrupted(Some(error)),
                    false => CallError::Execution(error),
                };
                output.error = ran.error.map(judged);
                output.repaired = ran.repaired;
                return Ok(output);
            }
            Outcome::Replied(Err(reason)) => Some(reason),
            Outcome::Gone => Some(prompt::exited(end.as_ref())),
            Outcome::Stuck => None,
        };

        self.live = None;
        self.restarted = true;
        let error = match failure {
            Some(reason) if !interrupt.triggered() => failed(reason),
            _ => CallError::Interrupted(None),
        };
        output.error = Some(error);
        Ok(output)
    }

    /// The programs that calls left running and that ended since, with the output printed since
    /// the last call, or None while they all run. Dropping the future loses nothing
    pub(crate) async fn ended(&mut self) -> Option<DoneStep> {
        let live = self
            .live
            .as_mut()
            .filter(|live| !live.followed.left.is_empty())?;
        let members = live.members().await?;
        let mut programs = Vec::new();
        for top in live.followed.tops() {
            let alive = members.iter().any(|member| member.same(top));
            if !alive {
                programs.push(program(top));
            }
        }
        live.followed.left = members;
        if programs.is_empty() {
            return None;
        }

        let said = live.output.take();
        let output = said.shown(MAX_OBSERVATION_CHARS);
        info!("programs: ended [count={}]", programs.len());
        Some(DoneStep {
            programs,
            output,
            ..DoneStep::default()
        })
    }

    /// The programs that calls left running, as they were when last seen
    pub(crate) fn running(&self) -> Vec<Program> {
        let Some(live) = &self.live else {
            return Vec::new();
        };

        let tops = live.followed.tops();
        tops.into_iter().map(program).collect()
    }

    /// Resolves once a program that a call left running has ended, for `ended` to take,
    /// and never while none runs
    pub(crate) async fn ending(&self) {
        let Some(live) = &self.live else {
            return future::pending().await;
        };
        let tops = live.followed.tops();
        if tops.is_empty() {
            return future::pending().await;
        }

        loop {
            time::sleep(PROGRAMS_EVERY).await;
            let Some(members) = live.members().await else {
                continue;
            };
            let alive = |top: &&Member| members.iter().any(|member| member.same(top));
            if !tops.iter().all(alive) {
                return;
            }
        }
    }
}

impl Followed {
    /// The followed processes whose parent isn't followed too, each a program of its own,
    /// as one whose parent is belongs to that program until it outlives it
    fn tops(&self) -> Vec<&Member> {
        let mut tops = Vec::new();
        for member in &self.left {
            let parented = self.left.iter().any(|other| other.pid == member.parent);
            let helper = member.command.contains(MULTIPROCESSING);
            if !parented && !helper {
                tops.push(member);
            }
        }
        tops
    }
}

impl Live {
    /// Leaves out the worker's own processes, and is None when they can't be listed
    async fn members(&self) -> Option<Vec<Member>> {
        let mut members = self.process.members().await?;
        let own = &self.followed.own;
        members.retain(|member| !own.iter().any(|own| own.same(member)));
        Some(members)
    }

    fn send(&mut self, call: HostCall) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.deliver(&Message::Request { id, call });
        id
    }

    fn note(&self, note: HostNote) {
        self.deliver(&Message::Note(note));
    }

    fn answer(&mut self, id: u64, called: Called) {
        self.running.remove(&id);
        self.media.extend(called.media);
        let reply = Message::reply(id, called.result);
        self.deliver(&reply);
    }

    fn deliver(&self, message: &ToWorker) {
        let mut line = serde_json::to_string(message).expect("serialize");
        line.push('\n');
        self.control.send(line).ok();
    }

    /// Takes in finished calls here and returns only on some other event
    async fn next(&mut self) -> Event {
        loop {
            // A part already read must win over the worker exiting right after it
            select! {
                biased;
                Some(part) = self.output.part() => return Event::End(part),
                // Cancelling halfway through a line doesn't lose what was already read
                line = self.lines.next_line() => {
                    let Ok(Some(line)) = line else {
                        return Event::Gone;
                    };
                    let Ok(message) = serde_json::from_str::<FromWorker>(&line) else {
                        return Event::Gone;
                    };
                    match message {
                        Message::Request { id, call } => return Event::Call(id, call),
                        Message::Answer { id, result } => return Event::Answer(id, Ok(result)),
                        Message::Error { id, error } => return Event::Answer(id, Err(error)),
                        Message::Note(WorkerNote::Ready { functions }) => {
                            return Event::Ready(functions);
                        }
                        Message::Note(WorkerNote::Cancel { id }) => {
                            if let Some(abort) = self.running.get(&id) {
                                abort.abort();
                            }
                        }
                        // The code can write to this connection too, so only a name in the copies folder is taken
                        Message::Note(WorkerNote::Viewed { label, media }) => {
                            let mut parts = media.file.components();
                            let first = parts.next();
                            let named = matches!(first, Some(Component::Normal(_)));
                            if named && parts.next().is_none() {
                                self.media.push((label, media));
                            }
                        }
                    }
                }
                Some(joined) = self.calls.join_next() => {
                    if let Ok((id, called)) = joined {
                        self.answer(id, called);
                    }
                }
            }
        }
    }

    async fn install(&mut self, server: &Server, name: Option<&str>) -> Result<Installed, String> {
        let call = HostCall::AddServer {
            source: server.name.clone(),
            name: name.map(str::to_owned),
            description: server.description.clone(),
            tools: server.tools.clone(),
        };
        self.request(call, ADDING_SERVER).await
    }

    /// Asks between snippets, refusing what threads the code started ask meanwhile with `busy`
    async fn request<T: DeserializeOwned>(
        &mut self,
        call: HostCall,
        busy: &str,
    ) -> Result<T, String> {
        let id = self.send(call);

        let answered = loop {
            match self.next().await {
                Event::Answer(number, reply) if number == id => break reply?,
                Event::Gone => return Err(prompt::exited(None)),
                // A thread the code started waits for every call it makes
                Event::Call(number, _) => {
                    let refused = Err(busy.to_owned());
                    let reply = Message::reply(number, refused);
                    self.deliver(&reply);
                }
                Event::Answer(..) | Event::Ready(_) | Event::End(_) => {}
            }
        };
        serde_json::from_value(answered).map_err(|e| e.to_string())
    }
}

fn program(member: &Member) -> Program {
    let command = cut_inline(&member.command, MAX_COMMAND_CHARS);
    Program {
        pid: member.pid,
        command,
    }
}

async fn start_error(mut process: Process, output: Output) -> String {
    let (_, said) = process.remains(&output).await;
    let said = said.shown(MAX_OBSERVATION_CHARS / 2);
    prompt::unstarted(said.trim())
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::agent::{serve, Delegating};
    use crate::config::Access;
    use crate::prompt;
    use crate::steps::{CallError, MAX_OBSERVATION_CHARS};
    use crate::tests::{cancel_soon, temp_dir, write_file, Temp};
    use crate::tools::mcp::Server;
    use crate::tools::memories::Memories;
    use crate::utils::{block_on, new_id, truncated, Interrupt};
    use crate::worker::host::{Added, Called, Calling, Rank, Worker, STOP_WAIT};
    use crate::worker::workplace::tests::opened;
    use crate::worker::{AgentCall, Imported, Service, Variable};
    use serde_json::{json, Map, Value};
    use std::env;
    use std::fs;
    use std::ops::{Deref, DerefMut};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::time;

    /// Keeps track of installed servers in place of the MCP pool, and answers as the agent would
    struct Tried {
        worker: Worker,
        servers: Vec<Added>,
        delegate: Box<Delegating<'static>>,
        memories: Memories,
        writable: bool,
    }

    /// For code that asks the agent for nothing
    pub(crate) fn unserved(service: Service) -> Calling {
        panic!("{service:?}");
    }

    impl Deref for Tried {
        type Target = Worker;

        fn deref(&self) -> &Worker {
            &self.worker
        }
    }

    impl DerefMut for Tried {
        fn deref_mut(&mut self) -> &mut Worker {
            &mut self.worker
        }
    }

    /// A call's result with its output cleaned as the model is shown it, but uncut
    struct Shown {
        printed: String,
        error: Option<CallError>,
        repaired: Option<String>,
    }

    impl Tried {
        fn eval(&mut self, code: &str) -> Shown {
            let mut ignore = |_: &str| {};
            let servers: Vec<&Added> = self.servers.iter().collect();
            let delegate = &mut *self.delegate;
            let memories = &self.memories;
            let writable = self.writable;
            let mut serving = |service: Service| serve(service, delegate, memories, writable);
            let evaluating = self
                .worker
                .evaluate(code, &servers, &mut ignore, &mut serving);
            let output = block_on(evaluating).expect("evaluate");
            Shown {
                printed: output.printed.shown(usize::MAX),
                error: output.error,
                repaired: output.repaired,
            }
        }

        /// Replaces a server with the same name and returns its name in code
        fn add(&mut self, server: &Server) -> Result<String, String> {
            self.remove(&server.name);
            let servers: Vec<&Added> = self.servers.iter().collect();
            let adding = self.worker.add_server(server.clone(), &servers);
            let added = block_on(adding).map_err(|e| e.to_string())?;
            let name = added.name.clone();
            self.servers.push(added);
            Ok(name)
        }

        fn remove(&mut self, source: &str) {
            let found = self
                .servers
                .iter()
                .position(|added| added.server.name == source);
            if let Some(index) = found {
                let added = self.servers.remove(index);
                self.worker.remove_server(&added.name);
            }
        }
    }

    fn build(interrupt: Interrupt, cwd: PathBuf, rank: Rank) -> Temp<Tried> {
        let dir = temp_dir("session");
        let workplace = opened(&cwd, &dir, &Access::default());
        let restarted = false;
        let building = Worker::new(&interrupt, Arc::new(workplace), &[], rank, restarted);
        let worker = block_on(building).expect("worker");

        let no_agents = |call: AgentCall| -> Calling { panic!("{call:?}") };
        let value = Tried {
            worker,
            servers: Vec::new(),
            delegate: Box::new(no_agents),
            memories: Memories::new(&dir, &cwd),
            // Agents that agent() starts only recall
            writable: rank == Rank::Main,
        };
        Temp { value, dir }
    }

    fn fresh() -> Temp<Tried> {
        let (worker, _) = interruptible();
        worker
    }

    fn workspace(name: &str) -> (Temp<Tried>, PathBuf) {
        let dir = temp_dir(name);
        fs::create_dir_all(&dir).expect("dir");
        let worker = build(Interrupt::default(), dir.clone(), Rank::Main);
        (worker, dir)
    }

    fn interruptible() -> (Temp<Tried>, Interrupt) {
        let interrupt = Interrupt::default();
        let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let worker = build(interrupt.clone(), cwd, Rank::Main);
        (worker, interrupt)
    }

    fn server(name: &str, call: impl Fn(&str, &Value) -> Value + Send + Sync + 'static) -> Server {
        let echo = json!({
            "name": "echo-text",
            "description": "Echoes the text.",
            "inputSchema": {"type": "object", "properties": {"the-text": {"type": "string"}}},
        });
        let call = move |function: String, args: Map<String, Value>, _: String| -> Calling {
            let value = call(&function, &Value::Object(args));
            let called = Called::from(Ok(json!({ "text": value })));
            Box::pin(async move { called })
        };

        Server {
            name: name.to_owned(),
            description: format!("MCP server {name}"),
            tools: vec![echo],
            call: Arc::new(call),
        }
    }

    #[test]
    fn interrupt_escapes_except() {
        let (mut worker, interrupt) = interruptible();
        cancel_soon(&interrupt, 0);
        let code = "noted = 'café'\n\
            try:\n    while True:\n        pass\nexcept Exception:\n    print('caught')\n";
        let output = worker.eval(code);

        assert_eq!(output.printed, "");
        let stopped = matches!(output.error, Some(CallError::Interrupted(Some(_))));
        assert!(stopped, "{:?}", output.error);

        interrupt.begin(1);
        let output = worker.eval("[noted, 1 + 1]");
        assert_eq!(output.printed, "['café', 2]");
        assert!(!worker.take_restarted());
    }

    /// Names show as their def, class or import wrote them while they still hold what it bound,
    /// and listing them runs none of the code's own methods, metaclasses or annotations
    #[test]
    fn globals_listed() {
        let mut worker = fresh();
        let code = "import json, contextlib, importlib\nfrom os.path import join\nfrom math import pi\n\
            if True:\n    import collections as co\n\
            codec = importlib.import_module('json')\n\
            class Meta(type):\n    def __getattribute__(cls, name):\n        raise RuntimeError('meta')\n    \
            @property\n    def __name__(cls):\n        raise RuntimeError('name')\n    \
            def __eq__(cls, other):\n        raise RuntimeError('eq')\n    __hash__ = type.__hash__\n\
            class Trap(list, metaclass=Meta):\n    def __len__(self):\n        raise RuntimeError('len')\n    \
            def __getattribute__(self, name):\n        raise RuntimeError('attribute')\n\
            @contextlib.contextmanager\ndef opened(path: Missing, mode='r') -> Missing:\n    yield\n\
            class Lazy(type(json)):\n    def __getattribute__(self, name):\n        raise RuntimeError('module')\n\
            lazy = Lazy('naïve')\n\
            def row[T](n: T, word='café') -> T:\n    return n\n\
            pi *= 2\ntrap = Trap()\nwords = ['café', 'naïve']\ngone = 1\n\
            globals()['caf\\udce9'] = 1\n_hidden = 1";
        let output = worker.eval(code);
        assert_eq!(output.error, None, "{}", output.printed);
        // The import after the error never ran, though the line before it rebound its name
        let output = worker.eval("join = 'résumé'\n1 / 0\nfrom os.path import join");
        assert!(output.error.is_some(), "{}", output.printed);
        // A def whose name is gone gives no header once the name holds something else
        let output = worker.eval("def gone(n):\n    return n\ndel gone");
        assert_eq!(output.error, None, "{}", output.printed);
        let output = worker.eval("gone = None");
        assert_eq!(output.error, None, "{}", output.printed);

        let listing = worker.globals();
        let globals = block_on(listing);
        let imported = |name: &str, origin: Option<&str>| Imported {
            name: name.to_owned(),
            origin: origin.map(str::to_owned),
        };
        let imports = [
            imported("json", None),
            imported("contextlib", None),
            imported("importlib", None),
            imported("co", Some("collections")),
            imported("codec", Some("json")),
            imported("lazy", Some("naïve")),
        ];
        assert_eq!(globals.imports, imports);
        let defined = [
            "class Meta(type)",
            "class Trap(list, metaclass=Meta)",
            "def opened(path: Missing, mode='r') -> Missing",
            "class Lazy(type(json))",
            "def row[T](n: T, word='café') -> T",
        ];
        assert_eq!(globals.defined, defined);
        let variable = |name: &str, kind: &str, length: Option<usize>| Variable {
            name: name.to_owned(),
            kind: kind.to_owned(),
            length,
        };
        let values = [
            variable("join", "str", Some(6)),
            variable("pi", "float", None),
            variable("trap", "Trap", None),
            variable("words", "list", Some(2)),
            variable("gone", "NoneType", None),
        ];
        assert_eq!(globals.values, values);
    }

    /// A syntax error the tag repair can't fix comes back with its line,
    /// though what follows the parse assumes there was one
    #[test]
    fn syntax_errors_reported() {
        let mut worker = fresh();
        let output = worker.eval("x = (1, 'café'");
        let Some(CallError::Execution(error)) = &output.error else {
            panic!("{:?}", output.error);
        };
        assert!(error.contains("x = (1, 'café'"), "{error}");
    }

    #[test]
    fn stray_tags_removed() {
        let mut worker = fresh();
        let mut ran = |code: &str| {
            let output = worker.eval(code);
            assert_eq!(output.error, None, "{code}");
            let repaired = output.repaired.as_deref().map(str::trim).map(str::to_owned);
            (output.printed, repaired)
        };

        let wrapped = ran("<eval>\n<eval>\nx = 'café'\nprint(x)\n</eval>\n</eval>");
        let code = "x = 'café'\nprint(x)".to_owned();
        assert_eq!(wrapped, ("café".to_owned(), Some(code)));
        let inline = ran("print('naïve')</eval>");
        assert_eq!(inline.1.as_deref(), Some("print('naïve')"));
        let faked = ran("print(3)</eval>Observation:\n3");
        assert_eq!(faked.1.as_deref(), Some("print(3)"));
        let closed = ran("<eval>\nprint(4)\n<eval>\nThen check the output.");
        assert_eq!(closed, ("4".to_owned(), Some("print(4)".to_owned())));
        let bar = '\u{ff5c}';
        let wide = format!(
            "print('jalapeño')\n</{bar}{bar}X{bar}{bar} parameter>\n</{bar}{bar}X{bar}{bar} invoke>"
        );
        let wide = ran(&wide);
        assert_eq!(
            wide,
            ("jalapeño".to_owned(), Some("print('jalapeño')".to_owned()))
        );
        let led = format!(
            "<parameter name=\"code\">x = 'piñata'\nprint(x)\n</{bar}{bar}X{bar}{bar} parameter>"
        );
        let led = ran(&led);
        let code = "x = 'piñata'\nprint(x)".to_owned();
        assert_eq!(led, ("piñata".to_owned(), Some(code)));
        // Code in another call that follows isn't run
        let looped = "print('crème brûlée')\n</</eval>\n</invoke>\n<parameter name=\"code\">print('repeated')\n</parameter>\n</calls>";
        let looped = ran(looped);
        assert_eq!(
            looped,
            (
                "crème brûlée".to_owned(),
                Some("print('crème brûlée')".to_owned())
            )
        );

        // Tags inside strings are left alone
        let quoted = ran("s = '''\n</eval>\n'''\nprint(len(s), '<eval>')\n</eval>");
        let code = "s = '''\n</eval>\n'''\nprint(len(s), '<eval>')".to_owned();
        assert_eq!(quoted, ("9 <eval>".to_owned(), Some(code)));
        let page = ran("résumé = '''\n</div>\n'''\nprint(len(résumé))\n</invoke>");
        let code = "résumé = '''\n</div>\n'''\nprint(len(résumé))".to_owned();
        assert_eq!(page, ("8".to_owned(), Some(code)));
        let block = ran("façade = '''\n<div>\n'''\nprint(len(façade))\n</eval>");
        let code = "façade = '''\n<div>\n'''\nprint(len(façade))".to_owned();
        assert_eq!(block, ("7".to_owned(), Some(code)));
        let valid = ran("html = '<eval>café</eval>'\nprint(html)");
        assert_eq!(valid, ("<eval>café</eval>".to_owned(), None));

        let broken = worker.eval("print(1\n</eval>");
        assert_eq!(broken.repaired, None);
        assert!(matches!(broken.error, Some(CallError::Execution(_))));
    }

    /// A worker stuck after an interrupt and one that exits on its own are both replaced
    #[test]
    fn cancel_replaced() {
        let stuck = "print('stuck')\nimport time\n\
            while True:\n    try:\n        time.sleep(10)\n    \
            except BaseException:\n        pass";
        let exiting =
            "import os, time\ntry:\n    time.sleep(30)\nexcept BaseException:\n    os._exit(1)";
        let (mut worker, interrupt) = interruptible();

        let mut turn = 0;
        for (code, said) in [(stuck, "stuck"), (exiting, "")] {
            worker.eval("x = 1");
            cancel_soon(&interrupt, turn);
            let stopped = worker.eval(code);
            assert_eq!(stopped.printed, said, "{code}");
            assert_eq!(stopped.error, Some(CallError::Interrupted(None)), "{code}");

            turn += 1;
            interrupt.begin(turn);
            let output = worker.eval("'x' in globals()");
            assert_eq!(output.printed, "False", "{code}");
            assert!(worker.take_restarted(), "{code}");
        }
    }

    /// The code can write to the agent's connection itself, but a note naming anything but a copy is dropped.
    /// Windows sockets are handles that `os.fstat` doesn't see, and Windows has no sandbox to get past
    #[cfg(unix)]
    #[test]
    fn forged_views_dropped() {
        let code = "import json, os, stat\n\
            def forge(name):\n    \
                media = {'kind': 'image', 'mime': 'image/png', 'file': name}\n    \
                note = json.dumps({'method': 'viewed', 'params': {'label': name, 'media': media}})\n    \
                for fd in range(3, 64):\n        \
                    try:\n            mode = os.fstat(fd).st_mode\n        except OSError:\n            continue\n        \
                    if stat.S_ISSOCK(mode):\n            os.write(fd, (note + '\\n').encode())\n            return\n\
            for name in ['/etc/hostname', '../café.png', 'naïve.png']:\n    forge(name)";
        let mut worker = fresh();
        let mut ignore = |_: &str| {};
        let mut none = unserved;
        let evaluating = worker.evaluate(code, &[], &mut ignore, &mut none);
        let output = block_on(evaluating).expect("evaluate");
        assert_eq!(output.error, None);

        let labels: Vec<&str> = output.media.iter().map(|(label, _)| &**label).collect();
        assert_eq!(labels, ["naïve.png"]);
    }

    /// Like what `pip install -e` leaves, a .pth file in the venv imports in the same session
    #[test]
    fn pth_processed() {
        let dir = temp_dir("editable");
        fs::create_dir_all(&dir).expect("create dir");
        let name = new_id().replace('-', "_");
        let name = format!("smol_{name}");
        let module = dir.join(format!("{name}.py"));
        fs::write(&module, "value = 'naïve'\n").expect("write");

        let mut worker = fresh();
        let shown = dir.display();
        let code = format!(
            "import pathlib, site\n\
             pth = pathlib.Path(site.getsitepackages()[0]) / '{name}.pth'\n\
             _ = pth.write_text({shown:?})"
        );
        worker.eval(&code);
        let code = format!("import {name}\npth.unlink()\n{name}.value");
        let output = worker.eval(&code);
        fs::remove_dir_all(&dir).ok();

        assert_eq!(output.error, None);
        assert_eq!(output.printed, "naïve");
    }

    /// A venv the code makes, inside the session's or inside that one, gets its own python,
    /// pip and activation scripts
    #[cfg(unix)]
    #[test]
    fn venvs_nested() {
        let (mut worker, dir) = workspace("nested");
        let code = "import os, subprocess, venv\n\
             venv.create('inner', symlinks=True, with_pip=True)\n\
             names = os.listdir('inner/bin')\n\
             print('libsmol' in names, 'pip' in names, 'activate' in names)\n\
             script = '. inner/bin/activate && python -m venv --without-pip deeper && \
                 deeper/bin/python -c \"import sys; print(sys.prefix)\"'\n\
             run = subprocess.run(script, shell=True, capture_output=True, text=True)\n\
             print(run.returncode, run.stdout.strip() == os.path.abspath('deeper'))";
        let output = worker.eval(code);
        fs::remove_dir_all(&dir).ok();

        assert_eq!(output.error, None, "{}", output.printed);
        assert_eq!(output.printed, "False True True\n0 True");
    }

    /// A venv the code makes gets a python.exe that relays to the interpreter, pip and activation scripts
    #[cfg(windows)]
    #[test]
    fn venvs_nested() {
        let (mut worker, dir) = workspace("nested");
        let code = "import os, subprocess, venv\n\
             venv.create('café', with_pip=True)\n\
             names = os.listdir(r'café\\Scripts')\n\
             print('python.exe' in names, 'pip.exe' in names, 'activate.bat' in names)\n\
             probe = 'import sys; print(sys.prefix)'\n\
             run = subprocess.run([r'café\\Scripts\\python.exe', '-c', probe], capture_output=True, text=True)\n\
             print(run.returncode, run.stdout.strip() == os.path.abspath('café'))";
        let output = worker.eval(code);
        fs::remove_dir_all(&dir).ok();

        assert_eq!(output.error, None, "{}", output.printed);
        // venv warns first when the path has short names, as the CI runner's TEMP does
        assert!(
            output.printed.ends_with("True True True\n0 True"),
            "{}",
            output.printed
        );
    }

    /// Extension modules reach the interpreter by the names python314.dll and python3.dll,
    /// and the venv's python relays to it
    #[cfg(windows)]
    #[test]
    fn interpreter_named() {
        let (minor, _) = worker::PYTHON_VERSION.rsplit_once('.').expect("version");
        let name = format!("python{}.dll", minor.replace('.', ""));
        let mut worker = fresh();
        let code = format!(
            "import ctypes, subprocess, sys\n\
             address = lambda function: ctypes.cast(function, ctypes.c_void_p).value\n\
             forwarded = address(ctypes.WinDLL('python3').Py_IsInitialized) == address(ctypes.pythonapi.Py_IsInitialized)\n\
             kernel = ctypes.WinDLL('kernel32')\n\
             kernel.GetModuleHandleW.restype = ctypes.c_void_p\n\
             named = kernel.GetModuleHandleW({name:?}) == sys.dllhandle\n\
             probe = 'import sys; print(sys.prefix == sys.base_prefix, sys.executable == sys._base_executable)'\n\
             run = subprocess.run(['python', '-c', probe], capture_output=True, text=True)\n\
             print(forwarded, named, run.returncode, run.stdout.strip())"
        );
        let output = worker.eval(&code);

        assert_eq!(output.error, None, "{}", output.printed);
        assert_eq!(output.printed, "True True 0 False False");
    }

    /// A pool fails at once instead of waiting on a process that can't import the code
    #[test]
    fn pool_refuses_code() {
        let mut worker = fresh();
        let code = "import multiprocessing\n\
             def square(x):\n    return x * x\n\
             with multiprocessing.get_context('spawn').Pool(1) as pool:\n    \
                 pool.map(square, [3])";
        let output = worker.eval(code);
        let message = output.error.expect("error").to_string();
        assert!(message.starts_with("PicklingError: "), "{message}");

        let output = worker.eval("import pickle\npickle.loads(pickle.dumps(square))(3)");
        assert_eq!(output.error, None);
        assert_eq!(output.printed, "9");
    }

    /// Agents that agent() started only recall, even when their code asks the agent directly
    #[test]
    fn memories_nested() {
        let dir = temp_dir("nested");
        fs::create_dir_all(&dir).expect("dir");
        let mut worker = build(Interrupt::default(), dir.clone(), Rank::Delegated);
        let names: Vec<String> = worker.functions().iter().map(|f| f.name.clone()).collect();
        let code = "ask = recall.__globals__['_ask']\n\
             try:\n    ask('remember', {'text': 'Uses tabs', 'project': True})\n\
             except ValueError as error:\n    print(error)\n\
             recall('tabs')";
        let output = worker.eval(code);
        fs::remove_dir_all(&dir).ok();

        assert!(names.contains(&"recall".to_owned()), "{names:?}");
        assert!(!names.contains(&"remember".to_owned()), "{names:?}");
        assert!(!names.contains(&"forget".to_owned()), "{names:?}");
        let expected = format!("{}\n[]", prompt::NESTED);
        assert_eq!(output.printed, expected);
    }

    /// Dropping a stream made on fd 1 closes the fd, which a file opened next would take
    #[test]
    fn stdout_fd_restored() {
        let (mut worker, dir) = workspace("fd");
        let code = "import io, sys\n\
             sys.stdout = io.TextIOWrapper(open(sys.stdout.fileno(), 'wb', 0), write_through=True)\n\
             print('café')";
        let output = worker.eval(code);
        assert_eq!(output.printed, "café");

        let code = "import subprocess\n\
             notes = open('notes.txt', 'w')\n\
             print('naïve')\n\
             _ = subprocess.run([sys.executable, '-c', 'print(\"résumé\")'])\n\
             notes.close()";
        let output = worker.eval(code);
        let notes = fs::read_to_string(dir.join("notes.txt")).expect("notes");
        fs::remove_dir_all(&dir).ok();

        assert_eq!(output.error, None);
        assert_eq!(output.printed, "naïve\nrésumé");
        assert_eq!(notes, "");
    }

    /// A thread the code started gets a reply to a call the snippet's end stopped
    #[test]
    fn left_calls_answered() {
        let mut worker = fresh();
        let mut slow = server("slow", |_, _| json!("late"));
        slow.call = Arc::new(|_: String, _: Map<String, Value>, _: String| -> Calling {
            Box::pin(async {
                time::sleep(Duration::from_secs(30)).await;
                Called::from(Ok(json!({ "text": "late" })))
            })
        });
        worker.add(&slow).expect("add server");

        let code = "import threading, time\n\
             replies = []\n\
             def call():\n    try:\n        replies.append(slow.echo_text('café'))\n    \
                 except Exception as error:\n        replies.append(str(error))\n\
             caller = threading.Thread(target=call)\ncaller.start()\ntime.sleep(0.5)";
        let output = worker.eval(code);
        assert_eq!(output.error, None);

        let output = worker.eval("caller.join(5)\n[caller.is_alive(), replies]");
        let expected = format!("[False, ['slow.echo_text: {}']]", prompt::LEFT_BEHIND);
        assert_eq!(output.printed, expected);
    }

    /// A thread printing between snippets never blocks, and its output heads the next snippet
    #[test]
    fn idle_output_drained() {
        let mut worker = fresh();
        let code = "import threading\ndone = False\n\
             def chatter():\n    global done\n    for i in range(3000):\n        print(f'{i:04} café ' + 'x' * 190)\n    done = True\n\
             threading.Timer(1, chatter).start()";
        let output = worker.eval(code);
        assert_eq!(output.error, None, "{}", output.printed);
        let waiting = async { time::sleep(Duration::from_secs(3)).await };
        block_on(waiting);

        let output = worker.eval("done");
        let printed = &output.printed;
        assert!(printed.starts_with("0000 café "), "{printed}");
        assert!(printed.ends_with("True"), "{printed}");
        assert!(!printed.contains("1500 café "), "{printed}");
    }

    /// A program the code leaves running is followed until it ends, with what it printed,
    /// and one it started belongs to it, while one that ended within the call is never followed
    #[test]
    fn programs_followed() {
        let mut worker = fresh();
        let code = "import subprocess, sys, time\n\
            inner = \"import time; time.sleep(1); print('café done ' * 2000, flush=True)\"\n\
            outer = f'import subprocess, sys; subprocess.run([sys.executable, \"-c\", {inner!r}])'\n\
            ran = subprocess.run([sys.executable, '-c', 'pass'])\n\
            program = subprocess.Popen([sys.executable, '-c', outer])\n\
            time.sleep(0.5)\n\
            print(program.pid)";
        let output = worker.eval(code);
        assert_eq!(output.error, None, "{}", output.printed);
        let pid: u32 = output.printed.parse().expect("pid");
        assert_eq!(block_on(worker.ended()), None);

        // The inner program may end a moment before the one it belongs to
        let started = Instant::now();
        let done = loop {
            let waiting = async { time::timeout(Duration::from_secs(10), worker.ending()).await };
            block_on(waiting).expect("ending");
            if let Some(done) = block_on(worker.ended()) {
                break done;
            }
            assert!(started.elapsed() < Duration::from_secs(10));
        };
        let pids: Vec<u32> = done.programs.iter().map(|program| program.pid).collect();
        assert_eq!(pids, [pid], "{done:?}");
        let command = &done.programs[0].command;
        assert!(command.contains("import subprocess, sys;"), "{command}");
        let printed = "café done ".repeat(2000);
        assert_eq!(done.output, truncated(&printed, MAX_OBSERVATION_CHARS));
        assert_eq!(block_on(worker.ended()), None);
    }

    /// A pipe is flushed after each snippet, and one a thread is stuck writing to doesn't hold the snippet up
    #[test]
    fn pipes_flushed() {
        let mut worker = fresh();
        let code = "import os, threading, time\n\
             reader, writer = os.pipe()\n\
             os.set_blocking(reader, False)\n\
             pipe = open(writer, 'wb')\n\
             pipe.write('café'.encode())\n\
             unread, full = os.pipe()\n\
             stuck = open(full, 'wb')\n\
             threading.Thread(target=stuck.write, args=(b'x' * 1_000_000,), daemon=True).start()\n\
             time.sleep(0.3)";
        let output = worker.eval(code);
        assert_eq!(output.error, None, "{}", output.printed);

        let output = worker.eval("os.read(reader, 100).decode()");
        assert_eq!(output.printed, "café");

        // Each flush ends once its pipe drains
        let code = "drained = 0\n\
             while drained < 1_000_000:\n    drained += len(os.read(unread, 65536))\n\
             flushers = list(open.__globals__['_FLUSHERS'].values())\n\
             deadline = time.monotonic() + 5\n\
             while any(flusher.is_alive() for flusher in flushers) and time.monotonic() < deadline:\n    time.sleep(0.01)\n\
             [len(flushers), any(flusher.is_alive() for flusher in flushers)]";
        let output = worker.eval(code);
        assert_eq!(output.printed, "[2, False]");
    }

    /// A search interrupted halfway stops in place without waiting for the worker to be killed
    #[test]
    fn search_interrupted() {
        let dir = temp_dir("slow-grep");
        // This regex takes a long time on random a's and b's that never repeat
        let mut state: u32 = 1;
        let mut text = String::new();
        for _ in 0..4000 {
            for _ in 0..1000 {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let letter = if state >> 31 == 0 { 'a' } else { 'b' };
                text.push(letter);
            }
            text.push('\n');
        }
        write_file(&dir.join("random.txt"), text.as_bytes());
        let interrupt = Interrupt::default();
        let mut worker = build(interrupt.clone(), dir.clone(), Rank::Main);

        worker.eval("noted = 1");
        cancel_soon(&interrupt, 0);
        let started = Instant::now();
        let output = worker.eval("grep(r'a[ab]{30}c', 'random.txt')");
        let stopped = matches!(output.error, Some(CallError::Interrupted(Some(_))));
        assert!(stopped, "{:?}", output.error);
        assert!(started.elapsed() < STOP_WAIT, "{:?}", started.elapsed());

        interrupt.begin(1);
        let kept = worker.eval("noted");
        fs::remove_dir_all(&dir).ok();
        assert_eq!(kept.printed, "1");
        assert!(!worker.take_restarted());
    }
}
