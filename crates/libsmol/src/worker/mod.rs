use crate::model::Media;
use crate::utils::Reply;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub(crate) mod host;
pub(crate) mod python;
pub(crate) mod sandbox;

/// Holds the `address/token` the agent listens on
pub const CONTROL_ENV: &str = "SMOL_CONTROL";
pub const COPIES_ENV: &str = "SMOL_COPIES";
/// Holds `on`, or `roles` when `agent()` takes a role, and is unset for workers without `agent()`
pub const AGENTS_ENV: &str = "SMOL_AGENTS";
/// Holds the Seatbelt profile the worker confines itself with
#[cfg(target_os = "macos")]
pub const SANDBOX_ENV: &str = "SMOL_SANDBOX";
pub const END: &[u8] = b"\x02smol\x03end\n";

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
    AddServer {
        source: String,
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
    /// `called` is the `server.function` as written in the code
    Tool {
        server: String,
        tool: String,
        args: Map<String, Value>,
        called: String,
    },
    #[serde(untagged)]
    Agent(AgentCall),
}

/// A call to `agent()` or to a handle it returned, where a handle is `{"id", "name", "task", "role"}`
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum AgentCall {
    /// Answered with `{"answer"}` or `{"error"}` for each agent, in order, once all have ended
    Ask {
        agents: Vec<Asked>,
    },
    /// Answered at once with the handle of each agent, which runs in the background
    Start {
        agents: Vec<Asked>,
    },
    /// Answered with the handle of the agent last started with the name, or null
    Find {
        name: String,
    },
    /// Answered with `{"answer"}` or `{"error"}` once it ends, or null after `timeout` seconds
    Wait {
        id: usize,
        timeout: Option<f64>,
    },
    Status {
        id: usize,
    },
    /// Answered once it has stopped
    Stop {
        id: usize,
    },
}

/// An agent for `agent()` to start
#[derive(Debug, Serialize, Deserialize)]
pub struct Asked {
    pub(crate) task: String,
    pub(crate) role: Option<String>,
    pub(crate) name: Option<String>,
}

/// Which `agent()` a worker offers
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Delegation {
    Off,
    On,
    Roles,
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
