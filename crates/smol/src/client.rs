use agent_client_protocol_schema::v1::{
    AuthCapabilities, AuthMethod, AuthMethodTerminal, CancelNotification, ClientCapabilities,
    CloseSessionRequest, ContentBlock, Implementation, InitializeRequest, InitializeResponse,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, Meta,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SessionConfigKind,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue,
    SessionConfigSelect, SessionConfigSelectOptions, SessionInfo, SessionNotification,
    SessionUpdate, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason,
    AGENT_METHOD_NAMES, CLIENT_METHOD_NAMES,
};
use agent_client_protocol_schema::ProtocolVersion;
use anyhow::{anyhow, bail, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::env;
use std::error;
use std::fmt;
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

const EXITED: &str = "agent: exited";
const QUEUED: &str = "_smol/queued";
const NOTICE: &str = "_smol/notice";
const MCP: &str = "_smol/mcp";
pub(crate) const UPDATE: &str = "_smol/update";
/// Also set in the `_meta` of the tool calls it produces
pub(crate) const SHELL: &str = "_smol/shell";
/// Marks the tool calls of agents the code starts
pub(crate) const AGENT: &str = "_smol/agent";
const MAX_STDERR_LINES: usize = 20;
const STDERR_WAIT: Duration = Duration::from_millis(200);
/// The agent needs time to finish saving the cancelled turn
const EXIT_WAIT: Duration = Duration::from_secs(5);
const INVALID_PARAMS: i64 = -32602;
const METHOD_NOT_FOUND: i64 = -32601;
const AUTH_REQUIRED: i64 = -32000;

pub(crate) enum Incoming {
    Update(String, Box<SessionUpdate>),
    Notice(String, Notice),
    Mcp(String, Vec<McpStatus>),
    Queued(String, Asked),
    /// Sent only when no request is waiting for a reply
    Exited(String),
    Done(Result<StopReason>),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Notice {
    pub(crate) text: String,
    pub(crate) retry_in: Option<f64>,
    /// A step that has not produced any text yet still counts as the current step
    pub(crate) step: bool,
    pub(crate) unsaved: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct McpStatus {
    pub(crate) name: String,
    #[serde(flatten)]
    pub(crate) state: McpState,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub(crate) enum McpState {
    Off,
    Connecting,
    Connected { tools: usize },
    Failed { error: String },
}

impl fmt::Display for McpState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            McpState::Off => f.write_str("off"),
            McpState::Connecting => f.write_str("connecting"),
            McpState::Connected { tools: 1 } => f.write_str("connected, 1 tool"),
            McpState::Connected { tools } => write!(f, "connected, {tools} tools"),
            McpState::Failed { error } => write!(f, "failed: {error}"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Refused(String);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl error::Error for Refused {}

#[derive(Debug)]
pub(crate) struct AuthRequired(String);

impl fmt::Display for AuthRequired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl error::Error for AuthRequired {}

#[derive(Clone)]
pub(crate) struct Pick {
    /// What goes in the command
    pub(crate) value: String,
    pub(crate) label: String,
    pub(crate) description: String,
    pub(crate) current: bool,
}

type Reply = Box<dyn FnOnce(Result<Value>) + Send>;

struct Connection {
    /// Taking it closes the pipe, and the agent exits when it reads the end
    stdin: Mutex<Option<ChildStdin>>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Reply>>,
    tail: Mutex<VecDeque<String>>,
    /// Changed only while holding the `pending` lock
    exited: AtomicBool,
}

impl Connection {
    fn send(&self, message: Value) {
        let mut stdin = self.stdin.lock().expect("stdin lock");
        let Some(stdin) = stdin.as_mut() else {
            return;
        };

        if writeln!(stdin, "{message}").is_ok() {
            stdin.flush().ok();
        }
    }

    fn request<T: DeserializeOwned>(
        &self,
        method: &'static str,
        params: impl Serialize,
        done: impl FnOnce(Result<T>) + Send + 'static,
    ) {
        let reply: Reply = Box::new(move |result| {
            let value = match result {
                Ok(value) => value,
                Err(error) => return done(Err(error)),
            };
            match serde_json::from_value(value) {
                Ok(parsed) => done(Ok(parsed)),
                Err(e) => {
                    let error = anyhow!("acp: {method}: unexpected reply: {e}");
                    done(Err(error));
                }
            }
        });

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut pending = self.pending.lock().expect("pending lock");
        if self.exited.load(Ordering::Relaxed) {
            drop(pending);
            let text = self.exit_text();
            let error = anyhow!("{text}");
            return reply(Err(error));
        }
        pending.insert(id, reply);
        drop(pending);

        self.post(id, method, params);
    }

    fn post(&self, id: u64, method: &str, params: impl Serialize) {
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.send(message);
    }

    fn call<T: DeserializeOwned + Send + 'static>(
        &self,
        method: &'static str,
        params: impl Serialize,
    ) -> Result<T> {
        let (done, result) = mpsc::sync_channel(1);
        self.request(method, params, move |reply| _ = done.send(reply));

        let Ok(reply) = result.recv() else {
            bail!(EXITED);
        };
        reply
    }

    fn exit_text(&self) -> String {
        let tail = self.tail.lock().expect("tail lock");
        let lines: Vec<&str> = tail.iter().map(String::as_str).collect();
        let stderr = lines.join("\n");

        let text = format!("{EXITED}\n{stderr}");
        text.trim_end().to_owned()
    }
}

/// Dropping it without an answer sends an empty reply
pub(crate) struct Asked {
    connection: Arc<Connection>,
    id: Option<Value>,
}

impl Asked {
    pub(crate) fn answer(mut self, prompts: Vec<Vec<ContentBlock>>) {
        self.reply(prompts);
    }

    fn reply(&mut self, prompts: Vec<Vec<ContentBlock>>) {
        let Some(id) = self.id.take() else {
            return;
        };

        let result = json!({ "prompts": prompts });
        let message = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        self.connection.send(message);
    }
}

impl Drop for Asked {
    fn drop(&mut self) {
        self.reply(Vec::new());
    }
}

pub(crate) struct Opened {
    pub(crate) id: String,
    pub(crate) options: Vec<SessionConfigOption>,
}

pub(crate) struct Client {
    connection: Arc<Connection>,
    child: Child,
    events: Sender<Incoming>,
    command: Vec<String>,
    /// The agent's terminal auth method, which smol runs in its own terminal
    auth: Option<AuthMethodTerminal>,
    pub(crate) cwd: PathBuf,
    pub(crate) agent: Implementation,
}

impl Client {
    /// `command` must not be empty
    pub(crate) fn start(command: &[String]) -> Result<(Self, Receiver<Incoming>)> {
        let cwd = match env::current_dir() {
            Ok(cwd) => cwd,
            Err(e) => {
                let reason = reaper::cause(&e);
                let shown = env::var("PWD").unwrap_or_else(|_| ".".to_owned());
                bail!("cwd: {shown}: {reason}");
            }
        };

        let (program, args) = command.split_first().expect("command");
        let mut process = Command::new(program);
        process
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Terminal Ctrl-C, hangups and closed windows reach only smol,
        // and the agent saves and exits after reading EOF
        #[cfg(unix)]
        process.process_group(0);
        #[cfg(windows)]
        process.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);

        let spawned = process.spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => {
                let command = command.join(" ");
                let reason = reaper::cause(&e);
                bail!("agent: {command}: {reason}");
            }
        };

        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let stdin = child.stdin.take();
        let connection = Arc::new(Connection {
            stdin: Mutex::new(stdin),
            next_id: AtomicU64::new(0),
            pending: Mutex::default(),
            tail: Mutex::default(),
            exited: AtomicBool::new(false),
        });
        let (events, inbox) = mpsc::channel();

        let (stderr_done, stderr_closed) = mpsc::channel();
        let sink = Arc::clone(&connection);
        thread::spawn(move || {
            let mut stderr = BufReader::new(stderr);
            let mut line = Vec::new();
            while matches!(stderr.read_until(b'\n', &mut line), Ok(read) if read > 0) {
                let text = String::from_utf8_lossy(&line);
                let text = text.trim_end_matches(['\r', '\n']).to_owned();
                line.clear();

                let mut tail = sink.tail.lock().expect("tail lock");
                tail.push_back(text);
                if tail.len() > MAX_STDERR_LINES {
                    tail.pop_front();
                }
            }
            stderr_done.send(()).ok();
        });

        let reader = Arc::clone(&connection);
        let sender = events.clone();
        thread::spawn(move || {
            let mut stdout = BufReader::new(stdout);
            let mut line = Vec::new();
            while matches!(stdout.read_until(b'\n', &mut line), Ok(read) if read > 0) {
                let parsed = serde_json::from_slice::<Value>(&line);
                line.clear();
                let Ok(message) = parsed else {
                    continue;
                };

                let method = message["method"].as_str();
                let params = &message["params"];
                let session = params["sessionId"].as_str().unwrap_or_default().to_owned();

                let incoming = match (message.get("id"), method) {
                    (Some(id), None) => {
                        let Some(id) = id.as_u64() else {
                            continue;
                        };
                        let reply = reader.pending.lock().expect("pending lock").remove(&id);
                        let Some(reply) = reply else {
                            continue;
                        };

                        let result = match message.get("error") {
                            Some(error) => {
                                let text = error["message"].as_str().unwrap_or("unknown error");
                                let error = match error["code"].as_i64() {
                                    Some(INVALID_PARAMS) => Refused(text.to_owned()).into(),
                                    Some(AUTH_REQUIRED) => AuthRequired(text.to_owned()).into(),
                                    _ => anyhow!("{text}"),
                                };
                                Err(error)
                            }
                            None => Ok(message["result"].clone()),
                        };
                        reply(result);
                        continue;
                    }

                    (Some(id), Some(QUEUED)) => {
                        let asked = Asked {
                            connection: Arc::clone(&reader),
                            id: Some(id.clone()),
                        };
                        Incoming::Queued(session, asked)
                    }

                    (Some(id), Some(method)) => {
                        let text = format!("{method}: method not found");
                        let error = json!({ "code": METHOD_NOT_FOUND, "message": text });
                        let response = json!({ "jsonrpc": "2.0", "id": id, "error": error });
                        reader.send(response);
                        continue;
                    }

                    (None, Some(method)) if method == CLIENT_METHOD_NAMES.session_update => {
                        let Ok(notification) = SessionNotification::deserialize(params) else {
                            continue;
                        };
                        let update = Box::new(notification.update);
                        Incoming::Update(session, update)
                    }

                    (None, Some(NOTICE)) => match Notice::deserialize(params) {
                        Ok(notice) => Incoming::Notice(session, notice),
                        Err(_) => continue,
                    },

                    (None, Some(MCP)) => match Vec::<McpStatus>::deserialize(&params["servers"]) {
                        Ok(servers) => Incoming::Mcp(session, servers),
                        Err(_) => continue,
                    },

                    _ => continue,
                };
                sender.send(incoming).ok();
            }

            // stderr may not be fully read yet
            stderr_closed.recv_timeout(STDERR_WAIT).ok();
            let text = reader.exit_text();

            let mut pending = reader.pending.lock().expect("pending lock");
            reader.exited.store(true, Ordering::Relaxed);
            let replies: Vec<Reply> = pending.drain().map(|(_, reply)| reply).collect();
            drop(pending);

            let idle = replies.is_empty();
            for reply in replies {
                let error = anyhow!("{text}");
                reply(Err(error));
            }
            if idle {
                sender.send(Incoming::Exited(text)).ok();
            }
        });

        let info = Implementation::new("smol", env!("CARGO_PKG_VERSION"));
        let queued = (QUEUED.to_owned(), json!(true));
        let meta = Meta::from_iter([queued]);
        let auth = AuthCapabilities::new().terminal(true);
        let capabilities = ClientCapabilities::new().auth(auth).meta(meta);
        let initialize = InitializeRequest::new(ProtocolVersion::V1)
            .client_capabilities(capabilities)
            .client_info(info);
        let method = AGENT_METHOD_NAMES.initialize;
        let initialized: InitializeResponse = connection.call(method, initialize)?;

        let unnamed = || Implementation::new("agent", "");
        let agent = initialized.agent_info.unwrap_or_else(unnamed);

        let mut auth = None;
        for method in initialized.auth_methods {
            if let AuthMethod::Terminal(terminal) = method {
                auth = Some(terminal);
                break;
            }
        }

        let client = Self {
            connection,
            child,
            events,
            command: command.to_vec(),
            auth,
            cwd,
            agent,
        };
        Ok((client, inbox))
    }

    /// False when the agent offers no terminal auth or it failed
    pub(crate) fn authenticate(&self) -> Result<bool> {
        let Some(auth) = &self.auth else {
            return Ok(false);
        };

        let (program, args) = self.command.split_first().expect("command");
        let mut process = Command::new(program);
        // Unlike the agent it must stay in the terminal's foreground process group to read input
        process.args(args).args(&auth.args).envs(&auth.env);

        let status = match process.status() {
            Ok(status) => status,
            Err(e) => {
                let command = self.command.join(" ");
                let reason = reaper::cause(&e);
                bail!("agent: {command}: {reason}");
            }
        };
        Ok(status.success())
    }

    /// On resume the agent replays the history as notifications before replying,
    /// and `latest` skips the open `current`
    pub(crate) fn open(&self, resume: Option<&str>, current: Option<&str>) -> Result<Opened> {
        let cwd = self.cwd.clone();
        let (id, options) = match resume {
            None => {
                let request = NewSessionRequest::new(cwd);
                let method = AGENT_METHOD_NAMES.session_new;
                let opened: NewSessionResponse = self.connection.call(method, request)?;
                let id = opened.session_id.0.to_string();
                (id, opened.config_options)
            }
            Some(resume) => {
                let id = match resume {
                    "latest" => {
                        let sessions = self.sessions()?;
                        let other = |info: &&SessionInfo| Some(&*info.session_id.0) != current;
                        let Some(latest) = sessions.iter().find(other) else {
                            bail!("session: {}: nothing to resume", cwd.display());
                        };
                        latest.session_id.0.to_string()
                    }
                    id => id.to_owned(),
                };

                let request = LoadSessionRequest::new(id.clone(), cwd);
                let method = AGENT_METHOD_NAMES.session_load;
                let loaded: LoadSessionResponse = self.connection.call(method, request)?;
                (id, loaded.config_options)
            }
        };

        let options = options.unwrap_or_default();
        Ok(Opened { id, options })
    }

    pub(crate) fn set_option(
        &self,
        session: &str,
        option: &str,
        value: impl Into<SessionConfigOptionValue>,
    ) -> Result<Vec<SessionConfigOption>> {
        let session = session.to_owned();
        let option = option.to_owned();
        let request = SetSessionConfigOptionRequest::new(session, option, value);
        let method = AGENT_METHOD_NAMES.session_set_config_option;
        let response: SetSessionConfigOptionResponse = self.connection.call(method, request)?;
        Ok(response.config_options)
    }

    pub(crate) fn set_model(
        &self,
        session: &str,
        options: &[SessionConfigOption],
        model: &str,
    ) -> Result<Vec<SessionConfigOption>> {
        let Some(option) = model_option(options) else {
            bail!("agent: {}: has no model option", self.agent.name);
        };
        let id = option.id.0.to_string();
        self.set_option(session, &id, model)
    }

    pub(crate) fn sessions(&self) -> Result<Vec<SessionInfo>> {
        let method = AGENT_METHOD_NAMES.session_list;
        let cwd = self.cwd.clone();
        let request = ListSessionsRequest::new().cwd(cwd);
        let listed: ListSessionsResponse = self.connection.call(method, request)?;
        Ok(listed.sessions)
    }

    pub(crate) fn prompt(&self, session: &str, blocks: Vec<ContentBlock>) {
        let session = session.to_owned();
        let request = PromptRequest::new(session, blocks);

        let method = AGENT_METHOD_NAMES.session_prompt;
        self.start_turn(method, request);
    }

    pub(crate) fn shell(&self, session: &str, command: &str) {
        let params = json!({ "sessionId": session, "command": command });
        self.start_turn(SHELL, params);
    }

    fn start_turn(&self, method: &'static str, params: impl Serialize) {
        let events = self.events.clone();
        let done = move |reply: Result<PromptResponse>| {
            let stop = reply.map(|response| response.stop_reason);
            events.send(Incoming::Done(stop)).ok();
        };
        self.connection.request(method, params, done);
    }

    pub(crate) fn close(&self, session: &str) {
        // Registers no reply because an unanswered one keeps an agent exit from sending `Exited`
        let id = self.connection.next_id.fetch_add(1, Ordering::Relaxed);
        let method = AGENT_METHOD_NAMES.session_close;
        let session = session.to_owned();
        let params = CloseSessionRequest::new(session);
        self.connection.post(id, method, params);
    }

    pub(crate) fn exited(&self) -> bool {
        self.connection.exited.load(Ordering::Relaxed)
    }

    pub(crate) fn cancel(&self, session: &str) {
        let method = AGENT_METHOD_NAMES.session_cancel;
        let session = session.to_owned();
        let params = CancelNotification::new(session);
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.connection.send(message);
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.connection.stdin.lock().expect("stdin lock").take();

        let started = Instant::now();
        while started.elapsed() < EXIT_WAIT {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

pub(crate) fn model_of(options: &[SessionConfigOption]) -> Option<String> {
    let select = model_select(options)?;
    let current = models(options).into_iter().find(|choice| choice.current);

    let name = match current {
        Some(choice) => choice.label,
        None => select.current_value.0.to_string(),
    };
    Some(name)
}

pub(crate) fn models(options: &[SessionConfigOption]) -> Vec<Pick> {
    let Some(select) = model_select(options) else {
        return Vec::new();
    };

    let choices: Vec<_> = match &select.options {
        SessionConfigSelectOptions::Ungrouped(choices) => choices.iter().collect(),
        SessionConfigSelectOptions::Grouped(groups) => {
            groups.iter().flat_map(|group| &group.options).collect()
        }
        _ => Vec::new(),
    };

    let mut picks = Vec::new();
    for choice in choices {
        let description = choice.description.clone().unwrap_or_default();
        picks.push(Pick {
            value: choice.value.0.to_string(),
            label: choice.name.clone(),
            description,
            current: choice.value == select.current_value,
        });
    }
    picks
}

fn model_option(options: &[SessionConfigOption]) -> Option<&SessionConfigOption> {
    let model = Some(SessionConfigOptionCategory::Model);
    options.iter().find(|option| option.category == model)
}

fn model_select(options: &[SessionConfigOption]) -> Option<&SessionConfigSelect> {
    let option = model_option(options)?;
    match &option.kind {
        SessionConfigKind::Select(select) => Some(select),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::client::*;
    use crate::tests::{fake, shell};
    use agent_client_protocol_schema::v1::SessionConfigSelectOption;

    #[test]
    fn model_display_name() {
        let choices = vec![
            SessionConfigSelectOption::new("llama", "llama (qwen3)"),
            SessionConfigSelectOption::new("deepseek", "DeepSeek (deepseek-flash)"),
        ];
        let option = |current: &str| {
            SessionConfigOption::select("model", "Model", current.to_owned(), choices.clone())
                .category(SessionConfigOptionCategory::Model)
        };

        let model = |current| {
            let options = [option(current)];
            model_of(&options)
        };
        assert_eq!(
            model("deepseek").as_deref(),
            Some("DeepSeek (deepseek-flash)")
        );
        assert_eq!(model("other").as_deref(), Some("other"));

        let options = [option("llama")];
        let listed = models(&options);
        let current: Vec<bool> = listed.iter().map(|choice| choice.current).collect();
        assert_eq!(listed[1].value, "deepseek");
        assert_eq!(current, [true, false]);
    }

    #[test]
    fn startup_stderr_tail() {
        let agent = shell("printf '\\377\\n' >&2; echo startup failed >&2; exit 3");
        let Err(error) = Client::start(&agent) else {
            panic!("started");
        };

        let text = error.to_string();
        assert!(text.starts_with("agent: exited\n\u{FFFD}\n"), "{text}");
        assert!(text.ends_with("\nstartup failed"), "{text}");
    }

    #[test]
    fn requests_after_exit() {
        let agent = fake();
        let (client, inbox) = Client::start(&agent).expect("fake agent");
        let opened = client.open(None, None).expect("open");
        let blocks = vec![ContentBlock::from("exit")];
        client.prompt(&opened.id, blocks);

        let Ok(Incoming::Done(Err(error))) = inbox.recv() else {
            panic!("no failure");
        };
        assert_eq!(error.to_string(), "agent: exited\ngoodbye");

        let switched = client.set_option(&opened.id, "model", "x");
        let error = switched.err().map(|error| error.to_string());
        assert_eq!(error.as_deref(), Some("agent: exited\ngoodbye"));
    }

    #[test]
    fn mcp_wire_format() {
        let text = r#"{"sessionId":"s1","servers":[{"name":"web","state":"off"},{"name":"web","state":"connected","tools":3},{"name":"web","state":"failed","error":"connection refused"}]}"#;
        let params: Value = serde_json::from_str(text).expect("json");
        let servers = Vec::<McpStatus>::deserialize(&params["servers"]).expect("servers");

        let states: Vec<String> = servers
            .iter()
            .map(|server| server.state.to_string())
            .collect();
        assert_eq!(
            states,
            ["off", "connected, 3 tools", "failed: connection refused"]
        );
    }
}
