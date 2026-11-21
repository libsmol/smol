use crate::acp::rpc::{Notice, SessionPeer};
use crate::agent::{Event, Failure, Status, TaskView, TaskViews, Unseen};
use crate::prompt::{self, SKIPPED, TOOL};
use crate::steps::{
    estimate, spent, text_bytes, AgentMemory, AgentRecord, AgentStep, Call, CallError, DoneStep,
    Ending, Exit, MemoryStep, ShellStep, StepError, SummaryStep, TaskStep, Usage,
};
use agent_client_protocol_schema::v1::{
    ContentBlock, ContentChunk, MessageId, Meta, ResourceLink, SessionUpdate, ToolCall,
    ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind, UsageUpdate,
};
use reaper::one_line;
use reaper::wire::{
    Measured, Program, AGENT, BACKGROUND, ERROR, NAME, PARENT, SHELL, TASK, UPDATE, WAITING,
};
use reqwest::Url;
use serde_json::{json, Value};
use std::mem;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const ESTIMATE_EVERY: Duration = Duration::from_secs(1);
const MAX_TASK_CHARS: usize = 80;
const COMPACTING: &str = "Compact context";
const RESTARTED: &str = "python: restarted, earlier variables are gone";

pub(crate) struct Translator {
    peer: SessionPeer,
    ids: usize,
    tasks: usize,
    window: u64,
    used: u64,
    calls: Vec<Opened>,
    /// The open compaction call
    compacting: Option<String>,
    /// The programs the client last heard are running
    programs: Vec<Program>,
    progress: Progress,
    cwd: PathBuf,
    copies: PathBuf,
}

