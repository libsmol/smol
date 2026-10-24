use crate::acp::rpc::{Notice, Peer};
use crate::acp::{GOAL, SHELL, UPDATE};
use crate::agent::Event;
use crate::memory::{
    estimate, text_bytes, AgentMemory, AgentStep, Call, CallError, Check, Exit, Goal, MemoryStep,
    ShellStep, StepError, SummaryStep, TaskStep, Unchecked, Usage, Verdict,
};
use crate::prompt::{self, TOOL};
use agent_client_protocol_schema::v1::{
    ContentBlock, ContentChunk, MessageId, Meta, ResourceLink, SessionUpdate, ToolCall,
    ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind, UsageUpdate,
};
use reqwest::Url;
use serde_json::json;
use std::mem;
use std::path::{self, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ESTIMATE_EVERY: Duration = Duration::from_secs(1);
const COMPACTING: &str = "Compact context";
const CHECKING: &str = "Check goal";
const RESTARTED: &str = "python: restarted, earlier variables are gone";
const SKIPPED: &str = "skipped: an earlier call failed";

pub(crate) struct Translator {
    session: String,
    peer: Arc<Peer>,
    ids: usize,
    tasks: usize,
    window: u64,
    used: u64,
    calls: Vec<Opened>,
    /// Index within this step of the call whose code is running
    running: Option<usize>,
    /// The open compaction or goal check call
    thinking: Option<String>,
    /// The call for the user command that is running
    shell: Option<String>,
    progress: Progress,
    copies: PathBuf,
}

struct Opened {
    index: usize,
    id: String,
    code: String,
    arguments: Option<String>,
    ended: bool,
}

#[derive(Default)]
struct Progress {
    /// Counted with `text_bytes`
    bytes: usize,
    /// When generation started or the last estimate was reported
    since: Option<Instant>,
    /// The last reported output estimate, or 0 if none was reported
    reported: u64,
}

impl Translator {
    pub(crate) fn new(
        session: String,
        peer: Arc<Peer>,
        window: Option<u64>,
        copies: PathBuf,
    ) -> Self {
        Self {
            session,
            peer,
            ids: 0,
            tasks: 0,
            window: window.unwrap_or(0),
            used: 0,
            calls: Vec::new(),
            running: None,
            thinking: None,
            shell: None,
            progress: Progress::default(),
            copies,
        }
    }

    pub(crate) fn replay(&mut self, memory: &AgentMemory) {
        for step in &memory.steps {
            match step {
                MemoryStep::Task(task) => self.task(task, false),
                MemoryStep::Update(update) => self.task(update, true),
                MemoryStep::Action(step) => self.action(step),
                MemoryStep::Summary(summary) => {
                    self.think(COMPACTING);
                    self.compact_end(Ok(summary));
                }
                MemoryStep::Shell(shell) => {
                    self.shell(&shell.command);
                    self.shell_end(shell);
                }
                MemoryStep::Restarted => self.notice(RESTARTED, None),
                MemoryStep::Cleared => {}
            }
        }
    }

    pub(crate) fn translate(&mut self, event: Event) {
        match event {
            Event::Thought(text) => {
                self.streamed(text);
                let chunk = chunk(text);
                self.update(SessionUpdate::AgentThoughtChunk(chunk));
            }
            Event::Text(text) => {
                self.streamed(text);
                self.said(text);
            }
            Event::Code(index, text) => {
                self.streamed(text);
                self.code(index, text);
            }
            Event::Unread(index, call) => self.unread(index, call),
            Event::Execute(index) => {
                self.running = Some(index);
                if let Some(call) = self.opened(index) {
                    let fields = ToolCallUpdateFields::new().status(ToolCallStatus::InProgress);
                    let update = ToolCallUpdate::new(call.id.clone(), fields);
                    self.update(SessionUpdate::ToolCallUpdate(update));
                }
            }
            Event::Output(output) => {
                let running = self.running.and_then(|index| self.opened(index));
                if let Some(call) = running {
                    let code = fenced(&call.code);
                    let output = ToolCallContent::from(output);
                    let content = vec![code, output];
                    let fields = ToolCallUpdateFields::new().content(content);
                    let update = ToolCallUpdate::new(call.id.clone(), fields);
                    self.update(SessionUpdate::ToolCallUpdate(update));
                }
            }
            Event::Ran(index, call) => self.ran(index, call),
            Event::Restarted => self.notice(RESTARTED, None),
            Event::Retry { reason, wait } => {
                for call in mem::take(&mut self.calls) {
                    let head = head(&call.code, call.arguments.as_deref());
                    let content = vec![head];
                    let fields = ToolCallUpdateFields::new()
                        .status(ToolCallStatus::Failed)
                        .content(content);
                    let update = ToolCallUpdate::new(call.id, fields);
                    self.update(SessionUpdate::ToolCallUpdate(update));
                }
                self.notice(reason, Some(wait));
                // When the goal check is retried the answer step still counts
                if self.thinking.is_none() {
                    self.withdraw();
                }
            }
            Event::StepEnd(step) => self.step_end(step),
            Event::Compact => self.think(COMPACTING),
            Event::CompactEnd(result) => self.compact_end(result),
            Event::Check => self.think(CHECKING),
            Event::CheckEnd(result) => self.check_end(result),
            Event::Goal(goal) => self.goal(goal),
        }
    }

    pub(crate) fn goal(&self, goal: Option<&Goal>) {
        let params = json!({ "sessionId": self.session, "goal": goal });
        self.peer.notify(GOAL, params);
    }

    /// Without metrics the client doesn't count it as a step
    pub(crate) fn model_changed(&mut self, window: Option<u64>) {
        self.window = window.unwrap_or(0);
        self.used = 0;
        let update = UsageUpdate::new(0, self.window);
        self.update(SessionUpdate::UsageUpdate(update));
    }

    pub(crate) fn shell(&mut self, command: &str) {
        self.ids += 1;
        let id = format!("call_{}", self.ids);
        let marked = (SHELL.to_owned(), json!(true));
        let meta = Meta::from_iter([marked]);
        let started = ToolCall::new(id.clone(), command)
            .kind(ToolKind::Execute)
            .status(ToolCallStatus::InProgress)
            .meta(meta);
        self.update(SessionUpdate::ToolCall(started));
        self.shell = Some(id);
    }

    /// `output` is all the clipped output so far
    pub(crate) fn shell_output(&self, output: &str) {
        let Some(id) = &self.shell else {
            return;
        };
        let content = vec![ToolCallContent::from(output)];
        let fields = ToolCallUpdateFields::new().content(content);
        let update = ToolCallUpdate::new(id.clone(), fields);
        self.update(SessionUpdate::ToolCallUpdate(update));
    }

    pub(crate) fn shell_end(&mut self, shell: &ShellStep) {
        let Some(id) = self.shell.take() else {
            return;
        };

        let failed = !matches!(shell.end, Exit::Exited(0));
        let status = match failed {
            true => ToolCallStatus::Failed,
            false => ToolCallStatus::Completed,
        };
        let mut content = Vec::new();
        if !shell.output.is_empty() {
            let output = ToolCallContent::from(&shell.output);
            content.push(output);
        }

        let fields = ToolCallUpdateFields::new().status(status).content(content);
        let mut update = ToolCallUpdate::new(id, fields);
        if failed {
            let error = ("error".to_owned(), json!(shell.end.to_string()));
            let mut meta = Meta::from_iter([error]);
            // The client recognizes interrupts by this key, which matches the one on usage updates
            if shell.end == Exit::Interrupted {
                meta.insert("interrupted".to_owned(), true.into());
            }
            update = update.meta(meta);
        }
        self.update(SessionUpdate::ToolCallUpdate(update));
    }

    fn task(&mut self, asked: &TaskStep, appended: bool) {
        self.tasks += 1;
        let id = format!("task_{}", self.tasks);
        let id = MessageId::new(id);
        // Updates carried over by a continued goal were already shown where they first arrived
        let mut chunk = chunk(&asked.text).message_id(id);
        let flag = match (appended, asked.goal) {
            (true, _) => Some(UPDATE),
            (false, true) => Some(GOAL),
            (false, false) => None,
        };
        if let Some(flag) = flag {
            let flag = (flag.to_owned(), json!(true));
            let meta = Meta::from_iter([flag]);
            chunk = chunk.meta(meta);
        }
        self.update(SessionUpdate::UserMessageChunk(chunk));
    }

    /// A saved step, with each call's code sent in one piece
    fn action(&mut self, step: &AgentStep) {
        let reasoning = step.reasoning.as_deref().unwrap_or_default();
        if !reasoning.is_empty() {
            let chunk = chunk(reasoning);
            self.update(SessionUpdate::AgentThoughtChunk(chunk));
        }
        // Split the text where each call started to restore the live order
        let mut said = 0;
        for (index, call) in step.calls.iter().enumerate() {
            let at = call.at.min(step.text.len()).max(said);
            let before = step.text.get(said..at).unwrap_or_default();
            self.said(before);
            said = at;

            // Live calls open when their first code chunk arrives,
            // and unreadable ones open when received
            if !call.code.is_empty() {
                self.code(index, &call.code);
            } else if call.arguments.is_some() {
                self.unread(index, call);
            }
            if call.result.is_some() || call.error.is_some() {
                self.ran(index, call);
            }
        }
        let rest = step.text.get(said..).unwrap_or_default();
        self.said(rest);

        if let Some(check) = &step.check {
            self.think(CHECKING);
            self.check_end(Ok(check));
        }
        if let Some(unchecked) = &step.unchecked {
            let error = match unchecked {
                Unchecked::Failed(error) => error.clone(),
                Unchecked::Interrupted => StepError::Interrupted.to_string(),
            };
            self.think(CHECKING);
            self.think_end(ToolCallStatus::Failed, &error);
        }
        self.step_end(step);
    }

    fn said(&self, text: &str) {
        if !text.is_empty() {
            let chunk = chunk(text);
            self.update(SessionUpdate::AgentMessageChunk(chunk));
        }
    }

    fn streamed(&mut self, text: &str) {
        let progress = &mut self.progress;
        progress.bytes += text_bytes(text);
        let now = Instant::now();
        let since = *progress.since.get_or_insert(now);
        let output = estimate(progress.bytes);

        let due = now.duration_since(since) >= ESTIMATE_EVERY;
        if due && output != progress.reported {
            progress.since = Some(now);
            progress.reported = output;
            // 0 means not measured yet, which the client doesn't treat as context
            let used = match self.used {
                0 => 0,
                used => used + output,
            };
            let estimated = ("estimatedOutput".to_owned(), json!(output));
            let meta = Meta::from_iter([estimated]);
            let update = UsageUpdate::new(used, self.window).meta(meta);
            self.update(SessionUpdate::UsageUpdate(update));
        }
    }

    fn code(&mut self, index: usize, text: &str) {
        let opened = self.calls.iter_mut().find(|call| call.index == index);
        let update = match opened {
            Some(call) => {
                call.code.push_str(text);
                let code = fenced(&call.code);
                let content = vec![code];
                let fields = ToolCallUpdateFields::new().content(content);
                let update = ToolCallUpdate::new(call.id.clone(), fields);
                SessionUpdate::ToolCallUpdate(update)
            }
            None => {
                self.ids += 1;
                let id = format!("call_{}", self.ids);
                let code = fenced(text);
                let content = vec![code];
                let started = ToolCall::new(id.clone(), TOOL)
                    .kind(ToolKind::Execute)
                    .status(ToolCallStatus::Pending)
                    .content(content);
                let opened = Opened {
                    index,
                    id,
                    code: text.to_owned(),
                    arguments: None,
                    ended: false,
                };
                self.calls.push(opened);
                SessionUpdate::ToolCall(started)
            }
        };
        self.update(update);
    }

    /// A call that already streamed code keeps showing its code
    fn unread(&mut self, index: usize, call: &Call) {
        if self.opened(index).is_some() {
            return;
        }

        let (title, kind) = match &call.error {
            Some(CallError::NoTool(name)) => (name.as_str(), ToolKind::Other),
            _ => (TOOL, ToolKind::Execute),
        };
        self.ids += 1;
        let id = format!("call_{}", self.ids);
        let arguments = call.arguments.clone();
        let head = head("", arguments.as_deref());
        let started = ToolCall::new(id.clone(), title)
            .kind(kind)
            .status(ToolCallStatus::Pending)
            .content(vec![head]);
        let opened = Opened {
            index,
            id,
            code: String::new(),
            arguments,
            ended: false,
        };
        self.calls.push(opened);
        self.update(SessionUpdate::ToolCall(started));
    }

    fn opened(&self, index: usize) -> Option<&Opened> {
        self.calls.iter().find(|call| call.index == index)
    }

    fn ran(&mut self, index: usize, ran: &Call) {
        self.running = None;
        let opened = self.calls.iter_mut().find(|call| call.index == index);
        let Some(opened) = opened else {
            return;
        };
        opened.ended = true;
        let id = opened.id.clone();

        let content = self.content(ran);
        let status = match ran.error {
            Some(_) => ToolCallStatus::Failed,
            None => ToolCallStatus::Completed,
        };
        let fields = ToolCallUpdateFields::new().status(status).content(content);
        let update = ToolCallUpdate::new(id, fields);
        self.update(SessionUpdate::ToolCallUpdate(update));
    }

    /// Shows the saved code, which already has stray tags stripped
    fn content(&self, call: &Call) -> Vec<ToolCallContent> {
        let head = head(&call.code, call.arguments.as_deref());
        let mut content = vec![head];
        let result = call.result.as_ref();
        if let Some(printed) = result.and_then(|result| result.printed.as_ref()) {
            let printed = ToolCallContent::from(printed);
            content.push(printed);
        }
        for (path, media) in result.map(|result| &result.media).into_iter().flatten() {
            let copy = self.copies.join(&media.file);
            let Some(uri) = file_uri(&copy) else {
                continue;
            };
            let mime = media.mime.clone();
            let link = ResourceLink::new(path.clone(), uri).mime_type(mime);
            let link = ContentBlock::ResourceLink(link);
            let link = ToolCallContent::from(link);
            content.push(link);
        }
        if let Some(error) = &call.error {
            let error = error.to_string();
            let error = ToolCallContent::from(error);
            content.push(error);
        }
        content
    }

    fn step_end(&mut self, step: &AgentStep) {
        let error = step.error.as_ref();

        // Calls that never ran close as failed and show the saved call as replay does
        let opened = mem::take(&mut self.calls);
        let reported = !opened.is_empty();
        for call in opened {
            if call.ended {
                continue;
            }
            let stored = step.calls.get(call.index);
            let mut content = match stored {
                Some(stored) => self.content(stored),
                None => vec![fenced(&call.code)],
            };
            let reason = match error {
                // Unreadable calls already carry their own reason
                _ if stored.is_some_and(Call::unread) => None,
                // A model error is reported once, by a notice or by the turn's error
                Some(StepError::Model(_)) => None,
                Some(error) => Some(error.to_string()),
                None => Some(SKIPPED.to_owned()),
            };
            if let Some(reason) = reason {
                let reason = ToolCallContent::from(reason);
                content.push(reason);
            }
            let fields = ToolCallUpdateFields::new()
                .status(ToolCallStatus::Failed)
                .content(content);
            let update = ToolCallUpdate::new(call.id, fields);
            self.update(SessionUpdate::ToolCallUpdate(update));
        }

        if let Some(error @ (StepError::Cut | StepError::Interrupted)) = error {
            if !reported {
                let text = error.to_string();
                self.notice(&text, None);
            }
        }

        // A request the model failed is not recorded and doesn't count as a step
        if let Some(StepError::Model(_)) = error {
            return self.withdraw();
        }
        self.progress = Progress::default();

        // Goal check usage only adds to the token totals
        let mut usage = step.usage;
        if let Some(check) = &step.check {
            usage.add(&check.usage);
        }
        let stopped = match (error, &step.unchecked) {
            (Some(StepError::Interrupted), _) | (_, Some(Unchecked::Interrupted)) => {
                Some("interrupted")
            }
            (_, Some(Unchecked::Failed(_))) => Some("failed"),
            _ => None,
        };
        self.report_usage(&usage, step.used, stopped);
    }

    /// Takes back the reported estimate of a step that was thrown away
    fn withdraw(&mut self) {
        let progress = mem::take(&mut self.progress);
        // Without metrics it doesn't count as a step
        if progress.reported > 0 {
            let update = UsageUpdate::new(self.used, self.window);
            self.update(SessionUpdate::UsageUpdate(update));
        }
    }

    fn compact_end(&mut self, result: Result<&SummaryStep, &StepError>) {
        let (status, text) = match result {
            Ok(summary) => (ToolCallStatus::Completed, summary.summary.clone()),
            // The step that follows reports the interrupt
            Err(StepError::Interrupted) => (ToolCallStatus::Failed, String::new()),
            Err(error) => (ToolCallStatus::Failed, error.to_string()),
        };
        self.think_end(status, &text);

        if let Ok(summary) = result {
            self.report_usage(&summary.usage, summary.used, None);
        }
    }

    fn check_end(&mut self, result: Result<&Check, &StepError>) {
        let (status, text) = match result {
            Ok(check) if check.verdict == Verdict::Blocked => {
                (ToolCallStatus::Failed, prompt::verdict(check))
            }
            Ok(check) => (ToolCallStatus::Completed, prompt::verdict(check)),
            Err(error) => (ToolCallStatus::Failed, error.to_string()),
        };
        self.think_end(status, &text);
    }

    fn update(&self, update: SessionUpdate) {
        self.peer.update(&self.session, update);
    }

    fn notice(&self, text: &str, retry_in: Option<Duration>) {
        let kind = Notice::Step(retry_in);
        self.peer.notice(&self.session, text, kind);
    }

    fn think(&mut self, title: &str) {
        self.ids += 1;
        let id = format!("call_{}", self.ids);
        let started = ToolCall::new(id.clone(), title)
            .kind(ToolKind::Think)
            .status(ToolCallStatus::InProgress);
        self.update(SessionUpdate::ToolCall(started));
        self.thinking = Some(id);
    }

    fn think_end(&mut self, status: ToolCallStatus, text: &str) {
        let Some(id) = self.thinking.take() else {
            return;
        };

        let mut content = Vec::new();
        if !text.is_empty() {
            let text = ToolCallContent::from(text);
            content.push(text);
        }
        let fields = ToolCallUpdateFields::new().status(status).content(content);
        let update = ToolCallUpdate::new(id, fields);
        self.update(SessionUpdate::ToolCallUpdate(update));
    }

    fn report_usage(&mut self, usage: &Usage, used: Option<u64>, stopped: Option<&str>) {
        if let Some(used) = used {
            self.used = used;
        }

        let mut meta = Meta::new();
        meta.insert("duration".to_owned(), usage.duration.into());
        if let Some(tokens) = usage.tokens {
            meta.insert("inputTokens".to_owned(), tokens.input_tokens.into());
            meta.insert("outputTokens".to_owned(), tokens.output_tokens.into());
        }
        if let Some(decode) = usage.decode_duration {
            meta.insert("decodeDuration".to_owned(), decode.into());
        }
        // Replay has no stop reason for the turn,
        // so interrupts and failed goal checks can only go on the step
        if let Some(stopped) = stopped {
            meta.insert(stopped.to_owned(), true.into());
        }

        let update = UsageUpdate::new(self.used, self.window).meta(meta);
        self.update(SessionUpdate::UsageUpdate(update));
    }
}

fn fenced(code: &str) -> ToolCallContent {
    let code = code.trim_matches('\n');
    let text = format!("```python\n{code}\n```");
    ToolCallContent::from(text)
}

/// The first block of a call, which is the raw arguments when the code is unreadable
fn head(code: &str, arguments: Option<&str>) -> ToolCallContent {
    match arguments {
        Some(arguments) if code.is_empty() => ToolCallContent::from(arguments.to_owned()),
        _ => fenced(code),
    }
}

fn chunk(text: &str) -> ContentChunk {
    let block = ContentBlock::from(text);
    ContentChunk::new(block)
}

/// The config directory may be a relative path
fn file_uri(path: &Path) -> Option<Url> {
    let absolute = path::absolute(path).ok()?;
    Url::from_file_path(absolute).ok()
}

#[cfg(test)]
mod tests {
    use crate::acp::rpc::{Lines, Out};
    use crate::acp::tests::drained;
    use crate::acp::translator::*;
    use crate::agent::tests::{called, reply, scripted, Script, ScriptedModel};
    use crate::memory::tests::{asked, coded, metered, replied};
    use crate::memory::CodeResult;
    use crate::model::tests::png;
    use crate::model::{Finish, ModelError};
    use serde_json::Value;
    use std::env;
    use tokio::sync::mpsc;

    fn translator(window: u64) -> (Translator, Lines) {
        let (lines, sent) = mpsc::unbounded_channel();
        let peer = Peer::new(Out::Lines(lines));
        let copies = env::temp_dir().join("copies");
        let translator = Translator::new("session-1".to_owned(), peer, Some(window), copies);
        (translator, sent)
    }

    /// The restart notice is the same live and on replay
    #[test]
    fn restart_noticed() {
        let (mut live, mut sent) = translator(0);
        live.translate(Event::Restarted);
        let told = drained(&mut sent);

        let memory = AgentMemory {
            steps: vec![MemoryStep::Restarted],
        };
        let (mut replaying, mut sent) = translator(0);
        replaying.replay(&memory);
        let replayed = drained(&mut sent);
        assert_eq!(told, replayed);
        let notice = json!({
            "sessionId": "session-1",
            "text": "python: restarted, earlier variables are gone",
            "retryIn": null,
            "step": true,
            "unsaved": false,
        });
        assert_eq!(told[0]["params"], notice);
    }

    #[test]
    fn output_streamed() {
        let (mut translator, mut sent) = translator(1000);
        translator.translate(Event::Code(0, "for i in range(3):\n    print('item', i)\n"));
        translator.translate(Event::Execute(0));
        translator.translate(Event::Output("item 0\nitem 1\n"));

        let messages = drained(&mut sent);
        let running = &messages[2]["params"]["update"];
        assert_eq!(running["sessionUpdate"], "tool_call_update");
        assert!(running.get("status").is_none(), "{running}");
        let content = &running["content"];
        let code = "```python\nfor i in range(3):\n    print('item', i)\n```";
        assert_eq!(content[0]["content"]["text"], code);
        assert_eq!(content[1]["content"]["text"], "item 0\nitem 1\n");
    }

    fn replayed(memory: &AgentMemory) -> Vec<Value> {
        let (mut translator, mut sent) = translator(1000);
        translator.replay(memory);

        let messages = drained(&mut sent);
        let updates = messages.iter().map(|message| &message["params"]["update"]);
        updates.cloned().collect()
    }

    /// What the client ends up showing, with adjacent text joined and each call in its final state
    fn shown(messages: &[Value]) -> Vec<Value> {
        let mut items: Vec<Value> = Vec::new();
        for message in messages {
            let update = &message["params"]["update"];
            let kind = &update["sessionUpdate"];
            let text = update["content"]["text"].as_str().unwrap_or_default();
            let id = &update["toolCallId"];
            match kind.as_str().unwrap_or_default() {
                "user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk" => {
                    let last = items.last_mut().filter(|last| last["kind"] == *kind);
                    let Some(last) = last else {
                        items.push(json!({ "kind": kind, "text": text }));
                        continue;
                    };
                    let earlier = last["text"].as_str().unwrap_or_default();
                    let joined = format!("{earlier}{text}");
                    last["text"] = json!(joined);
                }
                "tool_call" => items.push(update.clone()),
                "tool_call_update" => {
                    let Some(call) = items.iter_mut().rfind(|item| item["toolCallId"] == *id)
                    else {
                        continue;
                    };
                    for field in ["status", "content"] {
                        if let Some(value) = update.get(field) {
                            call[field] = value.clone();
                        }
                    }
                }
                _ => items.push(update.clone()),
            }
        }
        for item in &mut items {
            if let Some(text) = item["text"].as_str() {
                item["text"] = json!(text.trim());
            }
        }
        items
    }

    /// The only difference is that code is sent in one piece, for malformed and cut-off replies too
    #[test]
    fn replay_matches_live() {
        let (mut live, mut sent) = translator(1000);
        let cut = Script {
            calls: vec![Ok("print('half".to_owned())],
            finish: Finish::Length,
            ..reply("Let me check the résumé first.\n")
        };
        let started = Script {
            reasoning: Some("The résumé check was cut off, so start smaller.".to_owned()),
            calls: vec![Ok("x = 1\nprint(x)".to_owned())],
            ..reply("Starting with the naïve version.\n")
        };
        let several = Script {
            calls: vec![
                Ok("y = 2".to_owned()),
                Ok("1 / 0".to_owned()),
                Ok("print('not run')".to_owned()),
            ],
            ..reply("")
        };
        let unknown = Script {
            calls: vec![
                Ok("z = 3".to_owned()),
                Err(("bash", r#"{"command": "ls docs"}"#)),
                Ok("print('not run either')".to_owned()),
            ],
            ..reply("Listing the café menu.")
        };
        let done = Script {
            reasoning: Some("x was set before the error.".to_owned()),
            ..reply("Done, x is 1.")
        };
        let replies = vec![
            Ok(cut),
            Ok(started),
            Ok(called("1 / 0")),
            Ok(several),
            Ok(unknown),
            Ok(done),
        ];
        let model = ScriptedModel::new(replies);
        let events = Box::new(move |event: Event| live.translate(event));
        let mut agent = scripted(model, events);
        let mut memory = AgentMemory::default();
        agent.turn(&mut memory, "Task").expect("run");
        let live = drained(&mut sent);
        let mut thoughts = Vec::new();
        for item in shown(&live) {
            if item["kind"] == "agent_thought_chunk" {
                thoughts.push(item["text"].clone());
            }
        }
        let expected = [
            "The résumé check was cut off, so start smaller.",
            "x was set before the error.",
        ];
        assert_eq!(thoughts, expected);

        let (mut replaying, mut sent) = translator(1000);
        replaying.replay(&memory);
        let replayed = drained(&mut sent);
        assert_eq!(shown(&replayed)[1..], shown(&live));
        assert!(replayed.len() < live.len(), "{replayed:?}");
    }

    /// Text that arrives after a call also comes after it on replay
    #[test]
    fn text_after_calls() {
        let (mut live, mut sent) = translator(1000);
        live.translate(Event::Text("Running the café check first.\n"));
        live.translate(Event::Code(0, "print('café')\n"));
        live.translate(Event::Text("More once it has run.\n"));
        let call = Call {
            code: "print('café')".to_owned(),
            at: "Running the café check first.\n".len(),
            ..Call::default()
        };
        let step = AgentStep {
            text: "Running the café check first.\nMore once it has run.".to_owned(),
            calls: vec![call],
            error: Some(StepError::Interrupted),
            ..AgentStep::default()
        };
        live.translate(Event::StepEnd(&step));
        let told = drained(&mut sent);

        let memory = AgentMemory {
            steps: vec![MemoryStep::Action(Box::new(step))],
        };
        let (mut replaying, mut sent) = translator(1000);
        replaying.replay(&memory);
        let again = drained(&mut sent);
        assert_eq!(shown(&again), shown(&told));

        let shown = shown(&again);
        let mut kinds = Vec::new();
        for item in &shown {
            let kind = item.get("sessionUpdate").unwrap_or(&item["kind"]);
            kinds.push(kind.as_str().unwrap_or_default());
        }
        let order = [
            "agent_message_chunk",
            "tool_call",
            "agent_message_chunk",
            "usage_update",
        ];
        assert_eq!(kinds, order, "{shown:?}");
    }

    /// An unreadable call is titled with the tool it called
    /// and shows the raw arguments and the reason
    #[test]
    fn unread_shown() {
        let (mut live, mut sent) = translator(1000);
        let bash = Call {
            arguments: Some(r#"{"command": "ls docs"}"#.to_owned()),
            error: Some(CallError::NoTool("bash".to_owned())),
            ..Call::default()
        };
        live.translate(Event::Unread(0, &bash));
        live.translate(Event::Ran(0, &bash));
        let step = AgentStep {
            calls: vec![bash],
            ..AgentStep::default()
        };
        live.translate(Event::StepEnd(&step));
        let told = drained(&mut sent);

        let memory = AgentMemory {
            steps: vec![MemoryStep::Action(Box::new(step))],
        };
        let (mut replaying, mut sent) = translator(1000);
        replaying.replay(&memory);
        let again = drained(&mut sent);
        assert_eq!(shown(&again), shown(&told));

        let opened = &told[0]["params"]["update"];
        assert_eq!(opened["title"], "bash");
        let closed = &told[1]["params"]["update"];
        assert_eq!(closed["status"], "failed");
        let content = &closed["content"];
        assert_eq!(content[0]["content"]["text"], r#"{"command": "ls docs"}"#);
        assert_eq!(content[1]["content"]["text"], "bash: no such tool");
    }

    /// Each call gets its own entry, closed when it runs or else when the step ends
    #[test]
    fn calls_closed_apart() {
        let (mut translator, mut sent) = translator(1000);
        translator.translate(Event::Code(0, "1 / 0\n"));
        translator.translate(Event::Code(1, "print('second')\n"));
        translator.translate(Event::Execute(0));
        let failed = Call {
            code: "1 / 0".to_owned(),
            result: Some(CodeResult::default()),
            error: Some(CallError::Execution("ZeroDivisionError".to_owned())),
            ..Call::default()
        };
        translator.translate(Event::Ran(0, &failed));
        let skipped = Call {
            code: "print('second')".to_owned(),
            ..Call::default()
        };
        let step = AgentStep {
            calls: vec![failed, skipped],
            ..AgentStep::default()
        };
        translator.translate(Event::StepEnd(&step));

        let messages = drained(&mut sent);
        let updates: Vec<&Value> = messages
            .iter()
            .map(|message| &message["params"]["update"])
            .collect();
        let [first, second, running, ran, closed, usage] = updates[..] else {
            panic!("{updates:?}");
        };
        assert_eq!(
            (&first["toolCallId"], &second["toolCallId"]),
            (&json!("call_1"), &json!("call_2"))
        );
        assert_eq!(running["status"], "in_progress");
        assert_eq!(ran["toolCallId"], "call_1");
        assert_eq!(ran["status"], "failed");
        assert_eq!(ran["content"][1]["content"]["text"], "ZeroDivisionError");
        assert_eq!(closed["toolCallId"], "call_2");
        assert_eq!(closed["status"], "failed");
        let reason = &closed["content"][1]["content"]["text"];
        assert_eq!(reason, "skipped: an earlier call failed");
        assert_eq!(usage["sessionUpdate"], "usage_update");
    }

    #[test]
    fn summary_replayed() {
        let summary = SummaryStep {
            summary: "x is 41".to_owned(),
            used: Some(321),
            ..SummaryStep::default()
        };
        let memory = AgentMemory {
            steps: vec![MemoryStep::Summary(summary)],
        };
        let updates = replayed(&memory);
        let [started, ended, usage] = &updates[..] else {
            panic!("{updates:?}");
        };
        assert_eq!(started["title"], "Compact context");
        assert_eq!(ended["status"], "completed");
        assert_eq!(ended["content"][0]["content"]["text"], "x is 41");
        assert_eq!(usage["used"], 321);
    }

    #[test]
    fn shell_shown() {
        let (mut translator, mut sent) = translator(1000);
        translator.shell("ls docs");
        translator.shell_output("a.txt\n");
        let ran = ShellStep {
            command: "ls docs".to_owned(),
            shell: "fish".to_owned(),
            output: "a.txt\nb.txt".to_owned(),
            end: Exit::Exited(2),
        };
        translator.shell_end(&ran);

        let messages = drained(&mut sent);
        let updates: Vec<&Value> = messages
            .iter()
            .map(|message| &message["params"]["update"])
            .collect();
        let [started, running, ended] = updates[..] else {
            panic!("{updates:?}");
        };
        assert_eq!(started["title"], "ls docs");
        assert_eq!(started["kind"], "execute");
        assert_eq!(started["_meta"]["_smol/shell"], true);
        assert_eq!(running["content"][0]["content"]["text"], "a.txt\n");
        assert_eq!(ended["status"], "failed");
        assert_eq!(ended["content"][0]["content"]["text"], "a.txt\nb.txt");
        assert_eq!(ended["_meta"]["error"], "exit 2");

        // Replay sends the same start and end, and an interrupted command carries an extra marker
        let stopped = ShellStep {
            end: Exit::Interrupted,
            ..ran.clone()
        };
        let steps = vec![MemoryStep::Shell(ran), MemoryStep::Shell(stopped)];
        let memory = AgentMemory { steps };
        let replayed = replayed(&memory);
        assert_eq!(replayed[..2], [started.clone(), ended.clone()]);
        let meta = json!({ "error": "interrupted", "interrupted": true });
        assert_eq!(replayed[3]["_meta"], meta);
    }

    #[test]
    fn media_linked() {
        let media = png("a1.png");
        let mut viewed = coded("view('screenshot.png')", Some("Viewed".to_owned()));
        let result = viewed.calls[0].result.as_mut().expect("result");
        result.media.push(("screenshot.png".to_owned(), media));
        let memory = AgentMemory {
            steps: vec![MemoryStep::Action(Box::new(viewed))],
        };

        let updates = replayed(&memory);
        assert_eq!(updates[0]["title"], "eval");
        let ended = updates
            .iter()
            .rfind(|update| update["status"] == "completed");
        let content = &ended.expect("ended")["content"];
        assert_eq!(content[1]["content"]["text"], "Viewed", "{content}");

        let link = &content[2]["content"];
        assert_eq!(link["type"], "resource_link");
        assert_eq!(
            (&link["name"], &link["mimeType"]),
            (&json!("screenshot.png"), &json!("image/png"))
        );
        let uri = link["uri"].as_str().and_then(|uri| Url::parse(uri).ok());
        let copy = env::temp_dir().join("copies").join("a1.png");
        assert_eq!(uri.and_then(|uri| uri.to_file_path().ok()), Some(copy));
        assert_eq!(content.as_array().map(Vec::len), Some(3));
    }

    #[test]
    fn ran_code_shown() {
        let (mut translator, mut sent) = translator(1000);
        let written = "<parameter name=\"code\">\nprint('seven')\n</parameter>\n";
        translator.translate(Event::Code(0, written));
        let step = coded("print('seven')", Some("seven".to_owned()));
        translator.translate(Event::Ran(0, &step.calls[0]));
        translator.translate(Event::StepEnd(&step));

        let mut completed = Value::Null;
        for message in drained(&mut sent) {
            let update = &message["params"]["update"];
            if update["status"] == "completed" {
                completed = update.clone();
            }
        }
        let shown = &completed["content"][0]["content"]["text"];
        assert_eq!(shown, "```python\nprint('seven')\n```");
        assert!(completed.get("rawInput").is_none(), "{completed}");
    }

    #[test]
    fn output_estimated() {
        let (mut translator, mut sent) = translator(1000);
        translator.used = 100;
        let mut usages = || {
            let mut found = Vec::new();
            for message in drained(&mut sent) {
                let update = &message["params"]["update"];
                if update["sessionUpdate"] == "usage_update" {
                    found.push(update.clone());
                }
            }
            found
        };
        let earlier = Instant::now().checked_sub(ESTIMATE_EVERY);
        let earlier = earlier.expect("instant");

        translator.translate(Event::Thought("Let me think"));
        assert!(usages().is_empty());
        translator.progress.since = Some(earlier);
        translator.translate(Event::Text("Found 42 résumés"));
        translator.translate(Event::Code(0, "pass()\n"));
        let estimated = json!({
            "sessionUpdate": "usage_update",
            "used": 111,
            "size": 1000,
            "_meta": { "estimatedOutput": 11 },
        });
        assert_eq!(usages(), [estimated]);

        // The step being retried is thrown away
        let retried = Event::Retry {
            reason: "model: connection dropped",
            wait: Duration::from_secs(1),
        };
        translator.translate(retried);
        let restored = json!({ "sessionUpdate": "usage_update", "used": 100, "size": 1000 });
        assert_eq!(usages(), [restored]);

        let replayed = replied("Thinking again");
        let memory = AgentMemory {
            steps: vec![MemoryStep::Action(Box::new(replayed))],
        };
        translator.progress.since = Some(earlier);
        translator.replay(&memory);
        let ended = usages();
        assert_eq!(ended.len(), 1, "{ended:?}");
        assert_eq!(ended[0]["_meta"]["estimatedOutput"], Value::Null);
    }

    #[test]
    fn check_retried() {
        let (mut translator, mut sent) = translator(1000);
        translator.used = 100;
        let earlier = Instant::now().checked_sub(ESTIMATE_EVERY);
        translator.progress.since = Some(earlier.expect("instant"));
        translator.translate(Event::Text("Wrote the café notes."));

        translator.translate(Event::Check);
        let busy = ModelError::transient("busy");
        let busy = StepError::Model(busy);
        let reason = busy.to_string();
        let wait = Duration::from_secs(2);
        let retried = Event::Retry {
            reason: &reason,
            wait,
        };
        translator.translate(retried);
        let check = Check {
            verdict: Verdict::Blocked,
            reason: "needs the database password".to_owned(),
            usage: Usage::default(),
        };
        translator.translate(Event::CheckEnd(Ok(&check)));
        let answered = AgentStep {
            usage: metered(100, 30),
            used: Some(130),
            ..replied("Wrote the café notes.")
        };
        translator.translate(Event::StepEnd(&answered));

        // The answer's estimate stands until the step ends because only the check was retried
        let messages = drained(&mut sent);
        assert_eq!(messages.len(), 6, "{messages:?}");
        let estimated = json!({
            "sessionUpdate": "usage_update",
            "used": 107,
            "size": 1000,
            "_meta": { "estimatedOutput": 7 },
        });
        assert_eq!(messages[0]["params"]["update"], estimated);
        let started = &messages[2]["params"]["update"];
        assert_eq!(
            (&started["title"], &started["status"]),
            (&json!("Check goal"), &json!("in_progress"))
        );
        let notice = json!({
            "sessionId": "session-1",
            "text": "model: busy",
            "retryIn": 2.0,
            "step": true,
            "unsaved": false,
        });
        assert_eq!(messages[3]["params"], notice);
        let ended = &messages[4]["params"]["update"];
        assert_eq!(ended["status"], "failed");
        let text = &ended["content"][0]["content"]["text"];
        assert_eq!(text, "blocked: needs the database password");
        let usage = &messages[5]["params"]["update"];
        assert_eq!(usage["used"], 130, "{usage}");
    }

    /// A step the model failed doesn't count, so it sends no usage
    #[test]
    fn failed_step_quiet() {
        let (mut translator, mut sent) = translator(1000);
        let busy = ModelError::transient("busy");
        let failed = AgentStep {
            error: Some(StepError::Model(busy)),
            usage: metered(100, 10),
            ..AgentStep::default()
        };
        translator.translate(Event::StepEnd(&failed));

        let messages = drained(&mut sent);
        assert!(messages.is_empty(), "{messages:?}");
    }

    #[test]
    fn retry_fails_calls() {
        let (mut translator, mut sent) = translator(1000);
        translator.translate(Event::Code(0, "print('café')\n"));
        let retried = Event::Retry {
            reason: "model: connection dropped",
            wait: Duration::from_secs(1),
        };
        translator.translate(retried);

        let messages = drained(&mut sent);
        assert_eq!(messages.len(), 3, "{messages:?}");
        let closed = &messages[1]["params"]["update"];
        assert_eq!(
            (&closed["toolCallId"], &closed["status"]),
            (&json!("call_1"), &json!("failed"))
        );
        let code = &closed["content"][0]["content"]["text"];
        assert_eq!(code, "```python\nprint('café')\n```");
        assert_eq!(messages[2]["params"]["text"], "model: connection dropped");
    }

    #[test]
    fn goal_replayed() {
        let task = TaskStep {
            carried: vec!["Add tests too".to_owned()],
            ..asked("Fix the parser", true)
        };
        let check = Check {
            verdict: Verdict::Unmet,
            reason: "the tests were not run".to_owned(),
            usage: metered(50, 5),
        };
        let answered = AgentStep {
            usage: metered(100, 20),
            used: Some(120),
            check: Some(check),
            ..replied("Fixed.")
        };
        let memory = AgentMemory {
            steps: vec![
                MemoryStep::Task(task),
                MemoryStep::Action(Box::new(answered)),
            ],
        };
        let updates = replayed(&memory);
        let kinds: Vec<&Value> = updates
            .iter()
            .map(|update| &update["sessionUpdate"])
            .collect();
        let expected = [
            "user_message_chunk",
            "agent_message_chunk",
            "tool_call",
            "tool_call_update",
            "usage_update",
        ];
        assert_eq!(kinds, expected, "{updates:?}");
        assert_eq!(updates[0]["_meta"]["_smol/goal"], true);
        assert_eq!(updates[0]["content"]["text"], "Fix the parser");

        let checking = &updates[2];
        assert_eq!(checking["title"], "Check goal");
        assert_eq!(
            (&checking["kind"], &checking["status"]),
            (&json!("think"), &json!("in_progress"))
        );
        let checked = &updates[3];
        assert_eq!(checked["status"], "completed");
        let text = &checked["content"][0]["content"]["text"];
        assert_eq!(text, "not met: the tests were not run");

        let usage = &updates[4];
        assert_eq!(usage["used"], 120);
        assert_eq!(usage["_meta"]["inputTokens"], 150);
        assert_eq!(usage["_meta"]["outputTokens"], 25);
    }

    /// The client keeps user chunks with the same message id in one message
    #[test]
    fn tasks_replayed() {
        let memory = AgentMemory {
            steps: vec![
                MemoryStep::Task(asked("Fix the café parser", false)),
                MemoryStep::Update(asked("Add tests too", false)),
            ],
        };
        let updates = replayed(&memory);
        let [task, update] = &updates[..] else {
            panic!("{updates:?}");
        };
        assert_eq!(task["messageId"], "task_1");
        assert!(task.get("_meta").is_none(), "{task}");
        assert_eq!(update["messageId"], "task_2");
        assert_eq!(update["_meta"], json!({ "_smol/update": true }));
        assert_eq!(update["content"]["text"], "Add tests too");
    }

    /// A step whose goal check reached no verdict replays as it ran live,
    /// with usage marked by how the turn stopped
    #[test]
    fn unchecked_replayed() {
        let broken = ModelError::new("http 500: internal error");
        let broken = StepError::Model(broken);
        let said = broken.to_string();
        let failed = Unchecked::Failed(said);
        let cases = [
            (broken, failed, "failed"),
            (
                StepError::Interrupted,
                Unchecked::Interrupted,
                "interrupted",
            ),
        ];
        for (error, unchecked, stopped) in cases {
            let answered = AgentStep {
                usage: metered(100, 10),
                unchecked: Some(unchecked),
                ..replied("Wrote notes.txt.")
            };
            let (mut live, mut sent) = translator(1000);
            live.translate(Event::Text("Wrote notes.txt."));
            live.translate(Event::Check);
            live.translate(Event::CheckEnd(Err(&error)));
            live.translate(Event::StepEnd(&answered));
            let told = drained(&mut sent);

            let memory = AgentMemory {
                steps: vec![MemoryStep::Action(Box::new(answered))],
            };
            let (mut replaying, mut sent) = translator(1000);
            replaying.replay(&memory);
            let again = drained(&mut sent);
            assert_eq!(shown(&again), shown(&told));

            let checking = &again[1]["params"]["update"];
            assert_eq!(checking["title"], "Check goal");
            let checked = &again[2]["params"]["update"];
            assert_eq!(checked["status"], "failed");
            let text = &checked["content"][0]["content"]["text"];
            let error = error.to_string();
            assert_eq!(text, error.as_str());
            let usage = &again[3]["params"]["update"];
            assert_eq!(usage["_meta"][stopped], true, "{usage}");
        }
    }

    #[test]
    fn interrupt_marked() {
        let (mut translator, mut sent) = translator(1000);
        let cut = AgentStep {
            error: Some(StepError::Interrupted),
            ..AgentStep::default()
        };
        translator.translate(Event::StepEnd(&cut));

        let messages = drained(&mut sent);
        let notice = json!({
            "sessionId": "session-1",
            "text": "interrupted",
            "retryIn": null,
            "step": true,
            "unsaved": false,
        });
        assert_eq!(messages[0]["params"], notice);
        let usage = &messages[1]["params"]["update"];
        assert_eq!(usage["_meta"]["interrupted"], true, "{usage}");
    }

    #[test]
    fn compact_interrupt_quiet() {
        let (mut translator, mut sent) = translator(1000);
        translator.translate(Event::Compact);
        let stopped = StepError::Interrupted;
        translator.translate(Event::CompactEnd(Err(&stopped)));

        let messages = drained(&mut sent);
        assert_eq!(messages.len(), 2, "{messages:?}");
        let updates: Vec<&Value> = messages
            .iter()
            .map(|message| &message["params"]["update"])
            .collect();
        assert_eq!(updates[1]["status"], "failed");
        assert_eq!(updates[1]["content"], json!([]));
    }

    #[test]
    fn file_uris() {
        let copy = env::temp_dir().join("copies").join("café 1.png");
        let uri = file_uri(&copy).expect("uri");
        assert_eq!(uri.to_file_path().ok(), Some(copy));
        assert!(uri.as_str().ends_with("/copies/caf%C3%A9%201.png"), "{uri}");

        let relative = file_uri(Path::new("copies/café.png")).expect("relative");
        let cwd = env::current_dir().expect("cwd");
        assert_eq!(
            relative.to_file_path().ok(),
            Some(cwd.join("copies/café.png"))
        );
    }
}
