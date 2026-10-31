use crate::acp::rpc::{Notice, Peer};
use crate::acp::servers::McpPool;
use crate::acp::translator::Translator;
use crate::acp::{rfc3339, task_of, to_value, App, QUEUED};
use crate::agent::{title_of, Agent, Asking, Event, Helpers, Nudging, Place, Queue, Sink, Task};
use crate::config::{self, Access, McpConfig, Models};
use crate::history::{Lock, SessionMeta, Settings};
use crate::memory::{AgentMemory, Exit, MemoryStep};
use crate::spawn::{spawn_shell, Group};
use crate::utils::Interrupt;
use crate::worker::host::Worker;
use crate::worker::python::Python;
use crate::worker::sandbox::Sandbox;
use crate::worker::Delegation;
use crate::{Error, Result};
use agent_client_protocol_schema::v1::{
    ContentBlock, LoadSessionResponse, NewSessionResponse, PromptResponse, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigOptionValue, SessionConfigSelectOption,
    SessionInfoUpdate, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, StopReason,
};
use reaper::tilde;
use serde::Deserialize;
use serde_json::{json, Value};
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

/// Must stay under the 5 seconds serve waits for sessions on exit
const NAME_WAIT: Duration = Duration::from_secs(3);
const MODEL_OPTION: &str = "model";
const MCP_OPTION: &str = "mcp.";

/// All of these wait in the channel while a turn is running
pub(crate) enum Message {
    Prompt(Prompt),
    Shell(Shell),
    Option(Value, SetSessionConfigOptionRequest),
}

pub(crate) struct Prompt {
    pub(crate) id: Value,
    pub(crate) task: Task,
    pub(crate) turn: u64,
}

pub(crate) struct Shell {
    pub(crate) id: Value,
    pub(crate) command: String,
    pub(crate) turn: u64,
}

/// The session task reads the config and saved session and sends the reply itself
pub(crate) struct Opening {
    pub(crate) id: Value,
    pub(crate) session: String,
    pub(crate) cwd: PathBuf,
    pub(crate) loading: bool,
    /// The previous task of the same session, which must end before locking
    /// and reading the saved session
    pub(crate) previous: Option<JoinHandle<()>>,
    pub(crate) models: Models,
    pub(crate) servers: Vec<McpConfig>,
    pub(crate) access: Access,
    pub(crate) app: Arc<App>,
    pub(crate) interrupt: Interrupt,
    pub(crate) enqueued: Arc<Notify>,
    /// Only `Router` holds the sender, and dropping it closes the session
    pub(crate) inbox: UnboundedReceiver<Message>,
}

#[derive(Deserialize)]
struct Queued {
    prompts: Vec<Vec<ContentBlock>>,
}

struct Session {
    inbox: UnboundedReceiver<Message>,
    interrupt: Interrupt,
    memory: AgentMemory,
    models: Models,
    agent: Agent,
    /// Lent to the agent during a turn, with MCP servers added and removed between turns
    worker: Worker,
    servers: McpPool,
    /// Programs that user commands left in the background, which end with the session
    background: Vec<Group>,
    keeper: Keeper,
    _lock: Option<Lock>,
}

/// Lent to the agent during a turn to show events and save
struct Keeper {
    meta: SessionMeta,
    app: Arc<App>,
    /// The titling task, which sends None when it fails
    titling: Option<(oneshot::Receiver<Option<String>>, AbortHandle)>,
    unsaved: Unsaved,
    translator: Translator,
}

