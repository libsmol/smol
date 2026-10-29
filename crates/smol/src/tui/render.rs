use crate::tui::highlight;
use crate::tui::markdown::{self, Block, Text, LINK};
use crate::tui::transcript::{Call, Entry, Group, Helper, Item, Retry, State, Step};
use crate::tui::{
    clipped, in_out, joined, spinner, theme, Spans, ACCENT, BOLD, BORDER, DIM, RED, STRONG,
};
use pulldown_cmark::Alignment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::ops::Range;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

const CODE_HEAD: usize = 8;
const OUTPUT_HEAD: usize = 6;
const TASK_HEAD: usize = 4;
const TASK_TAIL: usize = 2;
const MAX_THOUGHT_ROWS: usize = 3;
const MAX_PREVIEW_ROWS: usize = 4;
const MAX_TITLE_ROWS: usize = 2;
const MAX_TITLE_COLUMNS: usize = 80;
const MIN_COLUMN: usize = 6;

const THINKING: Style = DIM.add_modifier(Modifier::ITALIC);
const DIAGRAM: Style = Style::new().fg(theme::CYAN);

#[derive(Clone, Copy)]
struct Mark {
    first: &'static str,
    rest: &'static str,
    style: Style,
}

const FAILURE: Mark = Mark {
    first: "✗ ",
    rest: "  ",
    style: RED,
};
const TASK: Mark = Mark {
    first: "❯ ",
    rest: "  ",
    style: STRONG,
};
const SHELL: Mark = Mark {
    first: "! ",
    ..TASK
};
const ANSWER: Mark = Mark {
    first: "◆ ",
    rest: "  ",
    style: BOLD.fg(theme::GREEN),
};
const BODY: Mark = Mark {
    first: "  ",
    rest: "  ",
    style: Style::new(),
};
const THOUGHT: Mark = Mark {
    first: "  ✻ ",
    rest: "    ",
    style: DIM,
};
const CODE: Mark = Mark {
    first: "  │ ",
    rest: "  │ ",
    style: ACCENT,
};
const BRANCH: Mark = Mark {
    first: "  ├ ",
    rest: "  │ ",
    style: DIM,
};
const LAST_BRANCH: Mark = Mark {
    first: "  ╰ ",
    rest: "    ",
    style: DIM,
};
const TOOL: Mark = Mark {
    first: "  ⚙ ",
    rest: "    ",
    style: Style::new().fg(theme::CYAN),
};
const STEP_ERROR: Mark = Mark {
    first: "  ✗ ",
    rest: "    ",
    style: RED,
};
const STOPPED: Mark = Mark {
    style: DIM,
    ..STEP_ERROR
};
const RETRY: Mark = Mark {
    first: "  ↻ ",
    rest: "    ",
    style: Style::new().fg(theme::ORANGE),
};
/// Agents sit under a branch, and their wrapped rows line up after the status symbol
const HELPER: Mark = Mark {
    first: "  │   ",
    rest: "  │     ",
    style: DIM,
};
const LAST_HELPER: Mark = Mark {
    first: "      ",
    rest: "        ",
    style: DIM,
};
const PINNED: Mark = Mark {
    first: "  ├ ",
    rest: "  │   ",
    style: DIM,
};
const LAST_PINNED: Mark = Mark {
    first: "  ╰ ",
    rest: "      ",
    style: DIM,
};
const PULSE: [char; 5] = ['✢', '✳', '✶', '✻', '✽'];
const PULSE_EVERY: Duration = Duration::from_millis(240);

#[derive(Debug, Default)]
pub(crate) struct Row {
    pub(crate) line: Line<'static>,
    /// Selecting and copying skip the columns before this, and `usize::MAX` skips the row
    pub(crate) mark_width: usize,
    /// Whitespace a soft wrap folded away, or None at a newline in the source
    pub(crate) joint: Option<String>,
    /// Painted across the whole row when drawn instead of padding the text with spaces
    pub(crate) fill: Option<Color>,
    /// What opens in the browser when these columns are clicked
    pub(crate) targets: Vec<(Range<usize>, Target)>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Target {
    Url(String),
    /// Mermaid source, drawn as a web page only when clicked
    Diagram(String),
}

pub(crate) struct Render {
    width: usize,
    expanded: bool,
    pub(crate) rows: Vec<Row>,
    pub(crate) groups: Vec<Group>,
}

impl Render {
    pub(crate) fn new(width: u16, expanded: bool) -> Self {
        Self {
            width: usize::from(width).max(8),
            expanded,
            rows: Vec::new(),
            groups: Vec::new(),
        }
    }

    pub(crate) fn entry(&mut self, entry: &Entry) {
        match entry {
            Entry::Info(text) => self.paragraph(BODY, text, DIM),
            Entry::Failure(text) => self.paragraph(FAILURE, text, RED),
            Entry::Step(step) => self.step(step),

            Entry::Task(text) => {
                let lines = highlight::plain(text.trim(), BOLD);
                self.band(TASK, lines);
            }

            Entry::Shell(shell) => {
                let lines = highlight::plain(shell.command.trim(), BOLD);
                self.band(SHELL, lines);

                let reason = shell.end.as_deref().filter(|end| !end.is_empty());
                let output = shell.output.trim_matches('\n');
                if !output.is_empty() {
                    let mark = match reason {
                        Some(_) => BRANCH,
                        None => LAST_BRANCH,
                    };
                    let lines = highlight::plain(output, Style::new());
                    self.fold(mark, lines, OUTPUT_HEAD, 0);
                }
                if let Some(reason) = reason {
                    let (mark, style) = match shell.interrupted {
                        true => (STOPPED, DIM),
                        false => (STEP_ERROR, RED),
                    };
                    self.wrap(mark, reason, style);
                }
            }
        }
    }

