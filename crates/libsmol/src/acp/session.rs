use crate::acp::mcp::McpPool;
use crate::acp::rpc::{Notice, Peer, SessionPeer};
use crate::acp::translator::Translator;
use crate::acp::{rfc3339, task_of, to_value, App};
use crate::agent::{
    title_of, Agent, Asking, Brief, Event, Nudging, Place, Queue, Sink, Task, TaskViews, Team,
};
use crate::config::{self, AgentConfig, McpConfig, Models};
use crate::history::{Lock, Record, SessionMeta, Settings};
use crate::spawn::{run_shell, Process};
use crate::steps::{AgentMemory, Exit, MemoryStep};
use crate::tools::memories::{Digest, Memories};
use crate::utils::Interrupt;
use crate::worker::host::Worker;
use crate::worker::venv::Venv;
use crate::worker::workplace::Workplace;
use crate::{Error, Result};
use agent_client_protocol_schema::v1::{
    ContentBlock, LoadSessionResponse, NewSessionResponse, PromptResponse, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigOptionValue, SessionConfigSelectOption,
    SessionInfoUpdate, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, StopReason,
};
use reaper::tilde;
use reaper::wire::MCP_PREFIX;
use serde::Deserialize;
use serde_json::Value;
use std::future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::select;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::oneshot::{self, error::TryRecvError};
use tokio::sync::Notify;
use tokio::task::{self, AbortHandle, JoinHandle};
use tokio::time;
use tracing::{error, info, warn};

/// Starts once the closing turn is saved, and only the title is lost past serve's SAVE_WAIT
const TITLE_WAIT: Duration = Duration::from_secs(3);
const MODEL_OPTION: &str = "model";
const EFFORT_OPTION: &str = "effort";

/// All of these wait in the channel while a turn is running
pub(crate) enum Message {
    Prompt(Prompt),
    Shell(Shell),
    Option(Value, SetSessionConfigOptionRequest),
}

pub(crate) struct Prompt {
    pub(crate) id: Value,
    /// None when the client hands the turn to the programs that ended
    pub(crate) task: Option<Task>,
    pub(crate) turn: u64,
}

pub(crate) struct Shell {
    pub(crate) id: Value,
    pub(crate) command: String,
    pub(crate) turn: u64,
}

/// `Router::open` reads model.toml and agent.toml, and the session task reads the rest
/// and sends the reply itself
pub(crate) struct Opening {
    pub(crate) id: Value,
    pub(crate) session: String,
    pub(crate) cwd: PathBuf,
    pub(crate) loading: bool,
    /// The previous task of the same session, which must end before locking
    /// and reading the saved session
    pub(crate) previous: Option<JoinHandle<()>>,
    pub(crate) models: Models,
    pub(crate) agent: AgentConfig,
    pub(crate) app: Arc<App>,
    pub(crate) interrupt: Interrupt,
    pub(crate) enqueued: Arc<Notify>,
    pub(crate) memories: Memories,
}

#[derive(Deserialize)]
struct Queued {
    prompts: Vec<Vec<ContentBlock>>,
}

struct Session {
    memory: AgentMemory,
    models: Models,
    agent: Agent,
    /// Lent to the agent during a turn, with MCP servers added and removed between turns
    worker: Worker,
    servers: McpPool,
    /// Programs that user commands left in the background, which end with the session
    shell_left: Vec<Process>,
    keeper: Keeper,
    _lock: Option<Lock>,
}

/// Lent to the agent during a turn to show events and save
struct Keeper {
    meta: SessionMeta,
    /// Saved with the session
    team: Arc<Team>,
    app: Arc<App>,
    titling: Option<Titling>,
    unsaved: Unsaved,
    translator: Translator,
    peer: SessionPeer,
}

/// The model choosing a title for the session, which stops when this is dropped
struct Titling {
    /// Closes without a title when the model fails
    receiver: oneshot::Receiver<String>,
    task: AbortHandle,
}

impl Titling {
    /// Resolves once the model has chosen or failed
    async fn chosen(&mut self) -> Option<String> {
        let receiver = &mut self.receiver;
        receiver.await.ok()
    }
}