/// The messages the client queues during a turn, which the session's agent takes as task updates
struct ClientQueue {
    peer: Arc<Peer>,
    session: String,
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

pub(crate) async fn open(opening: Opening) {
    let id = opening.id.clone();
    let app = Arc::clone(&opening.app);
    match start(opening).await {
        Ok(session) => session.run().await,
        Err(error) => {
            error!("session: open failed, {error}");
            app.peer.respond(&id, Err(error));
        }
    }
}

async fn start(opening: Opening) -> Result<Session> {
    let Opening {
        id,
        session,
        cwd,
        loading,
        previous,
        models,
        servers,
        access,
        app,
        interrupt,
        enqueued,
        inbox,
    } = opening;
    let peer = &app.peer;

    if let Some(previous) = previous {
        previous.await.ok();
    }

    // Read the saved session only after locking because another process
    // saves its last step before releasing the lock
    let lock = app.store.lock(&session)?;
    let (mut meta, memory) = match loading {
        true => app.store.load(&session)?,
        false => {
            let meta = SessionMeta::new(session, Settings::default());
            (meta, AgentMemory::default())
        }
    };

    let mut notices = Vec::new();
    let previously = &meta.settings;
    let names = models.names();
    let model = match names.contains(&previously.model) {
        true => previously.model.clone(),
        false => names.first().cloned().unwrap_or_default(),
    };
    if !previously.model.is_empty() && model != previously.model {
        let gone = &previously.model;
        let notice = format!("model: {gone}: not in model.toml, using {model}");
        notices.push(notice);
    }

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
    let (roles, skipped) = config::roles(&app.config, &cwd, &models);
    for problem in skipped {
        let notice = problem.to_string();
        notices.push(notice);
    }

    let mut settings = Settings {
        cwd: cwd.clone(),
        model,
        name: previously.name.clone(),
        ..Settings::default()
    };
    for server in &servers {
        if let Some(on) = previously.mcp.get(&server.name) {
            settings.set_mcp(server, *on);
        }
    }
    meta.settings = settings;
    let built = models.make(&meta.settings.model)?;
    let options = options(&models, &meta.settings, &servers);

    let copies = app.store.copies(&meta.id)?;
    let window = built.context_window();
    let mut translator = Translator::new(
        meta.id.clone(),
        Arc::clone(peer),
        window,
        cwd.clone(),
        copies.clone(),
    );
    translator.replay(&memory);

    // The sandbox grants the runtime by the folders that exist when it is made
    let python = Python::install(&app.config, &cwd).await;
    let python = python.map_err(Error::Python)?;
    let sandbox = Sandbox::new(&cwd, &copies, &app.config, &skills, &access, python);
    let sandbox = sandbox.map_err(Error::Python)?;
    let sandbox = Arc::new(sandbox);
    // No restart notice again when no code has run since the last restart
    let ran = memory.ran_code();
    let delegation = match roles.is_empty() {
        true => Delegation::On,
        false => Delegation::Roles,
    };
    let worker = Worker::new(&interrupt, Arc::clone(&sandbox), &[], delegation, ran).await?;

    let response = match loading {
        true => {
            update_session(peer, &meta);
            let response = LoadSessionResponse::new().config_options(options);
            to_value(response)
        }
        false => {
            let response = NewSessionResponse::new(meta.id.clone()).config_options(options);
            to_value(response)
        }
    };
    peer.respond(&id, Ok(response));

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
        peer.notice(&meta.id, notice, Notice::Session);
    }

    let peer = Arc::clone(&app.peer);
    let pool = McpPool::new(servers, &meta.settings, &copies, peer, meta.id.clone());
    pool.report();

    let queue = ClientQueue {
        peer: Arc::clone(&app.peer),
        session: meta.id.clone(),
        enqueued,
    };
    let agent = Agent {
        model: Arc::from(built),
        instructions,
        skills,
        cwd,
        copies,
        interrupt: interrupt.clone(),
        sandbox,
        place: Place::Main {
            roles,
            team: Arc::default(),
            queue: Arc::new(queue),
        },
    };
    let keeper = Keeper {
        meta,
        app,
        titling: None,
        unsaved: Unsaved::default(),
        translator,
    };

    Ok(Session {
        inbox,
        interrupt,
        memory,
        models,
        agent,
        worker,
        servers: pool,
        background: Vec::new(),
        keeper,
        _lock: lock,
    })
}