    pub(crate) fn step(&mut self, step: &Step) {
        self.rows.push(Row::default());
        for retry in &step.retries {
            self.retry(retry);
        }

        let last = step.items.len().saturating_sub(1);
        let mut after_block = !step.retries.is_empty();
        for (index, item) in step.items.iter().enumerate() {
            // The last item left by an interrupt is not an error
            let (failure, style) = match step.interrupted && index == last {
                true => (STOPPED, DIM),
                false => (STEP_ERROR, RED),
            };

            match item {
                Item::Thought(text) if text.trim().is_empty() => {}
                Item::Thought(text) => {
                    let text = text.trim();
                    let start = self.rows.len();
                    match (self.expanded, step.interrupted) {
                        (true, _) => self.wrap(THOUGHT, text, THINKING),
                        (false, true) => {
                            let first = text.lines().next().unwrap_or_default().trim();
                            self.wrap(THOUGHT, first, THINKING);
                            self.rows.truncate(start + MAX_THOUGHT_ROWS);
                        }
                        (false, false) => {
                            let sentence = last_sentence(text);
                            self.wrap(THOUGHT, sentence, THINKING);

                            let rows = self.rows.len() - start;
                            let excess = rows.saturating_sub(MAX_THOUGHT_ROWS);
                            if excess > 0 {
                                self.rows.drain(start..start + excess);
                                let first = &mut self.rows[start];
                                first.line.spans[0] = Span::styled(THOUGHT.first, THOUGHT.style);
                                first.joint = None;
                            }
                        }
                    }
                }

                Item::Message { text, answer } => {
                    // Leading indentation is a Markdown code block, so only blank lines are trimmed
                    let text = text.trim_start_matches(['\n', '\r']).trim_end();
                    match answer {
                        true if after_block => {
                            self.rows.push(Row::default());
                            self.markdown(ANSWER, text);
                        }
                        true => self.markdown(ANSWER, text),
                        false if text.is_empty() => {}
                        false => self.markdown(BODY, text),
                    }
                }

                Item::Call(call) => {
                    let parts = call.parts();
                    match parts.code {
                        Some((language, code)) => {
                            let lines = highlight::code(language, code);
                            self.fold(CODE, lines, CODE_HEAD, 0);
                        }
                        None => self.wrap(TOOL, &call.title, DIM),
                    }
                    for retry in &call.retries {
                        self.retry(retry);
                    }

                    // Continuation rows with a bar look like code, so multi-line outputs go last
                    if !call.helpers.is_empty() {
                        self.helpers(call);
                    }
                    let count = parts.attachments.len() + parts.outputs.len();
                    let mut drawn = 0;
                    let mut branch = || {
                        drawn += 1;
                        match drawn == count {
                            true => LAST_BRANCH,
                            false => BRANCH,
                        }
                    };

                    for attachment in parts.attachments {
                        let mark = branch();
                        let kind = (attachment.kind.clone(), DIM);
                        let gap = (" ".to_owned(), DIM);
                        let name = (attachment.name.clone(), Style::new());
                        let line = vec![kind, gap, name];
                        self.push_lines(mark, &[line]);
                    }
                    for output in parts.outputs {
                        let mark = branch();
                        let output = output.trim_matches('\n');
                        let lines = highlight::plain(output, Style::new());
                        self.fold(mark, lines, OUTPUT_HEAD, 0);
                    }
                    if let Some(error) = parts.error {
                        let error = error.trim_matches('\n');
                        let lines = highlight::plain(error, style);
                        self.fold(failure, lines, OUTPUT_HEAD, 0);
                    }
                }

                Item::Notice(text) => {
                    let lines = highlight::plain(text, style);
                    self.fold(failure, lines, OUTPUT_HEAD, 0);
                }
            }

            after_block = matches!(item, Item::Call(_) | Item::Notice(_));
        }

        let Some(stats) = step.stats else {
            return;
        };

        let seconds = stats.duration;
        let elapsed = format!("{seconds:.1}s");
        let mut parts = vec![elapsed];
        if let (Some(input), Some(output)) = (stats.input, stats.output) {
            let tokens = in_out(input, output);
            parts.push(tokens);
        }

        // Too short a generation gives no meaningful speed
        let speed = match (stats.output, stats.decode) {
            (Some(tokens), Some(seconds)) if seconds >= 0.1 => Some(tokens as f64 / seconds),
            _ => None,
        };
        if let Some(speed) = speed {
            let speed = format!("{speed:.0} t/s");
            parts.push(speed);
        }

        let line = parts.join(" · ");
        self.push_status(BODY, vec![(line, DIM)]);
    }

    /// Each agent the call's code started under a summary of them, as rows that are drawn again
    /// on their own while the agents are at work
    pub(crate) fn helpers(&mut self, call: &Call) {
        // The call's outputs and error follow its agents
        let parts = call.parts();
        let shown = parts.attachments.len() + parts.outputs.len();
        let (head, row) = match shown > 0 || parts.error.is_some() {
            true => (BRANCH, HELPER),
            false => (LAST_BRANCH, LAST_HELPER),
        };

        let helpers: Vec<&Helper> = call.helpers.iter().collect();
        let summary = summary(&helpers, None);
        let start = self.rows.len();
        self.push_status(head, vec![(summary, DIM)]);

        for (index, helper) in helpers.iter().enumerate() {
            self.helper(helper, row);
            let stated = self.expanded && helper.totals.is_some();
            if stated && index + 1 < helpers.len() {
                let body = Mark {
                    first: row.rest,
                    ..row
                };
                self.push_lines(body, &[Vec::new()]);
            }
        }

        if helpers.iter().any(|helper| helper.waiting()) {
            self.groups.push(Group {
                call: call.id.clone(),
                entry: None,
                rows: start..self.rows.len(),
                stale: false,
            });
        }
    }

    /// The agents at work at the bottom of a conversation `height` rows tall, under a summary
    pub(crate) fn pinned(&mut self, helpers: &[&Helper], height: usize) {
        if helpers.is_empty() {
            return;
        }
        self.rows.push(Row::default());

        let first = helpers.iter().filter_map(|helper| helper.started).min();
        let seconds = first.map(|first| first.elapsed().as_secs());
        let summary = summary(helpers, seconds);
        let running = helpers
            .iter()
            .filter(|helper| helper.state() == State::Running);
        let colors: Vec<Color> = running.map(|helper| helper.color).collect();
        let (symbol, tint) = match first {
            Some(first) if !colors.is_empty() => {
                let elapsed = first.elapsed();
                let turn = elapsed.as_secs() as usize;
                let color = colors[turn % colors.len()];
                let frames = elapsed.as_millis() / PULSE_EVERY.as_millis();
                let pulse = PULSE[frames as usize % PULSE.len()];
                (pulse, Style::new().fg(color))
            }
            _ => ('◌', DIM),
        };
        let symbol = format!("{symbol} ");
        // Its symbol lines up with the spinner of the progress line above
        self.push_status(BODY, vec![(symbol, tint), (summary, DIM)]);

        // A third of the screen holds the blank row, the summary and the agents,
        // and those past it are only counted
        let fit = (height / 3).saturating_sub(2).max(1);
        let largest = MAX_TITLE_ROWS + MAX_PREVIEW_ROWS + 1;
        let full = helpers.len() * largest <= fit;
        let listed = match helpers.len() > fit {
            true => fit - 1,
            false => helpers.len(),
        };
        for (index, helper) in helpers.iter().take(listed).enumerate() {
            let row = match index + 1 == helpers.len() {
                true => LAST_PINNED,
                false => PINNED,
            };
            let (symbol, tint) = signal(helper);
            let row = Mark { style: tint, ..row };
            if full {
                self.helper(helper, row);
                continue;
            }

            let symbol = format!("{symbol} ");
            let tag = tag(helper);
            let taken = row.first.width() + symbol.width() + tag.width();
            let room = self.width.saturating_sub(taken);
            // What it is doing takes at most half the row and the title fits in the rest
            let status = status(helper, room / 2);
            let left = room.saturating_sub(spans_width(&status) + " · ".width());
            let title = clipped(&helper.title, left);

            let mut spans = vec![
                (symbol, tint),
                (tag, DIM),
                (title, Style::new()),
                (" · ".to_owned(), DIM),
            ];
            spans.extend(status);
            self.push_status(row, spans);
        }
        if listed < helpers.len() {
            let more = format!("+{} more", helpers.len() - listed);
            self.push_status(LAST_PINNED, vec![(more, DIM)]);
        }
    }

