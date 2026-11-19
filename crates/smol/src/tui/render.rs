use crate::conversation::{Call, Entry, Helper, Item, Retry, ShellEnd, State, Step, Tokens};
use crate::tui::highlight;
use crate::tui::markdown::{self, Block, Kind, LINK};
use crate::tui::picture::{self, Source};
use crate::tui::theme::HELPERS;
use crate::tui::transcript::Group;
use crate::tui::{
    clipped, joined, spinner, theme, Spans, ACCENT, BOLD, BORDER, DIM, RED, STRONG, YELLOW,
};
use pulldown_cmark::Alignment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use reaper::one_line;
use std::ops::Range;
use std::path::PathBuf;
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
const MIN_CELL_COLUMNS: usize = 6;
const MIN_RENDER_COLUMNS: usize = 8;

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
const WARNING: Mark = Mark {
    style: YELLOW,
    ..FAILURE
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
const ENDED: Mark = Mark {
    first: "◇ ",
    ..TASK
};
const ENDED_HEADING: &str = "programs ended";
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
    /// A spinner drawn at this column when the row is, because cached rows are not drawn again
    pub(crate) spin: Option<(usize, Instant)>,
}

#[derive(Debug, Clone, PartialEq, Hash)]
pub(crate) enum Target {
    Url(String),
    /// Mermaid source, drawn as a web page only when clicked
    Diagram(String),
    /// Opened in the system's image viewer
    Picture(Source),
}

pub(crate) struct Render {
    width: usize,
    expanded: bool,
    /// Where relative image paths start, and what agents' tasks write as `.`
    pub(crate) cwd: PathBuf,
    pub(crate) rows: Vec<Row>,
    pub(crate) groups: Vec<Group>,
}

impl Render {
    pub(crate) fn new(width: u16, expanded: bool) -> Self {
        Self {
            width: usize::from(width).max(MIN_RENDER_COLUMNS),
            expanded,
            cwd: PathBuf::new(),
            rows: Vec::new(),
            groups: Vec::new(),
        }
    }

