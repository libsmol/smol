use crate::conversation::{Entry, Item, Retry, ShellEnd, Step};
use crate::tui::highlight;
use crate::tui::markdown::{self, Block, Kind, LINK};
use crate::tui::picture::{self, Source};
use crate::tui::{joined, spinner, theme, Spans, ACCENT, BOLD, BORDER, DIM, RED, STRONG, YELLOW};
use pulldown_cmark::Alignment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::env;
use std::ops::Range;
use std::path::{Path, PathBuf, MAIN_SEPARATOR};
use std::time::Instant;
use unicode_width::UnicodeWidthStr;

const CODE_HEAD: usize = 8;
const OUTPUT_HEAD: usize = 6;
const TASK_HEAD: usize = 4;
const TASK_TAIL: usize = 2;
const MAX_THOUGHT_ROWS: usize = 3;
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
    /// Where relative image paths start, and what the commands of programs write as `.`
    pub(crate) cwd: PathBuf,
    pub(crate) rows: Vec<Row>,
}

impl Render {
    pub(crate) fn new(width: u16, expanded: bool) -> Self {
        Self {
            width: usize::from(width).max(MIN_RENDER_COLUMNS),
            expanded,
            cwd: PathBuf::new(),
            rows: Vec::new(),
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

/// The text on one line, with `cwd` as `.` and paths in it relative, and the home directory as `~`
pub(crate) fn one_line(text: &str, cwd: &Path) -> String {
    let cwd = cwd.display().to_string();
    let mut text = shortened(text, &cwd, ".", "");
    if let Some(home) = env::home_dir() {
        let home = home.display().to_string();
        let inside = format!("~{MAIN_SEPARATOR}");
        text = shortened(&text, &home, "~", &inside);
    }

    let words: Vec<&str> = text.split_whitespace().collect();
    words.join(" ")
}

/// Writes `root` as `name` and paths in it starting with `inside`,
/// except where `root` only begins or ends a longer path or word
fn shortened(text: &str, root: &str, name: &str, inside: &str) -> String {
    // An empty root would match at every position and never advance
    if root.is_empty() {
        return text.to_owned();
    }

    let pathy = |c: char| c.is_alphanumeric() || matches!(c, '/' | '\\' | '.' | '-' | '_' | '~');
    let mut kept = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(root) {
        let (before, after) = (&rest[..at], &rest[at + root.len()..]);
        kept.push_str(before);
        let joined = before.chars().next_back().is_some_and(pathy);
        rest = after;
        match after.chars().next() {
            _ if joined => kept.push_str(root),
            Some(MAIN_SEPARATOR) => {
                kept.push_str(inside);
                rest = &after[MAIN_SEPARATOR.len_utf8()..];
            }
            Some(next) if pathy(next) => kept.push_str(root),
            _ => kept.push_str(name),
        }
    }
    kept.push_str(rest);
    kept
}

fn spans_width(spans: &[(String, Style)]) -> usize {
    spans.iter().map(|(text, _)| text.width()).sum()
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
