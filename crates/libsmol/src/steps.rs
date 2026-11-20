use crate::model::{ChatMessage, Media, MediaKind, Part, Sealed, TokenUsage, ToolCall};
use crate::prompt;
use crate::worker::Globals;
use reaper::wire::Program;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::Not;

pub(crate) const MAX_OBSERVATION_CHARS: usize = 16_384;
/// Errs toward overestimating
const BYTES_PER_TOKEN: usize = 3;
/// Providers count differently, so this takes the high end
const IMAGE_TOKENS: usize = 1500;
const VIDEO_TOKENS: usize = 25_000;
/// Older images and videos drop out this many at a time, so up to twice as many are sent
const KEPT_MEDIA: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "type", content = "message", rename_all = "snake_case")]
pub(crate) enum StepError {
    #[error("reply: cut off at the output limit, nothing ran")]
    Cut,
    #[error("interrupted")]
    Interrupted,
    /// The agent that assigned the task interrupted it with `agent.stop()`
    #[error("stopped")]
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "type", content = "message", rename_all = "snake_case")]
pub(crate) enum CallError {
    #[error("{0}: no such tool")]
    NoTool(String),
    /// Also covers code that is nothing but tags
    #[error("{}: no code in the arguments", prompt::TOOL)]
    NoCode,
    #[error("{0}")]
    Execution(String),
    /// With the error the worker reported where the code stopped,
    /// none when the worker was killed or never got the code
    #[error("interrupted")]
    Interrupted(Option<String>),
    /// The agent that assigned the task interrupted it with `agent.stop()`
    #[error("stopped")]
    Stopped(Option<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub(crate) struct Usage {
    /// Seconds from the first request to its end, waits to retry and code that ran included
    pub(crate) duration: f64,
    /// Seconds since the first token
    pub(crate) decode_duration: Option<f64>,
    pub(crate) tokens: Option<TokenUsage>,
}

impl Usage {
    pub(crate) fn add(&mut self, other: &Usage) {
        self.duration += other.duration;
        self.decode_duration = match (self.decode_duration, other.decode_duration) {
            (Some(ours), Some(theirs)) => Some(ours + theirs),
            (ours, theirs) => ours.or(theirs),
        };
        self.tokens = match (self.tokens, other.tokens) {
            (Some(ours), Some(theirs)) => {
                let input = ours.input_tokens + theirs.input_tokens;
                let output = ours.output_tokens + theirs.output_tokens;
                let mut tokens = TokenUsage::new(input, output);
                tokens.cached_tokens = match (ours.cached_tokens, theirs.cached_tokens) {
                    (Some(ours), Some(theirs)) => Some(ours + theirs),
                    (ours, theirs) => ours.or(theirs),
                };
                Some(tokens)
            }
            (ours, theirs) => ours.or(theirs),
        };
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct AgentStep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sealed: Option<Sealed>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) calls: Vec<Call>,
    /// An error in the reply itself, which also records an interrupt while code runs
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<StepError>,
    pub(crate) usage: Usage,
    /// The context estimate after this step, computed the same way as for deciding compaction
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) used: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Call {
    /// The code that actually runs, with stray tags removed
    pub(crate) code: String,
    /// Bytes of reply text already received when the call started,
    /// which replay uses to restore the live order
    #[serde(default)]
    pub(crate) at: usize,
    /// The raw arguments when no code could be read, shown but never sent to the model
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) arguments: Option<String>,
    /// What the model wrote before the worker repaired it, saved but never sent to the model
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) unrepaired: Option<String>,
    /// Present only if the code ran
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<CodeResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<CallError>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct CodeResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) printed: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) media: Vec<(String, Media)>,
    /// The tasks the code sent to agents with `agent()`, shown on replay and never sent to the model
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) agents: Vec<Sent>,
}

/// A task that the code sent with `agent()`, as the call that sent it records it
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Sent {
    pub(crate) name: String,
    /// Its place among the tasks of its agent
    pub(crate) number: usize,
}

