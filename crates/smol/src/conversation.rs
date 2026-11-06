use crate::client::{Notice, Refusal};
use agent_client_protocol_schema::v1::{
    ContentBlock, SessionUpdate, StopReason, ToolCallContent, ToolCallStatus,
};
use anyhow::Result;
use reaper::wire::{
    AGENT, DECODE_DURATION, DURATION, ERROR, ESTIMATED_OUTPUT, INPUT_TOKENS, INTERRUPTED, NAME,
    OUTPUT_TOKENS, PARENT, ROLE, SHELL, STEPS, TASK, UPDATE,
};
use serde_json::Value;
use std::collections::BTreeSet;
use std::mem;
use std::time::{Duration, Instant};

pub(crate) const HUES: usize = 8;

pub(crate) enum Entry {
    Info(String),
    Failure(String),
    Task(String),
    Step(Step),
    Shell(Shell),
}

impl Entry {
    pub(crate) fn step(&self) -> Option<&Step> {
        match self {
            Entry::Step(step) => Some(step),
            _ => None,
        }
    }

    fn step_mut(&mut self) -> Option<&mut Step> {
        match self {
            Entry::Step(step) => Some(step),
            _ => None,
        }
    }
}

/// Whether a step is settled or held, at that index, or is the live one
#[derive(Debug, Clone, Copy)]
pub(crate) enum Spot {
    Settled(usize),
    Held(usize),
    Live,
}

pub(crate) struct Shell {
    pub(crate) id: String,
    pub(crate) command: String,
    pub(crate) output: String,
    /// `None` while running, empty after a clean exit, otherwise the reason it ended
    pub(crate) end: Option<String>,
    pub(crate) interrupted: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum Outcome {
    Answered,
    Cancelled,
    Failed,
}

#[derive(Debug, Default)]
pub(crate) struct Stats {
    pub(crate) turns: usize,
    pub(crate) steps: usize,
    pub(crate) input: u64,
    pub(crate) output: u64,
    /// Tokens of the agents the code started, which count apart from the session's
    pub(crate) agent_input: u64,
    pub(crate) agent_output: u64,
    pub(crate) context: Option<u64>,
    pub(crate) window: Option<u64>,
    /// Output estimated while the step is still generating, which makes the context an estimate too
    pub(crate) estimated: Option<u64>,
    stale: bool,
}

impl Stats {
    pub(crate) fn ratio(&self) -> Option<f64> {
        let (tokens, window) = (self.context?, self.window?);
        (!self.stale).then(|| tokens as f64 / window as f64)
    }
}

#[derive(Default)]
pub(crate) struct Conversation {
    entries: Vec<Entry>,
    /// The task or latest step that this turn may still change, and the entries after it
    held: Vec<Entry>,
    live: Option<Step>,
    stats: Stats,
    task_id: Option<String>,
    retry_at: Option<Instant>,
    /// Settled entries with agents at work
    lively: BTreeSet<usize>,
    /// Grows with every change to `held` or `live`, except agent updates that leave their call
    /// with agents at work, which grow only the call's
    revision: u64,
}

impl Conversation {
    pub(crate) fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub(crate) fn held(&self) -> &[Entry] {
        &self.held
    }