struct Opened {
    index: usize,
    id: String,
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
        peer: SessionPeer,
        window: Option<u64>,
        cwd: PathBuf,
        copies: PathBuf,
    ) -> Self {
        Self {
            peer,
            ids: 0,
            tasks: 0,
            window: window.unwrap_or(0),
            used: 0,
            calls: Vec::new(),
            compacting: None,
            programs: Vec::new(),
            progress: Progress::default(),
            cwd,
            copies,
        }
    }

    /// `agents` are the session's, whose tasks the calls that sent them show
    pub(crate) fn replay(&mut self, memory: &AgentMemory, agents: &[AgentRecord]) {
        for step in &memory.steps {
            match step {
                MemoryStep::Task(task) => self.task(task, false),
                MemoryStep::Update(update) => self.task(update, true),
                // Each call's code goes out in one piece
                MemoryStep::Action(step) => {
                    let reasoning = step.reasoning.as_deref().unwrap_or_default();
                    if !reasoning.is_empty() {
                        let chunk = chunk(reasoning);
                        self.peer.update(SessionUpdate::AgentThoughtChunk(chunk));
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

                        let result = call.result.as_ref();
                        let sent = result.map(|result| result.agents.as_slice());
                        let views = self.tasks(index);
                        for (offset, sent) in sent.unwrap_or_default().iter().enumerate() {
                            let found = agents.iter().find(|agent| agent.name == sent.name);
                            let Some(agent) = found else {
                                continue;
                            };
                            let Some(task) = agent.tasks.get(sent.number) else {
                                continue;
                            };
                            let mut view = views.open(offset + 1, &agent.name, &task.task);
                            let steps = agent.steps(sent.number);
                            view.close(&task.end, steps);
                        }
                        if call.result.is_some() || call.error.is_some() {
                            self.ran(index, call);
                        }
                    }
                    let rest = step.text.get(said..).unwrap_or_default();
                    self.said(rest);
                    self.step_end(step);
                }
                MemoryStep::Summary(summary) => {
                    self.compact();
                    self.compact_end(Ok(summary));
                }
                MemoryStep::Shell(shell) => {
                    let id = self.shell(&shell.command);
                    self.shell_end(id, shell);
                }
                MemoryStep::Restarted => self.notice(RESTARTED, None),
                MemoryStep::Done(done) if done.idle => self.ended(done),
                // As live, programs that end within a turn show nothing and agents showed their end
                MemoryStep::Done(_) => {}
            }
        }
    }

    pub(crate) fn translate(&mut self, event: Event) {
        match event {
            Event::Thought(text) => {
                self.streamed(text.added);
                let chunk = chunk(text.added);
                self.peer.update(SessionUpdate::AgentThoughtChunk(chunk));
            }
            Event::Text(text) => {
                self.streamed(text.added);
                self.said(text.added);
            }
            Event::Code(index, code) => {
                self.streamed(code.added);
                self.code(index, code.whole);
            }
            Event::Unread(index, call) => self.unread(index, call),
            Event::Run(index, _) => {
                if let Some(opened) = self.opened(index) {
                    let fields = ToolCallUpdateFields::new().status(ToolCallStatus::InProgress);
                    let update = ToolCallUpdate::new(opened.id.clone(), fields);
                    self.peer.update(SessionUpdate::ToolCallUpdate(update));
                }
            }
            Event::Output(index, call, output) => {
                if let Some(opened) = self.opened(index) {
                    let code = fenced(&call.code);
                    let output = ToolCallContent::from(output);
                    let content = vec![code, output];
                    let fields = ToolCallUpdateFields::new().content(content);
                    let update = ToolCallUpdate::new(opened.id.clone(), fields);
                    self.peer.update(SessionUpdate::ToolCallUpdate(update));
                }
            }
            Event::RunEnd(index, call) => self.ran(index, call),
            Event::Restarted => self.notice(RESTARTED, None),
            Event::Retry { reason, wait } => {
                for opened in mem::take(&mut self.calls) {
                    let fields = ToolCallUpdateFields::new().status(ToolCallStatus::Failed);
                    let update = ToolCallUpdate::new(opened.id, fields);
                    self.peer.update(SessionUpdate::ToolCallUpdate(update));
                }
                self.notice(reason, Some(wait));
                self.withdraw();
            }
            Event::StepEnd(step) => self.step_end(step),
            // The model error is reported once, by a notice or by the turn's error
            Event::StepFailed(step) => {
                self.close_unrun(step, None);
                self.withdraw();
            }
            Event::Compact => self.compact(),
            Event::CompactEnd(result) => self.compact_end(result),
            Event::Programs(running) => self.programs(running),
        }
    }

    /// Tells the client only of a change
    pub(crate) fn programs(&mut self, running: &[Program]) {
        if self.programs != running {
            self.programs = running.to_vec();
            self.peer.programs(running);
        }
    }

    /// Without metrics the client doesn't count it as a step
    pub(crate) fn model_changed(&mut self, window: Option<u64>) {
        self.window = window.unwrap_or(0);
        self.used = 0;
        let update = UsageUpdate::new(0, self.window);
        self.peer.update(SessionUpdate::UsageUpdate(update));
    }

    /// Returns the id of the call that shows the command
    pub(crate) fn shell(&mut self, command: &str) -> String {
        let id = self.next_id();
        let marked = (SHELL.to_owned(), json!(true));
        let meta = Meta::from_iter([marked]);
        let started = ToolCall::new(id.clone(), command)
            .kind(ToolKind::Execute)
            .status(ToolCallStatus::InProgress)
            .meta(meta);
        self.peer.update(SessionUpdate::ToolCall(started));
        id
    }

    /// `output` is all the clipped output so far
    pub(crate) fn shell_output(&self, id: &str, output: &str) {
        let content = vec![ToolCallContent::from(output)];
        let fields = ToolCallUpdateFields::new().content(content);
        let update = ToolCallUpdate::new(id.to_owned(), fields);
        self.peer.update(SessionUpdate::ToolCallUpdate(update));
    }

    pub(crate) fn shell_end(&self, id: String, shell: &ShellStep) {
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
            // The client recognizes interrupts as it does on usage updates
            let measured = Measured {
                interrupted: shell.end == Exit::Interrupted,
                ..Measured::default()
            };
            let mut meta = meta_of(&measured);
            meta.insert(ERROR.to_owned(), json!(shell.end.to_string()));
            update = update.meta(meta);
        }
        self.peer.update(SessionUpdate::ToolCallUpdate(update));
    }

    /// Shows a client that declared `BACKGROUND` the programs that ended while no turn ran
    pub(crate) fn ended(&mut self, done: &DoneStep) {
        if !self.peer.declared().background {
            return;
        }

        self.tasks += 1;
        let id = format!("task_{}", self.tasks);
        let id = MessageId::new(id);
        let lines: Vec<String> = done.programs.iter().map(prompt::program).collect();
        let text = lines.join("\n");
        let flag = (BACKGROUND.to_owned(), json!(true));
        let meta = Meta::from_iter([flag]);
        let chunk = chunk(&text).message_id(id).meta(meta);
        self.peer.update(SessionUpdate::UserMessageChunk(chunk));
    }

    fn task(&mut self, asked: &TaskStep, appended: bool) {
        self.tasks += 1;
        let id = format!("task_{}", self.tasks);
        let id = MessageId::new(id);
        let mut chunk = chunk(&asked.text).message_id(id);
        if appended {
            let flag = (UPDATE.to_owned(), json!(true));
            let meta = Meta::from_iter([flag]);
            chunk = chunk.meta(meta);
        }
        self.peer.update(SessionUpdate::UserMessageChunk(chunk));
    }

    fn said(&self, text: &str) {
        if !text.is_empty() {
            let chunk = chunk(text);
            self.peer.update(SessionUpdate::AgentMessageChunk(chunk));
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
            let measured = Measured {
                estimated_output: Some(output),
                ..Measured::default()
            };
            let meta = meta_of(&measured);
            let update = UsageUpdate::new(used, self.window).meta(meta);
            self.peer.update(SessionUpdate::UsageUpdate(update));
        }
    }

    /// `written` is all of the code so far, and the title carries it too,
    /// since clients such as Zed show it as the command that runs
    fn code(&mut self, index: usize, written: &str) {
        let title = written.trim_matches('\n');
        let code = fenced(written);
        let content = vec![code];
        let update = match self.opened(index) {
            Some(opened) => {
                let fields = ToolCallUpdateFields::new().title(title).content(content);
                let update = ToolCallUpdate::new(opened.id.clone(), fields);
                SessionUpdate::ToolCallUpdate(update)
            }
            None => {
                let id = self.next_id();
                let started = ToolCall::new(id.clone(), title)
                    .kind(ToolKind::Execute)
                    .status(ToolCallStatus::Pending)
                    .content(content);
                let opened = Opened {
                    index,
                    id,
                    ended: false,
                };
                self.calls.push(opened);
                SessionUpdate::ToolCall(started)
            }
        };
        self.peer.update(update);
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
        let id = self.next_id();
        let head = head("", call.arguments.as_deref());
        let started = ToolCall::new(id.clone(), title)
            .kind(kind)
            .status(ToolCallStatus::Pending)
            .content(vec![head]);
        let opened = Opened {
            index,
            id,
            ended: false,
        };
        self.calls.push(opened);
        self.peer.update(SessionUpdate::ToolCall(started));
    }

    fn next_id(&mut self) -> String {
        self.ids += 1;
        format!("call_{}", self.ids)
    }

    fn opened(&self, index: usize) -> Option<&Opened> {
        self.calls.iter().find(|call| call.index == index)
    }

    /// A call that was never opened shows none of the agents it starts
    pub(crate) fn tasks(&self, index: usize) -> Box<dyn TaskViews> {
        let Some(opened) = self.opened(index) else {
            return Box::new(Unseen);
        };

        Box::new(TaskCalls {
            peer: self.peer.clone(),
            parent: opened.id.clone(),
            cwd: self.cwd.clone(),
        })
    }

    fn ran(&mut self, index: usize, ran: &Call) {
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
        self.peer.update(SessionUpdate::ToolCallUpdate(update));
    }

    /// Shows the saved code, which already has stray tags stripped
    fn content(&self, call: &Call) -> Vec<ToolCallContent> {
        let head = head(&call.code, call.arguments.as_deref());
        let mut content = vec![head];
        if let Some(printed) = call.output() {
            let printed = ToolCallContent::from(printed);
            content.push(printed);
        }
        let result = call.result.as_ref();
        for (path, media) in result.map(|result| &result.media).into_iter().flatten() {
            let copy = self.copies.join(&media.file);
            let Ok(uri) = Url::from_file_path(&copy) else {
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
        let reported = !self.calls.is_empty();
        let reason = match error {
            Some(error) => error.to_string(),
            None => SKIPPED.to_owned(),
        };
        self.close_unrun(step, Some(&reason));

        if error.is_some() && !reported {
            self.notice(&reason, None);
        }
        self.progress = Progress::default();

        let interrupted = error == Some(&StepError::Interrupted);
        self.report_usage(&step.usage, step.used, interrupted);
    }

    /// Closes the calls that never ran as failed, showing the saved call as replay does
    /// and `reason` unless the call carries its own
    fn close_unrun(&mut self, step: &AgentStep, reason: Option<&str>) {
        for call in mem::take(&mut self.calls) {
            if call.ended {
                continue;
            }
            let stored = step.calls.get(call.index);
            let mut content = match stored {
                Some(stored) => self.content(stored),
                None => Vec::new(),
            };
            let unread = stored.is_some_and(Call::unread);
            if let Some(reason) = reason.filter(|_| !unread) {
                let reason = ToolCallContent::from(reason);
                content.push(reason);
            }
            let fields = ToolCallUpdateFields::new()
                .status(ToolCallStatus::Failed)
                .content(content);
            let update = ToolCallUpdate::new(call.id, fields);
            self.peer.update(SessionUpdate::ToolCallUpdate(update));
        }
    }

    /// Takes back the reported estimate of a step that was thrown away
    fn withdraw(&mut self) {
        let progress = mem::take(&mut self.progress);
        // Without metrics it doesn't count as a step
        if progress.reported > 0 {
            let update = UsageUpdate::new(self.used, self.window);
            self.peer.update(SessionUpdate::UsageUpdate(update));
        }
    }

    fn compact(&mut self) {
        let id = self.next_id();
        let started = ToolCall::new(id.clone(), COMPACTING)
            .kind(ToolKind::Think)
            .status(ToolCallStatus::InProgress);
        self.peer.update(SessionUpdate::ToolCall(started));
        self.compacting = Some(id);
    }

    fn compact_end(&mut self, result: Result<&SummaryStep, &Failure>) {
        let (status, text) = match result {
            Ok(summary) => (ToolCallStatus::Completed, summary.summary.clone()),
            // The step that follows reports the interrupt
            Err(Failure::Interrupted) => (ToolCallStatus::Failed, String::new()),
            Err(error) => (ToolCallStatus::Failed, error.to_string()),
        };
        if let Some(id) = self.compacting.take() {
            let mut content = Vec::new();
            if !text.is_empty() {
                let text = ToolCallContent::from(text);
                content.push(text);
            }
            let fields = ToolCallUpdateFields::new().status(status).content(content);
            let update = ToolCallUpdate::new(id, fields);
            self.peer.update(SessionUpdate::ToolCallUpdate(update));
        }

        if let Ok(summary) = result {
            self.report_usage(&summary.usage, summary.used, false);
        }
    }

    fn notice(&self, text: &str, retry_in: Option<Duration>) {
        let kind = Notice::Step(retry_in);
        self.peer.notice(text, kind);
    }

    fn report_usage(&mut self, usage: &Usage, used: Option<u64>, interrupted: bool) {
        if let Some(used) = used {
            self.used = used;
        }

        // Replay has no stop reason for the turn, so interrupts can only go on the step
        let measured = Measured {
            interrupted,
            ..measured(usage)
        };
        let meta = meta_of(&measured);
        let update = UsageUpdate::new(self.used, self.window).meta(meta);
        self.peer.update(SessionUpdate::UsageUpdate(update));
    }
}

/// What steps and agents report of their usage
fn measured(usage: &Usage) -> Measured {
    let tokens = usage.tokens;
    Measured {
        duration: Some(usage.duration),
        decode_duration: usage.decode_duration,
        input_tokens: tokens.map(|tokens| tokens.input_tokens),
        output_tokens: tokens.map(|tokens| tokens.output_tokens),
        ..Measured::default()
    }
}

fn meta_of(measured: &Measured) -> Meta {
    let value = serde_json::to_value(measured).expect("serialize");
    match value {
        Value::Object(meta) => meta,
        _ => Meta::new(),
    }
}

struct TaskCalls {
    peer: SessionPeer,
    parent: String,
    cwd: PathBuf,
}

struct TaskCall {
    peer: SessionPeer,
    id: String,
    started: bool,
    /// Agents still running when their snippet ends are aborted before they close
    closed: bool,
}

impl TaskViews for TaskCalls {
    fn open(&self, number: usize, name: &str, task: &str) -> Box<dyn TaskView> {
        let id = format!("{}.{number}", self.parent);
        // The start of the task on one line
        let line = one_line(task, &self.cwd);
        let mut title: String = line.chars().take(MAX_TASK_CHARS).collect();
        if title.len() < line.len() {
            title.push('…');
        }
        let title = format!("agent: {title}");
        let asked = json!({ TASK: task, NAME: name });
        let marked = (AGENT.to_owned(), json!(true));
        let parent = (PARENT.to_owned(), json!(self.parent));
        let meta = Meta::from_iter([marked, parent]);
        let opened = ToolCall::new(id.clone(), title)
            .kind(ToolKind::Other)
            .status(ToolCallStatus::Pending)
            .raw_input(asked)
            .meta(meta);
        self.peer.update(SessionUpdate::ToolCall(opened));

        let view = TaskCall {
            peer: self.peer.clone(),
            id,
            started: false,
            closed: false,
        };
        Box::new(view)
    }

    fn waiting(&self, names: &[String]) {
        let fields = ToolCallUpdateFields::new();
        let waiting = (WAITING.to_owned(), json!(names));
        let meta = Meta::from_iter([waiting]);
        let update = ToolCallUpdate::new(self.parent.clone(), fields).meta(meta);
        self.peer.update(SessionUpdate::ToolCallUpdate(update));
    }
}

impl TaskCall {
    fn send(&self, fields: ToolCallUpdateFields, meta: Meta) {
        let update = ToolCallUpdate::new(self.id.clone(), fields).meta(meta);
        self.peer.update(SessionUpdate::ToolCallUpdate(update));
    }

    /// Replay closes it right after opening it, and an aborted one ends as its saved record does,
    /// so every way it ends sends the same last update
    fn finish(&mut self, end: &Ending, steps: &[MemoryStep]) {
        let (status, text) = match end.outcome() {
            Ok(text) => (ToolCallStatus::Completed, text),
            Err(text) => (ToolCallStatus::Failed, text),
        };

        let (usage, steps) = spent(steps);
        // The client recognizes interrupts as it does on usage updates
        let measured = Measured {
            steps: Some(steps as u64),
            interrupted: matches!(end, Ending::Interrupted {} | Ending::Stopped {}),
            ..measured(&usage)
        };
        let meta = meta_of(&measured);

        let content = vec![ToolCallContent::from(text)];
        let fields = ToolCallUpdateFields::new().status(status).content(content);
        self.send(fields, meta);
        self.closed = true;
    }
}

impl TaskView for TaskCall {
    fn shown(&mut self, status: &Status) {
        let mut content = vec![ToolCallContent::from(status.doing.clone())];
        if !status.preview.is_empty() {
            content.push(ToolCallContent::from(status.preview.clone()));
        }
        let mut fields = ToolCallUpdateFields::new().content(content);
        if !self.started {
            fields = fields.status(ToolCallStatus::InProgress);
        }
        self.started = true;

        let measured = Measured {
            steps: Some(status.steps as u64),
            ..measured(&status.usage)
        };
        let meta = meta_of(&measured);
        self.send(fields, meta);
    }

    fn close(&mut self, end: &Ending, steps: &[MemoryStep]) {
        self.finish(end, steps);
    }
}

impl Drop for TaskCall {
    fn drop(&mut self) {
        if self.closed {
            return;
        }

        self.finish(&Ending::Unfinished {}, &[]);
    }
}

/// A fence longer than every run of backticks in the code keeps the code in one block
fn fenced(code: &str) -> ToolCallContent {
    let code = code.trim_matches('\n');
    let longest = code.split(|c| c != '`').map(str::len).max();
    let fence = "`".repeat(longest.unwrap_or(0).max(2) + 1);
    let text = format!("{fence}python\n{code}\n{fence}");
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

#[cfg(test)]
mod tests {
    use crate::acp::rpc::{Lines, Out, Peer};
    use crate::acp::tests::drained;
    use crate::acp::translator::*;
    use crate::agent::tests::{called, reply, scripted, streamed, Script, ScriptedModel};
    use crate::model::Finish;
    use crate::steps::tests::{asked, metered, replied};
    use crate::steps::{AgentTask, CodeResult, Sent};
    use serde_json::Value;
    use std::env;
    use tokio::sync::mpsc;

    fn translator(window: u64) -> (Translator, Lines) {
        let (lines, sent) = mpsc::unbounded_channel();
        let peer = Peer::new(Out::Lines(lines));
        let peer = SessionPeer::new(peer, "session-1");
        let cwd = env::temp_dir();
        let copies = cwd.join("copies");
        let translator = Translator::new(peer, Some(window), cwd, copies);
        (translator, sent)
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
                    for field in ["title", "status", "content"] {
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
        replaying.replay(&memory, &[]);
        let replayed = drained(&mut sent);
        assert_eq!(shown(&replayed)[1..], shown(&live));
        assert!(replayed.len() < live.len(), "{replayed:?}");
    }

    /// Each task the code sends an agent is a tool call of its own, the same live and on replay
    #[test]
    fn tasks_shown() {
        let (mut live, mut sent) = translator(1000);
        live.translate(Event::Code(
            0,
            streamed("agent('cafés', 'Count the cafés')\n"),
        ));
        let running = Call {
            code: "agent('cafés', 'Count the cafés')".to_owned(),
            ..Call::default()
        };
        live.translate(Event::Run(0, &running));
        let started = live.tasks(0);
        let counted = AgentStep {
            usage: metered(100, 20),
            ..replied("3 cafés")
        };
        let task = "Count the cafés\nin menu.txt";
        let memory = AgentMemory {
            steps: vec![
                MemoryStep::Task(asked(task)),
                MemoryStep::Action(Box::new(counted)),
            ],
        };
        let end = Ending::Answer("3 cafés".to_owned());
        let mut call = started.open(1, "cafés", task);
        let status = |doing: &str, preview: &str| Status {
            doing: doing.to_owned(),
            preview: preview.to_owned(),
            ..Status::default()
        };
        call.shown(&status("thinking", "Reading the menu"));
        let spent = Status {
            usage: metered(100, 20),
            ..status("running print(len(cafés))", "print(len(cafés))")
        };
        call.shown(&spent);
        call.close(&end, &memory.steps);
        let named = Sent {
            name: "cafés".to_owned(),
            number: 0,
        };
        let result = CodeResult {
            printed: Some("3 cafés".to_owned()),
            agents: vec![named],
            ..CodeResult::default()
        };
        let ran = Call {
            result: Some(result),
            ..running
        };
        live.translate(Event::RunEnd(0, &ran));
        let step = AgentStep {
            calls: vec![ran],
            ..AgentStep::default()
        };
        live.translate(Event::StepEnd(&step));
        let told = drained(&mut sent);

        let session = AgentMemory {
            steps: vec![MemoryStep::Action(Box::new(step))],
        };
        let tasked = AgentTask {
            task: task.to_owned(),
            start: 0,
            end,
        };
        let agent = AgentRecord {
            name: "cafés".to_owned(),
            memory,
            tasks: vec![tasked],
        };
        let (mut replaying, mut sent) = translator(1000);
        replaying.replay(&session, &[agent]);
        let again = drained(&mut sent);
        assert_eq!(shown(&again), shown(&told));

        let updates: Vec<&Value> = told
            .iter()
            .map(|message| &message["params"]["update"])
            .collect();
        let opened = updates[2];
        assert_eq!(opened["toolCallId"], "call_1.1");
        assert_eq!(opened["title"], "agent: Count the cafés in menu.txt");
        let asked = json!({ "task": "Count the cafés\nin menu.txt", "name": "cafés" });
        assert_eq!(opened["rawInput"], asked);
        let marked = json!({ "_smol/agent": true, "parent": "call_1" });
        assert_eq!(opened["_meta"], marked);
        let working = updates[3];
        assert_eq!(working["status"], "in_progress");
        assert_eq!(working["content"][0]["content"]["text"], prompt::THINKING);
        assert_eq!(working["content"][1]["content"]["text"], "Reading the menu");
        assert_eq!(working["_meta"], json!({ "duration": 0.0, "steps": 0 }));
        let running = updates[4];
        assert!(running.get("status").is_none(), "{running}");
        let spent = json!({ "duration": 0.0, "inputTokens": 100, "outputTokens": 20, "steps": 0 });
        assert_eq!(running["_meta"], spent);
        let content = &running["content"];
        let doing = format!("{} print(len(cafés))", prompt::RUNNING);
        assert_eq!(content[0]["content"]["text"], doing);
        assert_eq!(content[1]["content"]["text"], "print(len(cafés))");
        let closed = updates[5];
        assert_eq!(closed["status"], "completed");
        assert_eq!(closed["content"][0]["content"]["text"], "3 cafés");
        let meta = json!({
            "inputTokens": 100,
            "outputTokens": 20,
            "steps": 1,
            "duration": 0.0,
        });
        assert_eq!(closed["_meta"], meta);

        // An interrupted agent is marked like an interrupted command
        let (mut stopping, mut sent) = translator(1000);
        let code = streamed("agent('counter', 'Count the cafés')\n");
        stopping.translate(Event::Code(0, code));
        let started = stopping.tasks(0);
        let mut call = started.open(1, "counter", "Count the cafés");
        call.close(&Ending::Interrupted {}, &[]);

        // One aborted before it closes still ends, and so does one replayed while running
        let dropped = started.open(2, "counter", "Spell naïve");
        drop(dropped);
        let mut replayed = started.open(3, "counter", "Spell naïve");
        replayed.close(&Ending::Running {}, &[]);

        // The code's call says which agents it waits for, until it stops waiting
        started.waiting(&["counter".to_owned()]);
        started.waiting(&[]);

        let messages = drained(&mut sent);
        let opened = &messages[1]["params"]["update"]["rawInput"];
        assert_eq!(
            *opened,
            json!({ "task": "Count the cafés", "name": "counter" })
        );
        let closed = &messages[2]["params"]["update"];
        assert_eq!(closed["status"], "failed");
        assert_eq!(closed["_meta"]["interrupted"], true);
        let aborted = &messages[4]["params"]["update"];
        assert_eq!(aborted["toolCallId"], "call_1.2");
        assert_eq!(aborted["status"], "failed");
        let reason = &aborted["content"][0]["content"]["text"];
        assert_eq!(reason, "stopped before it answered");
        let unfinished = &messages[6]["params"]["update"];
        assert_eq!(unfinished["status"], "failed");
        assert_eq!(unfinished["content"], aborted["content"]);
        assert_eq!(unfinished["_meta"], aborted["_meta"]);
        let waiting = &messages[7]["params"]["update"];
        assert_eq!(waiting["toolCallId"], "call_1");
        assert_eq!(waiting["_meta"], json!({ "_smol/waiting": ["counter"] }));
        let waited = &messages[8]["params"]["update"];
        assert_eq!(waited["_meta"], json!({ "_smol/waiting": [] }));
        assert_eq!(messages.len(), 9, "{messages:?}");
    }
}
