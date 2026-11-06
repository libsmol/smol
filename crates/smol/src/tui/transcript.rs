use crate::conversation::{Conversation, Entry, Step};
use crate::tui::render::{Render, Row};
use crate::tui::SPIN_EVERY;
use std::ops::Range;
use std::path::PathBuf;
use std::time::Instant;

const UNSENT_HINT: &str = "Goes to the model with your next message";

/// The rows of the agents of one call with agents at work, counted from the start of its
/// settled entry or, with no entry, of the held rows
#[derive(Debug)]
pub(crate) struct Group {
    pub(crate) call: String,
    pub(crate) entry: Option<usize>,
    pub(crate) rows: Range<usize>,
    /// The revision of the call these rows were drawn at
    pub(crate) drawn: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct Rows<'a> {
    settled: &'a [Row],
    held: &'a [Row],
    starts: &'a [usize],
}

impl<'a> Rows<'a> {
    pub(crate) fn row_at(&self, anchor: (usize, usize)) -> usize {
        let (entry, offset) = anchor;
        let Some(&start) = self.starts.get(entry) else {
            return self.settled.len() + offset;
        };
        let next = self.starts.get(entry + 1);
        let end = next.copied().unwrap_or(self.settled.len());
        (start + offset).min(end.saturating_sub(1))
    }

