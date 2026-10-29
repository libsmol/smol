use crate::client::{Notice, Refused, AGENT, SHELL, UPDATE};
use crate::tui::render::{Render, Row};
use crate::tui::theme::HELPERS;
use crate::tui::SPIN_EVERY;
use agent_client_protocol_schema::v1::{
    ContentBlock, SessionUpdate, StopReason, ToolCallContent, ToolCallStatus,
};
use anyhow::Result;
use ratatui::style::Color;
use reaper::one_line;
use serde_json::Value;
use std::collections::BTreeSet;
use std::mem;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(crate) enum Entry {
    Info(String),
    Failure(String),
    Task(String),
    Step(Step),
    Shell(Shell),
}

impl Entry {
    fn step(&self) -> Option<&Step> {
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
enum Spot {
    Settled(usize),
    Held(usize),
    Live,
}

/// The rows of the agents of one call with agents at work, counted from the start of its
/// settled entry or, with no entry, of the held rows
#[derive(Debug)]
pub(crate) struct Group {
    pub(crate) call: String,
    pub(crate) entry: Option<usize>,
    pub(crate) rows: Range<usize>,
    /// One of its agents changed after these rows were drawn
    pub(crate) stale: bool,
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

    pub(crate) fn approx(&self) -> &'static str {
        match self.estimated {
            Some(_) => "≈",
            None => "",
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Rows<'a> {
    done: &'a [Row],
    held: &'a [Row],
    starts: &'a [usize],
}

impl<'a> Rows<'a> {
    pub(crate) fn row_at(&self, anchor: (usize, usize)) -> usize {
        let (entry, offset) = anchor;
        let Some(&start) = self.starts.get(entry) else {
            return self.done.len() + offset;
        };
        let next = self.starts.get(entry + 1);
        let end = next.copied().unwrap_or(self.done.len());
        (start + offset).min(end.saturating_sub(1))
    }

    pub(crate) fn len(&self) -> usize {
        self.done.len() + self.held.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn get(&self, index: usize) -> Option<&'a Row> {
        match index.checked_sub(self.done.len()) {
            Some(index) => self.held.get(index),
            None => self.done.get(index),
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &'a Row> {
        self.done.iter().chain(self.held)
    }
}

/// Settled entries are cached per width and expansion, and the rest are redrawn only after a change
#[derive(Default)]
pub(crate) struct Transcript {
    entries: Vec<Entry>,
    /// The task or latest step that this turn may still change, and the entries after it
    held: Vec<Entry>,
    live: Option<Step>,
    stats: Stats,
    rows: Vec<Row>,
    /// Where the first row of each laid-out entry in `entries` sits in `rows`
    starts: Vec<usize>,
    held_rows: Vec<Row>,
    /// Anything that touches `held` or `live` must set this
    changed: bool,
    layout: (u16, bool),
    task_id: Option<String>,
    retry_at: Option<Instant>,
    /// Settled entries with agents at work, or whose agents ended after their rows were drawn
    lively: BTreeSet<usize>,
    /// When the spinners in `groups` were last drawn moving on
    spun: Option<Instant>,
    groups: Vec<Group>,
    /// The rows on screen
    view: Option<Range<usize>>,
    /// Whether scrolling moved `view` since the frame before
    scrolled: bool,
    /// The calls whose agents the last frame drew at the bottom, by id
    pins: Vec<String>,
    /// The session's directory, which agents' titles write as `.`
    cwd: PathBuf,
}

impl Transcript {
    pub(crate) fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            ..Self::default()
        }
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
        self.changed = true;
        match self.held.is_empty() {
            true => self.entries.push(entry),
            false => self.held.push(entry),
        }
    }

    pub(crate) fn task(&mut self, text: &str) {
        self.changed = true;
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
        let asked = matches!(self.held[..], [Entry::Task(_)]);
        asked && self.live.is_none()
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

    pub(crate) fn view(&mut self, rows: Range<usize>, scrolled: bool) {
        self.view = Some(rows);
        self.scrolled = scrolled;
    }

    /// The steps that may hold agents at work, oldest first
    fn working(&self) -> Vec<(Spot, &Step)> {
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
                            taken.push(helper.color);
                        }
                    }
                }
            }

            if let Some((spot, at, index)) = place {
                let step = match spot {
                    Spot::Settled(index) => self.entries.get_mut(index).and_then(Entry::step_mut),
                    Spot::Held(index) => self.held.get_mut(index).and_then(Entry::step_mut),
                    Spot::Live => self.live.as_mut(),
                };
                let item = step.and_then(|step| step.items.get_mut(at));
                let Some(Item::Call(started)) = item else {
                    return false;
                };
                let parent = started.id.clone();
                let helper = &mut started.helpers[index];

                let fields = &call.fields;
                if let Some(status) = fields.status {
                    // The color at its place in its call, or the lowest free one when a running agent holds that
                    if status == ToolCallStatus::InProgress && helper.started.is_none() {
                        let place = HELPERS[index % HELPERS.len()];
                        let free = HELPERS.iter().find(|color| !taken.contains(color));
                        helper.color = match taken.contains(&place) {
                            true => free.copied().unwrap_or(place),
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
                if get("interrupted").and_then(Value::as_bool) == Some(true) {
                    helper.interrupted = true;
                }
                let count = |key: &str| get(key).and_then(Value::as_u64);
                let tokens = count("inputTokens").zip(count("outputTokens"));
                if tokens.is_some() {
                    helper.tokens = tokens;
                }

                // Every update carries its usage so far, so only the last one counts
                let ended = matches!(
                    fields.status,
                    Some(ToolCallStatus::Completed | ToolCallStatus::Failed)
                );
                if ended {
                    if let Some(steps) = count("steps") {
                        let duration = get("duration").and_then(Value::as_f64);
                        let duration = duration.unwrap_or_default();
                        helper.totals = Some(Totals { steps, duration });
                    }
                    let (input, output) = tokens.unwrap_or_default();
                    self.stats.agent_input += input;
                    self.stats.agent_output += output;
                }

                // Only the rows of its call change, and ones not drawn yet come with the held rows
                let group = self.groups.iter_mut().find(|group| group.call == parent);
                match group {
                    Some(group) => group.stale = true,
                    None => self.changed = true,
                }
                return false;
            }
        }

        self.changed = true;
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
                let cwd = &self.cwd;
                self.live.get_or_insert_default().update(update, cwd);
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
                        step.update(update, &self.cwd);
                    }
                    return false;
                };

