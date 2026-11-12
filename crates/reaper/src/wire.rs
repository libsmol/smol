// Methods, the markers on standard messages and the capabilities clients declare
pub const NOTICE: &str = "_smol/notice";
pub const MCP: &str = "_smol/mcp";
/// Declared by the client in `_meta` and asked by the agent during a turn
pub const QUEUED: &str = "_smol/queued";
/// Sent by a client that declared `QUEUED` when it queues a message
pub const ENQUEUED: &str = "_smol/enqueued";
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
/// Marks the tool calls of agents the code starts, whose `rawInput` is what they were given
pub const AGENT: &str = "_smol/agent";
pub const MEMORIES: &str = "_smol/memories";
pub const FORGET: &str = "_smol/forget";

/// Starts the id of each MCP server's boolean config option
pub const MCP_PREFIX: &str = "mcp.";

// Keys in the `rawInput` of `AGENT` calls
pub const TASK: &str = "task";
pub const ROLE: &str = "role";
/// Given to one started in the background
pub const NAME: &str = "name";

// Keys in `_meta`
/// `true` on usage updates, shell ends and agent calls
pub const INTERRUPTED: &str = "interrupted";
/// The id of the call running the code, on `AGENT` calls
pub const PARENT: &str = "parent";
/// How a shell command ended, as a string
pub const ERROR: &str = "error";
pub const STEPS: &str = "steps";
/// Seconds, on agent calls and the usage updates of steps and compactions
pub const DURATION: &str = "duration";
pub const DECODE_DURATION: &str = "decodeDuration";
pub const INPUT_TOKENS: &str = "inputTokens";
pub const OUTPUT_TOKENS: &str = "outputTokens";
/// Tokens, on usage updates sent while a reply is generating
pub const ESTIMATED_OUTPUT: &str = "estimatedOutput";