/// An agent that `agent()` started, with its whole conversation and the tasks it was sent
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct AgentRecord {
    pub(crate) name: String,
    pub(crate) memory: AgentMemory,
    pub(crate) tasks: Vec<AgentTask>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct AgentTask {
    pub(crate) task: String,
    /// Where its steps start in the agent's conversation, which run to the next task's start
    pub(crate) start: usize,
    /// Saved beside the others as one key named after how it ended
    #[serde(flatten)]
    pub(crate) end: Ending,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Ending {
    Answer(String),
    Error(String),
    /// Braces let it flatten into an `interrupted` key
    Interrupted {},
    /// `agent.stop()` ended it
    Stopped {},
    /// Aborted or panicked before it ended
    Unfinished {},
    /// A task keeps this record until it ends, or for good if libsmol exits first
    Running {},
}

impl AgentRecord {
    /// The steps of its task numbered `number`
    pub(crate) fn steps(&self, number: usize) -> &[MemoryStep] {
        let steps = &self.memory.steps;
        let start = self.tasks[number].start;
        let next = self.tasks.get(number + 1);
        let end = next.map_or(steps.len(), |next| next.start);
        &steps[start.min(end)..end]
    }
}

impl AgentTask {
    pub(crate) fn running(&self) -> bool {
        self.end == (Ending::Running {})
    }
}

impl Ending {
    /// The answer, or the error the code gets instead
    pub(crate) fn outcome(&self) -> Result<&str, &str> {
        match self {
            Ending::Answer(answer) => Ok(answer),
            Ending::Error(error) => Err(error),
            Ending::Interrupted {} => Err(prompt::INTERRUPTED_BY_USER),
            Ending::Stopped {} => Err(prompt::STOPPED_BY_CODE),
            Ending::Unfinished {} | Ending::Running {} => Err(prompt::UNFINISHED),
        }
    }
}

impl AgentStep {
    pub(crate) fn answer(&self) -> Option<&str> {
        let text = self.text.trim();
        let replied = self.calls.is_empty() && self.error.is_none() && !text.is_empty();
        replied.then_some(text)
    }

    /// The text and calls the model sees again,
    /// none of either for a reply cut off at the output limit
    pub(crate) fn seen(&self) -> (&str, &[Call]) {
        match self.error == Some(StepError::Cut) {
            true => ("", &[]),
            false => (self.text.trim(), &self.calls),
        }
    }
}

impl Call {
    pub(crate) fn unread(&self) -> bool {
        matches!(self.error, Some(CallError::NoTool(_) | CallError::NoCode))
    }

    /// What the code printed, or a note when it ran without printing or showing anything
    pub(crate) fn output(&self) -> Option<&str> {
        let result = self.result.as_ref()?;
        if let Some(printed) = &result.printed {
            return Some(printed);
        }

        let quiet = self.error.is_none() && result.media.is_empty();
        quiet.then_some(prompt::NO_OUTPUT)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct TaskStep {
    pub(crate) text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) files: Vec<Attachment>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Attachment {
    /// As shown to the model, with directories ending in /, and a resource's URI
    pub(crate) path: String,
    #[serde(flatten)]
    pub(crate) content: Content,
}

/// Text is already cut, and is why the content couldn't be read when that failed
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Content {
    File {
        text: String,
        /// The first and last line of the file that `text` holds, counted from 1
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lines: Option<(u32, u32)>,
    },
    /// The names in it
    Directory {
        text: String,
    },
    /// Text the client sent for something that is not a local file
    Resource {
        text: String,
    },
    Media {
        media: Media,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ShellStep {
    pub(crate) command: String,
    pub(crate) shell: String,
    /// Combined stdout and stderr, already truncated
    pub(crate) output: String,
    pub(crate) end: Exit,
}

/// Agents whose tasks ended before the code took their answers,
/// and programs the code left running that ended, with what was printed since the last call
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct DoneStep {
    pub(crate) agents: Vec<Returned>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) programs: Vec<Program>,
    /// Already truncated
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) output: String,
    /// Taken while no turn ran, as a message of its own
    #[serde(default, skip_serializing_if = "Not::not")]
    pub(crate) idle: bool,
}

/// An agent whose task ended before the code took its answer
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Returned {
    pub(crate) name: String,
    /// Saved beside the name as one key named after how it ended
    #[serde(flatten)]
    pub(crate) end: Ending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub(crate) enum Exit {
    Exited(i32),
    Signaled(i32),
    Interrupted,
    Unstarted(String),
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exit::Exited(code) => {
                let code = exit_code(*code);
                write!(f, "exit {code}")
            }
            Exit::Signaled(signal) => write!(f, "signal {signal}"),
            Exit::Interrupted => f.write_str("interrupted"),
            Exit::Unstarted(reason) => write!(f, "did not start: {reason}"),
        }
    }
}

/// On Windows a crash exits with an NTSTATUS, which is negative as an i32
pub(crate) fn exit_code(code: i32) -> String {
    match cfg!(windows) && code < 0 {
        true => format!("{:#X}", code as u32),
        false => code.to_string(),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct SummaryStep {
    pub(crate) summary: String,
    /// As the worker listed them when the summary was written
    #[serde(default, skip_serializing_if = "Globals::is_empty")]
    pub(crate) globals: Globals,
    /// How many steps before the summary are kept verbatim
    pub(crate) kept: usize,
    pub(crate) usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) used: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum MemoryStep {
    Task(TaskStep),
    Update(TaskStep),
    Action(Box<AgentStep>),
    Summary(SummaryStep),
    Shell(ShellStep),
    Restarted,
    Done(DoneStep),
}

impl MemoryStep {
    /// [`AgentMemory::conversation`] places the summary, calls are numbered on from `calls`,
    /// and media the model doesn't take, as `takes` tells, is a note
    pub(crate) fn messages(
        &self,
        calls: &mut usize,
        takes: &dyn Fn(MediaKind) -> bool,
    ) -> Vec<ChatMessage> {
        let step = match self {
            MemoryStep::Task(task) => {
                let parts = prompt::task(task, takes);
                return vec![ChatMessage::User(parts)];
            }
            MemoryStep::Update(update) => {
                let parts = prompt::update(update, takes);
                return vec![ChatMessage::User(parts)];
            }
            MemoryStep::Shell(shell) => {
                let text = prompt::shell(shell);
                return vec![ChatMessage::user(text)];
            }
            MemoryStep::Restarted => return vec![ChatMessage::user(prompt::RESTARTED)],
            MemoryStep::Done(done) => {
                let text = prompt::done(done);
                return vec![ChatMessage::user(text)];
            }
            MemoryStep::Action(step) => step,
            MemoryStep::Summary(_) => return Vec::new(),
        };

        let mut messages = Vec::new();

        let (text, shown) = step.seen();
        let mut sent = Vec::new();
        let mut ids = Vec::new();
        for call in shown {
            *calls += 1;
            let id = format!("call_{calls}");
            ids.push(id.clone());
            // Unread calls don't echo their arguments because the model would copy the mistake
            let called = match &call.error {
                Some(CallError::NoTool(name)) => ToolCall::unread(id, name),
                Some(CallError::NoCode) => ToolCall::unread(id, prompt::TOOL),
                _ => ToolCall::eval(id, &call.code),
            };
            sent.push(called);
        }
        if !sent.is_empty() || !text.is_empty() {
            // The ends of sealed items count from where the text the model sees starts
            let lead = step.text.len() - step.text.trim_start().len();
            let mut sealed = step.sealed.clone();
            if let Some(sealed) = &mut sealed {
                for end in &mut sealed.ends {
                    *end = end.saturating_sub(lead);
                }
            }

            let reply = ChatMessage::Assistant {
                text: text.to_owned(),
                reasoning: step.reasoning.clone(),
                sealed,
                calls: sent,
            };
            messages.push(reply);
        }

        // Every call needs a reply, and one that didn't run says why
        for (call, id) in shown.iter().zip(ids) {
            let mut parts = Vec::new();
            if let Some(result) = &call.result {
                let printed = call.output();
                let shown = prompt::result(printed, &result.media, takes);
                parts.extend(shown);
            }
            if let Some(text) = prompt::call_error(step, call) {
                if !parts.is_empty() {
                    parts.push(Part::Text(String::new()));
                }
                parts.push(Part::Text(text));
            }
            messages.push(ChatMessage::Tool { id, parts });
        }

        if let Some(text) = prompt::step_error(step) {
            messages.push(ChatMessage::user(text));
        }

        messages
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct AgentMemory {
    pub(crate) steps: Vec<MemoryStep>,
}

impl AgentMemory {
    pub(crate) fn visible(&self) -> Visible<'_> {
        let mut visible = Visible {
            summary: None,
            task: None,
            updates: Vec::new(),
            steps: Vec::new(),
        };

        for step in &self.steps {
            let MemoryStep::Summary(summary) = step else {
                visible.steps.push(step);
                continue;
            };

            let cut = visible.steps.len().saturating_sub(summary.kept);
            for dropped in visible.steps.drain(..cut) {
                match dropped {
                    MemoryStep::Task(task) => {
                        visible.task = Some(task);
                        visible.updates.clear();
                    }
                    MemoryStep::Update(update) => visible.updates.push(&update.text),
                    MemoryStep::Action(_)
                    | MemoryStep::Summary(_)
                    | MemoryStep::Shell(_)
                    | MemoryStep::Restarted
                    | MemoryStep::Done(_) => {}
                }
            }
            visible.summary = Some(summary);
        }

        visible
    }

    /// Takes out the last task if it asked `text` and only restarts and summaries follow it
    pub(crate) fn withdraw(&mut self, text: &str) -> Option<TaskStep> {
        let last = self
            .steps
            .iter()
            .rposition(|step| !matches!(step, MemoryStep::Restarted | MemoryStep::Summary(_)))?;
        let resent = matches!(&self.steps[last], MemoryStep::Task(task) if task.text == text);
        if !resent {
            return None;
        }

        let MemoryStep::Task(task) = self.steps.remove(last) else {
            panic!("task");
        };
        // Summaries after it count it among their `kept` steps
        for step in &mut self.steps[last..] {
            if let MemoryStep::Summary(summary) = step {
                summary.kept = summary.kept.saturating_sub(1);
            }
        }

        Some(task)
    }

    pub(crate) fn conversation(
        &self,
        system: &str,
        takes: &dyn Fn(MediaKind) -> bool,
        window: Option<u64>,
    ) -> Vec<ChatMessage> {
        let visible = self.visible();
        let message = ChatMessage::System(system.to_owned());
        let mut messages = vec![message];

        if let Some(summary) = visible.summary {
            let tasked = visible
                .steps
                .iter()
                .any(|step| matches!(step, MemoryStep::Task(_)));
            let task = visible.task.filter(|_| !tasked);
            let task = task.map(|task| task.text.as_str());

            // Updates aren't refused when oversized like tasks are,
            // so once they and the task pass half the window only the latest that fit come back
            let mut kept = visible.updates.len();
            if let Some(window) = window {
                let mut size = task.map_or(0, text_bytes);
                kept = 0;
                for update in visible.updates.iter().rev() {
                    size += text_bytes(update);
                    if estimate(size) > window / 2 {
                        break;
                    }
                    kept += 1;
                }
            }
            let updates = &visible.updates[visible.updates.len() - kept..];

            let text = prompt::summarized(summary, task, updates);
            messages.push(ChatMessage::user(text));
        }

        // Numbered in order of appearance, so new steps never renumber earlier ones
        let mut calls = 0;
        for step in visible.steps {
            let added = step.messages(&mut calls, takes);
            messages.extend(added);
        }

        let mut attached = Vec::new();
        for message in &mut messages {
            if let ChatMessage::User(parts) | ChatMessage::Tool { parts, .. } = message {
                let media = parts
                    .iter_mut()
                    .filter(|part| matches!(part, Part::Media(..)));
                attached.extend(media);
            }
        }

        // A batch at a time keeps the prompt cache matching until the next batch goes
        let dropped = attached.len().saturating_sub(KEPT_MEDIA) / KEPT_MEDIA * KEPT_MEDIA;
        for part in attached.into_iter().take(dropped) {
            if let Part::Media(label, media) = part {
                let note = prompt::dropped(label, media);
                *part = Part::Text(note);
            }
        }

        messages
    }

    /// The latest measured usage plus an estimate for text added since
    pub(crate) fn tokens(
        &self,
        system: &str,
        takes: &dyn Fn(MediaKind) -> bool,
        window: Option<u64>,
    ) -> u64 {
        let mut unmeasured = Vec::new();
        for step in self.steps.iter().rev() {
            let measured = match step {
                MemoryStep::Task(_)
                | MemoryStep::Update(_)
                | MemoryStep::Shell(_)
                | MemoryStep::Restarted
                | MemoryStep::Done(_) => None,
                MemoryStep::Action(action) => {
                    let (text, calls) = action.seen();
                    let shown = !text.is_empty() || !calls.is_empty();
                    match (action.usage.tokens, shown) {
                        (Some(tokens), true) => Some(tokens.input_tokens + tokens.output_tokens),
                        // A dropped reply and its reasoning count in the measured output
                        // but aren't in the conversation
                        (Some(tokens), false) => Some(tokens.input_tokens),
                        (None, _) => None,
                    }
                }
                MemoryStep::Summary(_) => break,
            };

            let Some(measured) = measured else {
                unmeasured.push(step);
                continue;
            };

            let mut added = 0;
            for step in unmeasured {
                let messages = step.messages(&mut 0, takes);
                added += bytes(&messages);
            }

            // The measurement omits this step's results and errors, which may hold viewed images
            let mut after = step.messages(&mut 0, takes);
            after.retain(|message| !matches!(message, ChatMessage::Assistant { .. }));
            let after = bytes(&after);
            return measured + estimate(added + after);
        }

        let conversation = self.conversation(system, takes, window);
        let sent = bytes(&conversation);
        estimate(sent)
    }

    /// Only looks at steps since the last restart
    pub(crate) fn ran_code(&self) -> bool {
        for step in self.steps.iter().rev() {
            match step {
                MemoryStep::Action(action) => {
                    let ran = action.calls.iter().any(|call| call.result.is_some());
                    if ran {
                        return true;
                    }
                }
                MemoryStep::Restarted => return false,
                _ => {}
            }
        }
        false
    }
}

pub(crate) struct Visible<'a> {
    pub(crate) summary: Option<&'a SummaryStep>,
    /// The last task among the summarized steps
    task: Option<&'a TaskStep>,
    /// Updates after that task that were summarized too
    updates: Vec<&'a str>,
    pub(crate) steps: Vec<&'a MemoryStep>,
}

/// Reasoning goes back as `reasoning_content` or sealed items, counted by its text,
/// and media converts to bytes from a fixed token count
pub(crate) fn bytes(messages: &[ChatMessage]) -> usize {
    let mut bytes = 0;
    for message in messages {
        let parts = match message {
            ChatMessage::System(text) => {
                bytes += text_bytes(text);
                continue;
            }
            ChatMessage::Assistant {
                text,
                reasoning,
                calls,
                ..
            } => {
                let reasoning = reasoning.as_deref().unwrap_or_default();
                bytes += text_bytes(reasoning) + text_bytes(text);
                for call in calls {
                    bytes += text_bytes(&call.arguments);
                }
                continue;
            }
            ChatMessage::User(parts) | ChatMessage::Tool { parts, .. } => parts,
        };
        bytes += parts.len().saturating_sub(1);

        for part in parts {
            let (label, media) = match part {
                Part::Text(text) => {
                    bytes += text_bytes(text);
                    continue;
                }
                Part::Media(label, media) => (label, media),
            };

            let tokens = match media.kind {
                MediaKind::Image => IMAGE_TOKENS,
                MediaKind::Video => VIDEO_TOKENS,
            };
            let tag = prompt::media_tag(label, media);
            bytes += text_bytes(&tag) + tokens * BYTES_PER_TOKEN;
        }
    }
    bytes
}

/// Each digit counts as a token because counting bytes underestimates output full of numbers
pub(crate) fn text_bytes(text: &str) -> usize {
    let digits = text.bytes().filter(u8::is_ascii_digit).count();
    text.len() + digits * (BYTES_PER_TOKEN - 1)
}

pub(crate) fn estimate(bytes: usize) -> u64 {
    (bytes / BYTES_PER_TOKEN) as u64
}

/// What the requests of `steps` cost, compaction included, and how many steps were taken
pub(crate) fn spent(steps: &[MemoryStep]) -> (Usage, usize) {
    let mut total = Usage::default();
    let mut taken = 0;
    for step in steps {
        match step {
            MemoryStep::Action(action) => {
                total.add(&action.usage);
                taken += 1;
            }
            MemoryStep::Summary(summary) => total.add(&summary.usage),
            _ => {}
        }
    }
    (total, taken)
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::config::Kind;
    use crate::model::tests::png;
    use crate::steps::*;
    use crate::tests::content;
    use serde_json::json;

    pub(crate) fn metered(input: u64, output: u64) -> Usage {
        Usage {
            tokens: Some(TokenUsage::new(input, output)),
            ..Usage::default()
        }
    }

    pub(crate) fn asked(text: &str) -> TaskStep {
        TaskStep {
            text: text.to_owned(),
            files: Vec::new(),
        }
    }

    pub(crate) fn one_task(text: &str) -> AgentMemory {
        let task = asked(text);
        AgentMemory {
            steps: vec![MemoryStep::Task(task)],
        }
    }

    pub(crate) fn replied(text: &str) -> AgentStep {
        AgentStep {
            text: text.to_owned(),
            ..AgentStep::default()
        }
    }

    /// A step with code, which ran if `printed` is given
    pub(crate) fn coded(code: &str, printed: Option<String>) -> AgentStep {
        let ran = |printed| CodeResult {
            printed: Some(printed),
            ..CodeResult::default()
        };
        let call = Call {
            code: code.to_owned(),
            result: printed.map(ran),
            ..Call::default()
        };
        AgentStep {
            calls: vec![call],
            ..AgentStep::default()
        }
    }

    /// An agent keeps the saved format, with each task's end beside it, and the steps of a task
    /// run to where the next one starts, while a call names the tasks it sent
    #[test]
    fn agents_saved() {
        let saved = r#"{"name": "cafés", "memory": {"steps": [
            {"type": "task", "text": "Count the cafés"},
            {"type": "restarted"},
            {"type": "task", "text": "Spell naïve"}
        ]}, "tasks": [
            {"task": "Count the cafés", "start": 0, "answer": "3 cafés"},
            {"task": "Spell naïve", "start": 2, "error": "model: http 503: busy"},
            {"task": "Read the résumé", "start": 3, "interrupted": {}}
        ]}"#;
        let record: AgentRecord = serde_json::from_str(saved).expect("deserialize");
        let ends: Vec<&Ending> = record.tasks.iter().map(|task| &task.end).collect();
        let answered = Ending::Answer("3 cafés".to_owned());
        let failed = Ending::Error("model: http 503: busy".to_owned());
        assert_eq!(ends, [&answered, &failed, &Ending::Interrupted {}]);
        let counts: Vec<usize> = (0..3).map(|number| record.steps(number).len()).collect();
        assert_eq!(counts, [2, 1, 0]);

        let again = serde_json::to_value(&record).expect("serialize");
        let saved: serde_json::Value = serde_json::from_str(saved).expect("parse");
        assert_eq!(again, saved);

        let called = r#"{"agents": [{"name": "cafés", "number": 2}]}"#;
        let result: CodeResult = serde_json::from_str(called).expect("deserialize");
        let sent = Sent {
            name: "cafés".to_owned(),
            number: 2,
        };
        assert_eq!(result.agents, [sent]);
    }

    /// Steps saved before interrupts kept where the code stopped
    /// and before summaries listed the worker's names still load
    #[test]
    fn old_steps_read() {
        let saved = r#"{"steps": [
            {"type": "action", "calls": [
                {"code": "while True: pass", "result": {}, "error": {"type": "interrupted"}}
            ], "usage": {"duration": 1.5}},
            {"type": "summary", "summary": "Counted the cafés", "kept": 0, "usage": {"duration": 2.5}}
        ]}"#;
        let memory: AgentMemory = serde_json::from_str(saved).expect("deserialize");

        let MemoryStep::Action(action) = &memory.steps[0] else {
            panic!("{:?}", memory.steps);
        };
        assert_eq!(action.calls[0].error, Some(CallError::Interrupted(None)));
        let MemoryStep::Summary(summary) = &memory.steps[1] else {
            panic!("{:?}", memory.steps);
        };
        assert!(summary.globals.is_empty(), "{:?}", summary.globals);
    }

    #[test]
    fn sealed_replayed() {
        let thought =
            json!({ "type": "reasoning", "id": "rs_1", "encrypted_content": "gAAAA-café" });
        let said = json!({ "type": "message", "id": "msg_1", "phase": "final_answer" });
        let text = "\n\nVoilà, the café";
        let sealed = Sealed {
            provider: "openai".to_owned(),
            kind: Kind::Responses,
            items: vec![thought, said],
            ends: vec![2, text.len()],
        };
        let step = AgentStep {
            sealed: Some(sealed.clone()),
            ..replied(text)
        };
        let saved = serde_json::to_value(&step).expect("serialize");
        assert_eq!(saved["sealed"]["provider"], "openai");
        assert_eq!(saved["sealed"]["items"][1]["phase"], "final_answer");
        let loaded: AgentStep = serde_json::from_value(saved).expect("deserialize");
        assert_eq!(loaded, step);

        let mut calls = 0;
        let action = MemoryStep::Action(Box::new(step));
        let messages = action.messages(&mut calls, &|_| true);
        let ChatMessage::Assistant {
            text: seen,
            sealed: sent,
            ..
        } = &messages[0]
        else {
            panic!("{messages:?}");
        };
        let moved = Sealed {
            ends: vec![0, text.len() - 2],
            ..sealed
        };
        assert_eq!(seen, "Voilà, the café");
        assert_eq!(sent.as_ref(), Some(&moved));

        let plain = serde_json::to_value(replied("Voilà")).expect("serialize");
        assert!(plain.get("sealed").is_none(), "{plain}");
    }

    /// Older images give way to a note a batch at a time,
    /// which leaves the start of the conversation as it was until the next batch
    #[test]
    fn old_media_dropped() {
        let viewing = |count: usize, takes: &dyn Fn(MediaKind) -> bool| {
            let mut steps = vec![MemoryStep::Task(asked("Compare the café menus"))];
            for n in 0..count {
                let label = format!("menu {n}.png");
                let result = CodeResult {
                    media: vec![(label, png("café.png"))],
                    ..CodeResult::default()
                };
                let mut step = coded("view('menu.png')", None);
                step.calls[0].result = Some(result);
                steps.push(MemoryStep::Action(Box::new(step)));
            }
            AgentMemory { steps }.conversation("", takes, None)
        };
        let note = prompt::dropped("menu 0.png", &png("café.png"));
        let counted = |messages: &[ChatMessage]| {
            let (mut shown, mut noted) = (0, 0);
            for message in messages {
                let (ChatMessage::User(parts) | ChatMessage::Tool { parts, .. }) = message else {
                    continue;
                };
                for part in parts {
                    match part {
                        Part::Media(..) => shown += 1,
                        Part::Text(text) if text.ends_with("no longer shown]") => noted += 1,
                        Part::Text(_) => {}
                    }
                }
            }
            (shown, noted)
        };

        let all = |_| true;
        assert_eq!(counted(&viewing(15, &all)), (15, 0));
        let dropped = viewing(16, &all);
        assert_eq!(counted(&dropped), (8, 8));
        assert!(
            dropped.iter().any(|message| content(message) == note),
            "{dropped:?}"
        );
        let fewer = viewing(17, &all);
        let more = viewing(23, &all);
        assert_eq!(counted(&more), (15, 8));
        assert_eq!(more[..fewer.len()], fewer[..]);
        assert_eq!(counted(&viewing(24, &all)), (8, 16));

        // Media the model doesn't take become notes, which don't count toward a batch
        let videos = |kind| kind == MediaKind::Video;
        let unseen = viewing(16, &videos);
        assert_eq!(counted(&unseen), (0, 0));
        let last = prompt::unseen("menu 15.png", &png("café.png"));
        assert!(
            unseen
                .iter()
                .any(|message| content(message).contains(&last)),
            "{unseen:?}"
        );
    }
}