    pub(crate) fn len(&self) -> usize {
        self.settled.len() + self.held.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn get(&self, index: usize) -> Option<&'a Row> {
        match index.checked_sub(self.settled.len()) {
            Some(index) => self.held.get(index),
            None => self.settled.get(index),
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &'a Row> {
        self.settled.iter().chain(self.held)
    }
}

/// Caches settled entries per width and expansion and redraws the rest only after a change,
/// so a new conversation needs a new transcript
#[derive(Default)]
pub(crate) struct Transcript {
    settled: Vec<Row>,
    /// Where the first row of each laid-out settled entry sits in `settled`
    starts: Vec<usize>,
    held: Vec<Row>,
    /// The conversation's revision when `held` was drawn
    drawn: u64,
    /// Width, expansion, and how many pictures had loaded
    layout: (u16, bool, u64),
    /// How many pictures had loaded when the frame began, which `anchor` and `rows` must agree on
    pub(crate) pictures: u64,
    /// When the spinners in `groups` were last drawn moving on
    spun: Option<Instant>,
    groups: Vec<Group>,
    /// The rows on screen
    view: Option<Range<usize>>,
    /// Whether scrolling moved `view` since the frame before
    scrolled: bool,
    /// The calls whose agents the last frame drew at the bottom, by id
    pins: Vec<String>,
    /// The session's directory, which agents' tasks write as `.`
    cwd: PathBuf,
}

impl Transcript {
    pub(crate) fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            ..Self::default()
        }
    }

    pub(crate) fn view(&mut self, rows: Range<usize>, scrolled: bool) {
        self.view = Some(rows);
        self.scrolled = scrolled;
    }

    /// Before a layout change, records which entry `row` belongs to and its offset there
    /// for `Rows::row_at` to find after relayout
    pub(crate) fn anchor(&self, row: usize, width: u16, expanded: bool) -> Option<(usize, usize)> {
        if self.layout == (width, expanded, self.pictures) {
            return None;
        }

        // Rows past the cache are the part redrawn every time, counted from its start
        if row >= self.settled.len() {
            let offset = row - self.settled.len();
            return Some((self.starts.len(), offset));
        }

        let after = self.starts.partition_point(|start| *start <= row);
        let entry = after.saturating_sub(1);
        Some((entry, row - self.starts[entry]))
    }

    fn render(&self, width: u16, expanded: bool) -> Render {
        let mut render = Render::new(width, expanded);
        render.cwd = self.cwd.clone();
        render
    }

    pub(crate) fn rows(&mut self, talk: &Conversation, width: u16, expanded: bool) -> Rows<'_> {
        // A picture that finished loading changes the height of its rows
        let layout = (width, expanded, self.pictures);
        let relaid = self.layout != layout;
        if relaid {
            self.layout = layout;
            self.settled.clear();
            self.starts.clear();
            self.groups.clear();
            // Rows are numbered anew, so the agents at the bottom are found again
            self.view = None;
            self.pins.clear();
        }

        let mut render = self.render(width, expanded);
        let rendered = self.starts.len();
        for (index, entry) in talk.entries().iter().enumerate().skip(rendered) {
            let start = render.rows.len();
            self.starts.push(self.settled.len() + start);
            render.entry(entry);
            for mut group in render.groups.drain(..) {
                group.entry = Some(index);
                group.rows = group.rows.start - start..group.rows.end - start;
                self.groups.push(group);
            }
        }
        self.settled.append(&mut render.rows);

        let redrawn = relaid || self.drawn != talk.revision();
        if redrawn {
            self.drawn = talk.revision();
            for entry in talk.held() {
                render.entry(entry);
            }
            if let Some(step) = talk.live() {
                render.step(step);
            }
            if talk.shell_unsent() {
                let hint = Entry::Info(UNSENT_HINT.to_owned());
                render.entry(&hint);
            }
            self.groups.retain(|group| group.entry.is_some());
            self.groups.append(&mut render.groups);
            self.held = render.rows;
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
                None => self.settled.len(),
            };
            let shown = start + group.rows.start..start + group.rows.end;
            let hidden = group.entry.is_some() && !self.on_screen(shown);
            if fresh || hidden {
                at += 1;
                continue;
            }

            let steps: Vec<&Step> = match group.entry {
                Some(index) => talk.entries()[index].step().into_iter().collect(),
                None => {
                    let held = talk.held().iter().filter_map(Entry::step);
                    held.chain(talk.live()).collect()
                }
            };
            let mut calls = steps.into_iter().flat_map(Step::calls);
            let Some(call) = calls.find(|call| call.id == group.call) else {
                self.groups.remove(at);
                continue;
            };
            if !spinning && call.revision == group.drawn {
                at += 1;
                continue;
            }

            let mut drawn = self.render(width, expanded);
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
                    self.settled.splice(replaced, drawn.rows);
                    for later in &mut self.starts[index + 1..] {
                        *later = moved(*later);
                    }
                }
                None => {
                    self.held.splice(old.clone(), drawn.rows);
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
                continue;
            }
            let group = &mut self.groups[at];
            group.rows = old.start..old.start + height;
            group.drawn = call.revision;
            at += 1;
        }

        Rows {
            settled: &self.settled,
            held: &self.held,
            starts: &self.starts,
        }
    }

    /// The agents at work of each call whose rows went off screen, for the bottom of a conversation
    /// `height` rows tall
    pub(crate) fn pinned(&mut self, talk: &Conversation, width: u16, height: usize) -> Vec<Row> {
        let mut hidden = Vec::new();
        let mut pins = Vec::new();
        for (_, step) in talk.working() {
            for call in step.calls() {
                // Rows not drawn yet count as on screen
                let found = self.groups.iter().find(|group| group.call == call.id);
                let Some(group) = found else {
                    continue;
                };
                let start = match group.entry {
                    Some(index) => self.starts[index],
                    None => self.settled.len(),
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

        let mut render = self.render(width, false);
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

#[cfg(test)]
mod tests {
    use crate::client::Notice;
    use crate::conversation::{Entry, Outcome};
    use crate::tests::{chunk, content, end_step, message, moved, retrying, started, told};
    use crate::tui::transcript::*;
    use crate::tui::SPINNER;
    use agent_client_protocol_schema::v1::{
        MessageId, Meta, SessionUpdate, ToolCall, ToolCallStatus, ToolCallUpdate,
        ToolCallUpdateFields, ToolKind, UsageUpdate,
    };
    use anyhow::anyhow;
    use serde_json::json;

    fn rows(transcript: &mut Transcript, talk: &Conversation) -> Vec<String> {
        let rows = transcript.rows(talk, 40, false);
        rows.iter().map(|row| row.line.to_string()).collect()
    }

    #[test]
    fn retries_folded() {
        let mut talk = Conversation::default();
        let mut transcript = Transcript::default();
        talk.task("Hello");
        let reply = message("Half done");
        talk.update(&reply);
        talk.notice(&retrying("model: busy: req_1"));
        assert!(talk.retry_in().is_some());
        let estimate = UsageUpdate::new(10, 100);
        talk.update(&SessionUpdate::UsageUpdate(estimate));
        assert!(talk.retry_in().is_some());
        talk.update(&reply);
        assert!(talk.retry_in().is_none());

        talk.notice(&retrying("model: busy: req_2"));
        let expected = ["", "❯ Hello", "", "  ↻ model: busy: req_2 ×2"];
        assert_eq!(rows(&mut transcript, &talk), expected);

        talk.update(&reply);
        let failed = anyhow!("model: busy: req_3");
        talk.done(Err(failed));
        let expected = ["", "❯ Hello", "", "✗ model: busy: req_3 (after 2 retries)"];
        assert_eq!(rows(&mut transcript, &talk), expected);
    }

    #[test]
    fn notice_survives_retry() {
        let restart = "python: restarted, variables lost";
        let started = |talk: &mut Conversation| {
            talk.task("Read the orders");
            let restarted = Notice {
                step: true,
                ..told(restart)
            };
            talk.notice(&restarted);
            talk.update(&message("Half done"));
            talk.notice(&retrying("model: http 503: busy"));
        };

        let mut answered = Conversation::default();
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
        let mut transcript = Transcript::default();
        assert_eq!(rows(&mut transcript, &answered), expected);

        let mut failed = Conversation::default();
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
        let mut transcript = Transcript::default();
        assert_eq!(rows(&mut transcript, &failed), expected);
    }

    #[test]
    fn failure_shown_once() {
        let mut talk = Conversation::default();
        talk.task("Count the lines");
        let compacting = ToolCall::new("call_1", "Compact context")
            .kind(ToolKind::Think)
            .status(ToolCallStatus::InProgress);
        talk.update(&SessionUpdate::ToolCall(compacting));
        talk.notice(&retrying("model: busy"));
        let failed = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Failed)
            .content(vec![content("model: bad request")]);
        let failed = ToolCallUpdate::new("call_1", failed);
        talk.update(&SessionUpdate::ToolCallUpdate(failed));

        let error = anyhow!("model: bad request");
        talk.done(Err(error));
        let expected = [
            "",
            "❯ Count the lines",
            "",
            "  ⚙ Compact context",
            "  ↻ model: busy",
            "  ✗ model: bad request",
        ];
        let mut transcript = Transcript::default();
        assert_eq!(rows(&mut transcript, &talk), expected);
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

        let mut talk = Conversation::default();
        talk.task("Read the menu");
        talk.update(&SessionUpdate::ToolCall(started));
        talk.update(&SessionUpdate::ToolCallUpdate(closed));
        talk.notice(&retrying("model: http 503: busy"));
        talk.update(&message("The menu is in French."));
        end_step(&mut talk, 10, 1);
        talk.finish_turn(Outcome::Answered);

        let expected = [
            "",
            "❯ Read the menu",
            "",
            "  ↻ model: http 503: busy",
            "",
            "◆ The menu is in French.",
            "  0.2s · ⇡10 ⇣1 · 2 t/s",
        ];
        let mut transcript = Transcript::default();
        assert_eq!(rows(&mut transcript, &talk), expected);
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

        let mut talk = Conversation::default();
        let mut transcript = Transcript::default();
        talk.task("Check it");
        talk.update(&SessionUpdate::ToolCall(started));
        talk.update(&SessionUpdate::ToolCallUpdate(finished));
        end_step(&mut talk, 10, 1);
        talk.update(&message("42"));
        end_step(&mut talk, 20, 2);
        let before = rows(&mut transcript, &talk);
        assert!(before.contains(&"  42".to_owned()), "{before:?}");

        talk.finish_turn(Outcome::Answered);
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
        assert_eq!(rows(&mut transcript, &talk), expected);
    }

    #[test]
    fn amend_keeps_turn() {
        let flag = ("_smol/update".to_owned(), json!(true));
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

        let mut talk = Conversation::default();
        talk.task("Do this first");
        talk.update(&message("Working on it"));
        end_step(&mut talk, 10, 1);
        talk.update(&added);
        talk.update(&message("Done"));
        end_step(&mut talk, 20, 2);
        talk.finish_turn(Outcome::Answered);

        assert_eq!(talk.stats().turns, 1);
        assert_eq!(talk.sent(), ["Do this first", "Add one more"]);
        let mut transcript = Transcript::default();
        assert_eq!(rows(&mut transcript, &talk), expected);
    }

    #[test]
    fn cancelled_no_answer() {
        let mut talk = Conversation::default();
        talk.task("First");
        let thought = SessionUpdate::AgentThoughtChunk(chunk("Think it through.\nThen"));
        talk.update(&thought);
        let reply = message("Working on it");
        talk.update(&reply);
        // Sent by the agent for the interrupted step
        talk.notice(&told("interrupted"));
        talk.finish_turn(Outcome::Cancelled);
        talk.task("Second");

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
        let mut transcript = Transcript::default();
        assert_eq!(rows(&mut transcript, &talk), expected);
    }

    #[test]
    fn failed_no_answer() {
        let mut talk = Conversation::default();
        talk.task("First");
        let reply = message("Working on it");
        talk.update(&reply);
        end_step(&mut talk, 10, 1);
        talk.finish_turn(Outcome::Failed);
        talk.task("Second");

        let mut transcript = Transcript::default();
        let rows = rows(&mut transcript, &talk);
        assert_eq!(rows[3], "  Working on it", "{rows:?}");
    }

    #[test]
    fn notice_outside_step() {
        let mut talk = Conversation::default();
        talk.notice(&told("Retry later"));
        talk.task("Work it out");
        talk.notice(&told("Retrying"));
        talk.update(&message("The answer is 42"));
        end_step(&mut talk, 10, 1);
        talk.notice(&told("history: disk full"));
        talk.finish_turn(Outcome::Answered);

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
        let mut transcript = Transcript::default();
        assert_eq!(rows(&mut transcript, &talk), expected);
    }

    #[test]
    fn unstarted_after_info() {
        let mut talk = Conversation::default();
        talk.task("Search the café menus");
        let connected = "mcp: web: connected, 2 tools".to_owned();
        talk.push(Entry::Info(connected));
        assert!(talk.unstarted());

        talk.notice(&told("Retrying"));
        assert!(!talk.unstarted());
        talk.update(&message("Found three"));
        end_step(&mut talk, 10, 1);
        assert!(!talk.unstarted());
        talk.finish_turn(Outcome::Answered);

        let expected = [
            "",
            "❯ Search the café menus",
            "",
            "  mcp: web: connected, 2 tools",
            "",
            "  ✗ Retrying",
            "",
            "◆ Found three",
            "  0.2s · ⇡10 ⇣1 · 2 t/s",
        ];
        let mut transcript = Transcript::default();
        assert_eq!(rows(&mut transcript, &talk), expected);
    }

    #[test]
    fn step_notice_kept() {
        let mut talk = Conversation::default();
        talk.task("Count the lines");
        let reply = message("Counting the first part");
        talk.update(&reply);
        end_step(&mut talk, 10, 1);

        // The second step is interrupted before any text, and a replayed turn has no stop reason
        let stopped = Notice {
            step: true,
            ..told("interrupted")
        };
        talk.notice(&stopped);
        let meta = json!({ "duration": 1.25, "interrupted": true });
        let meta = meta.as_object().cloned();
        let usage = UsageUpdate::new(11, 100).meta(meta);
        talk.update(&SessionUpdate::UsageUpdate(usage));
        talk.finish_turn(Outcome::Answered);

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
        let mut transcript = Transcript::default();
        assert_eq!(rows(&mut transcript, &talk), expected);
        let Some(Entry::Step(step)) = talk.entries().last() else {
            panic!("step");
        };
        assert!(step.interrupted);
    }

    #[test]
    fn anchor_kept() {
        let mut talk = Conversation::default();
        for n in 1..=3 {
            let task = format!("Task {n}");
            talk.task(&task);
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
            talk.update(&SessionUpdate::ToolCall(started));
            let finished = ToolCallUpdate::new(id, done);
            talk.update(&SessionUpdate::ToolCallUpdate(finished));
            end_step(&mut talk, 10, 1);
            talk.finish_turn(Outcome::Answered);
        }

        let mut transcript = Transcript::default();
        let folded = rows(&mut transcript, &talk);
        let row = folded.iter().position(|row| row == "  │ print(3)");
        let row = row.expect("third");
        let anchor = transcript.anchor(row, 40, true).expect("anchor");
        let expanded = transcript.rows(&talk, 40, true);
        let top = expanded.row_at(anchor);
        let shown = expanded.get(top).map(|row| row.line.to_string());
        assert_eq!(shown.as_deref(), Some("  │ print(3)"));
        assert_eq!(transcript.anchor(top, 40, true), None);

        // The blank row above the code is the first row of the third step
        let blank = transcript.anchor(top - 1, 40, false).expect("blank");
        let refolded = transcript.rows(&talk, 40, false);
        assert_eq!(refolded.row_at(blank), row - 1);
    }

    #[test]
    fn rows_cached() {
        let mut talk = Conversation::default();
        let mut transcript = Transcript::default();
        talk.push(Entry::Info("Started".to_owned()));
        let rows = transcript.rows(&talk, 40, false).len();

        talk.push(Entry::Info("Finished".to_owned()));
        assert_eq!(transcript.rows(&talk, 40, false).len(), rows + 2);
        assert_eq!(transcript.starts.len(), 2);

        let reply = message("Working on it");
        talk.update(&reply);
        assert_eq!(transcript.rows(&talk, 40, false).len(), rows + 4);

        // The same revision draws nothing again, and a new layout draws everything
        transcript.held.clear();
        assert_eq!(transcript.rows(&talk, 40, false).len(), rows + 2);
        assert_eq!(transcript.rows(&talk, 20, false).len(), rows + 4);
        assert_eq!(transcript.starts.len(), 2);
    }

    #[test]
    fn shell_tracked() {
        let mut talk = Conversation::default();
        let mut transcript = Transcript::default();
        let marked = Meta::from_iter([("_smol/shell".to_owned(), json!(true))]);
        let started = ToolCall::new("call_1", "ls docs").meta(marked);
        talk.update(&SessionUpdate::ToolCall(started));
        let running = ToolCallUpdateFields::new().content(vec![content("notes.md\n")]);
        let running = ToolCallUpdate::new("call_1", running);
        talk.update(&SessionUpdate::ToolCallUpdate(running));
        assert!(talk.shelling());
        let expected = [
            "",
            "! ls docs",
            "  ╰ notes.md",
            "",
            "  Goes to the model with your next",
            "  message",
        ];
        assert_eq!(rows(&mut transcript, &talk), expected);

        let error = Meta::from_iter([("error".to_owned(), json!("exit 2"))]);
        let failed = ToolCallUpdateFields::new().status(ToolCallStatus::Failed);
        let failed = ToolCallUpdate::new("call_1", failed).meta(error);
        talk.update(&SessionUpdate::ToolCallUpdate(failed));
        assert!(!talk.shelling());

        // After it ends, calls with the same ID or without the flag belong to the model
        talk.task("Summarize the naïve notes");
        let late = ToolCallUpdateFields::new().content(vec![content("todo.md")]);
        let late = ToolCallUpdate::new("call_1", late);
        talk.update(&SessionUpdate::ToolCallUpdate(late));
        let code = ToolCall::new("call_2", "eval");
        talk.update(&SessionUpdate::ToolCall(code));
        let expected = [
            "",
            "! ls docs",
            "  ├ notes.md",
            "  ✗ exit 2",
            "",
            "❯ Summarize the naïve notes",
            "",
            "  ⚙ eval",
        ];
        assert_eq!(rows(&mut transcript, &talk), expected);

        // Interrupts are recognized by the flag
        let marked = Meta::from_iter([("_smol/shell".to_owned(), json!(true))]);
        let started = ToolCall::new("call_3", "sleep 30").meta(marked);
        talk.update(&SessionUpdate::ToolCall(started));
        let meta = json!({ "error": "interrupted", "interrupted": true });
        let meta = meta.as_object().cloned();
        let stopped = ToolCallUpdateFields::new().status(ToolCallStatus::Failed);
        let stopped = ToolCallUpdate::new("call_3", stopped).meta(meta);
        talk.update(&SessionUpdate::ToolCallUpdate(stopped));
        let Some(Entry::Shell(shell)) = talk.held().last() else {
            panic!("shell");
        };
        assert!(shell.interrupted);
    }

    /// Only scrolling or a new layout takes a call's agents back out of the area at the bottom
    #[test]
    fn hidden_pinned() {
        let mut talk = Conversation::default();
        let mut transcript = Transcript::default();
        started(&mut talk, 2);
        moved(&mut talk, 1, ToolCallStatus::InProgress);
        moved(&mut talk, 2, ToolCallStatus::InProgress);
        let pinned = |transcript: &mut Transcript| {
            transcript.rows(&talk, 60, false);
            let area = transcript.pinned(&talk, 60, 40);
            let shown: Vec<String> = area.iter().map(|row| row.line.to_string()).collect();
            shown.iter().filter(|row| row.contains("menu ")).count()
        };

        let rows = transcript.rows(&talk, 60, false);
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
        transcript.rows(&talk, 70, false);
        assert!(transcript.pinned(&talk, 70, 40).is_empty());
    }

    /// Drawing a call's agents again on their own gives the rows drawing everything anew does
    #[test]
    fn groups_redrawn() {
        let anew = |transcript: &mut Transcript, talk: &Conversation| {
            // Spinners move on between the two
            let shown = |row: &Row| row.line.to_string().replace(SPINNER, "*");
            let rows = transcript.rows(talk, 60, false);
            let drawn: Vec<String> = rows.iter().map(shown).collect();
            let mut fresh = Render::new(60, false);
            let mut starts = Vec::new();
            for entry in talk.entries() {
                starts.push(fresh.rows.len());
                fresh.entry(entry);
            }
            for entry in talk.held() {
                fresh.entry(entry);
            }
            let expected: Vec<String> = fresh.rows.iter().map(shown).collect();
            assert_eq!(drawn, expected);
            assert_eq!(transcript.starts, starts);
        };
        let previewed = |talk: &mut Conversation, preview: &str| {
            let content = vec![content("writing x = 1"), content(preview)];
            let fields = ToolCallUpdateFields::new().content(content);
            let update = ToolCallUpdate::new("call_1.1", fields);
            talk.update(&SessionUpdate::ToolCallUpdate(update));
        };

        let mut talk = Conversation::default();
        let mut transcript = Transcript::default();
        talk.task("Count the cafés");
        started(&mut talk, 2);
        moved(&mut talk, 1, ToolCallStatus::InProgress);
        end_step(&mut talk, 10, 1);
        talk.push(Entry::Info("Counted the naïve ones".to_owned()));
        anew(&mut transcript, &talk);
        let revision = talk.revision();
        previewed(&mut talk, "x = 1\nprint(x)");
        assert_eq!(talk.revision(), revision);
        anew(&mut transcript, &talk);

        talk.finish_turn(Outcome::Answered);
        talk.push(Entry::Info("Done with the résumé".to_owned()));
        anew(&mut transcript, &talk);
        previewed(&mut talk, "x = 1");
        anew(&mut transcript, &talk);
        moved(&mut talk, 2, ToolCallStatus::InProgress);
        previewed(&mut talk, "x = 1\ny = 2\nprint(x, y)");
        anew(&mut transcript, &talk);

        // A call whose agents all ended keeps its rows settled for good
        moved(&mut talk, 1, ToolCallStatus::Completed);
        moved(&mut talk, 2, ToolCallStatus::Completed);
        anew(&mut transcript, &talk);
        assert!(transcript.groups.is_empty(), "{:?}", transcript.groups);
        assert!(talk.working().is_empty());
    }

    /// The held rows show a change to a call whose agents all ended, which has no group left
    #[test]
    fn late_update_drawn() {
        let mut talk = Conversation::default();
        let mut transcript = Transcript::default();
        let mut shown = |talk: &Conversation| -> Vec<String> {
            let rows = transcript.rows(talk, 60, true);
            rows.iter().map(|row| row.line.to_string()).collect()
        };
        talk.task("Count the cafés");
        started(&mut talk, 1);
        moved(&mut talk, 1, ToolCallStatus::InProgress);
        end_step(&mut talk, 10, 1);
        shown(&talk);
        moved(&mut talk, 1, ToolCallStatus::Completed);
        shown(&talk);

        let answer = vec![content("Counted 3 naïve cafés")];
        let answer = ToolCallUpdateFields::new().content(answer);
        let answer = ToolCallUpdate::new("call_1.1", answer);
        talk.update(&SessionUpdate::ToolCallUpdate(answer));
        let rows = shown(&talk);
        let answered = |row: &String| row.contains("Counted 3 naïve cafés");
        assert!(rows.iter().any(answered), "{rows:#?}");
    }
}