    pub(crate) fn entry(&mut self, entry: &Entry) {
        match entry {
            Entry::Info(text) => self.paragraph(BODY, text, DIM),
            Entry::Warning(text) => self.paragraph(WARNING, text, YELLOW),
            Entry::Failure(text) => self.paragraph(FAILURE, text, RED),
            Entry::Step(step) => self.step(step),

            Entry::Task(text) => {
                let lines = highlight::plain(text.trim(), BOLD);
                self.band(TASK, lines);
            }

            Entry::Ended(text) => {
                let mut shown = vec![ENDED_HEADING.to_owned()];
                for line in text.lines() {
                    let line = one_line(line, &self.cwd);
                    shown.push(line);
                }
                let text = shown.join("\n");
                let lines = highlight::plain(&text, Style::new());
                self.band(ENDED, lines);
            }

            Entry::Shell(shell) => {
                let lines = highlight::plain(shell.command.trim(), BOLD);
                self.band(SHELL, lines);

                let ended = match &shell.end {
                    ShellEnd::Failed(reason) => Some((reason, STEP_ERROR, RED)),
                    ShellEnd::Interrupted(reason) => Some((reason, STOPPED, DIM)),
                    ShellEnd::Running | ShellEnd::Exited => None,
                };
                let ended = ended.filter(|(reason, _, _)| !reason.is_empty());
                let output = shell.output.trim_matches('\n');
                if !output.is_empty() {
                    let mark = match ended {
                        Some(_) => BRANCH,
                        None => LAST_BRANCH,
                    };
                    let lines = highlight::plain(output, Style::new());
                    self.fold(mark, lines, OUTPUT_HEAD, 0);
                }
                if let Some((reason, mark, style)) = ended {
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

                        // Images the code viewed show only when expanded, as there can be many,
                        // and their name is already on the row above
                        if self.expanded && attachment.kind == "image" {
                            let bar = [Span::styled(mark.rest, mark.style)];
                            let source = Source::of(&attachment.uri, &self.cwd);
                            self.picture(&bar, &bar, source, "");
                        }
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
        if let Some(tokens) = stats.tokens {
            parts.push(tokens.to_string());
        }

        // Too short a generation gives no meaningful speed
        let output = stats.tokens.map(|tokens| tokens.output);
        let speed = match (output, stats.decode) {
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
                drawn: call.revision,
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
        let colors: Vec<Color> = running.map(|helper| HELPERS[helper.hue]).collect();
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
            let used = spans_width(&status) + " · ".width();
            let left = room.saturating_sub(used);
            let task = one_line(&helper.task, &self.cwd);
            let title = clipped(&task, left);

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
        let rest_width = row.rest.width();

        let most = match (self.expanded, helper.waiting()) {
            (true, _) => usize::MAX,
            (false, true) => MAX_TITLE_ROWS,
            (false, false) => 1,
        };
        let task = one_line(&helper.task, &self.cwd);
        let mut ranges = wrap_ranges(&task, limit);
        let cut = ranges.len() > most;
        if cut {
            ranges.truncate(most);
        }
        let last = ranges.len().saturating_sub(1);
        for (at, (range, joint)) in ranges.into_iter().enumerate() {
            let mut title = task[range].to_owned();
            if cut && at == last {
                let marked = format!("{title}…");
                title = clipped(&marked, limit);
            }
            let (lead, mark_width) = match at {
                0 => (&first[..], width),
                _ => (&rest[..], rest_width),
            };
            let mut line = lead.to_vec();
            line.push(Span::raw(title));
            self.rows.push(Row {
                line: Line::from(line),
                mark_width,
                joint,
                ..Row::default()
            });
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
            if let Some(tokens) = helper.tokens {
                stats.push(tokens.to_string());
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
            self.block(mark, lead, block);
            lead = mark.rest;
        }
    }

    /// `lead` goes before its first line in the style of `mark`
    fn block(&mut self, mark: Mark, lead: &'static str, block: Block) {
        let width = mark.rest.width() + block.indent.width();
        let first = [
            Span::styled(lead, mark.style),
            Span::styled(block.marker, ACCENT),
        ];
        let rest = [
            Span::styled(mark.rest, mark.style),
            Span::styled(block.indent, ACCENT),
        ];

        match block.kind {
            Kind::Text(text) => {
                let mark_width = match text.copy_marker {
                    true => mark.rest.width(),
                    false => width,
                };
                self.push_rows(&first, &rest, mark_width, &text.spans, &text.links);
            }

            Kind::Table { aligns, rows } => {
                let room = self.width.saturating_sub(width);
                let lines = table_lines(&rows, &aligns, room);
                self.push_drawn(&first, &rest, width, lines);
            }

            Kind::Diagram {
                lines,
                code,
                source,
            } => {
                let room = self.width.saturating_sub(width);
                let widest = lines.iter().map(|line| line.width()).max();
                let widest = widest.unwrap_or(0);
                if widest <= room {
                    let mut drawn = Vec::new();
                    for line in lines {
                        drawn.push((vec![(line, DIAGRAM)], None));
                    }
                    return self.push_drawn(&first, &rest, width, drawn);
                }

                // Wrapping garbles a diagram, so one too wide becomes a link to open it,
                // with its source listed when expanded
                let mut note = format!("diagram {widest} columns wide, click to open in browser");
                if !self.expanded {
                    note.push_str(" (ctrl+o for source)");
                }
                let start = self.rows.len();
                self.push_rows(&first, &rest, width, &[(note, LINK)], &[]);
                for row in &mut self.rows[start..] {
                    let target = Target::Diagram(code.clone());
                    row.targets.push((0..usize::MAX, target));
                }

                if self.expanded {
                    for block in source {
                        self.block(mark, mark.rest, block);
                    }
                }
            }

            Kind::Image { alt, url } => {
                let source = Source::of(&url, &self.cwd);
                self.picture(&first, &rest, source, &alt);
            }
        }
    }

    /// Lines drawn to fit the width, which never wrap
    fn push_drawn(
        &mut self,
        first: &[Span<'static>],
        rest: &[Span<'static>],
        mark_width: usize,
        lines: Vec<(Spans, Option<String>)>,
    ) {
        for (i, (spans, joint)) in lines.into_iter().enumerate() {
            let mut line = match i {
                0 => first.to_vec(),
                _ => rest.to_vec(),
            };
            for (text, style) in spans {
                line.push(Span::styled(text, style));
            }

            self.rows.push(Row {
                line: Line::from(line),
                mark_width,
                joint,
                ..Row::default()
            });
        }
    }

    /// The picture after `first` and then `rest`, or its alt text as when pictures weren't drawn,
    /// behind a spinner while it loads
    fn picture(
        &mut self,
        first: &[Span<'static>],
        rest: &[Span<'static>],
        source: Option<Source>,
        alt: &str,
    ) {
        let width: usize = first.iter().map(Span::width).sum();
        let state = source.as_ref().map(picture::state);
        match (source, state) {
            (Some(source), Some(picture::Load::Ready(ready))) => {
                let room = self.width.saturating_sub(width);
                let lines = picture::drawn(&source, &ready, room);
                for (index, cells) in lines.into_iter().enumerate() {
                    let mut line = match index {
                        0 => first.to_vec(),
                        _ => rest.to_vec(),
                    };
                    line.extend(cells);
                    let target = Target::Picture(source.clone());
                    self.rows.push(Row {
                        line: Line::from(line),
                        mark_width: usize::MAX,
                        targets: vec![(0..usize::MAX, target)],
                        ..Row::default()
                    });
                }
            }
            (_, Some(picture::Load::Loading(since))) => {
                let mut spans = vec![(spinner(since).to_string(), DIM)];
                if !alt.is_empty() {
                    spans.push((format!(" {alt}"), DIM));
                }
                // Copying skips the spinner and the space after it
                let start = self.rows.len();
                self.push_rows(first, rest, width + 2, &spans, &[]);
                if let Some(row) = self.rows.get_mut(start) {
                    row.spin = Some((width, since));
                }
            }
            _ if alt.is_empty() => {}
            _ => {
                let alt = [(alt.to_owned(), DIM)];
                self.push_rows(first, rest, width, &alt, &[]);
            }
        }
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
            // A <br> breaks a cell's lines, and unicode-width counts the newline as a column
            let text = joined(cell);
            let lines = text.split('\n').map(UnicodeWidthStr::width);
            let width = lines.max().unwrap_or(0);
            natural[column] = natural[column].max(width);
        }
    }

    let gaps = 2 * columns.saturating_sub(1);
    let available = width.saturating_sub(gaps);
    let total: usize = natural.iter().sum();
    let squeezed = total > available;

    let mut lines = Vec::new();
    if squeezed && available < columns * MIN_CELL_COLUMNS {
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
pub(crate) fn signal(helper: &Helper) -> (char, Style) {
    match helper.state() {
        State::Queued => ('◌', DIM),
        State::Running => {
            let since = helper.started.unwrap_or_else(Instant::now);
            (spinner(since), Style::new().fg(HELPERS[helper.hue]))
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
            theme::shimmer(&doing, THINKING, HELPERS[helper.hue], since)
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
    let mut parts = vec!["sub-agents".to_owned()];
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
    let mut tokens = Tokens::default();
    for spent in helpers.iter().filter_map(|helper| helper.tokens) {
        tokens += spent;
    }
    if !tokens.is_zero() {
        parts.push(tokens.to_string());
    }

    parts.join(" · ")
}

/// The name an agent goes by, which comes before its title
fn tag(helper: &Helper) -> String {
    match &helper.name {
        Some(name) => format!("{name} "),
        None => String::new(),
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