impl Session {
    async fn run(mut self) {
        loop {
            let titling = &mut self.keeper.titling;
            let titled = async {
                match titling {
                    Some((receiver, _)) => receiver.await,
                    None => future::pending().await,
                }
            };
            // MCP events and titles that arrived during a turn come before the next message
            let message = select! {
                biased;
                Some(event) = self.servers.recv() => {
                    self.servers.handle(event, &mut self.worker).await;
                    continue;
                }
                title = titled => {
                    self.keeper.titling = None;
                    if let Ok(Some(name)) = title {
                        self.keeper.rename(name, &self.memory);
                    }
                    continue;
                }
                message = self.inbox.recv() => message,
            };
            let Some(message) = message else {
                break;
            };

            // Messages still queued after close must not run,
            // and prompts and commands are answered as cancelled by turn
            let closed = self.inbox.is_closed();
            match message {
                Message::Prompt(prompt) => self.prompt(prompt).await,
                Message::Shell(shell) => self.shell(shell).await,
                Message::Option(id, _) if closed => {
                    let session = self.keeper.meta.id.clone();
                    let error = Error::SessionNotFound(session);
                    self.keeper.app.peer.respond(&id, Err(error));
                }
                Message::Option(id, option) => {
                    let result = self.option(option);
                    self.keeper.app.peer.respond(&id, result);
                }
            }
        }

        // When a turn finishes before its title, wait a little for the title before closing
        if let Some((receiver, _)) = self.keeper.titling.take() {
            let title = time::timeout(NAME_WAIT, receiver).await;
            if let Ok(Ok(Some(name))) = title {
                self.keeper.rename(name, &self.memory);
            }
        }
        info!("session: closed");
    }

