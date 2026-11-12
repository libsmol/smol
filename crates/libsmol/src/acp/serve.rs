use crate::acp::rpc::{Lines, Out, Peer};
use crate::acp::session::{self, Message, Opening, Prompt, Shell};
use crate::acp::{rfc3339, task_of, to_value, App};
use crate::config;
use crate::config::setup::SETUP_ARG;
use crate::history::HistoryStore;
use crate::tools::memories::{Kept, Memories};
use crate::utils::{block_on, new_id, Interrupt};
use crate::worker::sandbox;
use crate::{Error, Result};
use agent_client_protocol_schema::v1::{
    AgentCapabilities, AuthMethod, AuthMethodTerminal, CancelNotification, CloseSessionRequest,
    CloseSessionResponse, Implementation, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, NewSessionRequest, PromptCapabilities, PromptRequest,
    SessionCapabilities, SessionCloseCapabilities, SessionInfo, SessionListCapabilities,
    SetSessionConfigOptionRequest, AGENT_METHOD_NAMES,
};
use agent_client_protocol_schema::ProtocolVersion;
use reaper::cause;
use reaper::wire::{BACKGROUND, ENQUEUED, FORGET, IMAGES, MEMORIES, QUEUED, SHELL};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::env::consts::OS;
use std::fmt;
use std::fs;
use std::future::Future;
use std::io::{self, BufRead};
use std::panic::{self, PanicHookInfo};
use std::path::PathBuf;
use std::process;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::Notify;
use tokio::task::{self, JoinHandle};
use tokio::time;
use tracing::{error, error_span, info, Event, Instrument, Span, Subscriber};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::fmt::format::{Format, Full, Writer};
use tracing_subscriber::fmt::time::ChronoUtc;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::EnvFilter;

const SAVE_WAIT: Duration = Duration::from_secs(5);
/// One file per day
const MAX_LOG_FILES: usize = 5;
/// rmcp logs the whole URL of a server it can't reach, which may carry a secret
const DEFAULT_LOG: &str = "warn,libsmol=info,rmcp=off";
const TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

pub fn run() -> Result<()> {
    reaper::guard();

    let dir = config::dir();
    let logs = dir.join("logs");
    // Without the directory tracing-appender prints a line to stderr when pruning old files
    fs::create_dir_all(&logs).ok();
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("libsmol")
        .filename_suffix("log")
        .max_log_files(MAX_LOG_FILES)
        .build(logs);

    // Logs go only to the file because smol shows stderr when the agent exits
    if let Ok(appender) = appender {
        let filter = EnvFilter::try_from_env("SMOL_LOG");
        let filter = filter.unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG));
        let time = ChronoUtc::new(TIME_FORMAT.to_owned());
        let theirs = Format::default().with_timer(time);
        let ours = theirs.clone().with_target(false);
        let line = Line { ours, theirs };

        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(appender)
            .with_ansi(false)
            .event_format(line)
            .init();
    }

    // tokio swallows panics in session tasks and this hook is the only place that can log them
    let default = panic::take_hook();
    let hook = move |info: &PanicHookInfo| {
        error!("agent: panicked, {info}");
        default(info);
    };
    panic::set_hook(Box::new(hook));

    let version = env!("CARGO_PKG_VERSION");
    let pid = process::id();
    info!("agent: started [version={version}, os={OS}, pid={pid}]");
    thread::spawn(sandbox::sweep);

    let app = App {
        peer: Peer::new(Out::Stdout),
        store: HistoryStore::new(&dir),
        config: dir,
    };

    let (lines, input) = mpsc::unbounded_channel();
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        loop {
            let mut line = Vec::new();
            match stdin.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    lines.send(Err(e)).ok();
                    break;
                }
            }

            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']).to_owned();
            if lines.send(Ok(line)).is_err() {
                break;
            }
        }
    });

    let serving = serve(input, Arc::new(app));
    let ended = block_on(serving);

    info!("agent: exited [pid={pid}]");
    reaper::kill_all();

    ended
}

