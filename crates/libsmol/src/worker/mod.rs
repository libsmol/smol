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
pub(crate) const CONTROL_ENV: &str = "SMOL_CONTROL";
pub(crate) const COPIES_ENV: &str = "SMOL_COPIES";
/// Starts libsmol's executable as a session's worker, followed by the venv it runs as
pub(crate) const WORKER_ARG: &str = "--worker";
/// The worker cuts each `print()` to it
pub(crate) const MAX_PRINT_CHARS: usize = 8192;

/// The marker the worker writes after each snippet's output,
/// made from the token because the output may hold any fixed marker
pub(crate) fn end_marker(token: &str) -> Vec<u8> {
    let marker = format!("\x02{token}\x03end\n");
    marker.into_bytes()
}

/// What `CONTROL_ENV` holds
pub(crate) fn control(address: &str, token: &str) -> String {
    format!("{address}/{token}")
}

/// The address and token in what `control` made
pub(crate) fn controlled(control: &str) -> Option<(&str, &str)> {
    control.split_once('/')
}

/// The worker's first line, with its version since one of another version would misread the messages
pub(crate) fn greeting(token: &str) -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!("{token} {version}")
}

/// The version in a greeting with `token`, None for a line that isn't one
pub(crate) fn greeted<'a>(line: &'a str, token: &str) -> Option<&'a str> {
    let (said, version) = line.split_once(' ')?;
    (said == token).then_some(version)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Function {
    pub(crate) name: String,
    pub(crate) header: String,
    pub(crate) summary: String,
}

/// `untagged` tries variants in order and `Note`, which ignores `id`, must come last
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum Message<C, N> {
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
    pub(crate) fn reply(id: u64, result: Reply) -> Self {
        match result {
            Ok(result) => Message::Answer { id, result },
            Err(error) => Message::Error { id, error },
        }
    }
}

pub(crate) type ToWorker = Message<HostCall, HostNote>;
pub(crate) type FromWorker = Message<WorkerCall, WorkerNote>;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub(crate) enum HostCall {
    Run {
        code: String,
    },
    /// `name` is the one a worker chose before a restart, and the worker picks it itself when None
    AddServer {
        source: String,
        name: Option<String>,
        description: String,
        tools: Vec<Value>,
    },
    /// Answered with [`Globals`]
    Globals,
}

/// The names the code left in the worker's globals, as `_globals` returns them
/// and the session file saves them
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Globals {
    pub(crate) imports: Vec<Imported>,
    /// The `def` and `class` lines of what the code defined, as written
    pub(crate) defined: Vec<String>,
    pub(crate) values: Vec<Variable>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Imported {
    pub(crate) name: String,
    /// What the code imported under the name, unless it is the module of that name
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) origin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Variable {
    pub(crate) name: String,
    /// The name of its type
    pub(crate) kind: String,
    /// Only a type that keeps the built-in `len()` of strings, bytes or containers gives one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) length: Option<usize>,
}

impl Globals {
    pub(crate) fn is_empty(&self) -> bool {
        self.imports.is_empty() && self.defined.is_empty() && self.values.is_empty()
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub(crate) enum HostNote {
    Cancel { id: u64 },
    RemoveServer { name: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub(crate) enum WorkerCall {
    /// `called` is the `server.function` as written in the code, and the answer is `{"text"}`,
    /// with `"structured"` too when the result holds structured content
    Tool {
        server: String,
        tool: String,
        args: Map<String, Value>,
        called: String,
    },
    #[serde(untagged)]
    Memory(MemoryCall),
}

/// A call to `remember()`, `recall()` or `forget()`
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub(crate) enum MemoryCall {
    /// Answered with the new memory's id
    Remember { text: String, project: bool },
    /// Answered with `[[id, text], ...]`
    Recall { query: String },
    /// Answered with null
    Forget { id: i64 },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub(crate) enum WorkerNote {
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