    /// An agent's title, then what it is doing while it works, why it failed,
    /// or when expanded the start of its answer
    fn helper(&mut self, helper: &Helper, row: Mark) {
        let body = Mark {
            first: row.rest,
            ..row
        };
        // Titles and status rows both start after the symbol
        let room = self.width.saturating_sub(body.first.width());
        let limit = room.min(MAX_TITLE_COLUMNS);

        let (symbol, tint) = signal(helper);
        let status = status(helper, room);

        let symbol = format!("{symbol} ");
        let tag = tag(helper);
        // Titles wrap before the tag goes in front, so its width comes off every row
        let limit = limit.saturating_sub(tag.width()).max(1);
        let first = [
            Span::styled(row.first, row.style),
            Span::styled(symbol, tint),
            Span::styled(tag, DIM),
        ];
        let rest = [Span::styled(row.rest, row.style)];
        let width = first.iter().map(Span::width).sum();

        let most = match (self.expanded, helper.waiting()) {
            (true, _) => usize::MAX,
            (false, true) => MAX_TITLE_ROWS,
            (false, false) => 1,
        };
        let mut ranges = wrap_ranges(&helper.title, limit);
        let cut = ranges.len() > most;
        if cut {
            ranges.truncate(most);
        }
        let last = ranges.len().saturating_sub(1);
        for (at, (range, joint)) in ranges.into_iter().enumerate() {
            let mut title = helper.title[range].to_owned();
            if cut && at == last {
                let marked = format!("{title}…");
                title = clipped(&marked, limit);
            }
            let lead = match at {
                0 => &first[..],
                _ => &rest[..],
            };
            let line = [(title, Style::new())];
            self.push_rows(lead, &rest, width, &line, &[]);
            // Rows wrapped here copy back to the text they came from
            if let Some(row) = self.rows.last_mut() {
                row.joint = joint;
            }
        }

        if !helper.preview.is_empty() {
            let bar = [
                Span::styled(body.rest, body.style),
                Span::styled("│ ", tint),
            ];
            let width = bar.iter().map(Span::width).sum();
            let lines = highlight::plain(&helper.preview, DIM);
            let start = self.rows.len();
            for line in &lines {
                self.push_rows(&bar, &bar, width, line, &[]);
            }

            let excess = (self.rows.len() - start).saturating_sub(MAX_PREVIEW_ROWS);
            self.rows.drain(start..start + excess);
            // Copying must not join the first kept row onto the title above
            if let Some(first) = self.rows.get_mut(start) {
                first.joint = None;
            }
        }

        match (self.expanded, helper.state()) {
            (true, State::Done) => {
                let start = self.rows.len();
                self.markdown(body, helper.text.trim());
                let hidden = (self.rows.len() - start).saturating_sub(MAX_PREVIEW_ROWS);
                if hidden > 0 {
                    self.rows.truncate(start + MAX_PREVIEW_ROWS);
                    let more = format!("… +{hidden} lines");
                    self.push_status(body, vec![(more, DIM)]);
                }
            }
            (true, State::Failed) => self.wrap(body, helper.text.trim(), RED),
            (false, State::Failed) => self.push_lines(body, &[status]),
            (_, State::Done | State::Interrupted) => {}
            (_, State::Running | State::Queued) => self.push_status(body, status),
        }

        if let Some(totals) = helper.totals.filter(|_| self.expanded) {
            let mut stats = vec![
                format!("{} steps", totals.steps),
                format!("{:.1}s", totals.duration),
            ];
            if let Some((input, output)) = helper.tokens {
                stats.push(in_out(input, output));
            }
            let stats = stats.join(" · ");
            self.push_status(body, vec![(stats, DIM)]);
        }
    }

    fn paragraph(&mut self, mark: Mark, text: &str, style: Style) {
        self.rows.push(Row::default());
        self.wrap(mark, text, style);
    }

    fn band(&mut self, mark: Mark, lines: Vec<Spans>) {
        self.rows.push(Row::default());
        let start = self.rows.len();
        self.fold(mark, lines, TASK_HEAD, TASK_TAIL);
        for row in &mut self.rows[start..] {
            row.fill = Some(theme::BAND);
        }
    }

    fn retry(&mut self, retry: &Retry) {
        let text = match retry.count {
            1 => retry.text.clone(),
            count => format!("{} ×{count}", retry.text),
        };
        self.wrap(RETRY, &text, DIM);
    }

    /// Counts wrapped screen rows, so a single very long line still folds
    fn fold(&mut self, mark: Mark, lines: Vec<Spans>, head: usize, tail: usize) {
        let start = self.rows.len();
        self.push_lines(mark, &lines);
        let rows = self.rows.len() - start;
        if self.expanded || rows <= head + tail + 1 {
            return;
        }

        let mut ending = self.rows.split_off(start + rows - tail);
        self.rows.truncate(start + head);
        let hidden = rows - head - tail;
        let more = format!("… +{hidden} lines (ctrl+o to expand)");
        let rest = Mark {
            first: mark.rest,
            ..mark
        };
        self.push_status(rest, vec![(more, DIM)]);

        // The row before is now the hint, so copying must not join onto it
        if let Some(first) = ending.first_mut() {
            first.joint = None;
        }
        self.rows.extend(ending);
    }

    fn wrap(&mut self, mark: Mark, text: &str, style: Style) {
        let lines = highlight::plain(text, style);
        self.push_lines(mark, &lines);
    }

    /// Status rather than content, which selecting and copying skip whole
    fn push_status(&mut self, mark: Mark, spans: Spans) {
        let start = self.rows.len();
        self.push_lines(mark, &[spans]);
        for row in &mut self.rows[start..] {
            row.mark_width = usize::MAX;
        }
    }

    fn push_lines(&mut self, mark: Mark, lines: &[Spans]) {
        let rest = [Span::styled(mark.rest, mark.style)];
        for (i, line) in lines.iter().enumerate() {
            let lead = if i == 0 { mark.first } else { mark.rest };
            let first = [Span::styled(lead, mark.style)];
            let width = lead.width();
            self.push_rows(&first, &rest, width, line, &[]);
        }
    }

    fn markdown(&mut self, mark: Mark, text: &str) {
        let blocks = markdown::blocks(text);
        if blocks.is_empty() {
            return self.wrap(mark, text, Style::new());
        }

        let mut lead = mark.first;
        for block in blocks {
            match block {
                Block::Text(text) => self.text_block(mark, lead, text),

                Block::Table {
                    marker,
                    indent,
                    aligns,
                    rows,
                } => {
                    let width = mark.rest.width() + indent.width();
                    let room = self.width.saturating_sub(width);
                    let lines = table_lines(&rows, &aligns, room);
                    for (i, (spans, joint)) in lines.into_iter().enumerate() {
                        let prefix = if i == 0 { &marker } else { &indent };
                        let mut line = vec![
                            Span::styled(lead, mark.style),
                            Span::styled(prefix.clone(), ACCENT),
                        ];
                        for (text, style) in spans {
                            line.push(Span::styled(text, style));
                        }

                        self.rows.push(Row {
                            line: Line::from(line),
                            mark_width: width,
                            joint,
                            ..Row::default()
                        });
                        lead = mark.rest;
                    }
                }

                Block::Diagram {
                    marker,
                    indent,
                    lines,
                    code,
                    source,
                } => {
                    let width = mark.rest.width() + indent.width();
                    let room = self.width.saturating_sub(width);
                    let widest = lines.iter().map(|line| line.width()).max();
                    let widest = widest.unwrap_or(0);

                    // Wrapping garbles a diagram, so one too wide becomes a link to open it,
                    // with its source listed when expanded
                    match widest > room {
                        true => {
                            let mut note =
                                format!("diagram {widest} columns wide, click to open in browser");
                            if !self.expanded {
                                note.push_str(" (ctrl+o for source)");
                            }
                            let start = self.rows.len();
                            let link = Text {
                                marker,
                                indent,
                                copy_marker: false,
                                spans: vec![(note, LINK)],
                                links: Vec::new(),
                            };
                            self.text_block(mark, lead, link);
                            for row in &mut self.rows[start..] {
                                let target = Target::Diagram(code.clone());
                                row.targets.push((0..usize::MAX, target));
                            }

                            if self.expanded {
                                for text in source {
                                    self.text_block(mark, mark.rest, text);
                                }
                            }
                        }
                        false => {
                            for (i, drawn) in lines.into_iter().enumerate() {
                                let prefix = if i == 0 { &marker } else { &indent };
                                let line = vec![
                                    Span::styled(lead, mark.style),
                                    Span::styled(prefix.clone(), ACCENT),
                                    Span::styled(drawn, DIAGRAM),
                                ];

                                self.rows.push(Row {
                                    line: Line::from(line),
                                    mark_width: width,
                                    ..Row::default()
                                });
                                lead = mark.rest;
                            }
                        }
                    }
                }
            }
            lead = mark.rest;
        }
    }