impl Drop for Titling {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The messages the client queues during a turn, which the session's agent takes as task updates
struct ClientQueue {
    peer: SessionPeer,
    enqueued: Arc<Notify>,
}

#[derive(Default)]
struct Unsaved(Option<String>);

impl Unsaved {
    /// Pass None after a successful save, and the same error is returned only once
    fn report(&mut self, error: Option<String>) -> Option<String> {
        let repeated = self.0 == error;
        self.0 = error.clone();
        error.filter(|_| !repeated)
    }
}

/// Only `Router` holds the sender of `inbox`, and dropping it closes the session
pub(crate) async fn open(opening: Opening, mut inbox: UnboundedReceiver<Message>) {
    let Opening {
        id,
        session,
        cwd,
        loading,
        previous,
        models,
        agent,
        app,
        interrupt,
        enqueued,
        memories,
    } = opening;

    let starting = async {
        if let Some(previous) = previous {
            previous.await.ok();
        }
        if inbox.is_closed() {
            return Err(Error::SessionNotFound(session.clone()));
        }

        // Read the saved session only after locking because another process
        // saves its last step before releasing the lock
        let lock = app.store.lock(&cwd, &session)?;
        let record = match loading {
            true => app.store.load(&cwd, &session)?,
            false => Record {
                meta: SessionMeta::new(session.clone(), Settings::default()),
                memory: AgentMemory::default(),
                agents: Vec::new(),
            },
        };
        let Record {
            mut meta,
            memory,
            agents,
        } = record;
        // Agents the code started in earlier turns take further tasks
        let team = Arc::new(Team::restored(agents));
        let peer = SessionPeer::new(Arc::clone(&app.peer), &meta.id);

        let (settings, mut notices) = meta.settings.resumed(&cwd, &models, &agent);
        meta.settings = settings;

        let (instructions, skipped) = config::instructions(&app.config, &cwd);
        for problem in skipped {
            let notice = problem.to_string();
            notices.push(notice);
        }
        let (skills, skipped) = config::skills(&app.config, &cwd);
        for problem in skipped {
            let notice = problem.to_string();
            notices.push(notice);
        }
        for entry in &agent.skipped {
            let notice = entry.problem.to_string();
            notices.push(notice);
        }
        let digest = match memories.digest().await {
            Ok(digest) => digest,
            Err(problem) => {
                let notice = Error::Memory(problem).to_string();
                notices.push(notice);
                Digest::default()
            }
        };

        let copies = app.store.copies(&cwd, &meta.id)?;
        let effort = meta.settings.effort.as_deref();
        let built = models.make(&meta.settings.model, effort, &copies)?;
        let options = options(&models, &meta.settings, &agent.servers);

        // Servers need neither the venv nor the worker, so they connect while those start
        let pool = McpPool::new(agent.servers, &meta.settings, &copies, peer.clone());
        let window = built.context_window();
        let mut translator = Translator::new(peer.clone(), window, cwd.clone(), copies.clone());
        let agents = team.records();
        translator.replay(&memory, &agents);

        let project = app.store.project(&cwd);
        let venv = Venv::open(&project, &cwd).await;
        let venv = venv.map_err(Error::Python)?;
        if inbox.is_closed() {
            return Err(Error::SessionNotFound(meta.id));
        }
        let opening = Workplace::open(
            cwd.clone(),
            copies.clone(),
            venv,
            &app.config,
            &skills,
            &agent.access,
        );
        let workplace = opening.map_err(Error::Python)?;
        let workplace = Arc::new(workplace);

        let queue = ClientQueue {
            peer: peer.clone(),
            enqueued,
        };
        let place = Place::Main {
            team: Arc::clone(&team),
            queue: Arc::new(queue),
        };

        // No restart notice again when no code has run since the last restart
        let ran = memory.ran_code();
        let rank = place.rank();
        let launching = Worker::new(&interrupt, Arc::clone(&workplace), &[], rank, ran);
        let worker = launching.await?;

        let response = match loading {
            true => {
                update_session(&peer, &meta);
                let response = LoadSessionResponse::new().config_options(options);
                to_value(response)
            }
            false => {
                let response = NewSessionResponse::new(meta.id.clone()).config_options(options);
                to_value(response)
            }
        };
        app.peer.respond(&id, Ok(response));

        let opened = match loading {
            true => "resumed",
            false => "opened",
        };
        let shown = tilde(&cwd);
        let model = &meta.settings.model;
        info!("session: {opened} [cwd={shown}, model={model}]");
        // The client only knows a new session once it has the reply
        for notice in &notices {
            warn!("session: open issue, {notice}");
            peer.notice(notice, Notice::Session);
        }
        pool.report();

        let declared = app.peer.declared();
        let brief = Brief {
            instructions,
            skills,
            digest,
            images_shown: declared.images,
            background: declared.background,
        };
        let agent = Agent {
            model: Arc::from(built),
            brief,
            interrupt,
            workplace,
            memories,
            place,
        };
        let keeper = Keeper {
            meta,
            team,
            app: Arc::clone(&app),
            titling: None,
            unsaved: Unsaved::default(),
            translator,
            peer,
        };

        Ok(Session {
            memory,
            models,
            agent,
            worker,
            servers: pool,
            shell_left: Vec::new(),
            keeper,
            _lock: lock,
        })
    };
    let started: Result<Session> = starting.await;

    match started {
        Ok(opened) => opened.run(inbox).await,
        Err(error) => {
            match matches!(error, Error::SessionNotFound(_)) && inbox.is_closed() {
                true => info!("session: closed while opening"),
                false => error!("session: open failed, {error}"),
            }
            // A request that came in after the queue was answered would be lost
            inbox.close();
            refuse_queued(&mut inbox, &app.peer, &session);
            app.peer.respond(&id, Err(error));
        }
    }
}

impl Session {
    async fn run(mut self, mut inbox: UnboundedReceiver<Message>) {
        loop {
            let titling = &mut self.keeper.titling;
            let titled = async {
                match titling {
                    Some(titling) => titling.chosen().await,
                    None => future::pending().await,
                }
            };
            // MCP events, titles and programs that ended come before the next message
            let message = select! {
                biased;
                Some(event) = self.servers.recv() => {
                    self.servers.handle(event, &mut self.worker).await;
                    continue;
                }
                title = titled => {
                    self.keeper.titling = None;
                    if let Some(title) = title {
                        self.keeper.retitle(title, &self.memory);
                    }
                    continue;
                }
                () = self.worker.ending() => {
                    let ended = self.worker.ended().await;
                    let running = self.worker.running();
                    self.keeper.translator.programs(&running);
                    if let Some(mut done) = ended {
                        done.idle = true;
                        self.keeper.translator.ended(&done);
                        self.memory.steps.push(MemoryStep::Done(done));
                        self.keeper.save(&self.memory);
                        self.keeper.peer.background_ended();
                    }
                    continue;
                }
                message = inbox.recv() => message,
            };
            let Some(message) = message else {
                break;
            };

            // Messages still queued after close must not run
            if inbox.is_closed() {
                let peer = &self.keeper.app.peer;
                let session = &self.keeper.meta.id;
                refused(message, peer, session);
                refuse_queued(&mut inbox, peer, session);
                break;
            }
            match message {
                Message::Prompt(prompt) => self.prompt(prompt).await,
                Message::Shell(shell) => self.shell(shell).await,
                Message::Option(id, option) => {
                    let result = self.option(option);
                    self.keeper.app.peer.respond(&id, result);
                }
            }
        }

        // When a turn finishes before its title, wait a little for the title before closing
        if let Some(mut titling) = self.keeper.titling.take() {
            let chosen = titling.chosen();
            let title = time::timeout(TITLE_WAIT, chosen).await;
            if let Ok(Some(title)) = title {
                self.keeper.retitle(title, &self.memory);
            }
        }
        info!("session: closed");
    }

