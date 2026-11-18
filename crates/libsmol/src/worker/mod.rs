use crate::model::Media;
use crate::utils::Reply;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub(crate) mod guest;
pub(crate) mod host;
pub(crate) mod sandbox;
pub(crate) mod venv;
pub(crate) mod workplace;

/// Holds the `address/token` the agent listens on
pub const CONTROL_ENV: &str = "SMOL_CONTROL";
pub const COPIES_ENV: &str = "SMOL_COPIES";
/// Holds `on`, and is unset for workers without `agent()`
pub const AGENTS_ENV: &str = "SMOL_AGENTS";
/// Starts libsmol's executable as a session's worker, followed by the venv it runs as
pub(crate) const WORKER_ARG: &str = "--worker";
/// The worker cuts each `print()` to it
pub(crate) const MAX_PRINT_CHARS: usize = 8192;

/// The marker the worker writes after each snippet's output,
/// made from the token because the output may hold any fixed marker
pub fn end_marker(token: &str) -> Vec<u8> {
    let marker = format!("\x02{token}\x03end\n");
    marker.into_bytes()
}

/// The worker's first line, with its version since one of another version would misread the messages
pub(crate) fn greeting(token: &str) -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!("{token} {version}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Function {
    pub name: String,
    pub header: String,
    pub summary: String,
}

/// `untagged` tries variants in order and `Note`, which ignores `id`, must come last
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message<C, N> {
    Request {
        id: u64,
        #[serde(flatten)]
        call: C,
    },
    Answer {
        id: u64,
        result: Value,
    },
    Error {
        id: u64,
        error: String,
    },
    Note(N),
}

impl<C, N> Message<C, N> {
    pub fn reply(id: u64, result: Reply) -> Self {
        match result {
            Ok(result) => Message::Answer { id, result },
            Err(error) => Message::Error { id, error },
        }
    }
}

pub type ToWorker = Message<HostCall, HostNote>;
pub type FromWorker = Message<WorkerCall, WorkerNote>;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum HostCall {
    Run {
        code: String,
    },
    /// `name` is the one the session's first worker chose, which the worker picks itself when None
    AddServer {
        source: String,
        name: Option<String>,
        description: String,
        tools: Vec<Value>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum HostNote {
    Cancel { id: u64 },
    RemoveServer { name: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum WorkerCall {
    /// `called` is the `server.function` as written in the code, and the answer is `{"text"}`,
    /// with `"structured"` too when the result holds structured content
    Tool {
        server: String,
        tool: String,
        args: Map<String, Value>,
        called: String,
    },
    #[serde(untagged)]
    Service(Service),
}

/// What the agent answers itself rather than the worker
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Service {
    Agent(AgentCall),
    Memory(MemoryCall),
}

/// A call to `remember()`, `recall()` or `forget()`
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum MemoryCall {
    /// Answered with the new memory's id
    Remember { text: String, project: bool },
    /// Answered with `[[id, text], ...]`
    Recall { query: String },
    /// Answered with null
    Forget { id: i64 },
}

/// A call to `agent()`, `agent.wait()` or `agent.stop()`, which name agents
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum AgentCall {
    /// Answered with null at once
    Send(Asked),
    /// Answered once the agents end or after `timeout` seconds with `{"answers", "working"}`,
    /// `[name, {"answer"} or {"error"}]` for each that ended and `[name, status]` for the rest
    Wait {
        names: Vec<String>,
        timeout: Option<f64>,
    },
    /// Answered with null once it has stopped
    Stop { name: String },
}

/// A task for the agent named `name`
#[derive(Debug, Serialize, Deserialize)]
pub struct Asked {
    pub(crate) name: String,
    pub(crate) task: String,
}

/// Whether a worker offers `agent()`
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Delegation {
    Off,
    On,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum WorkerNote {
    Ready {
        functions: Vec<Function>,
    },
    Cancel {
        id: u64,
    },
    /// Must be sent before the reply to its snippet
    Viewed {
        label: String,
        media: Media,
    },
}