    fn text_block(&mut self, mark: Mark, lead: &'static str, text: Text) {
        let hidden = if text.copy_marker {
            0
        } else {
            text.marker.width()
        };
        let width = mark.rest.width() + hidden;
        let first = [
            Span::styled(lead, mark.style),
            Span::styled(text.marker, ACCENT),
        ];
        let rest = [
            Span::styled(mark.rest, mark.style),
            Span::styled(text.indent, ACCENT),
        ];
        self.push_rows(&first, &rest, width, &text.spans, &text.links);
    }

    /// `links` ranges are byte offsets into the joined text of `spans`
    fn push_rows(
        &mut self,
        first: &[Span<'static>],
        rest: &[Span<'static>],
        mark_width: usize,
        spans: &[(String, Style)],
        links: &[(Range<usize>, String)],
    ) {
        let text = joined(spans);
        let prefix: usize = first.iter().map(Span::width).sum();
        let width = self.width.saturating_sub(prefix).max(1);
        let rest_width: usize = rest.iter().map(Span::width).sum();

        let ranges = wrap_ranges(&text, width);
        for (i, (range, joint)) in ranges.into_iter().enumerate() {
            let (lead, mark_width) = match i {
                0 => (first, mark_width),
                _ => (rest, rest_width),
            };

            // Each row a link wraps onto records the columns it covers
            let shown: usize = lead.iter().map(Span::width).sum();
            let mut targets = Vec::new();
            for (bytes, url) in links {
                let from = bytes.start.max(range.start);
                let to = bytes.end.min(range.end);
                if from < to {
                    let start = shown + text[range.start..from].width();
                    let end = start + text[from..to].width();
                    let target = Target::Url(url.clone());
                    targets.push((start..end, target));
                }
            }

            let mut line = lead.to_vec();
            for (text, style) in styled_range(spans, range) {
                line.push(Span::styled(text, style));
            }

            self.rows.push(Row {
                line: Line::from(line),
                mark_width,
                joint,
                targets,
                ..Row::default()
            });
        }
    }
}

fn table_lines(
    rows: &[Vec<Spans>],
    aligns: &[Alignment],
    width: usize,
) -> Vec<(Spans, Option<String>)> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut natural = vec![0; columns];
    for row in rows {
        for (column, cell) in row.iter().enumerate() {
            let width = spans_width(cell);
            natural[column] = natural[column].max(width);
        }
    }

    let gaps = 2 * columns.saturating_sub(1);
    let available = width.saturating_sub(gaps);
    let total: usize = natural.iter().sum();
    let squeezed = total > available;

    let mut lines = Vec::new();
    if squeezed && available < columns * MIN_COLUMN {
        let Some((header, records)) = rows.split_first() else {
            return lines;
        };

        let mut groups = Vec::new();
        for record in records {
            let mut fields = Vec::new();
            for (name, cell) in header.iter().zip(record) {
                let mut spans = bold(name);
                spans.push((": ".to_owned(), BOLD));
                spans.extend_from_slice(cell);
                fields.push(spans);
            }
            groups.push(fields);
        }
        if groups.is_empty() {
            let names = header.iter().map(|name| bold(name));
            let names = names.collect();
            groups.push(names);
        }

        let room = width.max(1);
        for fields in groups {
            if !lines.is_empty() {
                lines.push((Spans::new(), None));
            }
            for spans in fields {
                let text = joined(&spans);
                for (range, joint) in wrap_ranges(&text, room) {
                    let part = styled_range(&spans, range);
                    lines.push((part, joint));
                }
            }
        }

        return lines;
    }

    let mut widths = natural.clone();
    if squeezed {
        let mut order: Vec<usize> = (0..columns).collect();
        order.sort_by_key(|&column| natural[column]);

        let mut remaining = available;
        for (placed, &column) in order.iter().enumerate() {
            let share = remaining / (columns - placed);
            widths[column] = natural[column].min(share);
            remaining -= widths[column];
        }
    }

    for (index, row) in rows.iter().enumerate() {
        let header = index == 0;
        let mut cells = Vec::new();
        for (column, &width) in widths.iter().enumerate() {
            let cell = row.get(column).map_or(&[][..], Vec::as_slice);
            let cell = match header {
                true => bold(cell),
                false => cell.to_vec(),
            };
            let text = joined(&cell);

            let room = width.max(1);
            let mut wrapped = Vec::new();
            for (range, _) in wrap_ranges(&text, room) {
                let part = styled_range(&cell, range);
                wrapped.push(part);
            }
            cells.push(wrapped);
        }
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);

        for line in 0..height {
            let mut spans = Spans::new();
            for (column, cell) in cells.iter().enumerate() {
                let part = cell.get(line).cloned().unwrap_or_default();
                let used = spans_width(&part);
                let padding = widths[column].saturating_sub(used);
                let before = match aligns.get(column) {
                    Some(Alignment::Right) => padding,
                    Some(Alignment::Center) => padding / 2,
                    _ => 0,
                };
                if before > 0 {
                    let indent = " ".repeat(before);
                    spans.push((indent, Style::new()));
                }
                spans.extend(part);
                if column + 1 < cells.len() {
                    let gap = " ".repeat(padding - before + 2);
                    spans.push((gap, Style::new()));
                }
            }
            lines.push((spans, None));
        }

        if header {
            let length: usize = widths.iter().sum();
            let rule = "─".repeat(length + gaps);
            let ruled = vec![(rule, BORDER)];
            lines.push((ruled, None));
        }
    }