    async fn prompt(&mut self, prompt: Prompt) {
        // A turn the user's next task started may already have taken up the programs that ended
        let last = self.memory.steps.last();
        let pending = matches!(last, Some(MemoryStep::Done(done)) if done.idle);
        if prompt.task.is_none() && !pending {
            return finish(&self.keeper.app.peer, &prompt.id, StopReason::EndTurn);
        }

        // One the user interrupted before it started is still recorded and ends as interrupted
        self.agent.interrupt.begin(prompt.turn);

        // A session with no task yet gets its title from the model without waiting for it
        let untitled = self.keeper.meta.model_title.is_empty();
        let asked = prompt.task.as_ref().map(|task| task.text.as_str());
        let asked = asked.unwrap_or_default();
        let worded = !asked.trim().is_empty();
        let tasked = |step: &MemoryStep| matches!(step, MemoryStep::Task(_));
        let first = !self.memory.steps.iter().any(tasked);
        let titling = first && untitled && worded && self.keeper.titling.is_none();
        if titling {
            let model = Arc::clone(&self.agent.model);
            let task = asked.to_owned();
            let (sender, receiver) = oneshot::channel();
            let choosing = task::spawn(async move {
                if let Some(title) = title_of(model, task).await {
                    sender.send(title).ok();
                }
            });
            let task = choosing.abort_handle();
            self.keeper.titling = Some(Titling { receiver, task });
        }

        let servers = self.servers.installed();
        let worker = &mut self.worker;
        let memory = &mut self.memory;
        let running = self
            .agent
            .run(worker, &servers, memory, prompt.task, &mut self.keeper);
        let answer = running.await;
        // A turn that ends interrupted or failed never told the client what still runs
        let programs = self.worker.running();
        self.keeper.translator.programs(&programs);

        // A refused task stays out of the record and so must the title made from it
        let refused = matches!(answer, Err(Error::Refused(_)));
        if titling && refused {
            self.keeper.titling = None;
        }

        let peer = &self.keeper.app.peer;
        let answer = match answer {
            Ok(answer) => answer,
            Err(error @ Error::Refused(_)) => {
                info!("turn: refused, {error}");
                return peer.respond(&prompt.id, Err(error));
            }
            Err(error) => {
                error!("turn: failed, {error}");
                return peer.respond(&prompt.id, Err(error));
            }
        };
        let (stop, ended) = match answer {
            Some(_) => (StopReason::EndTurn, "answered"),
            None => (StopReason::Cancelled, "interrupted"),
        };
        info!("turn: {ended}");
        finish(peer, &prompt.id, stop);
    }