                let fields = &call.fields;
                if let Some(content) = &fields.content {
                    let (texts, _) = contents(content);
                    shell.output = texts.concat();
                }

                let get = |key: &str| call.meta.as_ref().and_then(|meta| meta.get(key));
                let error = get("error").and_then(Value::as_str).unwrap_or_default();
                let interrupted = get("interrupted").and_then(Value::as_bool);
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
                self.stats.estimated = get("estimatedOutput").and_then(Value::as_u64);

                // Model switches and mid-generation estimates carry no `duration` and are not steps
                let Some(duration) = get("duration").and_then(Value::as_f64) else {
                    return false;
                };

                let stats = StepStats {
                    duration,
                    decode: get("decodeDuration").and_then(Value::as_f64),
                    input: get("inputTokens").and_then(Value::as_u64),
                    output: get("outputTokens").and_then(Value::as_u64),
                };

                self.stats.steps += 1;
                self.stats.input += stats.input.unwrap_or(0);
                self.stats.output += stats.output.unwrap_or(0);

                let flag = |key: &str| get(key).and_then(Value::as_bool).unwrap_or(false);
                let mut step = self.live.take().unwrap_or_default();
                // A replayed turn has no stop reason, which leaves this as the only sign of an interrupt
                step.interrupted = flag("interrupted");
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
        self.changed = true;
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
        let asked = matches!(self.held.last(), Some(Entry::Task(_)));
        if self.live.is_none() && !asked && !notice.step {
            return self.push(Entry::Failure(text));
        }

