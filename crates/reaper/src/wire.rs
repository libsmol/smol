use serde::{Deserialize, Serialize};
use std::ops::Not;

// Methods, the markers on standard messages and the capabilities clients declare
pub const NOTICE: &str = "_smol/notice";
pub const MCP: &str = "_smol/mcp";
/// Declared by the client in `_meta` and asked by the agent during a turn
pub const QUEUED: &str = "_smol/queued";
/// Declared by a client that shows the images a reply writes in Markdown
pub const IMAGES: &str = "_smol/images";
/// Declared by the client, sent to it when programs the code left running end while no turn runs,
/// and set to `true` on the user chunk that lists them and on the prompt that hands them a turn
pub const BACKGROUND: &str = "_smol/background";
/// Sent with the programs the code left running whenever they change
pub const PROGRAMS: &str = "_smol/programs";
/// Marks a user chunk that amends the task, with the value `true`
pub const UPDATE: &str = "_smol/update";
/// Replied to like a prompt, and marks its tool calls with `true`
pub const SHELL: &str = "_smol/shell";
pub const MEMORIES: &str = "_smol/memories";
pub const FORGET: &str = "_smol/forget";

/// Starts the id of each MCP server's boolean config option
pub const MCP_PREFIX: &str = "mcp.";

// Keys in `_meta`, besides those of `Measured`
/// How a shell command ended, as a string
pub const ERROR: &str = "error";

/// What `NOTICE` carries besides the session's id
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    pub text: String,
    /// Seconds until the step is tried again
    pub retry_in: Option<f64>,
    /// It belongs to the step in progress, also before that shows any text
    pub step: bool,
    /// A save failed
    pub unsaved: bool,
}

/// A server in the list `MCP` sends
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpStatus {
    pub name: String,
    #[serde(flatten)]
    pub state: McpState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum McpState {
    Off,
    Connecting,
    Connected { tools: usize },
    Failed { error: String },
    Unauthorized { error: String },
}

/// The reply to `MEMORIES`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Memories {
    pub memories: Vec<Memory>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    pub id: i64,
    pub text: String,
    /// Kept for the working directory rather than for every directory
    pub project: bool,
}

/// The params of a `_smol/` message, the session's id beside what its method carries, if anything
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Addressed<T = ()> {
    pub session_id: String,
    #[serde(flatten)]
    pub body: T,
}

/// What `FORGET` carries besides the session's id
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Forget {
    pub id: i64,
}

/// What `SHELL` carries besides the session's id
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Shell {
    pub command: String,
}

/// What `MCP` carries besides the session's id
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Servers {
    pub servers: Vec<McpStatus>,
}

/// What `PROGRAMS` carries besides the session's id
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Programs {
    pub programs: Vec<Program>,
}

/// A program the code left running, which a session also keeps among those that ended
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Program {
    pub pid: u32,
    pub command: String,
}

/// The reply to `QUEUED`, with the content blocks of each message it takes
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Queued<B> {
    pub prompts: Vec<Vec<B>>,
}

/// What the `_meta` of usage updates and shell ends tells of their usage and end
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Measured {
    /// Seconds, on the usage updates of steps and compactions
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Seconds since the first token
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode_duration: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Tokens, on usage updates sent while a reply is generating
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_output: Option<u64>,
    #[serde(default, skip_serializing_if = "Not::not")]
    pub interrupted: bool,
}
