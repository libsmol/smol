use crate::acp::rpc::{Lines, Out, Peer};
use crate::acp::session::{self, Message, Opening, Prompt, Shell};
use crate::acp::{prompt_parts, rfc3339, to_value, App, CLEAR_GOAL, GOAL, QUEUED, SHELL};
use crate::config;
use crate::history::HistoryStore;
use crate::setup::SETUP_ARG;
use crate::utils::{block_on, new_id, Interrupt};
use crate::{Error, Result};
use agent_client_protocol_schema::v1::{
    AgentCapabilities, AuthMethod, AuthMethodTerminal, CancelNotification, CloseSessionRequest,
    CloseSessionResponse, Implementation, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, NewSessionRequest, PromptRequest,
    SessionCapabilities, SessionCloseCapabilities, SessionInfo, SessionListCapabilities,
    SetSessionConfigOptionRequest, AGENT_METHOD_NAMES,
};
use agent_client_protocol_schema::ProtocolVersion;
use reaper::cause;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::env::consts::OS;
use std::fmt;
use std::fs;
use std::io::{self, BufRead};
use std::panic::{self, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedSender};
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
const DEFAULT_LOG: &str = "warn,libsmol=info";
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

    let app = App {
        peer: Peer::new(Out::Stdout),
        store: HistoryStore::new(dir.join("history")),
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
                if let Some(session) = router.sessions.get(id) {
                    session.span.in_scope(|| info!("turn: interrupt received"));
                    session.interrupt.cancel(session.received);
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
    /// Prompts and commands received so far, and a cancel covers every turn up to this count
    received: u64,
    /// The session task is instrumented with it too, which puts the session id on every log line
    span: Span,
    task: JoinHandle<()>,
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
struct GoalParams {
    session_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShellParams {
    session_id: String,
    command: String,
}

impl Router {
    /// On None the session task sends the reply
    fn request(&mut self, id: &Value, method: &str, params: Value) -> Result<Option<Value>> {
        let names = &AGENT_METHOD_NAMES;
        let result = match method {
            m if m == names.initialize => {
                let client = &params["clientCapabilities"];
                let queued = client["_meta"][QUEUED].as_bool() == Some(true);
                self.app.peer.set_queued(queued);
                let terminal = client["auth"]["terminal"].as_bool() == Some(true);

                let close = SessionCloseCapabilities::new();
                let list = SessionListCapabilities::new();
                let sessions = SessionCapabilities::new().close(close).list(list);
                let caps = AgentCapabilities::new()
                    .load_session(true)
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
                // On Windows a typed directory may differ in case from the saved one
                let folded = |path: &Path| {
                    let text = path.to_string_lossy();
                    match cfg!(windows) {
                        true => text.to_lowercase(),
                        false => text.into_owned(),
                    }
                };

                let wanted = request.cwd.as_deref().map(folded);
                let mut sessions = Vec::new();
                for meta in self.app.store.list()? {
                    let cwd = folded(&meta.settings.cwd);
                    if wanted.as_ref().is_some_and(|wanted| *wanted != cwd) {
                        continue;
                    }
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
                let (text, files) = prompt_parts(&request.prompt);
                let goal = request.meta.as_ref().and_then(|meta| meta.get(GOAL));
                let goal = goal.and_then(Value::as_bool) == Some(true);

                let session = &request.session_id.0;
                let turn = self.next_turn(session)?;
                let prompt = Prompt {
                    id: id.clone(),
                    text,
                    files,
                    goal,
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
            CLEAR_GOAL => {
                let request: GoalParams = parsed(params)?;
                let session = &request.session_id;
                self.send(session, Message::ClearGoal(id.clone()))?;
                None
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
        // An unreadable config must not close the open session with the same id
        let models = config::models(&self.app.config)?;
        let servers = config::servers(&self.app.config)?;
        // A closed session may still be saving and must finish before it is read
        let previous = match self.sessions.remove(&session) {
            Some(old) => Some(old.close()),
            None => self.closing.remove(&session),
        };

        let interrupt = Interrupt::default();
        let (sender, inbox) = mpsc::unbounded_channel();
        let opening = Opening {
            id: id.clone(),
            session: session.clone(),
            cwd,
            loading,
            previous,
            models,
            servers,
            app: Arc::clone(&self.app),
            interrupt: interrupt.clone(),
            inbox,
        };

        // Error level keeps the id on log lines even when only warn is enabled
        let span = error_span!("", id = %session);
        let running = session::open(opening).instrument(span.clone());
        let task = task::spawn(running);

        let handle = Handle {
            sender,
            interrupt,
            received: 0,
            span,
            task,
        };
        self.sessions.insert(session, handle);
        Ok(())
    }

    fn next_turn(&mut self, session: &str) -> Result<u64> {
        let Some(handle) = self.sessions.get_mut(session) else {
            return Err(unknown(session));
        };
        handle.received += 1;
        Ok(handle.received)
    }

    fn send(&self, session: &str, message: Message) -> Result<()> {
        let Some(handle) = self.sessions.get(session) else {
            return Err(unknown(session));
        };
        handle.sender.send(message).map_err(|_| unknown(session))
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
    use crate::memory::tests::one_task;
    use crate::tests::temp_dir;
    use crate::utils::{block_on, Interrupt};
    use crate::Error;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;
    #[cfg(windows)]
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio::{task, time};
    use tracing::Span;

    fn router(dir: &Path) -> (Router, Lines) {
        let (lines, sent) = mpsc::unbounded_channel();
        let app = App {
            peer: Peer::new(Out::Lines(lines)),
            store: HistoryStore::new(dir.to_owned()),
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

        let opening = json!({"cwd": "/", "mcpServers": []});
        let refused = router.request(&json!(3), "session/new", opening);
        let refused = refused.expect_err("unconfigured");
        assert!(matches!(refused, Error::Unconfigured(_)), "{refused:?}");
        fs::remove_dir_all(&dir).ok();
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
            fs::write(dir.join("s1.json"), "half saved").expect("write");
            let saving = Arc::clone(&app);
            let old = task::spawn(async move {
                time::sleep(Duration::from_millis(200)).await;
                let settings = Settings::default();
                saved(&saving.store, &memory, "s1", &settings);
            });
            let (sender, _inbox) = mpsc::unbounded_channel();
            let opened = Handle {
                sender,
                interrupt: interrupt.clone(),
                received: 0,
                span: Span::none(),
                task: old,
            };
            router.sessions.insert("s1".to_owned(), opened);

            let params = json!({"sessionId": "s1", "cwd": "/", "mcpServers": []});
            let asked = router.request(&json!(1), "session/load", params);
            assert!(matches!(asked, Ok(None)), "{asked:?}");
            assert!(interrupt.cancelled(0));

            // Replayed notifications come before the reply
            let answered = loop {
                let receiving = sent.recv();
                let waiting = time::timeout(Duration::from_secs(10), receiving);
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

    #[test]
    #[cfg(windows)]
    fn list_ignores_case() {
        let dir = temp_dir("case");
        let (mut router, _sent) = router(&dir);
        let memory = one_task("Task");
        let settings = Settings {
            cwd: PathBuf::from(r"C:\Users\Éloïse\Project"),
            ..Settings::default()
        };
        saved(&router.app.store, &memory, "s1", &settings);

        let params = json!({"cwd": r"c:\users\éloïse\PROJECT"});
        let listed = router.request(&json!(1), "session/list", params);
        let listed = listed.expect("list").expect("result");
        assert_eq!(
            listed["sessions"].as_array().map(Vec::len),
            Some(1),
            "{listed}"
        );
        fs::remove_dir_all(&dir).ok();
    }
}