pub(crate) async fn serve(mut input: Lines, app: Arc<App>) -> Result<()> {
    let peer = Arc::clone(&app.peer);
    let mut router = Router {
        app,
        sessions: HashMap::new(),
        closing: HashMap::new(),
    };

    let ended = loop {
        let Some(line) = input.recv().await else {
            break Ok(());
        };
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                let reason = cause(&e);
                error!("acp: stdin read failed, {reason}");
                let message = format!("stdin: {reason}");
                break Err(Error::Acp(message));
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        let message = match serde_json::from_str::<Value>(&line) {
            Ok(message) => message,
            Err(e) => {
                let reason = e.to_string();
                peer.respond(&Value::Null, Err(Error::Parse(reason)));
                continue;
            }
        };
        let id = message.get("id");
        let Some(method) = message["method"].as_str() else {
            let replied = message.get("result").is_some() || message.get("error").is_some();
            match (replied, id) {
                (true, _) => peer.answered(&message),
                (false, Some(id)) => peer.respond(id, Err(Error::InvalidRequest)),
                (false, None) => {}
            }
            continue;
        };
        let params = message["params"].clone();

        match id {
            Some(id) => match router.request(id, method, params) {
                Ok(Some(result)) => peer.respond(id, Ok(result)),
                Ok(None) => {}
                Err(error) => peer.respond(id, Err(error)),
            },
            None if method == AGENT_METHOD_NAMES.session_cancel => {
                let Ok(cancel) = serde_json::from_value::<CancelNotification>(params) else {
                    continue;
                };
                let id = &*cancel.session_id.0;
                if let Ok(session) = router.handle(id) {
                    session.span.in_scope(|| info!("turn: interrupt received"));
                    session.interrupt.cancel(session.received);
                }
            }
            None if method == ENQUEUED => {
                let Ok(enqueued) = serde_json::from_value::<SessionParams>(params) else {
                    continue;
                };
                if let Ok(session) = router.handle(&enqueued.session_id) {
                    session.enqueued.notify_one();
                }
            }
            None => {}
        }
    };

    // Wait for session tasks to save the interrupted step and any finished title
    let mut tasks = Vec::new();
    for session in router.sessions.into_values() {
        let closing = session.close();
        tasks.push(closing);
    }
    tasks.extend(router.closing.into_values());

    let saving = async {
        for task in tasks {
            task.await.ok();
        }
    };
    time::timeout(SAVE_WAIT, saving).await.ok();

    ended
}

struct Router {
    app: Arc<App>,
    sessions: HashMap<String, Handle>,
    closing: HashMap<String, JoinHandle<()>>,
}

struct Handle {
    sender: UnboundedSender<Message>,
    interrupt: Interrupt,
    enqueued: Arc<Notify>,
    /// Prompts and commands received so far, and a cancel covers every turn up to this count
    received: u64,
    /// The session task is instrumented with it too, which puts the session id on every log line
    span: Span,
    task: JoinHandle<()>,
    memories: Memories,
}