    pub(crate) fn live(&self) -> Option<&Step> {
        self.live.as_ref()
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn stats(&self) -> &Stats {
        &self.stats
    }

    pub(crate) fn retry_in(&self) -> Option<Duration> {
        let now = Instant::now();
        let left = self.retry_at?.checked_duration_since(now);
        left.filter(|left| !left.is_zero())
    }

    pub(crate) fn push(&mut self, entry: Entry) {
        self.revision += 1;
        match self.held.is_empty() {
            true => self.entries.push(entry),
            false => self.held.push(entry),
        }
    }

    pub(crate) fn task(&mut self, text: &str) {
        self.revision += 1;
        self.finish_turn(Outcome::Answered);
        self.stats.turns += 1;
        let text = text.to_owned();
        self.held.push(Entry::Task(text));
        self.task_id = None;
    }

    pub(crate) fn amend(&mut self, text: &str) {
        let text = text.to_owned();
        self.push(Entry::Task(text));
        self.task_id = None;
    }

    pub(crate) fn shelling(&self) -> bool {
        let running = |entry: &Entry| matches!(entry, Entry::Shell(shell) if shell.end.is_none());
        self.held.iter().any(running)
    }

    pub(crate) fn unstarted(&self) -> bool {
        let asked = matches!(self.held.first(), Some(Entry::Task(_)));
        let step = |entry: &Entry| matches!(entry, Entry::Step(_));
        let stepped = self.held.iter().any(step);
        asked && !stepped && self.live.is_none()
    }

    pub(crate) fn stepping(&self) -> bool {
        self.live.is_some()
    }

    /// Agents still queued or running, wherever the call that started them is
    pub(crate) fn helping(&self) -> usize {
        let working = self.working();
        working.iter().map(|(_, step)| step.helping()).sum()
    }

    /// Agents the turn waits for, which are those the running code waits for
    /// or, after an answer, every one still at work
    pub(crate) fn awaited(&self) -> usize {
        // Agents started in the background don't hold up the code
        if let Some(step) = &self.live {
            let blocking = |helper: &&Helper| helper.waiting() && helper.name.is_none();
            return step.helpers().filter(blocking).count();
        }

        // A message the turn took after the answer is what the model works on next
        let Some((Entry::Step(last), after)) = self.held.split_first() else {
            return 0;
        };
        if after.iter().any(|entry| matches!(entry, Entry::Task(_))) {
            return 0;
        }

        let mut said = false;
        for item in &last.items {
            match item {
                Item::Call(_) => return 0,
                Item::Message { text, .. } => said |= !text.trim().is_empty(),
                Item::Thought(_) | Item::Notice(_) => {}
            }
        }
        match said && !last.interrupted {
            true => self.helping(),
            false => 0,
        }
    }

    /// The steps that may hold agents at work, oldest first
    pub(crate) fn working(&self) -> Vec<(Spot, &Step)> {
        let mut steps = Vec::new();
        for index in &self.lively {
            if let Some(Entry::Step(step)) = self.entries.get(*index) {
                steps.push((Spot::Settled(*index), step));
            }
        }
        for (index, entry) in self.held.iter().enumerate() {
            if let Entry::Step(step) = entry {
                steps.push((Spot::Held(index), step));
            }
        }
        if let Some(step) = &self.live {
            steps.push((Spot::Live, step));
        }
        steps
    }

    pub(crate) fn writing(&self) -> bool {
        let Some(step) = &self.live else {
            return false;
        };

        let writes = |item: &Item| matches!(item, Item::Message { .. } | Item::Call(_));
        step.items.iter().any(writes)
    }

    pub(crate) fn sent(&self) -> Vec<String> {
        let mut sent: Vec<String> = Vec::new();
        for entry in self.entries.iter().chain(&self.held) {
            let text = match entry {
                Entry::Task(text) => text.trim().to_owned(),
                Entry::Shell(shell) => format!("! {}", shell.command),
                _ => continue,
            };
            if sent.last() != Some(&text) {
                sent.push(text);
            }
        }

        sent
    }

    /// Returns true when a step ended
    pub(crate) fn update(&mut self, update: &SessionUpdate) -> bool {
        // An agent the code started keeps updating after the step of its call has ended
        if let SessionUpdate::ToolCallUpdate(call) = update {
            let id = &*call.tool_call_id.0;
            let mut place = None;
            let mut taken = Vec::new();
            for (spot, step) in self.working() {
                for (at, item) in step.items.iter().enumerate() {
                    let Item::Call(started) = item else {
                        continue;
                    };
                    for (index, helper) in started.helpers.iter().enumerate() {
                        if helper.id == id {
                            place = Some((spot, at, index));
                        }
                        if helper.state() == State::Running {
                            taken.push(helper.hue);
                        }
                    }
                }
            }

            if let Some((spot, at, index)) = place {
                let step = match spot {
                    Spot::Settled(entry) => self.entries.get_mut(entry).and_then(Entry::step_mut),
                    Spot::Held(entry) => self.held.get_mut(entry).and_then(Entry::step_mut),
                    Spot::Live => self.live.as_mut(),
                };
                let Some(step) = step else {
                    return false;
                };
                let Some(Item::Call(started)) = step.items.get_mut(at) else {
                    return false;
                };
                let helper = &mut started.helpers[index];

                let fields = &call.fields;
                if let Some(status) = fields.status {
                    // The hue at its place in its call, or the lowest free one when a running agent holds that
                    if status == ToolCallStatus::InProgress && helper.started.is_none() {
                        let place = index % HUES;
                        let free = (0..HUES).find(|hue| !taken.contains(hue));
                        helper.hue = match taken.contains(&place) {
                            true => free.unwrap_or(place),
                            false => place,
                        };
                        helper.started = Some(Instant::now());
                    }
                    helper.status = status;
                }

                // A preview follows the status, and the last update holds only the answer or error
                if let Some(content) = &fields.content {
                    let (texts, _) = contents(content);
                    let mut texts = texts.into_iter();
                    helper.text = texts.next().unwrap_or_default();
                    helper.preview = texts.collect();
                }

                let meta = call.meta.as_ref();
                let get = |key: &str| meta.and_then(|meta| meta.get(key));
                if get(INTERRUPTED).and_then(Value::as_bool) == Some(true) {
                    helper.interrupted = true;
                }
                let count = |key: &str| get(key).and_then(Value::as_u64);
                let tokens = count(INPUT_TOKENS).zip(count(OUTPUT_TOKENS));
                if tokens.is_some() {
                    helper.tokens = tokens;
                }

                // Every update carries its usage so far, so only the last one counts
                let ended = matches!(
                    fields.status,
                    Some(ToolCallStatus::Completed | ToolCallStatus::Failed)
                );
                if ended {
                    if let Some(steps) = count(STEPS) {
                        let duration = get(DURATION).and_then(Value::as_f64);
                        let duration = duration.unwrap_or_default();
                        helper.totals = Some(Totals { steps, duration });
                    }
                    let (input, output) = tokens.unwrap_or_default();
                    self.stats.agent_input += input;
                    self.stats.agent_output += output;
                }

                started.revision += 1;
                let waiting = started.helpers.iter().any(Helper::waiting);
                match spot {
                    Spot::Settled(entry) if step.helping() == 0 => {
                        self.lively.remove(&entry);
                    }
                    // The call's rows are drawn on their own only while its agents are at work
                    Spot::Held(_) | Spot::Live if !waiting => self.revision += 1,
                    _ => {}
                }
                return false;
            }
        }

        self.revision += 1;
        // A retry notice is followed by a usage update that withdraws the estimate
        if !matches!(update, SessionUpdate::UsageUpdate(_)) {
            self.retry_at = None;
        }

        match update {
            // A replayed message may arrive in several chunks,
            // and only a changed `messageId` starts a new one
            SessionUpdate::UserMessageChunk(chunk) => {
                let text = text_of(&chunk.content);
                let id = chunk.message_id.as_ref().map(|id| id.0.to_string());
                let meta = chunk.meta.as_ref();
                let amends = meta.is_some_and(|meta| meta.contains_key(UPDATE));
                match (&self.live, self.held.last_mut()) {
                    (None, Some(Entry::Task(task))) if id == self.task_id => {
                        task.push_str(&text);
                    }
                    _ => {
                        match amends {
                            true => self.amend(&text),
                            false => self.task(&text),
                        }
                        self.task_id = id;
                    }
                }
            }

            // A user's command is not a model step and stays in the redrawn part while it runs
            SessionUpdate::ToolCall(call)
                if call
                    .meta
                    .as_ref()
                    .is_some_and(|meta| meta.contains_key(SHELL)) =>
            {
                let shell = Shell {
                    id: call.tool_call_id.0.to_string(),
                    command: call.title.clone(),
                    output: String::new(),
                    end: None,
                    interrupted: false,
                };
                self.held.push(Entry::Shell(shell));
            }

            SessionUpdate::AgentThoughtChunk(_)
            | SessionUpdate::AgentMessageChunk(_)
            | SessionUpdate::ToolCall(_) => {
                self.live.get_or_insert_default().update(update);
            }

            SessionUpdate::ToolCallUpdate(call) => {
                let mut running = None;
                for entry in self.held.iter_mut().rev() {
                    if let Entry::Shell(shell) = entry {
                        if shell.id == *call.tool_call_id.0 && shell.end.is_none() {
                            running = Some(shell);
                            break;
                        }
                    }
                }
                // Calls from a finished step are no longer updated and don't start a new step
                let Some(shell) = running else {
                    if let Some(step) = &mut self.live {
                        step.update(update);
                    }
                    return false;
                };

                let fields = &call.fields;
                if let Some(content) = &fields.content {
                    let (texts, _) = contents(content);
                    shell.output = texts.concat();
                }

                let get = |key: &str| call.meta.as_ref().and_then(|meta| meta.get(key));
                let error = get(ERROR).and_then(Value::as_str).unwrap_or_default();
                let interrupted = get(INTERRUPTED).and_then(Value::as_bool);
                shell.interrupted = interrupted.unwrap_or(false);
                shell.end = match fields.status {
                    Some(ToolCallStatus::Completed) => Some(String::new()),
                    Some(ToolCallStatus::Failed) => Some(error.to_owned()),
                    _ => None,
                };
            }

            SessionUpdate::UsageUpdate(usage) => {
                // 0 means the current model hasn't been measured yet
                self.stats.stale = usage.used == 0;
                if usage.used > 0 {
                    self.stats.context = Some(usage.used);
                }
                self.stats.window = (usage.size > 0).then_some(usage.size);
                let get = |key: &str| usage.meta.as_ref().and_then(|meta| meta.get(key));
                self.stats.estimated = get(ESTIMATED_OUTPUT).and_then(Value::as_u64);

                // Model switches and mid-generation estimates carry no `duration` and are not steps
                let Some(duration) = get(DURATION).and_then(Value::as_f64) else {
                    return false;
                };

                let stats = StepStats {
                    duration,
                    decode: get(DECODE_DURATION).and_then(Value::as_f64),
                    input: get(INPUT_TOKENS).and_then(Value::as_u64),
                    output: get(OUTPUT_TOKENS).and_then(Value::as_u64),
                };

                self.stats.steps += 1;
                self.stats.input += stats.input.unwrap_or(0);
                self.stats.output += stats.output.unwrap_or(0);

                let flag = |key: &str| get(key).and_then(Value::as_bool).unwrap_or(false);
                let mut step = self.live.take().unwrap_or_default();
                // A replayed turn has no stop reason, which leaves this as the only sign of an interrupt
                step.interrupted = flag(INTERRUPTED);
                step.stats = Some(stats);
                self.hold(step);
                return true;
            }

            _ => {}
        }

        false
    }

    /// Settles everything before this step, which may still change
    fn hold(&mut self, step: Step) {
        self.settle();
        self.held.push(Entry::Step(step));
    }

    /// Moves the held entries to the settled ones, which still follow the agents at work in them
    fn settle(&mut self) {
        for entry in self.held.drain(..) {
            if let Entry::Step(step) = &entry {
                if step.helping() > 0 {
                    self.lively.insert(self.entries.len());
                }
            }
            self.entries.push(entry);
        }
    }

    pub(crate) fn notice(&mut self, notice: &Notice) {
        self.revision += 1;
        let text = notice.text.clone();
        if let Some(seconds) = notice.retry_in {
            let wait = Duration::from_secs_f64(seconds);
            self.retry_at = Some(Instant::now() + wait);

            // An open call means compaction is retrying its request,
            // otherwise the step is retried and the failed reply is discarded
            let step = self.live.get_or_insert_default();
            if let Some(Item::Call(call)) = step.items.last_mut() {
                let finished = matches!(
                    call.status,
                    ToolCallStatus::Completed | ToolCallStatus::Failed
                );
                if !finished {
                    return record(&mut call.retries, text);
                }
            }
            step.discard();
            return record(&mut step.retries, text);
        }

        // A notice arriving after a step ends doesn't belong to the next one,
        // and counting it there would displace the answer
        let asked = matches!(self.held.last(), Some(Entry::Task(_))) || self.unstarted();
        if self.live.is_none() && !asked && !notice.step {
            return self.push(Entry::Failure(text));
        }

        let step = self.live.get_or_insert_default();
        step.items.push(Item::Notice(text));
    }

    pub(crate) fn finish_turn(&mut self, outcome: Outcome) {
        self.revision += 1;
        self.retry_at = None;
        if let Some(step) = self.live.take() {
            self.hold(step);
        }

        if let Some(Entry::Step(step)) = self.held.first_mut() {
            // A replayed turn interrupted at this step has no answer either
            match outcome {
                Outcome::Cancelled => step.interrupted = true,
                Outcome::Answered if !step.interrupted => step.mark_answer(),
                Outcome::Answered | Outcome::Failed => {}
            }
        }
        self.settle();
    }

    pub(crate) fn done(&mut self, stop: Result<StopReason>) {
        let error = match stop {
            Ok(StopReason::Cancelled) => return self.finish_turn(Outcome::Cancelled),
            Ok(_) => return self.finish_turn(Outcome::Answered),
            Err(error) => error,
        };

        // The agent doesn't record a refused task, and it won't count after a resume either
        let refused = matches!(
            error.downcast_ref::<Refusal>(),
            Some(Refusal::InvalidParams(_))
        );
        if refused {
            self.stats.turns = self.stats.turns.saturating_sub(1);
        }

        // Agents at work when the turn fails are never updated again, as when the agent exited
        let mut text = format!("{error:#}");
        for index in mem::take(&mut self.lively) {
            if let Some(Entry::Step(step)) = self.entries.get_mut(index) {
                step.abandon(&text);
            }
        }
        let held = self.held.iter_mut().filter_map(Entry::step_mut);
        for step in held.chain(&mut self.live) {
            step.abandon(&text);
        }

        // When the request still fails after every retry,
        // the last attempt's stream is discarded too and the retry count joins the error
        let mut count = 0;
        if let Some(step) = &self.live {
            count = step.retries.iter().map(|retry| retry.count).sum();
        }
        if count > 0 {
            let times = match count {
                1 => "retry",
                _ => "retries",
            };
            text = format!("{text} (after {count} {times})");
            let mut step = self.live.take().unwrap_or_default();
            step.retries.clear();
            step.discard();
            if !step.items.is_empty() {
                self.live = Some(step);
            }
        }
        self.finish_turn(Outcome::Failed);

        // Compaction in the turn's last step may have shown this error already
        let mut last = None;
        for entry in self.entries.iter().rev() {
            match entry {
                Entry::Step(step) => {
                    last = Some(step);
                    break;
                }
                Entry::Task(_) => break,
                _ => {}
            }
        }
        let shown = match last.and_then(|step| step.items.last()) {
            Some(Item::Call(call)) => call.parts().error,
            _ => None,
        };
        if shown != Some(text.as_str()) {
            self.push(Entry::Failure(text));
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct StepStats {
    pub(crate) duration: f64,
    /// Seconds, counted from the first token
    pub(crate) decode: Option<f64>,
    pub(crate) input: Option<u64>,
    pub(crate) output: Option<u64>,
}

#[derive(Debug, Default)]
pub(crate) struct Step {
    /// A retry discards everything before it, which keeps these records at the front
    pub(crate) retries: Vec<Retry>,
    pub(crate) items: Vec<Item>,
    pub(crate) stats: Option<StepStats>,
    pub(crate) interrupted: bool,
}

#[derive(Debug)]
pub(crate) enum Item {
    Thought(String),
    Message { text: String, answer: bool },
    Call(Call),
    Notice(String),
}

impl Item {
    fn call(&self) -> Option<&Call> {
        match self {
            Item::Call(call) => Some(call),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Retry {
    pub(crate) text: String,
    pub(crate) count: u32,
}

/// Code is in the first block and the error of a failed call in the last,
/// while compaction calls have no code
#[derive(Debug)]
pub(crate) struct Call {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) status: ToolCallStatus,
    pub(crate) content: Vec<String>,
    pub(crate) attachments: Vec<Attachment>,
    /// Retries of the request that compaction makes
    pub(crate) retries: Vec<Retry>,
    /// Agents its code started, in the order it started them
    pub(crate) helpers: Vec<Helper>,
    /// Grows with every update of one of its agents
    pub(crate) revision: u64,
}

#[derive(Debug)]
pub(crate) struct Helper {
    pub(crate) id: String,
    pub(crate) task: String,
    pub(crate) role: Option<String>,
    /// Given to one started in the background
    pub(crate) name: Option<String>,
    pub(crate) status: ToolCallStatus,
    /// What it is doing while it runs, then its answer or why it failed
    pub(crate) text: String,
    /// The end of what it is writing or printing while it runs
    pub(crate) preview: String,
    pub(crate) interrupted: bool,
    /// Unknown on replay, which reports the duration instead
    pub(crate) started: Option<Instant>,
    /// Chosen at its start from the hues no running agent holds, which no later frame can work out
    pub(crate) hue: usize,
    /// Input and output tokens so far
    pub(crate) tokens: Option<(u64, u64)>,
    /// Known once it ends
    pub(crate) totals: Option<Totals>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Totals {
    pub(crate) steps: u64,
    pub(crate) duration: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum State {
    Running,
    Queued,
    Done,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attachment {
    /// The MIME top-level type, or `file` when none was given
    pub(crate) kind: String,
    pub(crate) name: String,
    pub(crate) uri: String,
}

pub(crate) struct Parts<'a> {
    /// Language and code
    pub(crate) code: Option<(&'a str, &'a str)>,
    pub(crate) outputs: &'a [String],
    pub(crate) attachments: &'a [Attachment],
    pub(crate) error: Option<&'a str>,
}

impl Helper {
    pub(crate) fn state(&self) -> State {
        match (self.status, self.interrupted) {
            (ToolCallStatus::InProgress, _) => State::Running,
            (ToolCallStatus::Pending, _) => State::Queued,
            (ToolCallStatus::Completed, _) => State::Done,
            (_, true) => State::Interrupted,
            (_, false) => State::Failed,
        }
    }

    pub(crate) fn waiting(&self) -> bool {
        matches!(self.state(), State::Running | State::Queued)
    }
}

impl Call {
    pub(crate) fn parts(&self) -> Parts<'_> {
        let first = self.content.first().map(String::as_str);
        let code = first.and_then(fenced);
        let rest = match code {
            Some(_) => &self.content[1..],
            None => &self.content[..],
        };

        let failed = self.status == ToolCallStatus::Failed;
        let (outputs, error) = match rest.split_last() {
            Some((error, outputs)) if failed => (outputs, Some(error.as_str())),
            _ => (rest, None),
        };

        Parts {
            code,
            outputs,
            attachments: &self.attachments,
            error,
        }
    }
}

impl Step {
    fn update(&mut self, update: &SessionUpdate) {
        match update {
            SessionUpdate::AgentThoughtChunk(chunk) => {
                let text = text_of(&chunk.content);
                match self.items.last_mut() {
                    Some(Item::Thought(thought)) => thought.push_str(&text),
                    _ => self.items.push(Item::Thought(text)),
                }
            }

            SessionUpdate::AgentMessageChunk(chunk) => {
                let text = text_of(&chunk.content);
                match self.items.last_mut() {
                    Some(Item::Message { text: message, .. }) => message.push_str(&text),
                    _ => self.items.push(Item::Message {
                        text,
                        answer: false,
                    }),
                }
            }

            SessionUpdate::ToolCall(call) => {
                let id = call.tool_call_id.0.to_string();

                // An agent the code started belongs under the call running that code
                let meta = call.meta.as_ref();
                let marked = meta.is_some_and(|meta| meta.contains_key(AGENT));
                let parent = meta.and_then(|meta| meta.get(PARENT));
                let parent = parent.and_then(Value::as_str).filter(|_| marked);
                if let Some(owner) = parent.and_then(|parent| self.call_mut(parent)) {
                    let asked = call.raw_input.as_ref();
                    let given = |key: &str| asked.and_then(|asked| asked.get(key));
                    let given = |key: &str| given(key).and_then(Value::as_str);
                    let task = given(TASK).unwrap_or_default();
                    let role = given(ROLE).map(str::to_owned);
                    let name = given(NAME).map(str::to_owned);
                    owner.helpers.push(Helper {
                        id,
                        task: task.to_owned(),
                        role,
                        name,
                        status: call.status,
                        text: String::new(),
                        preview: String::new(),
                        interrupted: false,
                        started: None,
                        hue: 0,
                        tokens: None,
                        totals: None,
                    });
                    return;
                }

                let (content, attachments) = contents(&call.content);
                let call = Call {
                    id,
                    title: call.title.clone(),
                    status: call.status,
                    content,
                    attachments,
                    retries: Vec::new(),
                    helpers: Vec::new(),
                    revision: 0,
                };
                self.items.push(Item::Call(call));
            }

            SessionUpdate::ToolCallUpdate(update) => {
                let id = &*update.tool_call_id.0;
                if let Some(call) = self.call_mut(id) {
                    let fields = &update.fields;
                    if let Some(status) = fields.status {
                        call.status = status;
                    }
                    if let Some(content) = &fields.content {
                        (call.content, call.attachments) = contents(content);
                    }
                }
            }

            _ => {}
        }
    }

    /// Agents its code started that are still queued or running
    fn helping(&self) -> usize {
        self.helpers().filter(|helper| helper.waiting()).count()
    }

    pub(crate) fn calls(&self) -> impl Iterator<Item = &Call> {
        self.items.iter().filter_map(Item::call)
    }

    /// The agents its code started
    fn helpers(&self) -> impl Iterator<Item = &Helper> {
        self.calls().flat_map(|call| &call.helpers)
    }

    fn call_mut(&mut self, id: &str) -> Option<&mut Call> {
        for item in self.items.iter_mut().rev() {
            if let Item::Call(call) = item {
                if call.id == id {
                    return Some(call);
                }
            }
        }
        None
    }

    /// Notices such as a restart didn't come from the request stream and are kept
    fn discard(&mut self) {
        self.items.retain(|item| matches!(item, Item::Notice(_)));
    }

    /// Ends the agents its code started that are still at work as failed with `reason`
    fn abandon(&mut self, reason: &str) {
        for item in &mut self.items {
            let Item::Call(call) = item else {
                continue;
            };
            for helper in &mut call.helpers {
                if helper.waiting() {
                    helper.status = ToolCallStatus::Failed;
                    helper.text = reason.to_owned();
                    helper.preview.clear();
                    call.revision += 1;
                }
            }
        }
    }

    fn mark_answer(&mut self) {
        let start = self.answer_start();
        for item in &mut self.items[start..] {
            if let Item::Message { answer, .. } = item {
                *answer = true;
            }
        }
    }

    /// The answer follows the last call or notice
    fn answer_start(&self) -> usize {
        let block = |item: &Item| matches!(item, Item::Call(_) | Item::Notice(_));
        let found = self.items.iter().rposition(block);
        found.map_or(0, |index| index + 1)
    }
}

/// Consecutive repeats of an error share one record,
/// and errors often carry a different request ID each time
fn record(retries: &mut Vec<Retry>, text: String) {
    if let Some(last) = retries.last_mut() {
        if subject(&last.text) == subject(&text) {
            last.text = text;
            last.count += 1;
            return;
        }
    }
    retries.push(Retry { text, count: 1 });
}

/// The first two parts of `label: object: problem`
fn subject(text: &str) -> &str {
    let mut colons = text.match_indices(": ").map(|(at, _)| at);
    match colons.nth(1) {
        Some(at) => &text[..at],
        None => text,
    }
}

fn text_of(content: &ContentBlock) -> String {
    match content {
        ContentBlock::Text(text) => text.text.clone(),
        _ => String::new(),
    }
}

fn contents(content: &[ToolCallContent]) -> (Vec<String>, Vec<Attachment>) {
    let mut texts = Vec::new();
    let mut attachments = Vec::new();
    for item in content {
        let ToolCallContent::Content(item) = item else {
            continue;
        };
        let ContentBlock::ResourceLink(link) = &item.content else {
            let text = text_of(&item.content);
            texts.push(text);
            continue;
        };

        let mime = link.mime_type.as_deref().unwrap_or_default();
        let (kind, _) = mime.split_once('/').unwrap_or(("file", ""));
        attachments.push(Attachment {
            kind: kind.to_owned(),
            name: link.name.clone(),
            uri: link.uri.clone(),
        });
    }

    (texts, attachments)
}

/// The closing fence may not have arrived yet
fn fenced(text: &str) -> Option<(&str, &str)> {
    let rest = text.trim_start_matches('`');
    let ticks = text.len() - rest.len();
    if ticks < 3 {
        return None;
    }

    let fence = &text[..ticks];
    let (language, code) = rest.split_once('\n').unwrap_or((rest, ""));
    let code = code.trim_end().strip_suffix(fence).unwrap_or(code);
    Some((language.trim(), code.trim_matches('\n')))
}

#[cfg(test)]
mod tests {
    use crate::conversation::*;
    use crate::tests::{chunk, content, end_step, message, moved, started};
    use agent_client_protocol_schema::v1::{
        ContentChunk, MessageId, ResourceLink, ToolCall, ToolCallUpdate, ToolCallUpdateFields,
        UsageUpdate,
    };
    use anyhow::anyhow;
    use serde_json::json;

    #[test]
    fn messages_by_id() {
        let user = |text: &str, id: Option<&str>| {
            let id = id.map(MessageId::new);
            SessionUpdate::UserMessageChunk(chunk(text).message_id(id))
        };

        let mut talk = Conversation::default();
        talk.update(&user("first ", Some("task_1")));
        talk.update(&user("part", Some("task_1")));
        talk.update(&user("second", Some("task_2")));
        talk.update(&user("old ", None));
        talk.update(&user("chunks", None));
        talk.task("second");
        talk.task("now");

        let expected = ["first part", "second", "old chunks", "second", "now"];
        assert_eq!(talk.sent(), expected);
    }

    #[test]
    fn repeats_sent_once() {
        let mut talk = Conversation::default();
        talk.task("Run the tests");
        talk.task("Run the tests\n");
        talk.task("Fix the café menu");
        talk.task("Run the tests");

        let expected = ["Run the tests", "Fix the café menu", "Run the tests"];
        assert_eq!(talk.sent(), expected);
    }

    #[test]
    fn conversation_stats() {
        let mut talk = Conversation::default();
        talk.task("Tally");
        end_step(&mut talk, 10, 1);
        end_step(&mut talk, 20, 2);

        let stats = talk.stats();
        assert_eq!((stats.turns, stats.steps), (1, 2));
        assert_eq!((stats.input, stats.output), (30, 3));
        assert_eq!((stats.context, stats.window), (Some(22), Some(100)));
        assert_eq!(stats.ratio(), Some(0.22));

        let switched = SessionUpdate::UsageUpdate(UsageUpdate::new(0, 200));
        let ended = talk.update(&switched);
        assert!(!ended);
        let stats = talk.stats();
        assert_eq!((stats.context, stats.window), (Some(22), Some(200)));
        assert_eq!(stats.ratio(), None);

        end_step(&mut talk, 30, 3);
        let stats = talk.stats();
        assert_eq!((stats.steps, stats.ratio()), (3, Some(0.33)));
    }

    #[test]
    fn estimate_replaced() {
        let mut talk = Conversation::default();
        talk.task("Estimate it");
        end_step(&mut talk, 10, 1);

        let meta = json!({ "estimatedOutput": 7 });
        let meta = meta.as_object().cloned();
        let usage = UsageUpdate::new(18, 100).meta(meta);
        talk.update(&SessionUpdate::UsageUpdate(usage));
        let stats = talk.stats();
        assert_eq!((stats.context, stats.estimated), (Some(18), Some(7)));

        end_step(&mut talk, 20, 2);
        let stats = talk.stats();
        assert_eq!((stats.context, stats.estimated), (Some(22), None));
    }

    #[test]
    fn fence_split() {
        assert_eq!(fenced("```python\nx = 1\n```"), Some(("python", "x = 1")));
        assert_eq!(
            fenced("```python\nprint('café')"),
            Some(("python", "print('café')"))
        );
        assert_eq!(fenced("No code block"), None);

        let quoted = "```python\nfence = '```'\nprint(fence)\n```";
        let code = "fence = '```'\nprint(fence)";
        assert_eq!(fenced(quoted), Some(("python", code)));
        let nested = "````python\nnote = '''\n```\ncafé\n```\n'''\n````";
        let code = "note = '''\n```\ncafé\n```\n'''";
        assert_eq!(fenced(nested), Some(("python", code)));
    }

    #[test]
    fn answer_after_call() {
        let replied = |step: &Step| {
            let start = step.answer_start();
            let mut reply = String::new();
            for item in &step.items[start..] {
                if let Item::Message { text, .. } = item {
                    reply.push_str(text);
                }
            }
            reply
        };

        let mut step = Step::default();
        step.update(&message("Let me look."));
        let started = ToolCall::new("call_1", "eval").status(ToolCallStatus::Pending);
        step.update(&SessionUpdate::ToolCall(started));
        assert_eq!(replied(&step), "");

        step.update(&message("The answer is "));
        step.update(&message("42."));
        assert_eq!(replied(&step), "The answer is 42.");

        step.mark_answer();
        let mut marked = Vec::new();
        for item in &step.items {
            if let Item::Message { answer, .. } = item {
                marked.push(*answer);
            }
        }
        assert_eq!(marked, [false, true]);

        step.items.push(Item::Notice("Retrying".to_owned()));
        assert_eq!(replied(&step), "");
    }

    #[test]
    fn helper_colors() {
        let mut talk = Conversation::default();
        started(&mut talk, 10);
        for n in (1..=8).rev() {
            moved(&mut talk, n, ToolCallStatus::InProgress);
        }
        moved(&mut talk, 3, ToolCallStatus::Completed);
        moved(&mut talk, 9, ToolCallStatus::InProgress);
        moved(&mut talk, 1, ToolCallStatus::Completed);
        moved(&mut talk, 10, ToolCallStatus::InProgress);
        moved(&mut talk, 9, ToolCallStatus::InProgress);

        let step = talk.live().expect("live");
        let [Item::Call(call)] = &step.items[..] else {
            panic!("{:?}", step.items);
        };
        let hues: Vec<usize> = call.helpers.iter().map(|helper| helper.hue).collect();
        assert_eq!(hues, [0, 1, 2, 3, 4, 5, 6, 7, 2, 0]);
        assert_eq!(call.helpers[0].name.as_deref(), Some("cafés"));
    }

    /// An agent keeps updating under its call after the step that started it has ended
    #[test]
    fn background_followed() {
        let mut talk = Conversation::default();
        talk.task("Count the cafés");
        started(&mut talk, 1);
        moved(&mut talk, 1, ToolCallStatus::InProgress);
        let ended = |talk: &mut Conversation| {
            let meta = json!({ "duration": 0.5 });
            let usage = UsageUpdate::new(0, 1000).meta(meta.as_object().cloned());
            talk.update(&SessionUpdate::UsageUpdate(usage));
        };

        ended(&mut talk);
        assert_eq!(talk.awaited(), 0);

        let said = ContentChunk::new(ContentBlock::from("Started them."));
        talk.update(&SessionUpdate::AgentMessageChunk(said));
        ended(&mut talk);
        assert_eq!((talk.helping(), talk.awaited()), (1, 1));
        talk.amend("Count the naïve ones too");
        assert_eq!((talk.helping(), talk.awaited()), (1, 0));

        moved(&mut talk, 1, ToolCallStatus::Completed);
        assert_eq!((talk.helping(), talk.awaited()), (0, 0));
        let Some(Entry::Step(first)) = talk.entries().last() else {
            panic!("{}", talk.entries().len());
        };
        let Item::Call(call) = &first.items[0] else {
            panic!("{:?}", first.items);
        };
        assert_eq!(call.helpers[0].status, ToolCallStatus::Completed);
    }

    /// A settled step leaves the ones followed once its last agent ends
    #[test]
    fn lively_pruned_early() {
        let mut talk = Conversation::default();
        talk.task("Count the cafés");
        started(&mut talk, 2);
        moved(&mut talk, 1, ToolCallStatus::InProgress);
        moved(&mut talk, 2, ToolCallStatus::InProgress);
        end_step(&mut talk, 10, 1);
        talk.finish_turn(Outcome::Answered);
        assert_eq!(talk.working().len(), 1);

        let revision = talk.revision();
        moved(&mut talk, 1, ToolCallStatus::Completed);
        assert_eq!(talk.working().len(), 1);
        moved(&mut talk, 2, ToolCallStatus::Completed);
        assert!(talk.working().is_empty());
        assert_eq!(talk.revision(), revision);
    }

    /// An agent that exits never ends the agents its code started, so the failed turn does
    #[test]
    fn exit_fails_helpers() {
        let mut talk = Conversation::default();
        talk.task("Count the cafés");
        started(&mut talk, 2);
        moved(&mut talk, 1, ToolCallStatus::InProgress);
        assert_eq!(talk.helping(), 2);

        let exited = anyhow!("agent: exited\nnaïve crash");
        talk.done(Err(exited));
        assert_eq!(talk.helping(), 0);
        assert!(talk.working().is_empty());
        let step = talk.entries().iter().find_map(Entry::step).expect("step");
        let call = step.calls().next().expect("call");
        let ended: Vec<(State, &str)> = call
            .helpers
            .iter()
            .map(|helper| (helper.state(), helper.text.as_str()))
            .collect();
        let failed = (State::Failed, "agent: exited\nnaïve crash");
        assert_eq!(ended, [failed, failed]);
    }

    #[test]
    fn retry_subject() {
        let text = "model: http 500: internal error (request 7)";
        assert_eq!(subject(text), "model: http 500");
        assert_eq!(subject("model: empty reply"), "model: empty reply");
        assert_eq!(subject("disconnected"), "disconnected");
    }

    #[test]
    fn update_splits_links() {
        let mut step = Step::default();
        let started = ToolCall::new("call_1", "eval").status(ToolCallStatus::Pending);
        step.update(&SessionUpdate::ToolCall(started));

        let image = ResourceLink::new("screenshot.png", "file:///copies/a1.png")
            .mime_type("image/png".to_owned());
        let image = ToolCallContent::from(ContentBlock::ResourceLink(image));
        let file = ResourceLink::new("notes", "file:///copies/a2");
        let file = ToolCallContent::from(ContentBlock::ResourceLink(file));
        let finished = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![content("hello"), image, file]);
        let finished = ToolCallUpdate::new("call_1", finished);
        step.update(&SessionUpdate::ToolCallUpdate(finished));
        let unknown = ToolCallUpdateFields::new().status(ToolCallStatus::Failed);
        let unknown = ToolCallUpdate::new("call_9", unknown);
        step.update(&SessionUpdate::ToolCallUpdate(unknown));

        let [Item::Call(call)] = &step.items[..] else {
            panic!("{:?}", step.items);
        };
        assert_eq!(call.status, ToolCallStatus::Completed);
        assert_eq!(call.content, ["hello"]);
        let attached = |kind: &str, name: &str, uri: &str| Attachment {
            kind: kind.to_owned(),
            name: name.to_owned(),
            uri: uri.to_owned(),
        };
        let expected = [
            attached("image", "screenshot.png", "file:///copies/a1.png"),
            attached("file", "notes", "file:///copies/a2"),
        ];
        assert_eq!(call.attachments, expected);
    }
}