    lines
}

fn bold(spans: &[(String, Style)]) -> Spans {
    let mut styled = Spans::new();
    for (text, style) in spans {
        let style = style.patch(BOLD);
        styled.push((text.clone(), style));
    }

    styled
}

fn spans_width(spans: &[(String, Style)]) -> usize {
    spans.iter().map(|(text, _)| text.width()).sum()
}

/// The symbol of the state an agent is in, in the style its rows take
fn signal(helper: &Helper) -> (char, Style) {
    match helper.state() {
        State::Queued => ('◌', DIM),
        State::Running => {
            let since = helper.started.unwrap_or_else(Instant::now);
            (spinner(since), Style::new().fg(helper.color))
        }
        State::Done => ('◆', ANSWER.style),
        State::Interrupted => ('✗', DIM),
        State::Failed => ('✗', RED),
    }
}

/// What an agent is doing while it waits or runs, or why it failed, in at most `width` columns
fn status(helper: &Helper, width: usize) -> Spans {
    match helper.state() {
        State::Queued => vec![(clipped("queued", width), DIM)],
        State::Running => {
            let since = helper.started.unwrap_or_else(Instant::now);
            let seconds = since.elapsed().as_secs();
            let doing = format!("{} · {seconds}s", helper.text.trim());
            let doing = clipped(&doing, width);
            theme::shimmer(&doing, THINKING, helper.color, since)
        }
        State::Done | State::Interrupted => Vec::new(),
        State::Failed => {
            let mut lines = helper.text.lines().map(str::trim);
            let error = lines.find(|line| !line.is_empty()).unwrap_or_default();
            vec![(clipped(error, width), RED)]
        }
    }
}

/// How many agents are in each state, how long since the first started, and their tokens so far
fn summary(helpers: &[&Helper], seconds: Option<u64>) -> String {
    let states = [
        (State::Running, "running"),
        (State::Queued, "queued"),
        (State::Done, "done"),
        (State::Failed, "failed"),
        (State::Interrupted, "interrupted"),
    ];
    let mut parts = vec!["agents".to_owned()];
    for (state, name) in states {
        let count = helpers.iter().filter(|helper| helper.state() == state);
        let count = count.count();
        if count > 0 {
            parts.push(format!("{count} {name}"));
        }
    }

    if let Some(seconds) = seconds {
        parts.push(format!("{seconds}s"));
    }
    let (mut input, mut output) = (0, 0);
    for (spent_in, spent_out) in helpers.iter().filter_map(|helper| helper.tokens) {
        input += spent_in;
        output += spent_out;
    }
    if input + output > 0 {
        parts.push(in_out(input, output));
    }
    parts.join(" · ")
}

/// The role and name an agent goes by, which come before its title
fn tag(helper: &Helper) -> String {
    let mut tags = Vec::new();
    if let Some(role) = &helper.role {
        tags.push(role.as_str());
    }
    if let Some(name) = &helper.name {
        tags.push(name.as_str());
    }
    match tags.is_empty() {
        true => String::new(),
        false => format!("{} ", tags.join(" · ")),
    }
}

pub(crate) fn wrap_ranges(text: &str, width: usize) -> Vec<(Range<usize>, Option<String>)> {
    let mut room = width;
    loop {
        let mut ranges = Vec::new();
        let mut end = 0;
        for (i, row) in textwrap::wrap(text, room).iter().enumerate() {
            let row: &str = row;
            // textwrap by default returns slices of the input,
            // and slicing on after a row is not found could split a character
            let Some(found) = text[end..].find(row) else {
                break;
            };
            let start = end + found;
            let joint = (i > 0).then(|| text[end..start].to_owned());
            end = start + row.len();
            ranges.push((start..end, joint));
        }

        // textwrap measures each char, but an emoji with U+FE0F draws one column wider
        // and the overflow would be cut off
        let fits = ranges
            .iter()
            .all(|(range, _)| text[range.clone()].width() <= width);
        if room == 1 || fits {
            return ranges;
        }
        room -= 1;
    }
}

fn styled_range(spans: &[(String, Style)], range: Range<usize>) -> Spans {
    let mut picked = Spans::new();
    let mut offset = 0;
    for (text, style) in spans {
        let (start, end) = (offset, offset + text.len());
        offset = end;

        let from = range.start.max(start);
        let to = range.end.min(end);
        if from < to {
            let part = text[from - start..to - start].to_owned();
            picked.push((part, *style));
        }
    }

    picked
}

fn last_sentence(text: &str) -> &str {
    let mut start = 0;
    let mut prev = None;
    let mut chars = text.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        let Some(&(next_at, next)) = chars.peek() else {
            break;
        };

        let ends = match c {
            '。' | '！' | '？' | '\n' => true,
            '.' => next.is_whitespace() && prev != Some('.'),
            '!' | '?' => next.is_whitespace(),
            _ => false,
        };
        if ends {
            start = next_at;
        }
        prev = Some(c);
    }

    text[start..].trim()
}

#[cfg(test)]
mod tests {
    use crate::tui::render::*;
    use crate::tui::tests::helper;
    use crate::tui::transcript::{Attachment, Call, Helper, Shell, StepStats, Totals};
    use agent_client_protocol_schema::v1::ToolCallStatus;

    fn call(content: &[&str], status: ToolCallStatus) -> Call {
        Call {
            id: "call_1".to_owned(),
            title: "Read file".to_owned(),
            status,
            content: content.iter().map(|text| text.to_string()).collect(),
            attachments: Vec::new(),
            retries: Vec::new(),
            helpers: Vec::new(),
        }
    }