impl Handle {
    /// Interrupts every turn, and dropping the sender makes the session wind down
    fn close(self) -> JoinHandle<()> {
        self.interrupt.cancel(u64::MAX);
        self.task
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShellParams {
    session_id: String,
    command: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionParams {
    session_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForgetParams {
    session_id: String,
    id: i64,
}

impl Router {
    /// On None another task sends the reply
    fn request(&mut self, id: &Value, method: &str, params: Value) -> Result<Option<Value>> {
        let names = &AGENT_METHOD_NAMES;
        let result = match method {
            m if m == names.initialize => {
                let client = &params["clientCapabilities"];
                let queued = client["_meta"][QUEUED].as_bool() == Some(true);
                self.app.peer.set_queued(queued);
                let images = client["_meta"][IMAGES].as_bool() == Some(true);
                self.app.peer.set_images(images);
                let background = client["_meta"][BACKGROUND].as_bool() == Some(true);
                self.app.peer.set_background(background);
                let terminal = client["auth"]["terminal"].as_bool() == Some(true);

                let close = SessionCloseCapabilities::new();
                let list = SessionListCapabilities::new();
                let sessions = SessionCapabilities::new().close(close).list(list);
                // Editors then send a selection as they show it, unsaved edits included
                let prompts = PromptCapabilities::new().embedded_context(true);
                let caps = AgentCapabilities::new()
                    .load_session(true)
                    .prompt_capabilities(prompts)
                    .session_capabilities(sessions);
                let info = Implementation::new("libsmol", env!("CARGO_PKG_VERSION"));
                let mut response = InitializeResponse::new(ProtocolVersion::V1)
                    .agent_capabilities(caps)
                    .agent_info(info);

                // ACP allows terminal methods only for clients that declared support for them
                if terminal {
                    let args = vec![SETUP_ARG.to_owned()];
                    let setup = AuthMethodTerminal::new(SETUP_ARG, "Set up a model")
                        .description("Choose an OpenAI-compatible endpoint and a model")
                        .args(args);
                    let methods = vec![AuthMethod::Terminal(setup)];
                    response = response.auth_methods(methods);
                }
                Some(to_value(response))
            }
            m if m == names.session_new => {
                let request: NewSessionRequest = parsed(params)?;
                let session = new_id();
                self.open(id, session, request.cwd, false)?;
                None
            }
            m if m == names.session_load => {
                let request: LoadSessionRequest = parsed(params)?;
                let session = request.session_id.0.to_string();
                self.open(id, session, request.cwd, true)?;
                None
            }
            m if m == names.session_list => {
                let request: ListSessionsRequest = parsed(params)?;
                let mut sessions = Vec::new();
                for meta in self.app.store.list(request.cwd.as_deref())? {
                    let updated = rfc3339(meta.updated_at);
                    let info = SessionInfo::new(meta.id, meta.settings.cwd)
                        .title(meta.title)
                        .updated_at(updated);
                    sessions.push(info);
                }

                let response = ListSessionsResponse::new(sessions);
                Some(to_value(response))
            }
            m if m == names.session_prompt => {
                let request: PromptRequest = parsed(params)?;
                let meta = request.meta.as_ref();
                let marked = meta.and_then(|meta| meta.get(BACKGROUND));
                let task = match marked.and_then(Value::as_bool) {
                    Some(true) => None,
                    _ => Some(task_of(&request.prompt)),
                };

                let session = &request.session_id.0;
                let turn = self.next_turn(session)?;
                let prompt = Prompt {
                    id: id.clone(),
                    task,
                    turn,
                };
                self.send(session, Message::Prompt(prompt))?;
                None
            }
            m if m == names.session_set_config_option => {
                let request: SetSessionConfigOptionRequest = parsed(params)?;
                let session = request.session_id.0.to_string();
                self.send(&session, Message::Option(id.clone(), request))?;
                None
            }
            MEMORIES => {
                let request: SessionParams = parsed(params)?;
                let handle = self.handle(&request.session_id)?;
                let memories = handle.memories.clone();
                let span = handle.span.clone();
                let listing = async move {
                    let seen = memories.seen().await;
                    let seen = seen.map_err(Error::Memory)?;

                    let mut listed = Vec::new();
                    for Kept { id, text, project } in seen {
                        let memory = json!({ "id": id, "text": text, "project": project });
                        listed.push(memory);
                    }
                    Ok(json!({ "memories": listed }))
                };
                self.answer(id, span, listing);
                None
            }
            FORGET => {
                let request: ForgetParams = parsed(params)?;
                let handle = self.handle(&request.session_id)?;
                let memories = handle.memories.clone();
                let span = handle.span.clone();
                let forgetting = async move {
                    let forgotten = memories.forget(request.id).await;
                    match forgotten.map_err(Error::Memory)? {
                        true => Ok(json!({})),
                        false => Err(Error::MemoryNotFound(request.id)),
                    }
                };
                self.answer(id, span, forgetting);
                None
            }
            m if m == names.session_close => {
                let request: CloseSessionRequest = parsed(params)?;
                let session = request.session_id.0.to_string();
                if let Some(old) = self.sessions.remove(&session) {
                    self.closing.retain(|_, task| !task.is_finished());
                    let closing = old.close();
                    self.closing.insert(session, closing);
                }

                let response = CloseSessionResponse::new();
                Some(to_value(response))
            }
            SHELL => {
                let request: ShellParams = parsed(params)?;
                let session = &request.session_id;
                let turn = self.next_turn(session)?;
                let shell = Shell {
                    id: id.clone(),
                    command: request.command,
                    turn,
                };
                self.send(session, Message::Shell(shell))?;
                None
            }
            other => return Err(Error::MethodNotFound(other.to_owned())),
        };

        Ok(result)
    }

    fn open(&mut self, id: &Value, session: String, cwd: PathBuf, loading: bool) -> Result<()> {
        if !cwd.is_absolute() {
            let shown = cwd.display();
            let message = format!("acp: invalid params: cwd: {shown}: not an absolute path");
            return Err(Error::InvalidParams(message));
        }

        // Error level keeps the id on log lines even when only warn is enabled
        let span = error_span!("", id = %session);

        // An unreadable config, or a session saved for another directory,
        // must not close the open session with the same id
        let models = config::models(&self.app.config);
        let agent = config::agent(&self.app.config);
        let located = match loading {
            true => self.app.store.locate(&cwd, &session),
            false => Ok(()),
        };
        let (models, (servers, access, mcp_skipped)) = match (models, agent, located) {
            (Ok(models), Ok(agent), Ok(())) => (models, agent),
            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                let _entered = span.enter();
                match &error {
                    Error::Unconfigured(_) => info!("session: open failed, {error}"),
                    _ => error!("session: open failed, {error}"),
                }
                return Err(error);
            }
        };

        // A closed session may still be saving and must finish before it is read
        let previous = match self.sessions.remove(&session) {
            Some(old) => Some(old.close()),
            None => self.closing.remove(&session),
        };
        // A session whose task ended unclosed would otherwise stay until a request names it
        self.sessions.retain(|_, handle| !handle.sender.is_closed());

        let interrupt = Interrupt::default();
        let enqueued = Arc::new(Notify::new());
        let memories = Memories::new(&self.app.config, &cwd);
        let (sender, inbox) = mpsc::unbounded_channel();
        let opening = Opening {
            id: id.clone(),
            session: session.clone(),
            cwd,
            loading,
            previous,
            models,
            servers,
            access,
            mcp_skipped,
            app: Arc::clone(&self.app),
            interrupt: interrupt.clone(),
            enqueued: Arc::clone(&enqueued),
            memories: memories.clone(),
        };

        let running = session::open(opening, inbox).instrument(span.clone());
        let task = task::spawn(running);

        let handle = Handle {
            sender,
            interrupt,
            enqueued,
            received: 0,
            span,
            task,
            memories,
        };
        self.sessions.insert(session, handle);

        Ok(())
    }

    fn next_turn(&mut self, session: &str) -> Result<u64> {
        let handle = self.handle(session)?;
        handle.received += 1;
        Ok(handle.received)
    }

    /// Forgets a session whose task ended unclosed, as when it failed to open
    fn handle(&mut self, session: &str) -> Result<&mut Handle> {
        let ended = self
            .sessions
            .get(session)
            .is_some_and(|handle| handle.sender.is_closed());
        if ended {
            self.sessions.remove(session);
        }
        self.sessions
            .get_mut(session)
            .ok_or_else(|| unknown(session))
    }

    fn send(&mut self, session: &str, message: Message) -> Result<()> {
        let handle = self.handle(session)?;
        handle.sender.send(message).map_err(|_| unknown(session))
    }

    /// Runs on a task of its own because the session task takes requests only between turns
    fn answer(
        &self,
        id: &Value,
        span: Span,
        answering: impl Future<Output = Result<Value>> + Send + 'static,
    ) {
        let peer = Arc::clone(&self.app.peer);
        let id = id.clone();
        let replying = async move {
            let result = answering.await;
            peer.respond(&id, result);
        };
        let replying = replying.instrument(span);
        task::spawn(replying);
    }
}

fn parsed<T: DeserializeOwned>(params: Value) -> Result<T> {
    match serde_json::from_value(params) {
        Ok(value) => Ok(value),
        Err(e) => {
            let message = format!("acp: invalid params: {e}");
            Err(Error::InvalidParams(message))
        }
    }
}

fn unknown(session: &str) -> Error {
    Error::SessionNotFound(session.to_owned())
}

/// One line per event, with the module path only for dependencies
struct Line {
    ours: Format<Full, ChronoUtc>,
    theirs: Format<Full, ChronoUtc>,
}

impl<S, N> FormatEvent<S, N> for Line
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        // Events forwarded from `log` have the target `log` and all come from dependencies
        let target = event.metadata().target();
        let format = match target.starts_with("libsmol") {
            true => &self.ours,
            false => &self.theirs,
        };

        let mut text = String::new();
        let buffer = Writer::new(&mut text);
        format.format_event(ctx, buffer, event)?;
        let line = text.trim_end().replace('\n', "\\n");
        writeln!(writer, "{line}")
    }
}

#[cfg(test)]
mod tests {
    use crate::acp::rpc::{Lines, Out, Peer};
    use crate::acp::serve::{Handle, Router};
    use crate::acp::App;
    use crate::history::tests::saved;
    use crate::history::{HistoryStore, Settings};
    use crate::steps::tests::one_task;
    use crate::tests::temp_dir;
    use crate::tools::memories::Memories;
    use crate::utils::{block_on, Interrupt};
    use crate::Error;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::fs;
    use std::future;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{mpsc, oneshot};
    use tokio::{task, time};
    use tracing::Span;