    async fn shell(&mut self, shell: Shell) {
        let interrupt = &self.agent.interrupt;
        if interrupt.cancelled(shell.turn) {
            return finish(&self.keeper.app.peer, &shell.id, StopReason::Cancelled);
        }

        interrupt.begin(shell.turn);
        let translator = &mut self.keeper.translator;
        translator.shell(&shell.command);
        let mut shown = |output: &str| translator.shell_output(output);
        let cwd = &self.agent.workplace.cwd;
        let (ran, left) = run_shell(&shell.command, cwd, interrupt, &mut shown).await;
        self.shell_left.extend(left);

        self.keeper.translator.shell_end(&ran);
        // Log only how the command ended because it may contain secrets
        match &ran.end {
            Exit::Exited(0) => info!("shell: done"),
            Exit::Interrupted => info!("shell: interrupted"),
            end => info!("shell: failed, {end}"),
        }
        let stop = match ran.end {
            Exit::Interrupted => StopReason::Cancelled,
            _ => StopReason::EndTurn,
        };
        self.memory.steps.push(MemoryStep::Shell(ran));
        self.keeper.save(&self.memory);

        finish(&self.keeper.app.peer, &shell.id, stop);
    }

    /// Only handled between turns
    fn option(&mut self, request: SetSessionConfigOptionRequest) -> Result<Value> {
        let keeper = &mut self.keeper;
        let option = &*request.config_id.0;
        let named = option.strip_prefix(MCP_PREFIX);
        let server = named.and_then(|name| self.servers.find(name));
        match (option, server, request.value) {
            // An unchanged value does nothing, while switching models clears the shown usage
            // and rewrites the saved session
            (MODEL_OPTION, _, SessionConfigOptionValue::ValueId { value }) => {
                if *value.0 != keeper.meta.settings.model {
                    let copies = &self.agent.workplace.copies;
                    let model = self.models.make(&value.0, None, copies)?;
                    let window = model.context_window();
                    self.agent.model = Arc::from(model);
                    keeper.translator.model_changed(window);
                    keeper.meta.settings.choose_model(value.0.to_string());
                    let name = &keeper.meta.settings.model;
                    info!("session: model switched [model={name}]");
                    keeper.save(&self.memory);
                }
            }
            (EFFORT_OPTION, _, SessionConfigOptionValue::ValueId { value }) => {
                let name = &keeper.meta.settings.model;
                let effort = &*value.0;
                self.models.check_effort(name, effort)?;

                let current = keeper.meta.settings.current_effort(&self.models);
                if current != Some(effort) {
                    let copies = &self.agent.workplace.copies;
                    let model = self.models.make(name, Some(effort), copies)?;
                    self.agent.model = Arc::from(model);
                    keeper.meta.settings.effort = Some(effort.to_owned());
                    info!("session: effort switched [effort={effort}]");
                    keeper.save(&self.memory);
                }
            }
            (_, Some(index), SessionConfigOptionValue::Boolean { value }) => {
                let config = self.servers.set(index, value, &self.worker);
                keeper.meta.settings.set_mcp(config, value);
                keeper.save(&self.memory);
            }
            (MODEL_OPTION, _, _) => {
                let message = "option: model: takes a model name".to_owned();
                return Err(Error::InvalidParams(message));
            }
            (EFFORT_OPTION, _, _) => {
                let message = "option: effort: takes an effort name".to_owned();
                return Err(Error::InvalidParams(message));
            }
            (_, Some(_), _) => {
                let message = format!("option: {option}: takes true or false");
                return Err(Error::InvalidParams(message));
            }
            _ => {
                let message = match named {
                    Some(name) => format!("mcp: {name}: not in agent.toml"),
                    None => format!("option: {option}: unknown"),
                };
                return Err(Error::InvalidParams(message));
            }
        }

        let configs = self.servers.configs();
        let options = options(&self.models, &keeper.meta.settings, configs);
        let response = SetSessionConfigOptionResponse::new(options);
        Ok(to_value(response))
    }
}

impl Keeper {
    fn save(&mut self, memory: &AgentMemory) {
        if memory.steps.is_empty() {
            return;
        }

        // The title may be ready in the middle of a turn
        if let Some(titling) = &mut self.titling {
            let title = titling.receiver.try_recv();
            if !matches!(title, Err(TryRecvError::Empty)) {
                self.titling = None;
            }
            if let Ok(title) = title {
                self.meta.model_title = title;
            }
        }

        self.meta.update_title(memory);
        let agents = self.team.records();
        let stored = self.app.store.save(memory, &agents, &mut self.meta);
        let error = stored.as_ref().err().map(Error::to_string);
        if let Some(text) = self.unsaved.report(error) {
            error!("session: save failed, {text}");
            self.peer.notice(&text, Notice::Unsaved);
        }
        if stored.is_ok() {
            update_session(&self.peer, &self.meta);
        }
    }