    async fn prompt(&mut self, prompt: Prompt) {
        // Skip prompts still queued after close, but record one the user interrupted
        // before it started and end it as interrupted
        if self.interrupt.cancelled(prompt.turn) && self.inbox.is_closed() {
            let peer = &self.keeper.app.peer;
            return finish(peer, &prompt.id, StopReason::Cancelled);
        }
        self.interrupt.begin(prompt.turn);

        // Requests stay in the channel while MCP servers connect,
        // and only an interrupt ends the wait
        while self.servers.connecting() && !self.interrupt.triggered() {
            select! {
                Some(event) = self.servers.recv() => {
                    self.servers.handle(event, &mut self.worker).await;
                }
                () = self.interrupt.wait() => {}
            }
        }

        // A session with no task yet gets its title from the model without waiting for it
        let untitled = self.keeper.meta.settings.name.is_empty();
        let worded = !prompt.task.text.trim().is_empty();
        let tasked = |step: &MemoryStep| matches!(step, MemoryStep::Task(_));
        let first = !self.memory.steps.iter().any(tasked);
        let titling = first && untitled && worded && self.keeper.titling.is_none();
        if titling {
            let model = Arc::clone(&self.agent.model);
            let task = prompt.task.text.clone();
            let (sender, receiver) = oneshot::channel();
            let naming = task::spawn(async move {
                let title = title_of(model, task).await;
                sender.send(title).ok();
            });
            self.keeper.titling = Some((receiver, naming.abort_handle()));
        }

        let servers = self.servers.installed();
        let worker = &mut self.worker;
        let memory = &mut self.memory;
        let running = self
            .agent
            .run(worker, &servers, memory, prompt.task, &mut self.keeper);
        let answer = running.await;

        // A refused task stays out of the record and so must the title made from it
        let refused = matches!(answer, Err(Error::Refused(_)));
        if titling && refused {
            if let Some((_, naming)) = self.keeper.titling.take() {
                naming.abort();
            }
        }

        let peer = &self.keeper.app.peer;
        let answer = match answer {
            Ok(answer) => answer,
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
        if self.interrupt.cancelled(shell.turn) {
            return finish(&self.keeper.app.peer, &shell.id, StopReason::Cancelled);
        }

        self.interrupt.begin(shell.turn);
        let translator = &mut self.keeper.translator;
        translator.shell(&shell.command);
        let mut shown = |output: &str| translator.shell_output(output);
        let cwd = &self.agent.cwd;
        let (ran, left) = spawn_shell(&shell.command, cwd, &self.interrupt, &mut shown).await;
        self.background.extend(left);

        self.keeper.translator.shell_end(&ran);
        // Log only how the command ended because it may contain secrets
        let end = &ran.end;
        info!("shell: finished ({end})");
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
        let named = option.strip_prefix(MCP_OPTION);
        let server = named.and_then(|name| self.servers.find(name));
        match (option, server, request.value) {
            // An unchanged value does nothing, while switching models clears the shown usage
            // and rewrites the saved session
            (MODEL_OPTION, _, SessionConfigOptionValue::ValueId { value }) => {
                if *value.0 != keeper.meta.settings.model {
                    let model = self.models.make(&value.0)?;
                    let window = model.context_window();
                    self.agent.model = Arc::from(model);
                    keeper.translator.model_changed(window);
                    keeper.meta.settings.model = value.0.to_string();
                    let name = &keeper.meta.settings.model;
                    info!("session: model switched [model={name}]");
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
        let options = options(&self.models, &keeper.meta.settings, &configs);
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
        if let Some((receiver, _)) = &mut self.titling {
            let title = receiver.try_recv();
            if !matches!(title, Err(TryRecvError::Empty)) {
                self.titling = None;
            }
            if let Ok(Some(name)) = title {
                self.meta.settings.name = name;
            }
        }

        let stored = self.app.store.save(memory, &mut self.meta);
        let error = stored.as_ref().err().map(Error::to_string);
        let peer = &self.app.peer;
        if let Some(text) = self.unsaved.report(error) {
            error!("session: save failed, {text}");
            peer.notice(&self.meta.id, &text, Notice::Unsaved);
        }
        if stored.is_ok() {
            update_session(peer, &self.meta);
        }
    }

    fn rename(&mut self, name: String, memory: &AgentMemory) {
        self.meta.settings.name = name;
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

    fn helpers(&mut self, index: usize) -> Box<dyn Helpers> {
        self.translator.helpers(index)
    }
}

impl Queue for ClientQueue {
    fn queued(&self) -> Asking<'_> {
        let peer = &self.peer;
        let session = &self.session;
        Box::pin(async move {
            if !peer.queued() {
                return Vec::new();
            }
            let params = json!({ "sessionId": session });
            let Some(result) = peer.call(QUEUED, params).await else {
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

fn finish(peer: &Peer, id: &Value, reason: StopReason) {
    let response = PromptResponse::new(reason);
    let response = to_value(response);
    peer.respond(id, Ok(response));
}

fn options(
    models: &Models,
    settings: &Settings,
    servers: &[McpConfig],
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

    for server in servers {
        let id = format!("{MCP_OPTION}{}", server.name);
        let label = format!("MCP {}", server.name);
        let on = settings.mcp_on(server);
        let option = SessionConfigOption::boolean(id, label, on);
        options.push(option);
    }
    options
}

fn update_session(peer: &Peer, meta: &SessionMeta) {
    let updated = rfc3339(meta.updated_at);
    let title = meta.title.clone();
    let info = SessionInfoUpdate::new().title(title).updated_at(updated);
    let update = SessionUpdate::SessionInfoUpdate(info);
    peer.update(&meta.id, update);
}

#[cfg(test)]
mod tests {
    use crate::acp::rpc::{Lines, Out, Peer};
    use crate::acp::serve::serve;
    use crate::acp::session::Unsaved;
    use crate::acp::tests::drained;
    use crate::acp::{rfc3339, App};
    use crate::history::tests::saved;
    use crate::history::{HistoryStore, Settings};
    use crate::memory::tests::{asked, coded, one_task, replied};
    use crate::memory::{AgentMemory, MemoryStep};
    use crate::prompt::{NAMER, RESTARTED};
    use crate::tests::temp_dir;
    use crate::tools::mcp::tests::SCRIPT;
    use crate::utils::block_on;
    use crate::worker::python::tests::interpreter;
    use crate::Error;
    use reaper::tilde;
    use serde_json::{json, Value};
    use std::fs;
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::Path;
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
                let content = json!({"choices": [{"index": 0, "delta": {"content": text}}]});
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
                store: HistoryStore::new(dir.join("history")),
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
            loop {
                let receiving = self.output.recv();
                let waiting = time::timeout(Duration::from_secs(60), receiving);
                let line = waiting.await.expect("reply").expect("open").expect("line");
                let message: Value = serde_json::from_str(&line).expect("json");
                self.seen.push(message.clone());
                if message["id"] == id && message.get("method").is_none() {
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

    fn naming(title: &'static str) -> String {
        model_server(move |body| {
            let text = match system_prompt(body) == NAMER {
                true => title,
                false => "Done.",
            };
            Ok(text.to_owned())
        })
    }

    fn stored(dir: &Path) -> HistoryStore {
        HistoryStore::new(dir.join("history"))
    }

    #[test]
    fn save_error_once() {
        let mut unsaved = Unsaved::default();
        let full = || Some("history: disk full".to_owned());
        assert_eq!(unsaved.report(full()), full());
        assert_eq!(unsaved.report(full()), None);
        assert_eq!(unsaved.report(None), None);
        assert_eq!(unsaved.report(full()), full());
    }

    #[test]
    fn new_session_named() {
        let dir = temp_dir("titled");
        let api = naming("“Snake game”。");
        let running = async {
            let mut connection = Connection::open(&dir, &api, "");
            let session = connection.new_session().await;
            connection.prompt(&session, "Make a snake game").await;
            let seen = connection.close().await;
            (session, seen)
        };
        let (session, seen) = block_on(running);

        let (meta, _) = stored(&dir).load(&session).expect("load");
        assert_eq!(meta.title, "Snake game");
        assert_eq!(meta.settings.name, "Snake game");
        // The client tells the user the session is saved only once it gets a save time
        let mut infos = Vec::new();
        for message in &seen {
            let update = &message["params"]["update"];
            if update["sessionUpdate"] == "session_info_update" {
                infos.push(update.clone());
            }
        }
        let updated = rfc3339(meta.updated_at);
        let last = json!({
            "sessionUpdate": "session_info_update",
            "title": "Snake game",
            "updatedAt": updated,
        });
        assert_eq!(infos.last(), Some(&last), "{infos:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refused_task_untitled() {
        let dir = temp_dir("refused");
        // The titling request replies with the start of the task
        let api = model_server(|body| {
            let messages = body["messages"].as_array().cloned().unwrap_or_default();
            let text: String = match system_prompt(body) == NAMER {
                true => {
                    let task = messages[1]["content"].as_str().unwrap_or_default();
                    task.chars().take(4).collect()
                }
                false => "Done.".to_owned(),
            };
            Ok(text)
        });
        let huge = "too long ".repeat(1000);
        let running = async {
            let mut connection = Connection::open(&dir, &api, ", context = 1024");
            let session = connection.new_session().await;
            let refused = connection.prompt(&session, &huge).await;
            assert!(refused["error"].is_object(), "{refused}");
            connection.prompt(&session, "Make a snake game").await;
            connection.close().await;
            session
        };
        let session = block_on(running);

        let (meta, _) = stored(&dir).load(&session).expect("load");
        assert_eq!(meta.title, "Make");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn closed_queue_refused() {
        let dir = temp_dir("after-close");
        // A slow reply keeps the turn running when the session closes
        let api = model_server(|body| {
            if system_prompt(body) != NAMER {
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

    #[test]
    fn early_cancel_recorded() {
        let dir = temp_dir("early-cancel");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let api = model_server(move |body| {
            if system_prompt(body) != NAMER {
                let sent = body["messages"].to_string();
                seen.lock().expect("requests").push(sent);
                thread::sleep(Duration::from_millis(500));
            }
            Ok("Done.".to_owned())
        });
        let running = async {
            let mut connection = Connection::open(&dir, &api, "");
            let session = connection.new_session().await;
            let text = json!([{"type": "text", "text": "Take your time"}]);
            let params = json!({"sessionId": session, "prompt": text});
            let prompt =
                json!({"jsonrpc": "2.0", "id": 100, "method": "session/prompt", "params": params});
            connection.send(prompt);
            let params = json!({"sessionId": session});
            let cancel = json!({"jsonrpc": "2.0", "method": "session/cancel", "params": params});
            connection.send(cancel);
            connection.prompt(&session, "Second task").await;
            let seen = connection.close().await;
            (session, seen)
        };
        let (session, seen) = block_on(running);

        let answer = seen.iter().find(|message| message["id"] == 100);
        let stop = answer.map(|answer| &answer["result"]["stopReason"]);
        assert_eq!(stop, Some(&json!("cancelled")));
        let (_, memory) = stored(&dir).load(&session).expect("load");
        let mut kinds = Vec::new();
        for step in &memory.steps {
            let kind = match step {
                MemoryStep::Task(task) => task.text.clone(),
                MemoryStep::Action(action) => format!("{:?}", action.error),
                _ => String::new(),
            };
            kinds.push(kind);
        }
        assert_eq!(
            kinds,
            ["Take your time", "Some(Interrupted)", "Second task", "None"]
        );

        let requests = requests.lock().expect("requests");
        let last = requests.last().expect("request");
        let told = "The user interrupted you before you replied.";
        assert!(last.contains(told), "{last}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn shell_saved() {
        let dir = temp_dir("shell");
        let memory = one_task("Take a look");
        let settings = Settings::default();
        saved(&stored(&dir), &memory, "s1", &settings);

        // The command queues behind the session load and is replied to only after it runs
        let running = async {
            let mut connection = Connection::open(&dir, "http://127.0.0.1:1/v1", "");
            let load = json!({"sessionId": "s1", "cwd": CWD, "mcpServers": []});
            let opening =
                json!({"jsonrpc": "2.0", "id": 100, "method": "session/load", "params": load});
            connection.send(opening);
            let typed = json!({"sessionId": "s1", "command": "echo déjà vu"});
            let reply = connection.request("_smol/shell", typed).await;
            let seen = connection.close().await;
            (reply, seen)
        };
        let (reply, seen) = block_on(running);

        assert_eq!(reply["result"]["stopReason"], "end_turn", "{reply}");
        let marked = |message: &Value| message["params"]["update"]["_meta"]["_smol/shell"] == true;
        assert!(seen.iter().any(marked), "{seen:?}");
        let (_, saved) = stored(&dir).load("s1").expect("load");
        let Some(MemoryStep::Shell(ran)) = saved.steps.last() else {
            panic!("{:?}", saved.steps);
        };
        assert_eq!(ran.output.trim(), "déjà vu");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restart_not_repeated() {
        let dir = temp_dir("restart-notice");
        let task = |text: &str| MemoryStep::Task(asked(text));
        let ran = coded("print(orders)", Some("Three orders".to_owned()));
        let answered = replied("Checked");
        let unrestarted = vec![task("Read the orders"), MemoryStep::Action(Box::new(ran))];
        let mut restarted = unrestarted.clone();
        restarted.extend([
            task("Look again"),
            MemoryStep::Restarted,
            MemoryStep::Action(Box::new(answered)),
        ]);
        let store = stored(&dir);
        let settings = Settings::default();
        for (session, steps) in [("s1", unrestarted), ("s2", restarted)] {
            let memory = AgentMemory { steps };
            saved(&store, &memory, session, &settings);
        }

        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let api = model_server(move |body| {
            let sent = body["messages"].to_string();
            seen.lock().expect("requests").push(sent);
            Ok("Done.".to_owned())
        });
        let running = async {
            let mut connection = Connection::open(&dir, &api, "");
            for session in ["s1", "s2"] {
                let load = json!({"sessionId": session, "cwd": CWD, "mcpServers": []});
                connection.request("session/load", load).await;
                connection.prompt(session, "Anything else?").await;
            }
            connection.close().await;
        };
        block_on(running);

        // A resumed session that ran code gets one restart notice,
        // and one with no code since its restart keeps only the original
        let requests = requests.lock().expect("requests");
        assert_eq!(requests.len(), 2);
        for sent in requests.iter() {
            assert_eq!(sent.matches(RESTARTED).count(), 1, "{sent}");
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mcp_status() {
        let dir = temp_dir("mcp-status");
        // Single quotes keep the backslashes of a Windows path out of TOML escapes
        let python = interpreter();
        let python = python.display();
        let servers = format!(
            "[mcp.fake]\ntype = \"stdio\"\ncommand = '{python}'\nargs = ['{SCRIPT}']\n\n\
            [mcp.down]\ntype = \"http\"\nurl = \"http://127.0.0.1:1/mcp\"\n\n\
            [mcp.off]\ntype = \"http\"\nurl = \"http://127.0.0.1:1/mcp\"\nenabled = false\n"
        );
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join("agent.toml"), servers).expect("agent.toml");
        let api = naming("Title");
        let running = async {
            let mut connection = Connection::open(&dir, &api, "");
            let session = connection.new_session().await;
            // The turn starts only after the servers finish connecting
            connection.prompt(&session, "Hello").await;
            connection.close().await
        };
        let seen = block_on(running);

        let mut statuses = Vec::new();
        for message in seen {
            if message["method"] == "_smol/mcp" {
                let mut listed = message["params"]["servers"].clone();
                for server in listed.as_array_mut().into_iter().flatten() {
                    if server["error"].is_string() {
                        server["error"] = json!("…");
                    }
                }
                statuses.push(listed);
            }
        }
        let first = json!([
            {"name": "down", "state": "connecting"},
            {"name": "fake", "state": "connecting"},
            {"name": "off", "state": "off"},
        ]);
        assert_eq!(statuses.first(), Some(&first), "{statuses:?}");
        let last = json!([
            {"name": "down", "state": "failed", "error": "…"},
            {"name": "fake", "state": "connected", "tools": 7},
            {"name": "off", "state": "off"},
        ]);
        assert_eq!(statuses.last(), Some(&last), "{statuses:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_names_refused() {
        let dir = temp_dir("unknown-names");
        let running = async {
            let mut connection = Connection::open(&dir, "http://127.0.0.1:1/v1", "");
            let session = connection.new_session().await;
            let mut refused = Vec::new();
            let options = [
                json!({"configId": "mcp.files", "type": "boolean", "value": true}),
                json!({"configId": "theme", "value": "dark"}),
                json!({"configId": "model", "value": "model2"}),
            ];
            for mut params in options {
                params["sessionId"] = json!(session);
                let asked = connection.request("session/set_config_option", params);
                let answered = asked.await;
                let message = answered["error"]["message"].as_str().unwrap_or_default();
                refused.push(message.to_owned());
            }
            connection.close().await;
            refused
        };
        let refused = block_on(running);

        let expected = [
            "mcp: files: not in agent.toml",
            "option: theme: unknown",
            "model: model2: not in model.toml (models: mock/m1)",
        ];
        assert_eq!(refused, expected);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opening_noticed() {
        let dir = temp_dir("open-notices");
        fs::create_dir_all(&dir).expect("dir");
        // An unreadable instructions file is skipped and the session still opens
        fs::write(dir.join("AGENTS.md"), b"\xff\xfe").expect("write");
        let memory = AgentMemory {
            steps: vec![MemoryStep::Task(asked("Read the orders"))],
        };
        let settings = Settings {
            model: "mock/old-model".to_owned(),
            ..Settings::default()
        };
        saved(&stored(&dir), &memory, "s1", &settings);

        let running = async {
            let mut connection = Connection::open(&dir, "http://127.0.0.1:1/v1", "");
            let load = json!({"sessionId": "s1", "cwd": CWD, "mcpServers": []});
            let loaded = connection.request("session/load", load).await;
            let seen = connection.close().await;
            (loaded, seen)
        };
        let (loaded, seen) = block_on(running);
        assert!(loaded["result"].is_object(), "{loaded}");

        let mut notices = Vec::new();
        for message in seen {
            if message["method"] == "_smol/notice" {
                let text = message["params"]["text"].as_str().unwrap_or_default();
                notices.push(text.to_owned());
            }
        }
        let shown = tilde(&dir.join("AGENTS.md"));
        let expected = [
            "model: mock/old-model: not in model.toml, using mock/m1".to_owned(),
            format!("config: {shown}: not UTF-8 text, skipped"),
        ];
        assert_eq!(notices, expected);
        fs::remove_dir_all(&dir).ok();
    }

    /// Roles reach the prompt and the signature, and one with an unknown model is skipped
    #[test]
    fn roles_read() {
        let dir = temp_dir("roles");
        fs::create_dir_all(&dir).expect("dir");
        let roles =
            "[reviewer]\ndescription = \"Reviews the café's code\"\nmodel = \"mock/m1\"\n\n\
            [critic]\ndescription = \"Critiques\"\nmodel = \"mock/gone\"\n";
        fs::write(dir.join("roles.toml"), roles).expect("write");
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&prompts);
        let api = model_server(move |body| {
            let system = system_prompt(body);
            if system != NAMER {
                seen.lock().expect("prompts").push(system);
            }
            Ok("Done.".to_owned())
        });
        let running = async {
            let mut connection = Connection::open(&dir, &api, "");
            let session = connection.new_session().await;
            connection.prompt(&session, "Review it").await;
            connection.close().await
        };
        let seen = block_on(running);

        let mut notices = Vec::new();
        for message in seen {
            if message["method"] == "_smol/notice" {
                let text = message["params"]["text"].as_str().unwrap_or_default();
                notices.push(text.to_owned());
            }
        }
        let shown = tilde(&dir.join("roles.toml"));
        let skipped = format!("config: {shown}: [critic]: model: mock/gone: not in model.toml (models: mock/m1), skipped");
        assert_eq!(notices, [skipped]);
        let prompts = prompts.lock().expect("prompts");
        let prompt = &prompts[0];
        let header =
            "\nagent(task: str | list[str], role: str | list[str] | None = None, *, name: ";
        assert!(prompt.contains(header), "{prompt}");
        let listed = "with one role per task.\n\n- `reviewer`: Reviews the café's code\n\n";
        assert!(prompt.contains(listed), "{prompt}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn models_read_fresh() {
        let dir = temp_dir("fresh-config");
        let added =
            "\n[model.\"new\"]\ntype = \"openai-completions\"\nmodels = [{ id = \"m2\" }]\n";
        let running = async {
            let mut connection = Connection::open(&dir, "http://127.0.0.1:1/v1", "");
            let params = json!({"cwd": CWD, "mcpServers": []});
            let before = connection.request("session/new", params.clone()).await;

            let path = dir.join("model.toml");
            let mut text = fs::read_to_string(&path).expect("read");
            text.push_str(added);
            fs::write(&path, text).expect("write");
            let after = connection.request("session/new", params).await;

            connection.close().await;
            [before, after]
        };
        let opened = block_on(running);

        let mut counted = Vec::new();
        for answer in &opened {
            let choices = &answer["result"]["configOptions"][0]["options"];
            let count = choices.as_array().map(Vec::len);
            counted.push(count);
        }
        assert_eq!(counted, [Some(1), Some(2)], "{opened:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broken_reload_kept() {
        let dir = temp_dir("broken-config");
        let api = naming("Reopened");
        let running = async {
            let mut connection = Connection::open(&dir, &api, "");
            let session = connection.new_session().await;

            fs::write(dir.join("model.toml"), "[model.broken\n").expect("write");
            let params = json!({"sessionId": session, "cwd": CWD, "mcpServers": []});
            let reloaded = connection.request("session/load", params).await;
            let answered = connection.prompt(&session, "Still there?").await;

            connection.close().await;
            (reloaded, answered)
        };
        let (reloaded, answered) = block_on(running);

        assert!(reloaded.get("error").is_some(), "{reloaded}");
        assert_eq!(answered["result"]["stopReason"], "end_turn", "{answered}");
        fs::remove_dir_all(&dir).ok();
    }
}