    fn router(dir: &Path) -> (Router, Lines) {
        let (lines, sent) = mpsc::unbounded_channel();
        let app = App {
            peer: Peer::new(Out::Lines(lines)),
            store: HistoryStore::new(dir),
            config: dir.to_owned(),
        };
        let router = Router {
            app: Arc::new(app),
            sessions: HashMap::new(),
            closing: HashMap::new(),
        };
        (router, sent)
    }

    #[test]
    fn setup_offered() {
        let dir = temp_dir("setup-offered");
        let (mut router, _sent) = router(&dir);

        let plain = json!({"protocolVersion": 1, "clientCapabilities": {}});
        let answered = router.request(&json!(1), "initialize", plain);
        let answered = answered.expect("initialize").expect("result");
        assert_eq!(answered["authMethods"], json!([]));
        let prompts = &answered["agentCapabilities"]["promptCapabilities"];
        assert_eq!(prompts["embeddedContext"], true, "{answered}");

        let terminal = json!({
            "protocolVersion": 1,
            "clientCapabilities": {"auth": {"terminal": true}},
        });
        let answered = router.request(&json!(2), "initialize", terminal);
        let answered = answered.expect("initialize").expect("result");
        let setup = json!({
            "type": "terminal",
            "id": "setup",
            "name": "Set up a model",
            "description": "Choose an OpenAI-compatible endpoint and a model",
            "args": ["setup"],
        });
        assert_eq!(answered["authMethods"], json!([setup]));

        let opening = json!({"cwd": dir, "mcpServers": []});
        let refused = router.request(&json!(3), "session/new", opening);
        let refused = refused.expect_err("unconfigured");
        assert!(matches!(refused, Error::Unconfigured(_)), "{refused:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn relative_cwd_refused() {
        let dir = temp_dir("relative");
        let (mut router, _sent) = router(&dir);
        let opening = json!({"cwd": "café", "mcpServers": []});
        let refused = router.request(&json!(1), "session/new", opening);
        let refused = refused.expect_err("relative");

        let expected = "acp: invalid params: cwd: café: not an absolute path";
        assert_eq!(refused.to_string(), expected);
        assert!(router.sessions.is_empty());
    }

    #[test]
    fn load_waits_runner() {
        let dir = temp_dir("serve");
        let (mut router, mut sent) = router(&dir);
        let app = Arc::clone(&router.app);
        let model = "[model.mock]\ntype = \"openai-completions\"\napi = \"http://127.0.0.1:1/v1\"\nmodels = [{ id = \"m1\" }]\n";
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join("model.toml"), model).expect("model.toml");
        let memory = one_task("Last turn");
        let interrupt = Interrupt::default();

        let loading = async {
            // The old task saves a readable session only after it is cancelled
            let project = app.store.project(&dir);
            fs::create_dir_all(&project).expect("project");
            fs::write(project.join("s1.json"), "half saved").expect("write");
            let saving = Arc::clone(&app);
            let cwd = dir.clone();
            let old = task::spawn(async move {
                time::sleep(Duration::from_millis(200)).await;
                let settings = Settings {
                    cwd,
                    ..Settings::default()
                };
                saved(&saving.store, &memory, "s1", &settings);
            });
            let (sender, _inbox) = mpsc::unbounded_channel();
            let opened = Handle {
                sender,
                interrupt: interrupt.clone(),
                enqueued: Arc::default(),
                received: 0,
                span: Span::none(),
                task: old,
                memories: Memories::new(&dir, &dir),
            };
            router.sessions.insert("s1".to_owned(), opened);

            let params = json!({"sessionId": "s1", "cwd": dir, "mcpServers": []});
            let asked = router.request(&json!(1), "session/load", params);
            assert!(matches!(asked, Ok(None)), "{asked:?}");
            assert!(interrupt.cancelled(0));

            // Replayed notifications come before the reply
            // Opening may first make the venv tests share, which is slow on Windows
            let answered = loop {
                let receiving = sent.recv();
                let waiting = time::timeout(Duration::from_secs(60), receiving);
                let line = waiting
                    .await
                    .expect("answered")
                    .expect("open")
                    .expect("line");
                let message: Value = serde_json::from_str(&line).expect("json");
                if message["id"] == json!(1) {
                    break message;
                }
            };

            let params = json!({"sessionId": "s1"});
            router.request(&json!(2), "session/close", params).ok();
            if let Some(closing) = router.closing.remove("s1") {
                closing.await.ok();
            }
            answered
        };
        let answered = block_on(loading);

        assert!(answered.get("result").is_some(), "{answered}");
        fs::remove_dir_all(&dir).ok();
    }