    fn retitle(&mut self, title: String, memory: &AgentMemory) {
        self.meta.model_title = title;
        self.save(memory);
    }
}

impl Sink for Keeper {
    fn event(&mut self, event: Event) {
        self.translator.translate(event);
    }

    fn saved(&mut self, memory: &AgentMemory) {
        self.save(memory);
    }

    fn tasks(&mut self, index: usize) -> Box<dyn TaskViews> {
        self.translator.tasks(index)
    }
}

impl Queue for ClientQueue {
    fn queued(&self) -> Asking<'_> {
        Box::pin(async move {
            // The notices of the messages this takes would otherwise wake a later wait for none
            select! {
                biased;
                () = self.enqueued.notified() => {}
                () = future::ready(()) => {}
            }
            let Some(result) = self.peer.queued().await else {
                return Vec::new();
            };
            let Ok(queued) = serde_json::from_value::<Queued>(result) else {
                return Vec::new();
            };

            let mut tasks = Vec::new();
            for blocks in &queued.prompts {
                let task = task_of(blocks);
                tasks.push(task);
            }
            tasks
        })
    }

    fn enqueued(&self) -> Nudging<'_> {
        Box::pin(self.enqueued.notified())
    }
}

/// Answers a message without running it, a prompt or command as cancelled
fn refused(message: Message, peer: &Peer, session: &str) {
    match message {
        Message::Prompt(Prompt { id, .. }) | Message::Shell(Shell { id, .. }) => {
            finish(peer, &id, StopReason::Cancelled);
        }
        Message::Option(id, _) => {
            let error = Error::SessionNotFound(session.to_owned());
            peer.respond(&id, Err(error));
        }
    }
}

/// Answers what is still queued once the session is closed or failed to open
fn refuse_queued(inbox: &mut UnboundedReceiver<Message>, peer: &Peer, session: &str) {
    while let Ok(message) = inbox.try_recv() {
        refused(message, peer, session);
    }
}

fn finish(peer: &Peer, id: &Value, reason: StopReason) {
    let response = PromptResponse::new(reason);
    let response = to_value(response);
    peer.respond(id, Ok(response));
}

fn options<'a>(
    models: &Models,
    settings: &Settings,
    servers: impl IntoIterator<Item = &'a McpConfig>,
) -> Vec<SessionConfigOption> {
    let mut choices = Vec::new();
    for name in models.names() {
        let choice = SessionConfigSelectOption::new(name.clone(), name);
        choices.push(choice);
    }

    let current = settings.model.clone();
    let model = SessionConfigOption::select(MODEL_OPTION, "Model", current, choices)
        .category(SessionConfigOptionCategory::Model);
    let mut options = vec![model];

    // A model without an effort in model.toml runs at the provider's default, which has no name
    if let Some(current) = settings.current_effort(models) {
        let mut choices = Vec::new();
        for effort in models.offered(&settings.model) {
            let choice = SessionConfigSelectOption::new(effort.clone(), effort);
            choices.push(choice);
        }

        let current = current.to_owned();
        let effort = SessionConfigOption::select(EFFORT_OPTION, "Effort", current, choices)
            .category(SessionConfigOptionCategory::ThoughtLevel);
        options.push(effort);
    }

    for server in servers {
        let id = format!("{MCP_PREFIX}{}", server.name);
        let label = format!("MCP {}", server.name);
        let on = settings.mcp_on(server);
        let option = SessionConfigOption::boolean(id, label, on);
        options.push(option);
    }

    options
}

fn update_session(peer: &SessionPeer, meta: &SessionMeta) {
    let updated = rfc3339(meta.updated_at);
    let title = meta.title.clone();
    let info = SessionInfoUpdate::new().title(title).updated_at(updated);
    peer.update(SessionUpdate::SessionInfoUpdate(info));
}

#[cfg(test)]
mod tests {
    use crate::acp::rpc::{Lines, Out, Peer};
    use crate::acp::serve::serve;
    use crate::acp::tests::drained;
    use crate::acp::App;
    use crate::history::HistoryStore;
    use crate::prompt::{self, titler};
    use crate::tests::temp_dir;
    use crate::tools::memories::Memories;
    use crate::utils::block_on;
    use crate::worker::MemoryCall;
    use crate::Error;
    use serde_json::{json, Value};
    use std::fs;
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;
    use tokio::sync::mpsc::{self, UnboundedSender};
    use tokio::task::{self, JoinHandle};
    use tokio::time;

    const CWD: &str = env!("CARGO_MANIFEST_DIR");

