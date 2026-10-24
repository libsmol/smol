use crate::model::{ChatMessage, Media, MediaKind, ModelError, Part, TokenUsage, ToolCall};
use crate::prompt;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::Not;

pub(crate) const MAX_OBSERVATION_CHARS: usize = 16_384;
/// Errs toward overestimating
const BYTES_PER_TOKEN: usize = 3;
/// Providers count differently, so this takes the high end
const IMAGE_TOKENS: usize = 1500;
const VIDEO_TOKENS: usize = 25_000;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "type", content = "message", rename_all = "snake_case")]
pub(crate) enum StepError {
    #[error("reply: cut off at the output limit, nothing ran")]
    Cut,
    /// A step with this error never enters memory and is never saved
    #[serde(skip)]
    #[error("model: {0}")]
    Model(ModelError),
    #[error("interrupted")]
    Interrupted,
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
    #[error("interrupted")]
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub(crate) struct Usage {
    /// Seconds since the request was sent
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct AgentStep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) check: Option<Check>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) unchecked: Option<Unchecked>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Unchecked {
    /// The error the user saw at the time
    Failed(String),
    Interrupted,
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Check {
    pub(crate) verdict: Verdict,
    pub(crate) reason: String,
    pub(crate) usage: Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Verdict {
    Met,
    Unmet,
    Blocked,
}

impl AgentStep {
    pub(crate) fn answer(&self) -> Option<&str> {
        let text = self.text.trim();
        let replied = self.calls.is_empty() && self.error.is_none() && !text.is_empty();
        replied.then_some(text)
    }

    /// A reply cut off at the output limit is never sent to the model and only shows on replay
    pub(crate) fn dropped(&self) -> bool {
        self.error == Some(StepError::Cut)
    }
}

impl Call {
    pub(crate) fn unread(&self) -> bool {
        matches!(self.error, Some(CallError::NoTool(_) | CallError::NoCode))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TaskStep {
    pub(crate) text: String,
    #[serde(default, skip_serializing_if = "Not::not")]
    pub(crate) goal: bool,
    /// Updates received earlier for the same goal
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) carried: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) files: Vec<Attachment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Attachment {
    /// Directories end in /
    pub(crate) path: String,
    pub(crate) content: String,
    /// When set, `content` is empty
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) media: Option<Media>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ShellStep {
    pub(crate) command: String,
    pub(crate) shell: String,
    /// Combined stdout and stderr, already truncated
    pub(crate) output: String,
    pub(crate) end: Exit,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct SummaryStep {
    pub(crate) summary: String,
    /// How many steps before the summary are kept verbatim
    pub(crate) kept: usize,
    pub(crate) usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) used: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum MemoryStep {
    Task(TaskStep),
    Update(TaskStep),
    Action(Box<AgentStep>),
    Summary(SummaryStep),
    Shell(ShellStep),
    /// The user cleared the goal
    Cleared,
    Restarted,
}

/// Also the format of the `_smol/goal` notification
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Goal {
    pub(crate) objective: String,
    pub(crate) status: GoalStatus,
    pub(crate) updates: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GoalStatus {
    /// Only exists during the turn working on it and is never saved
    Active,
    Paused,
    Blocked,
    Complete,
}

impl MemoryStep {
    /// [`AgentMemory::conversation`] places the summary, and calls are numbered on from `calls`
    pub(crate) fn messages(&self, calls: &mut usize) -> Vec<ChatMessage> {
        let step = match self {
            MemoryStep::Task(task) => {
                let parts = prompt::task(task);
                return vec![ChatMessage::User(parts)];
            }
            MemoryStep::Update(update) => {
                let parts = prompt::update(update);
                return vec![ChatMessage::User(parts)];
            }
            MemoryStep::Shell(shell) => {
                let text = prompt::shell(shell);
                return vec![ChatMessage::user(text)];
            }
            MemoryStep::Restarted => return vec![ChatMessage::user(prompt::RESTARTED)],
            MemoryStep::Action(step) => step,
            MemoryStep::Summary(_) | MemoryStep::Cleared => return Vec::new(),
        };

        let mut messages = Vec::new();

        let shown = !step.dropped();
        let text = step.text.trim();
        let mut sent = Vec::new();
        if shown {
            for call in &step.calls {
                *calls += 1;
                let id = format!("call_{calls}");
                // Unread calls don't echo their arguments because the model would copy the mistake
                let called = match &call.error {
                    Some(CallError::NoTool(name)) => ToolCall::unread(id, name),
                    Some(CallError::NoCode) => ToolCall::unread(id, prompt::TOOL),
                    _ => ToolCall::eval(id, &call.code),
                };
                sent.push(called);
            }
        }
        let called = !sent.is_empty();
        if shown && (called || !text.is_empty()) {
            let reply = ChatMessage::Assistant {
                text: text.to_owned(),
                reasoning: step.reasoning.clone(),
                calls: sent.clone(),
            };
            messages.push(reply);
        }

        // Every call needs a reply, and one that didn't run says why
        for (call, ToolCall { id, .. }) in step.calls.iter().zip(sent) {
            let mut parts = Vec::new();
            if let Some(result) = &call.result {
                let printed = result.printed.as_deref();
                let shown = prompt::result(printed, &result.media);
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

        if !called {
            if let Some(text) = prompt::error(step) {
                messages.push(ChatMessage::user(text));
            }
        }

        if let Some(check) = &step.check {
            let reason = &check.reason;
            let told = match check.verdict {
                Verdict::Met => None,
                Verdict::Unmet => Some(prompt::unmet(reason)),
                Verdict::Blocked => Some(prompt::blocked(reason)),
            };
            if let Some(text) = told {
                messages.push(ChatMessage::user(text));
            }
        }

        messages
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
                    | MemoryStep::Cleared
                    | MemoryStep::Restarted => {}
                }
            }
            visible.summary = Some(summary);
        }

        visible
    }

    pub(crate) fn conversation(&self, system: &str, window: Option<u64>) -> Vec<ChatMessage> {
        let visible = self.visible();
        let message = ChatMessage::System(system.to_owned());
        let mut messages = vec![message];

        if let Some(text) = visible.summarized(window) {
            messages.push(ChatMessage::user(text));
        }

        // Numbered in order of appearance, so new steps never renumber earlier ones
        let mut calls = 0;
        for step in visible.steps {
            let added = step.messages(&mut calls);
            messages.extend(added);
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
        let mut unmeasured = 0;
        for step in self.steps.iter().rev() {
            let measured = match step {
                MemoryStep::Task(_)
                | MemoryStep::Update(_)
                | MemoryStep::Shell(_)
                | MemoryStep::Cleared
                | MemoryStep::Restarted => None,
                MemoryStep::Action(action) => {
                    let said = !action.text.is_empty() || !action.calls.is_empty();
                    let shown = said && !action.dropped();
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

            let messages = step.messages(&mut 0);
            let Some(measured) = measured else {
                unmeasured += bytes(&messages, takes);
                continue;
            };

            // The measurement omits this step's results and errors, which may hold viewed images
            let mut after = messages;
            after.retain(|message| !matches!(message, ChatMessage::Assistant { .. }));
            let after = bytes(&after, takes);
            return measured + estimate(unmeasured + after);
        }

        let conversation = self.conversation(system, window);
        let sent = bytes(&conversation, takes);
        estimate(sent)
    }

    /// The last goal task sets the goal, everything the user says after it is an update,
    /// and the last step recorded in its turns sets the status, ignoring unrecorded later failures
    pub(crate) fn goal(&self) -> Option<Goal> {
        let mut goal: Option<Goal> = None;
        let mut working = false;
        for step in &self.steps {
            match step {
                MemoryStep::Task(task) => {
                    working = task.goal;
                    if task.goal {
                        goal = Some(Goal {
                            objective: task.text.clone(),
                            status: GoalStatus::Paused,
                            updates: task.carried.clone(),
                        });
                    } else if let Some(goal) = &mut goal {
                        goal.updates.push(task.text.clone());
                    }
                }
                MemoryStep::Cleared => {
                    goal = None;
                    working = false;
                }
                MemoryStep::Update(update) => {
                    if let Some(goal) = &mut goal {
                        goal.updates.push(update.text.clone());
                    }
                }
                MemoryStep::Action(action) if working => {
                    let verdict = action.check.as_ref().map(|check| check.verdict);
                    let status = match verdict {
                        Some(Verdict::Met) => GoalStatus::Complete,
                        Some(Verdict::Blocked) => GoalStatus::Blocked,
                        _ => GoalStatus::Paused,
                    };
                    if let Some(goal) = &mut goal {
                        goal.status = status;
                    }
                }
                _ => {}
            }
        }
        goal
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

impl Visible<'_> {
    /// The message that replaces the earlier conversation
    pub(crate) fn summarized(&self, window: Option<u64>) -> Option<String> {
        let summary = self.summary?;
        let tasked = self
            .steps
            .iter()
            .any(|step| matches!(step, MemoryStep::Task(_)));
        let task = self.task.filter(|_| !tasked);
        let task = task.map(|task| prompt::resumed(&task.text, &task.carried));
        let task = task.as_deref();

        // Updates aren't refused when oversized like tasks are,
        // so once they and the task pass half the window only the latest that fit come back
        let mut kept = self.updates.len();
        if let Some(window) = window {
            let mut size = task.map_or(0, text_bytes);
            kept = 0;
            for update in self.updates.iter().rev() {
                size += text_bytes(update);
                if estimate(size) > window / 2 {
                    break;
                }
                kept += 1;
            }
        }
        let updates = &self.updates[self.updates.len() - kept..];

        let text = prompt::summarized(&summary.summary, task, updates);
        Some(text)
    }
}

/// Reasoning goes back as `reasoning_content` too,
/// and media the model takes converts to bytes from a fixed token count
pub(crate) fn bytes(messages: &[ChatMessage], takes: &dyn Fn(MediaKind) -> bool) -> usize {
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

            if !takes(media.kind) {
                let note = prompt::unseen(label, media);
                bytes += text_bytes(&note);
                continue;
            }
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

#[cfg(test)]
pub(crate) mod tests {
    use crate::memory::*;
    use crate::model::tests::png;
    use crate::tests::content;

    pub(crate) fn metered(input: u64, output: u64) -> Usage {
        Usage {
            tokens: Some(TokenUsage::new(input, output)),
            ..Usage::default()
        }
    }

    pub(crate) fn asked(text: &str, goal: bool) -> TaskStep {
        TaskStep {
            text: text.to_owned(),
            goal,
            carried: Vec::new(),
            files: Vec::new(),
        }
    }

    pub(crate) fn one_task(text: &str) -> AgentMemory {
        let task = asked(text, false);
        AgentMemory {
            steps: vec![MemoryStep::Task(task)],
        }
    }

    fn task(text: &str) -> MemoryStep {
        let asked = asked(text, false);
        MemoryStep::Task(asked)
    }

    fn update(text: &str) -> MemoryStep {
        let asked = asked(text, false);
        MemoryStep::Update(asked)
    }

    pub(crate) fn replied(text: &str) -> AgentStep {
        AgentStep {
            text: text.to_owned(),
            ..AgentStep::default()
        }
    }

    fn action(text: &str) -> MemoryStep {
        let step = replied(text);
        MemoryStep::Action(Box::new(step))
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

    fn summary(text: &str, kept: usize) -> MemoryStep {
        let step = SummaryStep {
            summary: text.to_owned(),
            kept,
            ..SummaryStep::default()
        };
        MemoryStep::Summary(step)
    }

    fn tokens(memory: &AgentMemory) -> u64 {
        memory.tokens("", &|_| true, None)
    }

    fn contents(memory: &AgentMemory) -> Vec<String> {
        let messages = memory.conversation("", None).into_iter().skip(1);
        messages.map(|message| content(&message)).collect()
    }

    #[test]
    fn summary_keeps_recent() {
        let mut memory = AgentMemory {
            steps: vec![
                task("First task"),
                action("a1"),
                action("a2"),
                summary("s1", 1),
            ],
        };
        let first = prompt::summarized("s1", Some("First task"), &[]);
        assert_eq!(contents(&memory), [first, "a2".to_owned()]);

        memory.steps.extend([
            action("a3"),
            task("Second task"),
            action("a4"),
            summary("s2", 3),
        ]);
        let second = prompt::summarized("s2", None, &[]);
        let task = "New task:\nSecond task".to_owned();
        let expected = [second, "a3".to_owned(), task, "a4".to_owned()];
        assert_eq!(contents(&memory), expected);

        memory.steps.extend([action("a5"), summary("s3", 0)]);
        let third = prompt::summarized("s3", Some("Second task"), &[]);
        assert_eq!(contents(&memory), [third]);
    }

    #[test]
    fn summary_keeps_updates() {
        let mut memory = AgentMemory {
            steps: vec![
                task("Write a poem about autumn"),
                action("a1"),
                update("Make it winter"),
                action("a2"),
                update("Only four lines"),
                action("a3"),
                summary("s1", 1),
            ],
        };
        let text = "Earlier messages were replaced by this summary:\n\n\
            <summary>\ns1\n</summary>\n\n\
            Current task:\nWrite a poem about autumn\n\n\
            Task update:\nMake it winter\n\n\
            Task update:\nOnly four lines";
        assert_eq!(contents(&memory), [text, "a3"]);

        memory
            .steps
            .extend([task("A new task"), action("a4"), summary("s2", 1)]);
        let renewed = prompt::summarized("s2", Some("A new task"), &[]);
        assert_eq!(contents(&memory), [renewed, "a4".to_owned()]);
    }

    #[test]
    fn updates_fit_window() {
        let long = "crème brûlée ".repeat(100);
        let memory = AgentMemory {
            steps: vec![
                task("Write a poem about autumn"),
                action("a1"),
                update(&long),
                update("Make it winter"),
                update("Only four lines"),
                action("a2"),
                summary("s1", 0),
            ],
        };

        let conversation = memory.conversation("", Some(1000));
        let first = content(&conversation[1]);
        let recent = ["Make it winter", "Only four lines"];
        let fitted = prompt::summarized("s1", Some("Write a poem about autumn"), &recent);
        assert_eq!(first, fitted);

        let all = [long.as_str(), "Make it winter", "Only four lines"];
        let unbounded = prompt::summarized("s1", Some("Write a poem about autumn"), &all);
        assert_eq!(contents(&memory), [unbounded]);
    }

    #[test]
    fn restart_noted() {
        let saved = r#"{"type": "action", "calls": [{"code": "print(orders)", "result": {"printed": "Orders loaded"}}], "usage": {"duration": 0.0}}"#;
        let ran: MemoryStep = serde_json::from_str(saved).expect("deserialize");
        let memory = AgentMemory {
            steps: vec![task("Read the orders again"), MemoryStep::Restarted, ran],
        };
        let expected = [
            "New task:\nRead the orders again",
            prompt::RESTARTED,
            "eval:\nprint(orders)",
            "Orders loaded",
        ];
        assert_eq!(contents(&memory), expected);

        let steps: Vec<&MemoryStep> = memory.steps.iter().collect();
        let transcript = prompt::transcript(&steps, usize::MAX);
        let skipped =
            "[Task]: Read the orders again\n\n[Code]: print(orders)\n\n[Observation]: Orders loaded";
        assert_eq!(transcript, skipped);
    }

    #[test]
    fn goal_derived() {
        let goal = |text: &str| {
            let carried = vec!["Add tests".to_owned()];
            let task = TaskStep {
                carried,
                ..asked(text, true)
            };
            MemoryStep::Task(task)
        };
        let checked = |verdict| Check {
            verdict,
            reason: String::new(),
            usage: Usage::default(),
        };
        let ran = |verdict: Option<Verdict>| {
            let check = verdict.map(checked);
            let step = AgentStep {
                check,
                ..AgentStep::default()
            };
            MemoryStep::Action(Box::new(step))
        };

        let mut memory = AgentMemory {
            steps: vec![
                MemoryStep::Task(asked("Fix the parser", true)),
                update("Add tests"),
                ran(Some(Verdict::Blocked)),
                goal("Fix the parser"),
                ran(None),
                task("Drop the match"),
                ran(Some(Verdict::Met)),
                update("Switch to automatic signing"),
            ],
        };
        let updates = ["Add tests", "Drop the match", "Switch to automatic signing"];
        let expected = Goal {
            objective: "Fix the parser".to_owned(),
            status: GoalStatus::Paused,
            updates: updates.map(str::to_owned).to_vec(),
        };
        assert_eq!(memory.goal(), Some(expected));
        assert_eq!(AgentMemory::default().goal(), None);

        memory.steps.push(MemoryStep::Cleared);
        assert_eq!(memory.goal(), None);
        let renewed = MemoryStep::Task(asked("Migrate the database", true));
        memory.steps.extend([renewed, ran(Some(Verdict::Met))]);
        let renewed = memory.goal().expect("goal");
        assert_eq!(renewed.objective, "Migrate the database");
        assert_eq!(renewed.status, GoalStatus::Complete);
        assert!(renewed.updates.is_empty());
    }

    #[test]
    fn context_estimate() {
        let printed = "x".repeat(300);
        let ran = AgentStep {
            usage: metered(100, 20),
            ..coded("print(1)", Some(printed))
        };
        let mut memory = AgentMemory {
            steps: vec![task("a"), MemoryStep::Action(Box::new(ran))],
        };
        assert_eq!(tokens(&memory), 120 + 300 / 3);

        let long = "café".repeat(10);
        let asked = task(&long);
        memory.steps.push(asked);
        let unmeasured = 300 + "New task:\n".len() + long.len();
        assert_eq!(tokens(&memory), 120 + estimate(unmeasured));

        let thought = AgentStep {
            reasoning: Some("r".repeat(30)),
            ..replied(&"o".repeat(30))
        };
        memory.steps.push(MemoryStep::Action(Box::new(thought)));
        assert_eq!(tokens(&memory), 120 + estimate(unmeasured + 60));

        let summarized = SummaryStep {
            summary: "s".repeat(30),
            ..SummaryStep::default()
        };
        memory.steps.push(MemoryStep::Summary(summarized));
        let conversation = memory.conversation("", None);
        let sent = bytes(&conversation, &|_| true);
        assert_eq!(tokens(&memory), estimate(sent));
    }

    #[test]
    fn digits_counted() {
        assert_eq!(text_bytes("café 12345"), 11 + 5 * 2);
        assert_eq!(text_bytes("résumé"), 8);

        let numbers = "4096 ".repeat(100);
        let size = text_bytes(&numbers);
        assert_eq!(estimate(size), 400 + 100 / 3);
    }

    #[test]
    fn attached_files() {
        let file = |path: &str, content: &str| Attachment {
            path: path.to_owned(),
            content: content.to_owned(),
            media: None,
        };
        let files = vec![file("src/main.rs", "fn main() {}\n"), file("docs/", "a.md")];
        let task = TaskStep {
            files,
            ..asked("Look at @src/main.rs", false)
        };
        let asked = MemoryStep::Task(task);
        let text = "New task:\nLook at @src/main.rs\n\nAttached files:\n\
            <file path=\"src/main.rs\">\nfn main() {}\n</file>\n\
            <directory path=\"docs/\">\na.md\n</directory>";
        assert_eq!(content(&asked.messages(&mut 0)[0]), text);

        let ran = AgentStep {
            usage: metered(100, 20),
            ..replied("Looked at it")
        };
        let memory = AgentMemory {
            steps: vec![MemoryStep::Action(Box::new(ran)), asked],
        };
        assert_eq!(tokens(&memory), 120 + estimate(text.len()));
    }

    #[test]
    fn attached_media() {
        let media = png("/history/s1/a.png");
        let image = Attachment {
            path: "screenshot.png".to_owned(),
            content: String::new(),
            media: Some(media.clone()),
        };
        let notes = Attachment {
            path: "notes.txt".to_owned(),
            content: "Crème brûlée\n".to_owned(),
            media: None,
        };
        let update = TaskStep {
            files: vec![image, notes],
            ..asked("Look at this one", false)
        };

        let asked = MemoryStep::Update(update);
        let messages = asked.messages(&mut 0);
        let head = "Task update:\nLook at this one\n\nAttached files:";
        let file = "<file path=\"notes.txt\">\nCrème brûlée\n</file>";
        let parts = vec![
            Part::Text(head.to_owned()),
            Part::Media("screenshot.png".to_owned(), media.clone()),
            Part::Text(file.to_owned()),
        ];
        assert_eq!(messages[0], ChatMessage::User(parts));

        let tag = prompt::media_tag("screenshot.png", &media);
        let text = format!("{head}\n{tag}\n{file}");
        let sent = bytes(&messages, &|_| true);
        let estimated = estimate(sent);
        let expected = estimate(text.len()) + IMAGE_TOKENS as u64;
        assert_eq!(estimated, expected);
        let note = prompt::unseen("screenshot.png", &media);
        let unseen = bytes(&messages, &|_| false);
        assert_eq!(unseen, head.len() + note.len() + file.len() + 2);

        let transcript = prompt::transcript(&[&asked], usize::MAX);
        let placeholder = "[Task update]: Look at this one\n\n[Attached image screenshot.png]\n\n\
            [Attached notes.txt]: Crème brûlée";
        assert_eq!(transcript, placeholder);
    }

    #[test]
    fn viewed_media() {
        let media = png("a.png");
        let viewed = Part::Media("screenshot.png".to_owned(), media.clone());
        let usage = metered(100, 20);
        let step = |printed: Option<&str>| {
            let result = CodeResult {
                printed: printed.map(str::to_owned),
                media: vec![("screenshot.png".to_owned(), media.clone())],
            };
            let mut step = coded("view('screenshot.png')", None);
            step.calls[0].result = Some(result);
            step.usage = usage;
            step
        };
        let answer = |parts: Vec<Part>| ChatMessage::Tool {
            id: "call_1".to_owned(),
            parts,
        };

        let quiet = MemoryStep::Action(Box::new(step(None)));
        let messages = quiet.messages(&mut 0);
        assert_eq!(messages[1], answer(vec![viewed.clone()]));

        // Measured usage leaves out the later results, so their images must be estimated too
        let memory = AgentMemory { steps: vec![quiet] };
        let text = "<image path=\"screenshot.png\"/>";
        let expected = 120 + estimate(text.len()) + IMAGE_TOKENS as u64;
        assert_eq!(tokens(&memory), expected);

        let printed = step(Some("Printed"));
        let printed = MemoryStep::Action(Box::new(printed));
        let text = Part::Text("Printed".to_owned());
        assert_eq!(printed.messages(&mut 0)[1], answer(vec![text, viewed]));
    }
}