    /// What was queued behind an open that fails is still answered, and the session is then unknown
    #[test]
    fn failed_open_drained() {
        let dir = temp_dir("failed-open");
        let (mut router, mut sent) = router(&dir);
        let model = "[model.mock]\ntype = \"openai-completions\"\napi = \"http://127.0.0.1:1/v1\"\nmodels = [{ id = \"m1\" }]\n";
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join("model.toml"), model).expect("model.toml");
        let memory = one_task("Count the orders");
        let settings = Settings {
            cwd: dir.clone(),
            ..Settings::default()
        };
        let mut held = Vec::new();
        for session in ["s1", "s2"] {
            saved(&router.app.store, &memory, session, &settings);
            let lock = router.app.store.lock(&dir, session).expect("lock");
            held.push(lock);
        }

        let answering = async {
            // The open locks only after the old session ends, so the prompt is queued before the lock fails
            let (release, released) = oneshot::channel::<()>();
            let old = task::spawn(async move {
                released.await.ok();
            });
            let (sender, _inbox) = mpsc::unbounded_channel();
            let opened = Handle {
                sender,
                interrupt: Interrupt::default(),
                enqueued: Arc::default(),
                received: 0,
                span: Span::none(),
                task: old,
                memories: Memories::new(&dir, &dir),
            };
            router.sessions.insert("s1".to_owned(), opened);

            let params = json!({"sessionId": "s1", "cwd": dir, "mcpServers": []});
            router
                .request(&json!(1), "session/load", params)
                .expect("load");
            let text = json!({"type": "text", "text": "Count the cafés"});
            let params = json!({"sessionId": "s1", "prompt": [text]});
            router
                .request(&json!(2), "session/prompt", params)
                .expect("prompt");
            release.send(()).ok();

            let mut answers = Vec::new();
            while answers.len() < 2 {
                let receiving = sent.recv();
                let waiting = time::timeout(Duration::from_secs(10), receiving);
                let line = waiting
                    .await
                    .expect("answered")
                    .expect("open")
                    .expect("line");
                let message: Value = serde_json::from_str(&line).expect("json");
                if message.get("id").is_some() {
                    answers.push(message);
                }
            }

            // Opening another session drops the failed one, which no request named since
            let params = json!({"sessionId": "s2", "cwd": dir, "mcpServers": []});
            router
                .request(&json!(3), "session/load", params)
                .expect("load");
            let dropped = !router.sessions.contains_key("s1");
            loop {
                let receiving = sent.recv();
                let waiting = time::timeout(Duration::from_secs(10), receiving);
                let line = waiting
                    .await
                    .expect("answered")
                    .expect("open")
                    .expect("line");
                let message: Value = serde_json::from_str(&line).expect("json");
                if message["id"] == json!(3) {
                    break;
                }
            }

            let params = json!({"sessionId": "s2"});
            let listed = router.request(&json!(4), "_smol/memories", params);
            (answers, dropped, listed)
        };
        let (answers, dropped, listed) = block_on(answering);
        drop(held);
        fs::remove_dir_all(&dir).ok();