        let step = self.live.get_or_insert_default();
        step.items.push(Item::Notice(text));
    }

    pub(crate) fn finish_turn(&mut self, outcome: Outcome) {
        self.changed = true;
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
        if error.is::<Refused>() {
            self.stats.turns = self.stats.turns.saturating_sub(1);
        }

        // When the request still fails after every retry,
        // the last attempt's stream is discarded too and the retry count joins the error
        let mut text = format!("{error:#}");
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

    /// Before a layout change, records which entry `row` belongs to and its offset there
    /// for `Rows::row_at` to find after relayout
    pub(crate) fn anchor(&self, row: usize, width: u16, expanded: bool) -> Option<(usize, usize)> {
        if self.layout == (width, expanded) {
            return None;
        }
        // Rows past the cache are the part redrawn every time, counted from its start
        if row >= self.rows.len() {
            let offset = row - self.rows.len();
            return Some((self.starts.len(), offset));
        }

        let after = self.starts.partition_point(|start| *start <= row);
        let entry = after.saturating_sub(1);
        Some((entry, row - self.starts[entry]))
    }

    pub(crate) fn rows(&mut self, width: u16, expanded: bool) -> Rows<'_> {
        if self.layout != (width, expanded) {
            self.layout = (width, expanded);
            self.rows.clear();
            self.starts.clear();
            self.groups.clear();
            // Rows are numbered anew, so the agents at the bottom are found again
            self.view = None;
            self.pins.clear();
            self.changed = true;
        }

        let mut render = Render::new(width, expanded);
        let rendered = self.starts.len();
        for (index, entry) in self.entries.iter().enumerate().skip(rendered) {
            let start = render.rows.len();
            self.starts.push(self.rows.len() + start);
            render.entry(entry);
            for mut group in render.groups.drain(..) {
                group.entry = Some(index);
                group.rows = group.rows.start - start..group.rows.end - start;
                self.groups.push(group);
            }
        }
        self.rows.append(&mut render.rows);

        let redrawn = mem::take(&mut self.changed);
        if redrawn {
            for entry in &self.held {
                render.entry(entry);
            }
            if let Some(step) = &self.live {
                render.step(step);
            }
            self.groups.retain(|group| group.entry.is_some());
            self.groups.append(&mut render.groups);
            self.held_rows = render.rows;
        }

        // Agents at work show a spinner and their time, so the rows of their calls are drawn again
        // as these move on and after one of them changed, in settled entries only while on screen
        let spinning = self.spun.is_none_or(|at| at.elapsed() >= SPIN_EVERY);
        if spinning {
            self.spun = Some(Instant::now());
        }
        let mut at = 0;
        while at < self.groups.len() {
            let group = &self.groups[at];
            let fresh = match group.entry {
                Some(index) => index >= rendered,
                None => redrawn,
            };
            let start = match group.entry {
                Some(index) => self.starts[index],
                None => self.rows.len(),
            };
            let shown = start + group.rows.start..start + group.rows.end;
            let hidden = group.entry.is_some() && !self.on_screen(shown);
            if fresh || hidden || !(group.stale || spinning) {
                at += 1;
                continue;
            }

            let steps: Vec<&Step> = match group.entry {
                Some(index) => self.entries[index].step().into_iter().collect(),
                None => self
                    .held
                    .iter()
                    .filter_map(Entry::step)
                    .chain(&self.live)
                    .collect(),
            };
            let mut calls = steps.into_iter().flat_map(Step::calls);
            let Some(call) = calls.find(|call| call.id == group.call) else {
                self.groups.remove(at);
                continue;
            };
            let mut drawn = Render::new(width, expanded);
            drawn.helpers(call);

            // The rows after these move by as many as they grew or shrank
            let old = group.rows.clone();
            let entry = group.entry;
            let height = drawn.rows.len();
            let moved = |at: usize| at + height - old.len();
            match entry {
                Some(index) => {
                    let start = self.starts[index];
                    let replaced = start + old.start..start + old.end;
                    self.rows.splice(replaced, drawn.rows);
                    for later in &mut self.starts[index + 1..] {
                        *later = moved(*later);
                    }
                }
                None => {
                    self.held_rows.splice(old.clone(), drawn.rows);
                }
            }
            for later in &mut self.groups {
                if later.entry == entry && later.rows.start >= old.end {
                    later.rows = moved(later.rows.start)..moved(later.rows.end);
                }
            }

            // A call keeps its rows apart only while its agents are at work
            if drawn.groups.is_empty() {
                self.groups.remove(at);
                let left = |group: &Group| group.entry.is_some() && group.entry == entry;
                if let (Some(index), false) = (entry, self.groups.iter().any(left)) {
                    self.lively.remove(&index);
                }
                continue;
            }
            let group = &mut self.groups[at];
            group.rows = old.start..old.start + height;
            group.stale = false;
            at += 1;
        }

        Rows {
            done: &self.rows,
            held: &self.held_rows,
            starts: &self.starts,
        }
    }

    /// The agents at work of each call whose rows went off screen, for the bottom of a conversation
    /// `height` rows tall
    pub(crate) fn pinned(&mut self, width: u16, height: usize) -> Vec<Row> {
        let mut hidden = Vec::new();
        let mut pins = Vec::new();
        for (_, step) in self.working() {
            for call in step.calls() {
                // Rows not drawn yet count as on screen
                let found = self.groups.iter().find(|group| group.call == call.id);
                let Some(group) = found else {
                    continue;
                };
                let start = match group.entry {
                    Some(index) => self.starts[index],
                    None => self.rows.len(),
                };
                // Hiding the area moves these rows again, so only scrolling takes agents back out of it
                let kept = self.pins.contains(&call.id) && !self.scrolled;
                let rows = start + group.rows.start..start + group.rows.end;
                if !kept && self.on_screen(rows) {
                    continue;
                }

                pins.push(call.id.clone());
                let waiting = call.helpers.iter().filter(|helper| helper.waiting());
                hidden.extend(waiting);
            }
        }

        let mut render = Render::new(width, false);
        render.pinned(&hidden, height);
        self.pins = pins;
        render.rows
    }

    /// Before the screen is known every row counts as on it
    fn on_screen(&self, rows: Range<usize>) -> bool {
        match &self.view {
            Some(view) => rows.start < view.end && view.start < rows.end,
            None => true,
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
}

#[derive(Debug)]
pub(crate) struct Helper {
    pub(crate) id: String,
    /// The start of its task
    pub(crate) title: String,
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
    /// Taken when it starts and held by no other agent running at the same time
    pub(crate) color: Color,
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
    fn update(&mut self, update: &SessionUpdate, cwd: &Path) {
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
                let parent = meta.and_then(|meta| meta.get("parent"));
                let parent = parent.and_then(Value::as_str).filter(|_| marked);
                if let Some(owner) = parent.and_then(|parent| self.call_mut(parent)) {
                    let asked = call.raw_input.as_ref();
                    let given = |key: &str| asked.and_then(|asked| asked.get(key));
                    let given = |key: &str| given(key).and_then(Value::as_str);
                    let task = given("task").unwrap_or_default();
                    let role = given("role").map(str::to_owned);
                    let name = given("name").map(str::to_owned);
                    owner.helpers.push(Helper {
                        id,
                        title: one_line(task, cwd),
                        role,
                        name,
                        status: call.status,
                        text: String::new(),
                        preview: String::new(),
                        interrupted: false,
                        started: None,
                        color: HELPERS[0],
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

    fn calls(&self) -> impl Iterator<Item = &Call> {
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
        });
    }

    (texts, attachments)
}

/// The closing ``` may not have arrived yet
fn fenced(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("```")?;
    let (language, code) = rest.split_once('\n').unwrap_or((rest, ""));
    let code = code.trim_end().strip_suffix("```").unwrap_or(code);
    Some((language.trim(), code.trim_matches('\n')))
}

#[cfg(test)]
mod tests {
    use crate::tui::transcript::*;
    use crate::tui::SPINNER;
    use agent_client_protocol_schema::v1::{
        ContentChunk, MessageId, Meta, ResourceLink, ToolCall, ToolCallUpdate,
        ToolCallUpdateFields, ToolKind, UsageUpdate,
    };
    use anyhow::anyhow;
    use serde_json::json;

    fn chunk(text: &str) -> ContentChunk {
        let block = ContentBlock::from(text);
        ContentChunk::new(block)
    }

    fn content(text: &str) -> ToolCallContent {
        let block = ContentBlock::from(text);
        ToolCallContent::from(block)
    }

    fn message(text: &str) -> SessionUpdate {
        let chunk = chunk(text);
        SessionUpdate::AgentMessageChunk(chunk)
    }

    fn end_step(transcript: &mut Transcript, input: u64, output: u64) {
        let meta = json!({
            "inputTokens": input,
            "outputTokens": output,
            "decodeDuration": 0.5,
            "duration": 0.25,
        });
        let meta = meta.as_object().cloned();
        let usage = UsageUpdate::new(input + output, 100).meta(meta);
        let ended = transcript.update(&SessionUpdate::UsageUpdate(usage));
        assert!(ended);
    }

    fn rows(transcript: &mut Transcript) -> Vec<String> {
        let rows = transcript.rows(40, false);
        rows.iter().map(|row| row.line.to_string()).collect()
    }

    fn told(text: &str) -> Notice {
        Notice {
            text: text.to_owned(),
            retry_in: None,
            step: false,
            unsaved: false,
        }
    }

    fn retrying(text: &str) -> Notice {
        Notice {
            retry_in: Some(2.0),
            ..told(text)
        }
    }

    #[test]
    fn retries_folded() {
        let mut transcript = Transcript::default();
        transcript.task("Hello");
        let reply = message("Half done");
        transcript.update(&reply);
        transcript.notice(&retrying("model: busy: req_1"));
        assert!(transcript.retry_in().is_some());
        let estimate = UsageUpdate::new(10, 100);
        transcript.update(&SessionUpdate::UsageUpdate(estimate));
        assert!(transcript.retry_in().is_some());
        transcript.update(&reply);
        assert!(transcript.retry_in().is_none());

        transcript.notice(&retrying("model: busy: req_2"));
        let expected = ["", "❯ Hello", "", "  ↻ model: busy: req_2 ×2"];
        assert_eq!(rows(&mut transcript), expected);

        transcript.update(&reply);
        let failed = anyhow!("model: busy: req_3");
        transcript.done(Err(failed));
        let expected = ["", "❯ Hello", "", "✗ model: busy: req_3 (after 2 retries)"];
        assert_eq!(rows(&mut transcript), expected);
    }

    #[test]
    fn notice_survives_retry() {
        let restart = "python: restarted, variables lost";
        let started = |transcript: &mut Transcript| {
            transcript.task("Read the orders");
            let restarted = Notice {
                step: true,
                ..told(restart)
            };
            transcript.notice(&restarted);
            transcript.update(&message("Half done"));
            transcript.notice(&retrying("model: http 503: busy"));
        };

        let mut answered = Transcript::default();
        started(&mut answered);
        answered.update(&message("Read all the orders."));
        end_step(&mut answered, 10, 1);
        answered.finish_turn(Outcome::Answered);
        let noticed = format!("  ✗ {restart}");
        let expected = [
            "",
            "❯ Read the orders",
            "",
            "  ↻ model: http 503: busy",
            &noticed,
            "",
            "◆ Read all the orders.",
            "  0.2s · ⇡10 ⇣1 · 2 t/s",
        ];
        assert_eq!(rows(&mut answered), expected);

        let mut failed = Transcript::default();
        started(&mut failed);
        failed.update(&message("Half done again"));
        let error = anyhow!("model: http 503: busy");
        failed.done(Err(error));
        let expected = [
            "",
            "❯ Read the orders",
            "",
            &noticed,
            "",
            "✗ model: http 503: busy (after 1 retry)",
        ];
        assert_eq!(rows(&mut failed), expected);
    }

    #[test]
    fn failure_shown_once() {
        let mut transcript = Transcript::default();
        transcript.task("Count the lines");
        let compacting = ToolCall::new("call_1", "Compact context")
            .kind(ToolKind::Think)
            .status(ToolCallStatus::InProgress);
        transcript.update(&SessionUpdate::ToolCall(compacting));
        transcript.notice(&retrying("model: busy"));
        let failed = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Failed)
            .content(vec![content("model: bad request")]);
        let failed = ToolCallUpdate::new("call_1", failed);
        transcript.update(&SessionUpdate::ToolCallUpdate(failed));

        let error = anyhow!("model: bad request");
        transcript.done(Err(error));
        let expected = [
            "",
            "❯ Count the lines",
            "",
            "  ⚙ Compact context",
            "  ↻ model: busy",
            "  ✗ model: bad request",
        ];
        assert_eq!(rows(&mut transcript), expected);
    }

    #[test]
    fn retry_after_call() {
        let code = "```python\nprint('café')\n```";
        let started = ToolCall::new("call_1", "eval")
            .kind(ToolKind::Execute)
            .status(ToolCallStatus::Pending)
            .content(vec![content(code)]);
        // The agent closes the calls it never ran before the retry notice
        let closed = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Failed)
            .content(vec![content(code)]);
        let closed = ToolCallUpdate::new("call_1", closed);

        let mut transcript = Transcript::default();
        transcript.task("Read the menu");
        transcript.update(&SessionUpdate::ToolCall(started));
        transcript.update(&SessionUpdate::ToolCallUpdate(closed));
        transcript.notice(&retrying("model: http 503: busy"));
        transcript.update(&message("The menu is in French."));
        end_step(&mut transcript, 10, 1);
        transcript.finish_turn(Outcome::Answered);

        let expected = [
            "",
            "❯ Read the menu",
            "",
            "  ↻ model: http 503: busy",
            "",
            "◆ The menu is in French.",
            "  0.2s · ⇡10 ⇣1 · 2 t/s",
        ];
        assert_eq!(rows(&mut transcript), expected);
    }

    #[test]
    fn code_then_answer() {
        let code = "```python\nprint('checked')\n```";
        let started = ToolCall::new("call_1", "eval")
            .status(ToolCallStatus::Pending)
            .content(vec![content(code)]);
        let finished = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![content(code), content("checked")]);
        let finished = ToolCallUpdate::new("call_1", finished);

        let mut transcript = Transcript::default();
        transcript.task("Check it");
        transcript.update(&SessionUpdate::ToolCall(started));
        transcript.update(&SessionUpdate::ToolCallUpdate(finished));
        end_step(&mut transcript, 10, 1);
        transcript.update(&message("42"));
        end_step(&mut transcript, 20, 2);
        let before = rows(&mut transcript);
        assert!(before.contains(&"  42".to_owned()), "{before:?}");

        transcript.finish_turn(Outcome::Answered);
        let expected = [
            "",
            "❯ Check it",
            "",
            "  │ print('checked')",
            "  ╰ checked",
            "  0.2s · ⇡10 ⇣1 · 2 t/s",
            "",
            "◆ 42",
            "  0.2s · ⇡20 ⇣2 · 4 t/s",
        ];
        assert_eq!(rows(&mut transcript), expected);
    }

    #[test]
    fn messages_by_id() {
        let user = |text: &str, id: Option<&str>| {
            let id = id.map(MessageId::new);
            SessionUpdate::UserMessageChunk(chunk(text).message_id(id))
        };

        let mut transcript = Transcript::default();
        transcript.update(&user("first ", Some("task_1")));
        transcript.update(&user("part", Some("task_1")));
        transcript.update(&user("second", Some("task_2")));
        transcript.update(&user("old ", None));
        transcript.update(&user("chunks", None));
        transcript.task("second");
        transcript.task("now");

        let expected = ["first part", "second", "old chunks", "second", "now"];
        assert_eq!(transcript.sent(), expected);
    }

    #[test]
    fn repeats_sent_once() {
        let mut transcript = Transcript::default();
        transcript.task("Run the tests");
        transcript.task("Run the tests\n");
        transcript.task("Fix the café menu");
        transcript.task("Run the tests");

        let expected = ["Run the tests", "Fix the café menu", "Run the tests"];
        assert_eq!(transcript.sent(), expected);
    }

    #[test]
    fn amend_keeps_turn() {
        let flag = (UPDATE.to_owned(), json!(true));
        let meta = Meta::from_iter([flag]);
        let id = MessageId::new("task_2");
        let added = chunk("Add one more").message_id(id).meta(meta);
        let added = SessionUpdate::UserMessageChunk(added);
        let expected = [
            "",
            "❯ Do this first",
            "",
            "  Working on it",
            "  0.2s · ⇡10 ⇣1 · 2 t/s",
            "",
            "❯ Add one more",
            "",
            "◆ Done",
            "  0.2s · ⇡20 ⇣2 · 4 t/s",
        ];

        let mut transcript = Transcript::default();
        transcript.task("Do this first");
        transcript.update(&message("Working on it"));
        end_step(&mut transcript, 10, 1);
        transcript.update(&added);
        transcript.update(&message("Done"));
        end_step(&mut transcript, 20, 2);
        transcript.finish_turn(Outcome::Answered);

        assert_eq!(transcript.stats().turns, 1);
        assert_eq!(transcript.sent(), ["Do this first", "Add one more"]);
        assert_eq!(rows(&mut transcript), expected);
    }

    #[test]
    fn cancelled_no_answer() {
        let mut transcript = Transcript::default();
        transcript.task("First");
        let thought = SessionUpdate::AgentThoughtChunk(chunk("Think it through.\nThen"));
        transcript.update(&thought);
        let reply = message("Working on it");
        transcript.update(&reply);
        // Sent by the agent for the interrupted step
        transcript.notice(&told("interrupted"));
        transcript.finish_turn(Outcome::Cancelled);
        transcript.task("Second");

        let expected = [
            "",
            "❯ First",
            "",
            "  ✻ Think it through.",
            "  Working on it",
            "  ✗ interrupted",
            "",
            "❯ Second",
        ];
        assert_eq!(rows(&mut transcript), expected);
    }

    #[test]
    fn failed_no_answer() {
        let mut transcript = Transcript::default();
        transcript.task("First");
        let reply = message("Working on it");
        transcript.update(&reply);
        end_step(&mut transcript, 10, 1);
        transcript.finish_turn(Outcome::Failed);
        transcript.task("Second");

        let rows = rows(&mut transcript);
        assert_eq!(rows[3], "  Working on it", "{rows:?}");
    }

    #[test]
    fn notice_outside_step() {
        let mut transcript = Transcript::default();
        transcript.notice(&told("Retry later"));
        transcript.task("Work it out");
        transcript.notice(&told("Retrying"));
        transcript.update(&message("The answer is 42"));
        end_step(&mut transcript, 10, 1);
        transcript.notice(&told("history: disk full"));
        transcript.finish_turn(Outcome::Answered);

        let expected = [
            "",
            "✗ Retry later",
            "",
            "❯ Work it out",
            "",
            "  ✗ Retrying",
            "",
            "◆ The answer is 42",
            "  0.2s · ⇡10 ⇣1 · 2 t/s",
            "",
            "✗ history: disk full",
        ];
        assert_eq!(rows(&mut transcript), expected);
    }

    #[test]
    fn step_notice_kept() {
        let mut transcript = Transcript::default();
        transcript.task("Count the lines");
        let reply = message("Counting the first part");
        transcript.update(&reply);
        end_step(&mut transcript, 10, 1);
        // The second step is interrupted before any text, and a replayed turn has no stop reason
        let stopped = Notice {
            step: true,
            ..told("interrupted")
        };
        transcript.notice(&stopped);
        let meta = json!({ "duration": 1.25, "interrupted": true });
        let meta = meta.as_object().cloned();
        let usage = UsageUpdate::new(11, 100).meta(meta);
        transcript.update(&SessionUpdate::UsageUpdate(usage));
        transcript.finish_turn(Outcome::Answered);

        let expected = [
            "",
            "❯ Count the lines",
            "",
            "  Counting the first part",
            "  0.2s · ⇡10 ⇣1 · 2 t/s",
            "",
            "  ✗ interrupted",
            "  1.2s",
        ];
        assert_eq!(rows(&mut transcript), expected);
        let Some(Entry::Step(step)) = transcript.entries.last() else {
            panic!("step");
        };
        assert!(step.interrupted);
    }

    #[test]
    fn anchor_kept() {
        let mut transcript = Transcript::default();
        for n in 1..=3 {
            let task = format!("Task {n}");
            transcript.task(&task);
            let lines: Vec<String> = (1..=20)
                .map(|line| format!("Output {n}, line {line}"))
                .collect();
            let output = lines.join("\n");
            let code = format!("```python\nprint({n})\n```");
            let done = ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .content(vec![content(&code), content(&output)]);
            let id = format!("call_{n}");
            let started = ToolCall::new(id.clone(), "eval").status(ToolCallStatus::Pending);
            transcript.update(&SessionUpdate::ToolCall(started));
            let finished = ToolCallUpdate::new(id, done);
            transcript.update(&SessionUpdate::ToolCallUpdate(finished));
            end_step(&mut transcript, 10, 1);
            transcript.finish_turn(Outcome::Answered);
        }

        let folded = rows(&mut transcript);
        let row = folded.iter().position(|row| row == "  │ print(3)");
        let row = row.expect("third");
        let anchor = transcript.anchor(row, 40, true).expect("anchor");
        let expanded = transcript.rows(40, true);
        let top = expanded.row_at(anchor);
        let shown = expanded.get(top).map(|row| row.line.to_string());
        assert_eq!(shown.as_deref(), Some("  │ print(3)"));
        assert_eq!(transcript.anchor(top, 40, true), None);

        // The blank row above the code is the first row of the third step
        let blank = transcript.anchor(top - 1, 40, false).expect("blank");
        let refolded = transcript.rows(40, false);
        assert_eq!(refolded.row_at(blank), row - 1);
    }

    #[test]
    fn transcript_stats() {
        let mut transcript = Transcript::default();
        transcript.task("Tally");
        end_step(&mut transcript, 10, 1);
        end_step(&mut transcript, 20, 2);

        let stats = transcript.stats();
        assert_eq!((stats.turns, stats.steps), (1, 2));
        assert_eq!((stats.input, stats.output), (30, 3));
        assert_eq!((stats.context, stats.window), (Some(22), Some(100)));
        assert_eq!(stats.ratio(), Some(0.22));

        let switched = SessionUpdate::UsageUpdate(UsageUpdate::new(0, 200));
        let ended = transcript.update(&switched);
        assert!(!ended);
        let stats = transcript.stats();
        assert_eq!((stats.context, stats.window), (Some(22), Some(200)));
        assert_eq!(stats.ratio(), None);
        end_step(&mut transcript, 30, 3);
        let stats = transcript.stats();
        assert_eq!((stats.steps, stats.ratio()), (3, Some(0.33)));
    }

    #[test]
    fn estimate_replaced() {
        let mut transcript = Transcript::default();
        transcript.task("Estimate it");
        end_step(&mut transcript, 10, 1);

        let meta = json!({ "estimatedOutput": 7 });
        let meta = meta.as_object().cloned();
        let usage = UsageUpdate::new(18, 100).meta(meta);
        transcript.update(&SessionUpdate::UsageUpdate(usage));
        let stats = transcript.stats();
        assert_eq!((stats.context, stats.estimated), (Some(18), Some(7)));
        assert_eq!(stats.approx(), "≈");

        end_step(&mut transcript, 20, 2);
        let stats = transcript.stats();
        assert_eq!((stats.context, stats.estimated), (Some(22), None));
        assert_eq!(stats.approx(), "");
    }

    #[test]
    fn rows_cached() {
        let mut transcript = Transcript::default();
        transcript.push(Entry::Info("Started".to_owned()));
        let rows = transcript.rows(40, false).len();

        transcript.push(Entry::Info("Finished".to_owned()));
        assert_eq!(transcript.rows(40, false).len(), rows + 2);
        assert_eq!(transcript.starts.len(), 2);

        let reply = message("Working on it");
        transcript.update(&reply);
        assert_eq!(transcript.rows(40, false).len(), rows + 4);
        // Changes made without an update aren't redrawn
        transcript.live = None;
        assert_eq!(transcript.rows(40, false).len(), rows + 4);
        assert_eq!(transcript.rows(20, false).len(), rows + 2);
    }

    #[test]
    fn shell_tracked() {
        let mut transcript = Transcript::default();
        let marked = Meta::from_iter([(SHELL.to_owned(), json!(true))]);
        let started = ToolCall::new("call_1", "ls docs").meta(marked);
        transcript.update(&SessionUpdate::ToolCall(started));
        let running = ToolCallUpdateFields::new().content(vec![content("notes.md\n")]);
        let running = ToolCallUpdate::new("call_1", running);
        transcript.update(&SessionUpdate::ToolCallUpdate(running));
        assert!(transcript.shelling());
        assert_eq!(rows(&mut transcript), ["", "! ls docs", "  ╰ notes.md"]);

        let error = Meta::from_iter([("error".to_owned(), json!("exit 2"))]);
        let failed = ToolCallUpdateFields::new().status(ToolCallStatus::Failed);
        let failed = ToolCallUpdate::new("call_1", failed).meta(error);
        transcript.update(&SessionUpdate::ToolCallUpdate(failed));
        assert!(!transcript.shelling());

        // After it ends, calls with the same ID or without the flag belong to the model
        let late = ToolCallUpdateFields::new().content(vec![content("todo.md")]);
        let late = ToolCallUpdate::new("call_1", late);
        transcript.update(&SessionUpdate::ToolCallUpdate(late));
        let code = ToolCall::new("call_2", "eval");
        transcript.update(&SessionUpdate::ToolCall(code));
        let expected = [
            "",
            "! ls docs",
            "  ├ notes.md",
            "  ✗ exit 2",
            "",
            "  ⚙ eval",
        ];
        assert_eq!(rows(&mut transcript), expected);

        // Interrupts are recognized by the flag
        let marked = Meta::from_iter([(SHELL.to_owned(), json!(true))]);
        let started = ToolCall::new("call_3", "sleep 30").meta(marked);
        transcript.update(&SessionUpdate::ToolCall(started));
        let meta = json!({ "error": "interrupted", "interrupted": true });
        let meta = meta.as_object().cloned();
        let stopped = ToolCallUpdateFields::new().status(ToolCallStatus::Failed);
        let stopped = ToolCallUpdate::new("call_3", stopped).meta(meta);
        transcript.update(&SessionUpdate::ToolCallUpdate(stopped));
        let Some(Entry::Shell(shell)) = transcript.held.last() else {
            panic!("shell");
        };
        assert!(shell.interrupted);
    }

    #[test]
    fn fence_split() {
        assert_eq!(fenced("```python\nx = 1\n```"), Some(("python", "x = 1")));
        assert_eq!(
            fenced("```python\nprint('café')"),
            Some(("python", "print('café')"))
        );
        assert_eq!(fenced("No code block"), None);
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

        let cwd = Path::new("");
        let mut step = Step::default();
        step.update(&message("Let me look."), cwd);
        let started = ToolCall::new("call_1", "eval").status(ToolCallStatus::Pending);
        step.update(&SessionUpdate::ToolCall(started), cwd);
        assert_eq!(replied(&step), "");

        step.update(&message("The answer is "), cwd);
        step.update(&message("42."), cwd);
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

    /// Starts a call that starts `count` agents, the first of them named
    fn started(transcript: &mut Transcript, count: usize) {
        let code = ToolCall::new("call_1", "eval").status(ToolCallStatus::InProgress);
        transcript.update(&SessionUpdate::ToolCall(code));
        for n in 1..=count {
            let marked = json!({ "_smol/agent": true, "parent": "call_1" });
            let task = format!("Count the cafés in menu {n}");
            let name = (n == 1).then_some("cafés");
            let asked = json!({ "task": task, "role": null, "name": name });
            let id = format!("call_1.{n}");
            let opened = ToolCall::new(id, format!("agent: {task}"))
                .raw_input(asked)
                .meta(marked.as_object().cloned());
            transcript.update(&SessionUpdate::ToolCall(opened));
        }
    }

    fn moved(transcript: &mut Transcript, n: usize, status: ToolCallStatus) {
        let fields = ToolCallUpdateFields::new().status(status);
        let update = ToolCallUpdate::new(format!("call_1.{n}"), fields);
        transcript.update(&SessionUpdate::ToolCallUpdate(update));
    }

    #[test]
    fn helper_colors() {
        let mut transcript = Transcript::default();
        started(&mut transcript, 10);
        for n in (1..=8).rev() {
            moved(&mut transcript, n, ToolCallStatus::InProgress);
        }
        moved(&mut transcript, 3, ToolCallStatus::Completed);
        moved(&mut transcript, 9, ToolCallStatus::InProgress);
        moved(&mut transcript, 1, ToolCallStatus::Completed);
        moved(&mut transcript, 10, ToolCallStatus::InProgress);
        moved(&mut transcript, 9, ToolCallStatus::InProgress);

        let step = transcript.live.as_ref().expect("live");
        let [Item::Call(call)] = &step.items[..] else {
            panic!("{:?}", step.items);
        };
        let colors: Vec<Color> = call.helpers.iter().map(|helper| helper.color).collect();
        let places = [0, 1, 2, 3, 4, 5, 6, 7, 2, 0];
        assert_eq!(colors, places.map(|place| HELPERS[place]));
        assert_eq!(call.helpers[0].name.as_deref(), Some("cafés"));
    }

    /// An agent keeps updating under its call after the step that started it has ended
    #[test]
    fn background_followed() {
        let mut transcript = Transcript::default();
        transcript.task("Count the cafés");
        started(&mut transcript, 1);
        moved(&mut transcript, 1, ToolCallStatus::InProgress);
        let ended = |transcript: &mut Transcript| {
            let meta = json!({ "duration": 0.5 });
            let usage = UsageUpdate::new(0, 1000).meta(meta.as_object().cloned());
            transcript.update(&SessionUpdate::UsageUpdate(usage));
        };
        ended(&mut transcript);
        assert_eq!(transcript.awaited(), 0);

        let said = ContentChunk::new(ContentBlock::from("Started them."));
        transcript.update(&SessionUpdate::AgentMessageChunk(said));
        ended(&mut transcript);
        assert_eq!((transcript.helping(), transcript.awaited()), (1, 1));
        transcript.amend("Count the naïve ones too");
        assert_eq!((transcript.helping(), transcript.awaited()), (1, 0));

        moved(&mut transcript, 1, ToolCallStatus::Completed);
        assert_eq!((transcript.helping(), transcript.awaited()), (0, 0));
        let Some(Entry::Step(first)) = transcript.entries.last() else {
            panic!("{}", transcript.entries.len());
        };
        let Item::Call(call) = &first.items[0] else {
            panic!("{:?}", first.items);
        };
        assert_eq!(call.helpers[0].status, ToolCallStatus::Completed);
    }

    /// Only scrolling or a new layout takes a call's agents back out of the area at the bottom
    #[test]
    fn hidden_pinned() {
        let mut transcript = Transcript::default();
        started(&mut transcript, 2);
        moved(&mut transcript, 1, ToolCallStatus::InProgress);
        moved(&mut transcript, 2, ToolCallStatus::InProgress);
        let pinned = |transcript: &mut Transcript| {
            transcript.rows(60, false);
            let area = transcript.pinned(60, 40);
            let shown: Vec<String> = area.iter().map(|row| row.line.to_string()).collect();
            shown.iter().filter(|row| row.contains("menu ")).count()
        };
        let rows = transcript.rows(60, false);
        let second = rows
            .iter()
            .position(|row| row.line.to_string().contains("menu 2"));
        let second = second.expect("second");

        // Only the second agent is on screen at first
        transcript.view(second..second + 40, false);
        assert_eq!(pinned(&mut transcript), 0);
        transcript.view(30..70, false);
        assert_eq!(pinned(&mut transcript), 2);
        transcript.view(0..40, false);
        assert_eq!(pinned(&mut transcript), 2);
        transcript.view(0..40, true);
        assert_eq!(pinned(&mut transcript), 0);
        transcript.view(30..70, true);
        assert_eq!(pinned(&mut transcript), 2);
        transcript.rows(70, false);
        assert!(transcript.pinned(70, 40).is_empty());
    }

    /// Drawing a call's agents again on their own gives the rows drawing everything anew does
    #[test]
    fn groups_redrawn() {
        let anew = |transcript: &mut Transcript| {
            // Spinners move on between the two
            let shown = |row: &Row| row.line.to_string().replace(SPINNER, "*");
            let rows = transcript.rows(60, false);
            let drawn: Vec<String> = rows.iter().map(shown).collect();
            let mut fresh = Render::new(60, false);
            let mut starts = Vec::new();
            for entry in &transcript.entries {
                starts.push(fresh.rows.len());
                fresh.entry(entry);
            }
            for entry in &transcript.held {
                fresh.entry(entry);
            }
            let expected: Vec<String> = fresh.rows.iter().map(shown).collect();
            assert_eq!(drawn, expected);
            assert_eq!(transcript.starts, starts);
        };
        let previewed = |transcript: &mut Transcript, preview: &str| {
            let content = vec![content("writing x = 1"), content(preview)];
            let fields = ToolCallUpdateFields::new().content(content);
            let update = ToolCallUpdate::new("call_1.1", fields);
            transcript.update(&SessionUpdate::ToolCallUpdate(update));
        };

        let mut transcript = Transcript::default();
        transcript.task("Count the cafés");
        started(&mut transcript, 2);
        moved(&mut transcript, 1, ToolCallStatus::InProgress);
        end_step(&mut transcript, 10, 1);
        transcript.push(Entry::Info("Counted the naïve ones".to_owned()));
        anew(&mut transcript);
        assert!(!transcript.changed);
        previewed(&mut transcript, "x = 1\nprint(x)");
        assert!(!transcript.changed);
        anew(&mut transcript);

        transcript.finish_turn(Outcome::Answered);
        transcript.push(Entry::Info("Done with the résumé".to_owned()));
        anew(&mut transcript);
        previewed(&mut transcript, "x = 1");
        anew(&mut transcript);
        moved(&mut transcript, 2, ToolCallStatus::InProgress);
        previewed(&mut transcript, "x = 1\ny = 2\nprint(x, y)");
        anew(&mut transcript);

        // A call whose agents all ended keeps its rows settled for good
        moved(&mut transcript, 1, ToolCallStatus::Completed);
        moved(&mut transcript, 2, ToolCallStatus::Completed);
        anew(&mut transcript);
        assert!(transcript.groups.is_empty(), "{:?}", transcript.groups);
        assert!(transcript.lively.is_empty(), "{:?}", transcript.lively);
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
        let cwd = Path::new("");
        let mut step = Step::default();
        let started = ToolCall::new("call_1", "eval").status(ToolCallStatus::Pending);
        step.update(&SessionUpdate::ToolCall(started), cwd);

        let image = ResourceLink::new("screenshot.png", "file:///copies/a1.png")
            .mime_type("image/png".to_owned());
        let image = ToolCallContent::from(ContentBlock::ResourceLink(image));
        let file = ResourceLink::new("notes", "file:///copies/a2");
        let file = ToolCallContent::from(ContentBlock::ResourceLink(file));
        let finished = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![content("hello"), image, file]);
        let finished = ToolCallUpdate::new("call_1", finished);
        step.update(&SessionUpdate::ToolCallUpdate(finished), cwd);
        let unknown = ToolCallUpdateFields::new().status(ToolCallStatus::Failed);
        let unknown = ToolCallUpdate::new("call_9", unknown);
        step.update(&SessionUpdate::ToolCallUpdate(unknown), cwd);

        let [Item::Call(call)] = &step.items[..] else {
            panic!("{:?}", step.items);
        };
        assert_eq!(call.status, ToolCallStatus::Completed);
        assert_eq!(call.content, ["hello"]);
        let attached = |kind: &str, name: &str| Attachment {
            kind: kind.to_owned(),
            name: name.to_owned(),
        };
        let expected = [
            attached("image", "screenshot.png"),
            attached("file", "notes"),
        ];
        assert_eq!(call.attachments, expected);
    }
}