    fn text(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|row| row.line.to_string()).collect()
    }

    /// Pinned agents show in full only while all of them fit in a third of the screen
    #[test]
    fn pinned_fits_screen() {
        let running = Helper {
            role: Some("counter".to_owned()),
            name: Some("cafés".to_owned()),
            preview: "print(len(cafés))".to_owned(),
            tokens: Some((1200, 300)),
            started: Some(Instant::now()),
            ..helper("Count the cafés", ToolCallStatus::InProgress, "running len")
        };
        let mut full = Render::new(100, false);
        full.pinned(&[&running], 27);
        let rows = text(&full.rows);
        assert_eq!(rows.len(), 5, "{rows:?}");
        // The summary spins like the agents below it, and only ones still queued leave it waiting
        let tokens = in_out(1200, 300);
        let summary = format!(" agents · 1 running · 0s · {tokens}");
        assert!(rows[1].ends_with(&summary), "{rows:?}");
        assert!(!rows[1].contains('◌'), "{rows:?}");
        let waiting = helper("Spell naïve", ToolCallStatus::Pending, "");
        let mut queued = Render::new(100, false);
        queued.pinned(&[&waiting], 27);
        let rows_queued = text(&queued.rows);
        assert_eq!(rows_queued[1], "  ◌ agents · 1 queued");
        assert!(rows[2].starts_with("  ╰ "), "{rows:?}");
        assert!(
            rows[2].ends_with(" counter · cafés Count the cafés"),
            "{rows:?}"
        );
        assert_eq!(rows[3], "      │ print(len(cafés))");
        assert_eq!(rows[4], "      running len · 0s");

        let mut compact = Render::new(100, false);
        compact.pinned(&[&running, &running], 27);
        let rows = text(&compact.rows);
        assert_eq!(rows.len(), 4, "{rows:?}");
        let tokens = in_out(2400, 600);
        let summary = format!(" agents · 2 running · 0s · {tokens}");
        assert!(rows[1].ends_with(&summary), "{rows:?}");
        let row = " counter · cafés Count the cafés · running len · 0s";
        assert!(
            rows[2].starts_with("  ├ ") && rows[2].ends_with(row),
            "{rows:?}"
        );
        assert!(
            rows[3].starts_with("  ╰ ") && rows[3].ends_with(row),
            "{rows:?}"
        );
        assert!(compact.groups.is_empty(), "{:?}", compact.groups);
        // The branch of an agent at work takes its color
        let branch = &compact.rows[2].line.spans[0];
        assert_eq!(branch.content, "  ├ ");
        assert_eq!(branch.style.fg, Some(running.color));

        let mut counted = Render::new(100, false);
        counted.pinned(&[&running; 5], 12);
        let rows = text(&counted.rows);
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert!(rows[2].starts_with("  ├ "), "{rows:?}");
        assert_eq!(rows[3], "  ╰ +4 more");
    }

    /// A step whose only call started this agent
    fn helped(helper: Helper) -> Step {
        let mut started = call(&["```python\nagent(task)\n```"], ToolCallStatus::Completed);
        started.helpers.push(helper);
        Step {
            items: vec![Item::Call(started)],
            ..Step::default()
        }
    }

    #[test]
    fn helper_title_rows() {
        let title = "Review the café menu parser in menu.py, the naïve rounding of its prices and the résumé export 🐍 that reuses both, then report every bug with the line it is on and a fix that keeps the output for valid input";
        let step = helped(helper(title, ToolCallStatus::Pending, ""));

        let titled = |expanded: bool| {
            let mut render = Render::new(100, expanded);
            render.step(&step);
            let start = 3;
            let end = render.rows.len() - 1;
            render.rows.drain(start..end).collect::<Vec<Row>>()
        };
        let folded = text(&titled(false));
        assert_eq!(folded.len(), MAX_TITLE_ROWS, "{folded:#?}");
        assert!(folded[1].ends_with('…'), "{folded:#?}");
        let expanded = titled(true);
        let rows = text(&expanded);
        assert_eq!(rows.len(), 3, "{rows:#?}");
        for row in &rows {
            assert!(row.width() <= 8 + MAX_TITLE_COLUMNS, "{rows:#?}");
        }
        let parts = rows.iter().map(|row| row.trim_start_matches([' ', '◌']));
        let joined: Vec<&str> = parts.map(str::trim_end).collect();
        assert_eq!(joined.join(" "), title);
        assert_eq!(expanded[1].joint.as_deref(), Some(" "));
    }

    /// The role goes in front of the title, which still wraps within the width
    #[test]
    fn helper_role_tagged() {
        let title = "Review the café menu parser in menu.py and the naïve rounding of its prices";
        let tagged = Helper {
            role: Some("reviewer".to_owned()),
            ..helper(title, ToolCallStatus::Pending, "")
        };
        let step = helped(tagged);
        let mut render = Render::new(50, true);
        render.step(&step);
        let end = render.rows.len() - 1;
        let rows = text(&render.rows[3..end]);

        assert!(rows[0].starts_with("      ◌ reviewer Review"), "{rows:#?}");
        for row in &rows {
            assert!(row.width() <= 50, "{rows:#?}");
        }
        let parts = rows.iter().map(|row| row.trim_start_matches([' ', '◌']));
        let joined: Vec<&str> = parts.map(str::trim_end).collect();
        assert_eq!(joined.join(" "), format!("reviewer {title}"));
    }

    /// What the code printed shows after its agents like any call's output
    #[test]
    fn agent_output_shown() {
        let code = "```python\nagent(tasks)\n```";
        let printed = "café: 3\nnaïve: 1\nrésumé: 2";
        let mut started = call(&[code, printed], ToolCallStatus::Completed);
        let counted = helper("Count the menu", ToolCallStatus::Completed, "3 cafés");
        started.helpers.push(counted);
        let step = Step {
            items: vec![Item::Call(started)],
            ..Step::default()
        };

        let expected = [
            "",
            "  │ agent(tasks)",
            "  ├ agents · 1 done",
            "  │   ◆ Count the menu",
            "  ╰ café: 3",
            "    naïve: 1",
            "    résumé: 2",
        ];
        assert_eq!(rendered(&step, 40, false), expected);
    }

    #[test]
    fn helper_answer_cut() {
        let answer = "- café 1\n- café 2\n- café 3\n- café 4\n- naïve 5\n- résumé 6";
        let totals = Totals {
            steps: 2,
            duration: 10.0,
        };
        let spelled = Helper {
            totals: Some(totals),
            tokens: Some((3510, 211)),
            ..helper("Spell the menu", ToolCallStatus::Completed, answer)
        };
        let step = helped(spelled);
        let folded = rendered(&step, 40, false);
        assert_eq!(folded[3..], ["      ◆ Spell the menu"]);

        let mut render = Render::new(40, true);
        render.step(&step);
        let rows = &render.rows[4..];
        let expected = [
            "        • café 1",
            "        • café 2",
            "        • café 3",
            "        • café 4",
            "        … +2 lines",
            "        2 steps · 10.0s · ⇡3.51k ⇣211",
        ];
        assert_eq!(text(rows), expected);
        assert_eq!(rows[4].mark_width, usize::MAX);
        assert_eq!(rows[5].mark_width, usize::MAX);
    }

    fn rendered(step: &Step, width: u16, expanded: bool) -> Vec<String> {
        let mut render = Render::new(width, expanded);
        render.step(step);
        text(&render.rows)
    }

    fn thought(text: &str) -> Step {
        Step {
            items: vec![Item::Thought(text.to_owned())],
            ..Step::default()
        }
    }

    #[test]
    fn thought_tail() {
        let words: Vec<String> = (1..=40).map(|n| format!("word{n}")).collect();
        let words = words.join(" ");
        let step = thought(&words);
        let rows = rendered(&step, 20, false);

        let expected = [
            "",
            "  ✻ word35 word36",
            "    word37 word38",
            "    word39 word40",
        ];
        assert_eq!(rows, expected);
    }

    #[test]
    fn sentence_end() {
        let last = last_sentence("First I search. Then I compare.");
        assert_eq!(last, "Then I compare.");
        assert_eq!(last_sentence("Search first。Then compare"), "Then compare");

        let dotted = "pi is 3.14 and... more";
        assert_eq!(last_sentence(dotted), dotted);
        assert_eq!(last_sentence("one line\nsecond line\n"), "second line");
        assert_eq!(last_sentence("Done!"), "Done!");
    }

    #[test]
    fn long_blocks_fold() {
        let body = ["pass"; 12].join("\n");
        let code = format!("```python\n{body}\n```");
        let done = call(&[&code, "ok"], ToolCallStatus::Completed);
        let step = Step {
            items: vec![
                Item::Thought("first idea\nlast idea\n".to_owned()),
                Item::Call(done),
            ],
            ..Step::default()
        };

        let mut render = Render::new(40, false);
        render.step(&step);
        let folded = text(&render.rows);
        let passes = folded.iter().filter(|line| *line == "  │ pass").count();
        assert_eq!(folded[1], "  ✻ last idea");
        assert_eq!(passes, CODE_HEAD);
        let hint = folded
            .iter()
            .position(|line| line == "  │ … +4 lines (ctrl+o to expand)");
        let hint = hint.expect("hint");
        assert_eq!(render.rows[hint].mark_width, usize::MAX);
        assert_eq!(folded.last().map(String::as_str), Some("  ╰ ok"));

        let expanded = rendered(&step, 40, true);
        let passes = expanded.iter().filter(|line| *line == "  │ pass").count();
        assert!(expanded.contains(&"  ✻ first idea".to_owned()));
        assert_eq!(passes, 12);
    }

    #[test]
    fn long_task_folds() {
        let lines: Vec<String> = (1..=20).map(|n| format!("line {n}")).collect();
        let asked = Entry::Task;
        let task = asked(lines.join("\n"));
        let mut render = Render::new(40, false);
        render.entry(&task);
        let expected = [
            "",
            "❯ line 1",
            "  line 2",
            "  line 3",
            "  line 4",
            "  … +14 lines (ctrl+o to expand)",
            "  line 19",
            "  line 20",
        ];
        assert_eq!(text(&render.rows), expected);

        let mut render = Render::new(40, true);
        render.entry(&task);
        assert_eq!(render.rows.len(), 21);
        let short = asked(lines[..7].join("\n"));
        let mut render = Render::new(40, false);
        render.entry(&short);
        let expected = [
            "",
            "❯ line 1",
            "  line 2",
            "  line 3",
            "  line 4",
            "  line 5",
            "  line 6",
            "  line 7",
        ];
        assert_eq!(text(&render.rows), expected);

        let long = asked("🍕".repeat(400));
        let mut render = Render::new(40, false);
        render.entry(&long);
        let rows = &render.rows;
        assert_eq!(rows.len(), 1 + TASK_HEAD + 1 + TASK_TAIL);
        let fills: Vec<_> = rows.iter().map(|row| row.fill).collect();
        let banded = fills[1..].iter().all(|fill| *fill == Some(theme::BAND));
        assert!(fills[0].is_none() && banded, "{fills:?}");
        assert_eq!(rows[TASK_HEAD + 2].joint, None);
        assert_eq!(rows[TASK_HEAD + 3].joint.as_deref(), Some(""));
    }

    #[test]
    fn shell_rendered() {
        let shell = |end: Option<&str>, interrupted: bool| Shell {
            id: "call_1".to_owned(),
            command: "tail app.log".to_owned(),
            output: "first line\nsecond line\n".to_owned(),
            end: end.map(str::to_owned),
            interrupted,
        };
        let rendered = |shell: Shell| {
            let mut render = Render::new(40, false);
            render.entry(&Entry::Shell(shell));
            render.rows
        };

        // While running the output is the last part, and an interrupt is not an error
        let running = rendered(shell(None, false));
        let expected = ["", "! tail app.log", "  ╰ first line", "    second line"];
        assert_eq!(text(&running), expected);
        assert_eq!(running[1].fill, Some(theme::BAND));
        let stopped = rendered(shell(Some("interrupted"), true));
        assert_eq!(text(&stopped[4..]), ["  ✗ interrupted"]);
        assert_eq!(stopped[4].line.spans[0].style, STOPPED.style);
        let done = rendered(shell(Some(""), false));
        assert_eq!(text(&done)[2..], ["  ╰ first line", "    second line"]);
    }

    #[test]
    fn branches_drawn() {
        let content = [
            "```python\nview('screenshot.png')\n```",
            "first line\nsecond line",
            "it broke",
        ];
        let mut failed = call(&content, ToolCallStatus::Failed);
        failed.attachments = vec![Attachment {
            kind: "image".to_owned(),
            name: "screenshot.png".to_owned(),
        }];
        let step = Step {
            items: vec![Item::Call(failed)],
            ..Step::default()
        };

        let mut render = Render::new(40, false);
        render.step(&step);
        let rows = render.rows;
        let expected = [
            "",
            "  │ view('screenshot.png')",
            "  ├ image screenshot.png",
            "  ╰ first line",
            "    second line",
            "  ✗ it broke",
        ];
        assert_eq!(text(&rows), expected);
        assert_eq!(rows[2].mark_width, 4);

        let alone = call(&content[..1], ToolCallStatus::Completed);
        let alone = Step {
            items: vec![Item::Call(Call {
                attachments: vec![Attachment {
                    kind: "video".to_owned(),
                    name: "recording.mp4".to_owned(),
                }],
                ..alone
            })],
            ..Step::default()
        };
        assert_eq!(rendered(&alone, 40, false)[2], "  ╰ video recording.mp4");
    }

    #[test]
    fn step_stats_line() {
        let stats = StepStats {
            duration: 1.25,
            decode: Some(0.8),
            input: Some(1234),
            output: Some(56),
        };
        let done = Item::Message {
            text: "Done.".to_owned(),
            answer: false,
        };
        let step = Step {
            items: vec![done],
            stats: Some(stats),
            ..Step::default()
        };

        let expected = ["", "  Done.", "  1.2s · ⇡1.23k ⇣56 · 70 t/s"];
        assert_eq!(rendered(&step, 40, false), expected);

        let expected = ["  1.2s · ⇡1.23k ⇣56", "  · 70 t/s"];
        assert_eq!(rendered(&step, 20, false)[2..], expected);
        let mut render = Render::new(20, false);
        render.step(&step);
        let marks: Vec<usize> = render.rows[2..].iter().map(|row| row.mark_width).collect();
        assert_eq!(marks, [usize::MAX, usize::MAX]);
        let empty = Step::default();
        assert_eq!(rendered(&empty, 40, false), [""]);
    }

    #[test]
    fn answer_spaced() {
        let answer = |text: &str| Item::Message {
            text: text.to_owned(),
            answer: true,
        };
        let retry = Retry {
            text: "model: http 503: upstream busy".to_owned(),
            count: 1,
        };
        let retried = Step {
            retries: vec![retry],
            items: vec![answer("Finally fixed.")],
            ..Step::default()
        };
        let expected = [
            "",
            "  ↻ model: http 503: upstream busy",
            "",
            "◆ Finally fixed.",
        ];
        assert_eq!(rendered(&retried, 40, false), expected);

        let notice = Item::Notice("history: disk full".to_owned());
        let noticed = Step {
            items: vec![notice, answer("Done.")],
            ..Step::default()
        };
        let expected = ["", "  ✗ history: disk full", "", "◆ Done."];
        assert_eq!(rendered(&noticed, 40, false), expected);
    }

    #[test]
    fn wrap_keeps_joints() {
        let mut render = Render::new(12, false);
        let failure = "division by zero\nhttps://example.com/x".to_owned();
        render.entry(&Entry::Failure(failure));
        let rows = render.rows;

        let expected = [
            "",
            "✗ division",
            "  by zero",
            "  https://",
            "  example.co",
            "  m/x",
        ];
        assert_eq!(text(&rows), expected);

        let joints: Vec<Option<&str>> = rows.iter().map(|row| row.joint.as_deref()).collect();
        assert_eq!(joints, [None, None, Some(" "), None, Some(""), Some("")]);
        assert!(rows[1..].iter().all(|row| row.mark_width == 2));

        let mut render = Render::new(12, false);
        let failure = "🍕🍕🍕🍕🍕🍕🍕🍕🍕".to_owned();
        render.entry(&Entry::Failure(failure));
        let rows = render.rows;
        assert_eq!(text(&rows), ["", "✗ 🍕🍕🍕🍕🍕", "  🍕🍕🍕🍕"]);
        let joints: Vec<Option<&str>> = rows.iter().map(|row| row.joint.as_deref()).collect();
        assert_eq!(joints, [None, None, Some("")]);
    }

    #[test]
    fn emoji_rows_fit() {
        let text = "✔️ Done: changed the config file, reran the whole test suite 🧪, and everything passed 🎉.";
        for width in 2..40 {
            let ranges = wrap_ranges(text, width);
            for (range, _) in &ranges {
                let row = &text[range.clone()];
                assert!(row.width() <= width, "{width}: {row}");
            }
            let end = ranges.last().map(|(range, _)| range.end);
            assert_eq!(end, Some(text.len()), "{width}");
        }
    }

    #[test]
    fn indented_code_kept() {
        let message = Item::Message {
            text: "\n    indented code\n    second line".to_owned(),
            answer: true,
        };
        let step = Step {
            items: vec![message],
            ..Step::default()
        };

        let rows = rendered(&step, 40, false);
        assert_eq!(rows, ["", "◆ │ indented code", "  │ second line"]);
    }

    #[test]
    fn code_highlighted() {
        let content = ["```python\nx = 1  # one\n```"];
        let done = call(&content, ToolCallStatus::Completed);
        let step = Step {
            items: vec![Item::Call(done)],
            ..Step::default()
        };

        let mut render = Render::new(40, false);
        render.step(&step);
        let rows = render.rows;
        assert_eq!(text(&rows)[1], "  │ x = 1  # one");

        let style = |content: &str| {
            let mut spans = rows[1].line.spans.iter();
            let found = spans.find(|span| span.content == content);
            found.map(|span| span.style)
        };
        assert_eq!(style("1").and_then(|style| style.fg), Some(theme::PURPLE));
        assert_eq!(style("x = "), Some(Style::new()));
    }

    #[test]
    fn tables_fit_width() {
        let cell = |text: &str| vec![(text.to_owned(), Style::new())];
        let rows = vec![
            vec![cell("name"), cell("note")],
            vec![cell("alpha"), cell("a long note that wraps")],
            vec![cell("b"), cell("short")],
        ];
        let aligned = |rows: &[Vec<Spans>], aligns: &[Alignment], width| -> Vec<String> {
            let mut shown = Vec::new();
            for (spans, _) in table_lines(rows, aligns, width) {
                let line = joined(&spans);
                let line = line.trim_end().to_owned();
                shown.push(line);
            }
            shown
        };
        let lines = |rows: &[Vec<Spans>], width| aligned(rows, &[], width);

        let rule = "─".repeat(29);
        let expected = [
            "name   note",
            &rule,
            "alpha  a long note that wraps",
            "b      short",
        ];
        assert_eq!(lines(&rows, 40), expected);

        let rule = "─".repeat(20);
        let expected = [
            "name   note",
            &rule,
            "alpha  a long note",
            "       that wraps",
            "b      short",
        ];
        assert_eq!(lines(&rows, 20), expected);

        let expected = [
            "name: alpha",
            "note: a long",
            "note that",
            "wraps",
            "",
            "name: b",
            "note: short",
        ];
        assert_eq!(lines(&rows, 12), expected);
        assert_eq!(lines(&rows[..1], 8), ["name", "note"]);

        let wide = vec![
            vec![cell("dish"), cell("note")],
            vec![cell("🍕"), cell("crème brûlée")],
        ];
        let rule = "─".repeat(18);
        let expected = ["dish  note", &rule, "🍕    crème brûlée"];
        assert_eq!(lines(&wide, 40), expected);

        let money = vec![
            vec![cell("city"), cell("sales")],
            vec![cell("Zürich"), cell("18,420.50")],
            vec![cell("São Paulo"), cell("9,876.25")],
        ];
        let aligns = [Alignment::None, Alignment::Right];
        let rule = "─".repeat(20);
        let expected = [
            "city           sales",
            &rule,
            "Zürich     18,420.50",
            "São Paulo   9,876.25",
        ];
        assert_eq!(aligned(&money, &aligns, 40), expected);
    }

    #[test]
    fn diagram_fits_width() {
        let answer =
            "See the diagram:\n\n```mermaid\ngraph LR\n    A[read 🐍] --> B[write 🚀]\n```";
        let mut render = Render::new(40, false);
        render.markdown(ANSWER, answer);
        let rows = text(&render.rows);
        assert!(
            rows.iter().any(|row| row.contains("│ read 🐍 │")),
            "{rows:?}"
        );
        let drawn = render
            .rows
            .iter()
            .find(|row| row.line.to_string().contains("read 🐍"));
        let style = drawn
            .and_then(|row| row.line.spans.last())
            .map(|span| span.style);
        assert_eq!(style, Some(DIAGRAM));

        let mut render = Render::new(20, false);
        render.markdown(ANSWER, answer);
        let rows = text(&render.rows);
        assert!(
            rows[2].starts_with("  diagram ") && rows.len() > 3,
            "{rows:?}"
        );
        assert!(rows.iter().all(|row| !row.contains("graph LR")), "{rows:?}");
        let code = "graph LR\n    A[read 🐍] --> B[write 🚀]".to_owned();
        let whole = (0..usize::MAX, Target::Diagram(code));
        let linked = render.rows[2..]
            .iter()
            .all(|row| row.targets == [whole.clone()]);
        assert!(linked);

        let mut render = Render::new(20, true);
        render.markdown(ANSWER, answer);
        let rows = text(&render.rows);
        assert!(rows.contains(&"  │ graph LR".to_owned()), "{rows:?}");
        assert!(render.rows.last().is_some_and(|row| row.targets.is_empty()));

        // The 27-column diagram fits 28 columns but not the 26 left after the mark
        let mut render = Render::new(28, false);
        render.markdown(ANSWER, answer);
        let expected = [
            "  diagram 27 columns wide,",
            "  click to open in browser",
            "  (ctrl+o for source)",
        ];
        assert_eq!(text(&render.rows[2..]), expected);
    }

    #[test]
    fn link_columns_kept() {
        let mut render = Render::new(18, false);
        render.markdown(ANSWER, "see [the docs 📘](https://a.io/menü)");
        let rows = text(&render.rows);
        assert_eq!(rows, ["◆ see the docs 📘", "  (https://a.io/", "  menü)"]);

        let url = Target::Url("https://a.io/menü".to_owned());
        let targets: Vec<_> = render.rows.iter().map(|row| row.targets.clone()).collect();
        let expected = [
            vec![(6..17, url.clone())],
            vec![(2..16, url.clone())],
            vec![(2..7, url)],
        ];
        assert_eq!(targets, expected);
    }

    #[test]
    fn markdown_table_mark() {
        let mut render = Render::new(40, false);
        render.markdown(ANSWER, "| 🍕 | b |\n|---|---|\n| 1 | 2 |");
        let rows = render.rows;

        assert_eq!(text(&rows), ["◆ 🍕  b", "  ─────", "  1   2"]);
        assert!(rows.iter().all(|row| row.mark_width == 2));
    }

    #[test]
    fn centered_column_padded() {
        let table = "| dish | price |\n|:---:|---:|\n| 🍕 tart | 9.50 |\n| crème brûlée | 12.00 |";
        let mut render = Render::new(40, false);
        render.markdown(ANSWER, table);

        let rule = format!("  {}", "─".repeat(19));
        let expected = [
            "◆     dish      price",
            &rule,
            "    🍕 tart      9.50",
            "  crème brûlée  12.00",
        ];
        assert_eq!(text(&render.rows), expected);
    }
}