        // Queued requests are answered first, as when the session is closed while opening
        let ids: Vec<&Value> = answers.iter().map(|answer| &answer["id"]).collect();
        assert_eq!(ids, [&json!(2), &json!(1)]);
        assert_eq!(answers[0]["result"]["stopReason"], "cancelled");
        let refused = answers[1]["error"]["message"].as_str().unwrap_or_default();
        assert!(refused.contains("open in another process"), "{answers:?}");
        assert!(dropped);
        assert!(
            matches!(listed, Err(Error::SessionNotFound(_))),
            "{listed:?}"
        );
        assert!(!router.sessions.contains_key("s2"));
    }

    /// A load from another directory is refused before it closes the open session
    #[test]
    fn other_cwd_kept() {
        let dir = temp_dir("other-cwd");
        let (mut router, _sent) = router(&dir);
        let model = "[model.mock]\ntype = \"openai-completions\"\napi = \"http://127.0.0.1:1/v1\"\nmodels = [{ id = \"m1\" }]\n";
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join("model.toml"), model).expect("model.toml");
        let memory = one_task("Fix the café menu");
        let settings = Settings {
            cwd: dir.join("naïve"),
            ..Settings::default()
        };
        saved(&router.app.store, &memory, "s1", &settings);
        let interrupt = Interrupt::default();

        let loading = async {
            let running = future::pending::<()>();
            let (sender, _inbox) = mpsc::unbounded_channel();
            let opened = Handle {
                sender,
                interrupt: interrupt.clone(),
                enqueued: Arc::default(),
                received: 0,
                span: Span::none(),
                task: task::spawn(running),
                memories: Memories::new(&dir, &dir),
            };
            router.sessions.insert("s1".to_owned(), opened);

            let params = json!({"sessionId": "s1", "cwd": dir, "mcpServers": []});
            router.request(&json!(1), "session/load", params)
        };
        let refused = block_on(loading).expect_err("elsewhere").to_string();
        fs::remove_dir_all(&dir).ok();

        assert!(refused.contains(": belongs to "), "{refused}");
        assert!(router.sessions.contains_key("s1"));
        assert!(!interrupt.cancelled(0));
    }
}