    /// An `Err` from `reply` is the status code to send back,
    /// and the result is the `api` for model.toml
    fn model_server(
        reply: impl Fn(&Value) -> Result<String, u16> + Send + Sync + 'static,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let reply = Arc::new(reply);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let reply = Arc::clone(&reply);
                thread::spawn(move || answer(stream, &*reply));
            }
        });
        format!("http://{address}/v1")
    }

    fn answer(mut stream: TcpStream, reply: &dyn Fn(&Value) -> Result<String, u16>) {
        let cloned = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(cloned);
        let mut length = 0;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }

        let mut body = vec![0; length];
        reader.read_exact(&mut body).ok();
        let body: Value = serde_json::from_slice(&body).unwrap_or_default();

        let response = match reply(&body) {
            Ok(text) => {
                // A reply that starts with `eval:` calls the tool with the rest as its code
                let delta = match text.strip_prefix("eval:") {
                    Some(code) => {
                        let arguments = json!({ "code": code }).to_string();
                        let function = json!({ "name": "eval", "arguments": arguments });
                        let call = json!({"index": 0, "id": "call_1", "function": function});
                        json!({ "tool_calls": [call] })
                    }
                    None => json!({ "content": text }),
                };
                let content = json!({"choices": [{"index": 0, "delta": delta}]});
                let stop = json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]});
                let events = format!("data: {content}\n\ndata: {stop}\n\ndata: [DONE]\n\n");
                let length = events.len();
                format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {length}\r\nconnection: close\r\n\r\n{events}")
            }
            Err(code) => {
                format!("HTTP/1.1 {code} Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            }
        };
        stream.write_all(response.as_bytes()).ok();
    }

    fn system_prompt(body: &Value) -> String {
        body["messages"][0]["content"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    /// Must be used inside a runtime
    struct Connection {
        input: Option<UnboundedSender<io::Result<String>>>,
        output: Lines,
        serving: JoinHandle<Result<(), Error>>,
        next: u64,
        seen: Vec<Value>,
    }

    impl Connection {
        /// Config and saved sessions live in `dir`, and `extra` is appended to the model entry
        fn open(dir: &Path, api: &str, extra: &str) -> Self {
            let text = format!("[model.mock]\ntype = \"openai-completions\"\napi = \"{api}\"\nmodels = [{{ id = \"m1\"{extra} }}]\n");
            fs::create_dir_all(dir).expect("dir");
            fs::write(dir.join("model.toml"), text).expect("model.toml");

            let (input, lines) = mpsc::unbounded_channel();
            let (sent, output) = mpsc::unbounded_channel();
            let app = App {
                peer: Peer::new(Out::Lines(sent)),
                store: HistoryStore::new(dir),
                config: dir.to_owned(),
            };
            let serving = serve(lines, Arc::new(app));
            let serving = task::spawn(serving);

            Self {
                input: Some(input),
                output,
                serving,
                next: 0,
                seen: Vec::new(),
            }
        }

        fn send(&mut self, message: Value) {
            let line = message.to_string();
            let input = self.input.as_ref().expect("open");
            input.send(Ok(line)).ok();
        }

        /// Waits for the reply and keeps every notification that came before it in `seen`
        async fn request(&mut self, method: &str, params: Value) -> Value {
            self.next += 1;
            let id = self.next;
            let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            self.send(request);
            let reply = |message: &Value| message["id"] == id && message.get("method").is_none();
            self.until(reply).await
        }

        /// Keeps every message in `seen` up to and including the first that `done` accepts
        async fn until(&mut self, done: impl Fn(&Value) -> bool) -> Value {
            loop {
                let receiving = self.output.recv();
                let waiting = time::timeout(Duration::from_secs(60), receiving);
                let line = waiting
                    .await
                    .expect("message")
                    .expect("open")
                    .expect("line");
                let message: Value = serde_json::from_str(&line).expect("json");
                self.seen.push(message.clone());
                if done(&message) {
                    return message;
                }
            }
        }

        async fn new_session(&mut self) -> String {
            let params = json!({"cwd": CWD, "mcpServers": []});
            let opened = self.request("session/new", params).await;
            let session = opened["result"]["sessionId"].as_str();
            session.expect("session").to_owned()
        }

        async fn prompt(&mut self, session: &str, text: &str) -> Value {
            let params = json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]});
            self.request("session/prompt", params).await
        }

        /// Waits for every session to finish saving and returns all messages received
        async fn close(mut self) -> Vec<Value> {
            self.input.take();
            let serving = time::timeout(Duration::from_secs(60), &mut self.serving);
            serving
                .await
                .expect("ended")
                .expect("joined")
                .expect("served");

            let mut seen = self.seen;
            let rest = drained(&mut self.output);
            seen.extend(rest);
            seen
        }
    }

    #[test]
    fn closed_queue_refused() {
        let dir = temp_dir("after-close");
        // A slow reply keeps the turn running when the session closes
        let api = model_server(|body| {
            if system_prompt(body) != titler() {
                thread::sleep(Duration::from_millis(500));
            }
            Ok("Done.".to_owned())
        });
        let running = async {
            let mut connection = Connection::open(&dir, &api, "");
            let session = connection.new_session().await;
            let text = json!([{"type": "text", "text": "Make a snake game"}]);
            let prompt = json!({"sessionId": session, "prompt": text});
            let option = json!({"sessionId": session, "configId": "model", "value": "mock/m1"});
            let queued = [
                (100, "session/prompt", prompt.clone()),
                (101, "session/prompt", prompt),
                (102, "session/set_config_option", option),
            ];
            for (id, method, params) in queued {
                let request =
                    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
                connection.send(request);
            }
            let close = json!({"sessionId": session});
            connection.request("session/close", close).await;
            let seen = connection.close().await;
            (session, seen)
        };
        let (session, seen) = block_on(running);

        let mut answers = Vec::new();
        for id in [100, 101, 102] {
            let answer = seen.iter().find(|message| message["id"] == id);
            let answer = answer.expect("answered");
            let stop = answer["result"]["stopReason"].as_str();
            let error = answer["error"]["message"].as_str();
            let text = stop.or(error).unwrap_or_default();
            answers.push(text.to_owned());
        }
        let refused = format!("session: {session}: not found");
        assert_eq!(answers, ["cancelled", "cancelled", refused.as_str()]);
        fs::remove_dir_all(&dir).ok();
    }

    /// The chosen effort goes with every request but titles, and stays with the saved session
    #[test]
    fn effort_switched() {
        let dir = temp_dir("effort");
        let efforts = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&efforts);
        let api = model_server(move |body| {
            if system_prompt(body) != titler() {
                let effort = body["reasoning_effort"].clone();
                seen.lock().expect("efforts").push(effort);
            }
            Ok("Done.".to_owned())
        });
        let extra = ", effort = \"high\", efforts = [\"low\", \"high\"]";
        let running = async {
            let mut connection = Connection::open(&dir, &api, extra);
            let params = json!({"cwd": CWD, "mcpServers": []});
            let opened = connection.request("session/new", params).await;
            let session = opened["result"]["sessionId"].as_str().expect("session");
            let session = session.to_owned();
            connection.prompt(&session, "Count the cafés").await;

            let mut answers = Vec::new();
            for effort in ["low", "max"] {
                let params = json!({"sessionId": session, "configId": "effort", "value": effort});
                let answered = connection.request("session/set_config_option", params);
                answers.push(answered.await);
            }
            connection.prompt(&session, "Count them again").await;

            let load = json!({"sessionId": session, "cwd": CWD, "mcpServers": []});
            let loaded = connection.request("session/load", load).await;
            connection.close().await;
            (opened, answers, loaded)
        };
        let (opened, answers, loaded) = block_on(running);

        let effort_of = |options: &Value| {
            let list = options.as_array().cloned().unwrap_or_default();
            list.into_iter().find(|option| option["id"] == "effort")
        };
        let offered = json!({
            "id": "effort",
            "name": "Effort",
            "category": "thought_level",
            "type": "select",
            "currentValue": "high",
            "options": [{"value": "low", "name": "low"}, {"value": "high", "name": "high"}],
        });
        let first = effort_of(&opened["result"]["configOptions"]);
        assert_eq!(first, Some(offered), "{opened}");
        let switched = effort_of(&answers[0]["result"]["configOptions"]).expect("effort");
        assert_eq!(switched["currentValue"], "low", "{switched}");
        let refused = "effort: max: not offered by mock/m1 (efforts: low, high)";
        assert_eq!(answers[1]["error"]["message"], refused, "{:?}", answers[1]);
        let reloaded = effort_of(&loaded["result"]["configOptions"]).expect("effort");
        assert_eq!(reloaded["currentValue"], "low", "{reloaded}");

        let efforts = efforts.lock().expect("efforts");
        assert_eq!(*efforts, [json!("high"), json!("low")]);
        fs::remove_dir_all(&dir).ok();
    }

    /// The client sees and forgets only the memories the session's directory sees,
    /// without waiting for the turn in progress
    #[test]
    fn memories_forgotten() {
        let dir = temp_dir("memories");
        let here = Memories::new(&dir, Path::new(CWD));
        let elsewhere = Memories::new(&dir, Path::new("/home/zoë/naïve"));
        let remember = |text: &str, project: bool| MemoryCall::Remember {
            text: text.to_owned(),
            project,
        };
        let released = Arc::new(AtomicBool::new(false));
        let holding = Arc::clone(&released);
        let api = model_server(move |body| {
            while system_prompt(body) != titler() && !holding.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(10));
            }
            Ok("Done.".to_owned())
        });
        let running = async {
            let mut connection = Connection::open(&dir, &api, "");
            let own = here.answer(remember("Tests run with pytest", true), true);
            let own = own.await.expect("remember");
            let shared = here.answer(remember("Prefers résumé over CV", false), true);
            let shared = shared.await.expect("remember");
            let theirs = elsewhere.answer(remember("Builds with make", true), true);
            theirs.await.expect("remember");

            let session = connection.new_session().await;
            let text = json!([{"type": "text", "text": "Deploy the café"}]);
            let params = json!({"sessionId": session, "prompt": text});
            let prompt =
                json!({"jsonrpc": "2.0", "id": 100, "method": "session/prompt", "params": params});
            connection.send(prompt);
            let params = json!({"sessionId": session});
            let listed = connection.request("_smol/memories", params).await;
            let forget = json!({"sessionId": session, "id": own});
            let forgotten = connection.request("_smol/forget", forget.clone()).await;
            let again = connection.request("_smol/forget", forget).await;
            let ended = connection.seen.iter().any(|message| message["id"] == 100);
            released.store(true, Ordering::Relaxed);
            connection.close().await;
            (own, shared, listed, forgotten, again, ended)
        };
        let (own, shared, listed, forgotten, again, ended) = block_on(running);

        assert!(!ended);
        let memories = json!([
            {"id": own, "text": "Tests run with pytest", "project": true},
            {"id": shared, "text": "Prefers résumé over CV", "project": false},
        ]);
        assert_eq!(listed["result"]["memories"], memories, "{listed}");
        assert_eq!(forgotten["result"], json!({}), "{forgotten}");
        let missing = format!("memory: {own}: not found");
        assert_eq!(again["error"]["message"], missing, "{again}");
        assert_eq!(again["error"]["code"], -32002, "{again}");
        fs::remove_dir_all(&dir).ok();
    }

    /// Asked of the model, with the latest message of each request but titles
    fn latest(asked: &Arc<Mutex<Vec<String>>>) -> impl Fn(&Value) -> Result<String, u16> {
        let seen = Arc::clone(asked);
        let code = "eval:import subprocess, sys\n\
            program = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(0.5)'])";
        move |body| {
            if system_prompt(body) == titler() {
                return Ok("Café timer".to_owned());
            }
            let messages = body["messages"].as_array().cloned().unwrap_or_default();
            let last = messages.last().cloned().unwrap_or_default();
            let text = last["content"].as_str().unwrap_or_default();
            seen.lock().expect("asked").push(text.to_owned());
            // The task's first request runs the code, and the rest answer
            match messages.len() {
                2 => Ok(code.to_owned()),
                _ => Ok("Done.".to_owned()),
            }
        }
    }

    /// Programs the code left running that end between turns are shown and announced to a client
    /// that declared `_smol/background`, whose prompt hands them a turn only once
    #[test]
    fn background_handed() {
        let dir = temp_dir("background");
        let asked = Arc::new(Mutex::new(Vec::new()));
        let api = model_server(latest(&asked));
        let running = async {
            let mut connection = Connection::open(&dir, &api, "");
            let capabilities = json!({"_meta": {"_smol/background": true}});
            let params = json!({"protocolVersion": 1, "clientCapabilities": capabilities});
            connection.request("initialize", params).await;
            let session = connection.new_session().await;
            connection.prompt(&session, "Start the café timer").await;

            let announced = |message: &Value| message["method"] == "_smol/background";
            connection.until(announced).await;
            let meta = json!({"_smol/background": true});
            let handed = json!({"sessionId": session, "prompt": [], "_meta": meta});
            let first = connection.request("session/prompt", handed.clone()).await;
            let again = connection.request("session/prompt", handed).await;
            (connection.close().await, first, again)
        };
        let (seen, first, again) = block_on(running);

        assert_eq!(first["result"]["stopReason"], "end_turn", "{first}");
        assert_eq!(again["result"]["stopReason"], "end_turn", "{again}");
        let marked =
            |message: &&Value| message["params"]["update"]["_meta"]["_smol/background"] == true;
        let shown: Vec<&Value> = seen.iter().filter(marked).collect();
        assert_eq!(shown.len(), 1, "{seen:?}");
        let listed = shown[0]["params"]["update"]["content"]["text"].as_str();
        assert!(listed.unwrap_or_default().starts_with("pid "), "{shown:?}");
        let running = |message: &&Value| message["method"] == "_smol/programs";
        let counted =
            |message: &Value| message["params"]["programs"].as_array().map_or(9, Vec::len);
        let counts: Vec<usize> = seen.iter().filter(running).map(counted).collect();
        assert_eq!(counts, [1, 0], "{seen:?}");

        let asked = asked.lock().expect("asked");
        assert_eq!(asked.len(), 3, "{asked:?}");
        let done = format!("[{}]\npid ", prompt::PROGRAM_DONE);
        assert!(asked[2].starts_with(&done), "{asked:?}");
        fs::remove_dir_all(&dir).ok();
    }
}
