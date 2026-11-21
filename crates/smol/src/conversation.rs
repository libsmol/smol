use crate::client::Refusal;
use agent_client_protocol_schema::v1::{
    ContentBlock, Meta, SessionUpdate, StopReason, ToolCallContent, ToolCallStatus,
};
use anyhow::Result;
use reaper::wire::{
    Measured, Notice, AGENT, BACKGROUND, ERROR, NAME, PARENT, SHELL, TASK, UPDATE, WAITING,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::mem;
use std::ops::AddAssign;
use std::time::{Duration, Instant};

pub(crate) const HUES: usize = 8;

pub(crate) enum Entry {
    Info(String),
    Warning(String),
    Failure(String),
    Task(String),
    /// Programs the code left running that ended, which open a turn of their own
    Ended(String),
    Step(Step),
    Shell(Shell),
}

impl Entry {
    fn opens_turn(&self) -> bool {
        matches!(self, Entry::Task(_) | Entry::Ended(_))
    }

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
    pub(crate) end: ShellEnd,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ShellEnd {
    Running,
    Exited,
    /// With why it failed
    Failed(String),
    /// With how it was stopped
    Interrupted(String),
}

/// What the latest run said, which a user who looked away is told
pub(crate) enum Said<'a> {
    /// A turn's answer, empty when it gave none
    Answer(String),
    /// A command the user ran
    Shell(&'a Shell),
}

#[derive(Clone)]
pub(crate) enum Outcome {
    Answered,
    Cancelled,
    /// With the error the conversation shows
    Failed(String),
}

#[derive(Debug, Default)]
pub(crate) struct Stats {
    pub(crate) turns: usize,
    pub(crate) steps: usize,
    pub(crate) tokens: Tokens,
    pub(crate) context: Option<u64>,
    pub(crate) window: Option<u64>,
    /// Output estimated while the step is still generating, which makes the context an estimate too
    pub(crate) estimated: Option<u64>,
    stale: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Tokens {
    pub(crate) input: u64,
    pub(crate) output: u64,
}

impl Tokens {
    fn of(measured: &Measured) -> Option<Self> {
        let input = measured.input_tokens?;
        let output = measured.output_tokens?;
        Some(Self { input, output })
    }

    pub(crate) fn is_zero(&self) -> bool {
        self.input + self.output == 0
    }
}

impl AddAssign for Tokens {
    fn add_assign(&mut self, other: Self) {
        self.input += other.input;
        self.output += other.output;
    }
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
        let text = text.to_owned();
        self.open_turn(Entry::Task(text));
    }

    fn open_turn(&mut self, opening: Entry) {
        self.revision += 1;
        self.finish_turn(Outcome::Answered);
        self.stats.turns += 1;
        self.held.push(opening);
        self.task_id = None;
    }

    pub(crate) fn amend(&mut self, text: &str) {
        let text = text.to_owned();
        self.push(Entry::Task(text));
        self.task_id = None;
    }

    pub(crate) fn shelling(&self) -> bool {
        let running =
            |entry: &Entry| matches!(entry, Entry::Shell(shell) if shell.end == ShellEnd::Running);
        self.held.iter().any(running)
    }

    /// The agent sends the commands the user ran with the next task
    pub(crate) fn shell_unsent(&self) -> bool {
        let shell = |entry: &Entry| matches!(entry, Entry::Shell(_));
        self.latest().any(shell)
    }

    pub(crate) fn unstarted(&self) -> bool {
        let asked = self.held.first().is_some_and(Entry::opens_turn);
        let step = |entry: &Entry| matches!(entry, Entry::Step(_));
        let stepped = self.held.iter().any(step);
        asked && !stepped && self.live.is_none()
    }

    /// Agents still queued or running, wherever the call that started them is
    fn helping(&self) -> usize {
        let working = self.working();
        working.iter().map(|(_, step)| step.helping()).sum()
    }

    /// Agents the turn waits for, which are those the running code waits for
    /// or, after an answer, every one still at work
    pub(crate) fn awaited(&self) -> usize {
        if let Some(step) = &self.live {
            let waited: Vec<&String> = step.calls().flat_map(|call| &call.waiting).collect();
            let awaited = |helper: &&Helper| {
                let named = helper.name.as_ref();
                helper.waiting() && named.is_some_and(|name| waited.contains(&name))
            };
            return self.helpers().filter(awaited).count();
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

    /// Every task the code sent an agent, oldest first
    fn helpers(&self) -> impl Iterator<Item = &Helper> {
        let steps = self
            .entries
            .iter()
            .chain(&self.held)
            .filter_map(Entry::step);
        steps.chain(&self.live).flat_map(Step::helpers)
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

    pub(crate) fn said(&self) -> Said<'_> {
        let ran = |entry: &&Entry| matches!(entry, Entry::Step(_) | Entry::Shell(_));
        let step = match self.latest().find(ran) {
            Some(Entry::Shell(shell)) => return Said::Shell(shell),
            Some(Entry::Step(step)) => step,
            _ => return Said::Answer(String::new()),
        };

        let mut texts = Vec::new();
        for item in &step.items {
            if let Item::Message { text, answer: true } = item {
                texts.push(text.as_str());
            }
        }
        Said::Answer(texts.join("\n"))
    }

    /// The entries since the latest task or programs that ended, newest first
    fn latest(&self) -> impl Iterator<Item = &Entry> {
        let entries = self.entries.iter().chain(&self.held).rev();
        entries.take_while(|entry| !entry.opens_turn())
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
        // Updates of agents' tasks keep revisions of their own, which their arm sorts out,
        // and a retry notice is followed by a usage update that withdraws the estimate
        match update {
            SessionUpdate::ToolCallUpdate(_) => {}
            SessionUpdate::UsageUpdate(_) => self.revision += 1,
            _ => {
                self.revision += 1;
                self.retry_at = None;
            }
        }

        match update {
            // A replayed message may arrive in several chunks,
            // and only a changed `messageId` starts a new one
            SessionUpdate::UserMessageChunk(chunk) => {
                let text = text_of(&chunk.content);
                let id = chunk.message_id.as_ref().map(|id| id.0.to_string());
                let meta = chunk.meta.as_ref();
                let amends = meta.is_some_and(|meta| meta.contains_key(UPDATE));
                let background = meta.is_some_and(|meta| meta.contains_key(BACKGROUND));
                // Programs that ended as the user sent a task come before it, as a resume shows them,
                // and only a task sent here has no id
                let sent =
                    matches!(self.held.first(), Some(Entry::Task(_))) && self.task_id.is_none();
                let raced = background && sent && self.unstarted();
                match (&self.live, self.held.last_mut()) {
                    _ if raced => {
                        let task = mem::take(&mut self.held);
                        self.open_turn(Entry::Ended(text));
                        self.finish_turn(Outcome::Answered);
                        self.held = task;
                    }
                    (None, Some(Entry::Task(task))) if id == self.task_id => {
                        task.push_str(&text);
                    }
                    _ => {
                        match (background, amends) {
                            (true, _) => self.open_turn(Entry::Ended(text)),
                            (false, true) => self.amend(&text),
                            (false, false) => self.task(&text),
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
                    end: ShellEnd::Running,
                };
                self.held.push(Entry::Shell(shell));
            }

            SessionUpdate::AgentThoughtChunk(_)
            | SessionUpdate::AgentMessageChunk(_)
            | SessionUpdate::ToolCall(_) => {
                self.live.get_or_insert_default().update(update);
            }

            SessionUpdate::ToolCallUpdate(call) => {
                // An agent the code started keeps updating after the step of its call has ended
                let id = &*call.tool_call_id.0;
                let mut place = None;
                let mut named = None;
                let mut taken = Vec::new();
                for (spot, step) in self.working() {
                    for (at, item) in step.items.iter().enumerate() {
                        let Item::Call(started) = item else {
                            continue;
                        };
                        for (index, helper) in started.helpers.iter().enumerate() {
                            if helper.id == id {
                                place = Some((spot, at, index));
                                named = helper.name.clone().filter(|_| helper.started.is_none());
                            }
                            if helper.state() == State::Running {
                                taken.push(helper.hue);
                            }
                        }
                    }
                }

                // An agent keeps the hue of its earlier tasks, which only a task yet to start looks for
                let same = |helper: &&Helper| helper.started.is_some() && helper.name == named;
                let earlier = match named {
                    Some(_) => self.helpers().filter(same).last().map(|helper| helper.hue),
                    None => None,
                };
                if let Some((spot, at, index)) = place {
                    let step = match spot {
                        Spot::Settled(entry) => {
                            self.entries.get_mut(entry).and_then(Entry::step_mut)
                        }
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
                        // The hue of its agent's earlier tasks or at its place in its call,
                        // or the lowest free one when a running agent holds that
                        if status == ToolCallStatus::InProgress && helper.started.is_none() {
                            let wanted = earlier.unwrap_or(index % HUES);
                            let free = (0..HUES).find(|hue| !taken.contains(hue));
                            helper.hue = match taken.contains(&wanted) {
                                true => free.unwrap_or(wanted),
                                false => wanted,
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

                    let measured = measured(call.meta.as_ref());
                    if measured.interrupted {
                        helper.interrupted = true;
                    }
                    let tokens = Tokens::of(&measured);
                    if tokens.is_some() {
                        helper.tokens = tokens;
                    }

                    // Every update carries its usage so far, so only the last one counts
                    let ended = matches!(
                        fields.status,
                        Some(ToolCallStatus::Completed | ToolCallStatus::Failed)
                    );
                    if ended {
                        if let Some(steps) = measured.steps {
                            let duration = measured.duration.unwrap_or_default();
                            helper.totals = Some(Totals { steps, duration });
                        }
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

                self.revision += 1;
                self.retry_at = None;
                let mut running = None;
                for entry in self.held.iter_mut().rev() {
                    if let Entry::Shell(shell) = entry {
                        if shell.id == id && shell.end == ShellEnd::Running {
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

                let meta = call.meta.as_ref();
                let error = meta.and_then(|meta| meta.get(ERROR));
                let error = error.and_then(Value::as_str).unwrap_or_default().to_owned();
                let interrupted = measured(meta).interrupted;
                shell.end = match (fields.status, interrupted) {
                    (Some(ToolCallStatus::Completed), _) => ShellEnd::Exited,
                    (Some(ToolCallStatus::Failed), true) => ShellEnd::Interrupted(error),
                    (Some(ToolCallStatus::Failed), false) => ShellEnd::Failed(error),
                    _ => ShellEnd::Running,
                };
            }

            SessionUpdate::UsageUpdate(usage) => {
                // 0 means the current model hasn't been measured yet
                self.stats.stale = usage.used == 0;
                if usage.used > 0 {
                    self.stats.context = Some(usage.used);
                }
                self.stats.window = (usage.size > 0).then_some(usage.size);
                let measured = measured(usage.meta.as_ref());
                self.stats.estimated = measured.estimated_output;

                // Model switches and mid-generation estimates carry no `duration` and are not steps
                let Some(duration) = measured.duration else {
                    return false;
                };

                let stats = StepStats {
                    duration,
                    decode: measured.decode_duration,
                    tokens: Tokens::of(&measured),
                };

                self.stats.steps += 1;
                self.stats.tokens += stats.tokens.unwrap_or_default();

                let mut step = self.live.take().unwrap_or_default();
                // A replayed turn has no stop reason, which leaves this as the only sign of an interrupt
                step.interrupted = measured.interrupted;
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
                Outcome::Answered | Outcome::Failed(_) => {}
            }
        }
        self.settle();
    }

    pub(crate) fn done(&mut self, stop: Result<StopReason>) -> Outcome {
        let error = match stop {
            Ok(stop) => {
                let outcome = match stop {
                    StopReason::Cancelled => Outcome::Cancelled,
                    _ => Outcome::Answered,
                };
                self.finish_turn(outcome.clone());
                return outcome;
            }
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
        self.finish_turn(Outcome::Failed(text.clone()));

        // Compaction in the turn's last step may have shown this error already
        let last = self.latest().find_map(Entry::step);
        let shown = match last.and_then(|step| step.items.last()) {
            Some(Item::Call(call)) => call.parts().error,
            _ => None,
        };
        if shown != Some(text.as_str()) {
            self.push(Entry::Failure(text.clone()));
        }
        Outcome::Failed(text)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct StepStats {
    pub(crate) duration: f64,
    /// Seconds, counted from the first token
    pub(crate) decode: Option<f64>,
    pub(crate) tokens: Option<Tokens>,
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
    /// The tasks its code sent agents, in the order it sent them
    pub(crate) helpers: Vec<Helper>,
    /// The agents its code waits for
    pub(crate) waiting: Vec<String>,
    /// Grows with every update of one of its agents
    pub(crate) revision: u64,
}

#[derive(Debug)]
pub(crate) struct Helper {
    pub(crate) id: String,
    pub(crate) task: String,
    /// The agent's, which older sessions may lack
    pub(crate) name: Option<String>,
    pub(crate) status: ToolCallStatus,
    /// What it is doing while it runs, then its answer or why it failed
    pub(crate) text: String,
    /// The end of what it is writing or printing while it runs
    pub(crate) preview: String,
    pub(crate) interrupted: bool,
    /// Unknown on replay, which reports the duration instead
    pub(crate) started: Option<Instant>,
    /// Its agent's, or chosen at its start from the hues no running agent holds,
    /// which no later frame can work out
    pub(crate) hue: usize,
    /// So far
    pub(crate) tokens: Option<Tokens>,
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
                    let name = given(NAME).map(str::to_owned);
                    owner.helpers.push(Helper {
                        id,
                        task: task.to_owned(),
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
                    waiting: Vec::new(),
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
                    let waiting = update.meta.as_ref().and_then(|meta| meta.get(WAITING));
                    if let Some(Value::Array(names)) = waiting {
                        let names = names.iter().filter_map(Value::as_str);
                        call.waiting = names.map(str::to_owned).collect();
                    }
                }
            }

            _ => {}
        }
    }

    /// Tasks its code sent that are still queued or running
    fn helping(&self) -> usize {
        self.helpers().filter(|helper| helper.waiting()).count()
    }

    pub(crate) fn calls(&self) -> impl Iterator<Item = &Call> {
        self.items.iter().filter_map(Item::call)
    }

    /// The tasks its code sent
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

    /// The answer follows the last call or notice
    fn mark_answer(&mut self) {
        let block = |item: &Item| matches!(item, Item::Call(_) | Item::Notice(_));
        let found = self.items.iter().rposition(block);
        let start = found.map_or(0, |index| index + 1);
        for item in &mut self.items[start..] {
            if let Item::Message { answer, .. } = item {
                *answer = true;
            }
        }
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

/// What `_meta` tells of usage and ends, nothing when there is no `_meta`
fn measured(meta: Option<&Meta>) -> Measured {
    let Some(meta) = meta else {
        return Measured::default();
    };
    Measured::deserialize(meta).unwrap_or_default()
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
    use crate::conversation::{Conversation, Entry, Outcome};
    use agent_client_protocol_schema::v1::SessionUpdate;
    use reaper::wire::BACKGROUND;
    use serde_json::json;

    fn chunk(text: &str, id: &str, background: bool) -> SessionUpdate {
        let mut update = json!({
            "sessionUpdate": "user_message_chunk",
            "content": { "type": "text", "text": text },
            "messageId": id,
        });
        if background {
            update["_meta"] = json!({ BACKGROUND: true });
        }
        serde_json::from_value(update).expect("update")
    }

    fn shown(conversation: &Conversation) -> Vec<String> {
        let mut shown = Vec::new();
        for entry in conversation.entries().iter().chain(conversation.held()) {
            let text = match entry {
                Entry::Task(text) => format!("task {text}"),
                Entry::Ended(text) => format!("ended {text}"),
                _ => continue,
            };
            shown.push(text);
        }
        shown
    }

    /// Programs that end as the user sends a task come before it live, as a resume replays them
    #[test]
    fn ended_before_task() {
        let ended = chunk("Programs ended: café.py", "task_1", true);
        let mut live = Conversation::default();
        live.task("Count the cafés");
        live.update(&ended);
        live.finish_turn(Outcome::Answered);

        let mut replayed = Conversation::default();
        replayed.update(&ended);
        replayed.update(&chunk("Count the cafés", "task_2", false));
        replayed.finish_turn(Outcome::Answered);

        assert_eq!(shown(&live), shown(&replayed));
        assert_eq!(live.stats.turns, replayed.stats.turns);

        // A task whose turn failed before its first step stays before programs that ended later
        let mut failed = Conversation::default();
        failed.update(&chunk("Count the cafés", "task_1", false));
        failed.update(&chunk("Programs ended: café.py", "task_2", true));
        let order = ["task Count the cafés", "ended Programs ended: café.py"];
        assert_eq!(shown(&failed), order);
    }
}
