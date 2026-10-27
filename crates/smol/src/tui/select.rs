use crate::tui::render::Row;
use crate::tui::theme::LINE;
use crate::tui::transcript::Rows;
use ratatui::buffer::Buffer;
use ratatui::crossterm::clipboard::CopyToClipboard;
use ratatui::crossterm::execute;
use ratatui::layout::Rect;
use ratatui::style::Style;
use std::env;
use std::io::{stdout, Write};
use std::ops::Range;
use std::process::{Command, Stdio};
use std::thread;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const SELECTED: Style = Style::new().bg(LINE);

/// Rows count those scrolled off screen, and columns are display columns
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Point {
    pub(crate) row: usize,
    pub(crate) col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) anchor: Point,
    pub(crate) cursor: Point,
}

impl Selection {
    fn ordered(&self) -> (Point, Point) {
        let start = self.anchor.min(self.cursor);
        let end = self.anchor.max(self.cursor);
        (start, end)
    }

    fn columns(&self, row: usize, mark_width: usize) -> Option<Range<usize>> {
        let (start, end) = self.ordered();
        if row < start.row || row > end.row {
            return None;
        }

        let from = if row == start.row { start.col } else { 0 };
        let to = if row == end.row {
            end.col + 1
        } else {
            usize::MAX
        };
        let from = from.max(mark_width);
        (from < to).then_some(from..to)
    }

    pub(crate) fn highlight(&self, rows: Rows, top: usize, area: Rect, buf: &mut Buffer) {
        let width = usize::from(area.width);
        for (y, index) in (area.top()..area.bottom()).zip(top..) {
            let Some(row) = rows.get(index) else {
                break;
            };
            let Some(columns) = self.columns(index, row.mark_width) else {
                continue;
            };

            let end = columns.end.min(width);
            if columns.start < end {
                let x = area.x + columns.start as u16;
                let width = (end - columns.start) as u16;
                let cells = Rect::new(x, y, width, 1);
                buf.set_style(cells, SELECTED);
            }
        }
    }

    /// A word split by a soft wrap (an empty joint) continues into the rows above and below
    pub(crate) fn word(rows: Rows, point: Point) -> Option<Self> {
        rows.get(point.row)?;
        let glued = |index| {
            rows.get(index)
                .is_some_and(|row| row.joint.as_deref() == Some(""))
        };
        let (mut top, mut bottom) = (point.row, point.row);
        while top > 0 && glued(top) {
            top -= 1;
        }
        while glued(bottom + 1) {
            bottom += 1;
        }

        let mut cells = Vec::new();
        for index in top..=bottom {
            let row = rows.get(index)?;
            for (col, grapheme) in graphemes(row) {
                if col >= row.mark_width {
                    let at = Point { row: index, col };
                    let width = grapheme.width().max(1);
                    cells.push((at, width, grapheme));
                }
            }
        }

        let worded = |c: char| c.is_alphanumeric() || c == '_';
        let in_word = |&(_, _, grapheme): &(Point, usize, &str)| grapheme.starts_with(worded);
        let under = |&(at, width, _): &(Point, usize, &str)| {
            at.row == point.row && (at.col..at.col + width).contains(&point.col)
        };
        let hit = cells.iter().position(under)?;
        if !in_word(&cells[hit]) {
            return None;
        }

        let before = cells[..hit].iter().rposition(|cell| !in_word(cell));
        let first = before.map_or(0, |i| i + 1);
        let length = cells[hit..].iter().take_while(|cell| in_word(cell)).count();
        let last = hit + length - 1;

        let (end, width, _) = cells[last];
        let cursor = Point {
            col: end.col + width - 1,
            ..end
        };
        Some(Self {
            anchor: cells[first].0,
            cursor,
        })
    }

    pub(crate) fn text(&self, rows: Rows) -> String {
        let (start, end) = self.ordered();
        let mut text = String::new();
        let mut copied = false;

        for index in start.row..=end.row {
            let Some(row) = rows.get(index) else {
                break;
            };
            // A row left out whole adds no line break either
            let Some(columns) = self.columns(index, row.mark_width) else {
                continue;
            };
            if copied {
                text += row.joint.as_deref().unwrap_or("\n");
            }
            copied = true;

            for (col, grapheme) in graphemes(row) {
                if columns.contains(&col) {
                    text.push_str(grapheme);
                }
            }
        }

        text
    }
}

/// Measures graphemes like ratatui does,
/// since composed emoji measured per char come out several columns too wide
fn graphemes(row: &Row) -> Vec<(usize, &str)> {
    let mut found = Vec::new();
    let mut col = 0;
    let spans = row.line.spans.iter();
    for grapheme in spans.flat_map(|span| span.content.graphemes(true)) {
        found.push((col, grapheme));
        col += grapheme.width();
    }
    found
}

/// Some terminals ignore OSC 52
pub(crate) fn copy(text: &str) {
    let osc = CopyToClipboard::to_clipboard_from(text);
    execute!(stdout(), osc).ok();

    let commands: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["pbcopy"]]
    } else if cfg!(windows) {
        &[&["clip"]]
    } else if env::var_os("WAYLAND_DISPLAY").is_some() {
        &[&["wl-copy"]]
    } else if env::var_os("DISPLAY").is_some() {
        &[
            &["xclip", "-selection", "clipboard"],
            &["xsel", "--clipboard", "--input"],
        ]
    } else {
        &[]
    };

    // clip reads input in the console code page unless it is UTF-16 with a BOM
    let bytes = match cfg!(windows) {
        true => {
            let mut bytes = vec![0xFF, 0xFE];
            let units = text.encode_utf16().flat_map(u16::to_le_bytes);
            bytes.extend(units);
            bytes
        }
        false => text.as_bytes().to_vec(),
    };

    for command in commands {
        let child = Command::new(command[0])
            .args(&command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        if let Ok(mut child) = child {
            // stdin must be closed before waiting or the child never sees the end of input
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(&bytes).ok();
            }
            thread::spawn(move || child.wait());
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::tui::select::*;
    use crate::tui::transcript::{Call, Entry, Helper, Item, Step, Transcript};
    use agent_client_protocol_schema::v1::ToolCallStatus;

    fn rows(transcript: &mut Transcript) -> Rows<'_> {
        transcript.rows(12, false)
    }

    fn select(from: (usize, usize), to: (usize, usize)) -> Selection {
        let point = |(row, col)| Point { row, col };
        Selection {
            anchor: point(to),
            cursor: point(from),
        }
    }

    #[test]
    fn copy_skips_marks() {
        let mut transcript = Transcript::default();
        let failure = "division by zero\nline 2".to_owned();
        transcript.push(Entry::Failure(failure));

        let rows = rows(&mut transcript);
        let all = select((1, 0), (3, 40)).text(rows);
        assert_eq!(all, "division by zero\nline 2");
        assert_eq!(select((1, 4), (2, 3)).text(rows), "vision by");
        assert_eq!(select((1, 0), (1, 1)).text(rows), "");
    }

    #[test]
    fn double_click_word() {
        let mut transcript = Transcript::default();
        transcript.push(Entry::Failure("MAX_LEN crème_brûlée x.rs".to_owned()));

        let rows = rows(&mut transcript);
        let word = |row, col| Selection::word(rows, Point { row, col }).map(|word| word.text(rows));
        assert_eq!(word(1, 5).as_deref(), Some("MAX_LEN"));
        assert_eq!(word(1, 2).as_deref(), Some("MAX_LEN"));
        assert_eq!(word(2, 11).as_deref(), Some("crème_brûlée"));
        assert_eq!(word(3, 3).as_deref(), Some("crème_brûlée"));
        assert_eq!(word(3, 5).as_deref(), Some("x"));
        assert_eq!(word(3, 4), None);
        assert_eq!(word(1, 0), None);
    }

    #[test]
    fn emoji_columns() {
        for text in ["✔️ done", "👨‍💻 done", "👍🏽 done"] {
            let mut transcript = Transcript::default();
            transcript.push(Entry::Failure(text.to_owned()));

            let rows = rows(&mut transcript);
            assert_eq!(select((1, 5), (1, 6)).text(rows), "do", "{text}");
            let word = Selection::word(rows, Point { row: 1, col: 5 });
            let word = word.map(|word| word.text(rows));
            assert_eq!(word.as_deref(), Some("done"), "{text}");
        }
    }

    #[test]
    fn quote_copies_text() {
        let message = Item::Message {
            text: "> first\n>\n> second".to_owned(),
            answer: false,
        };
        let step = Step {
            items: vec![message],
            ..Step::default()
        };
        let mut transcript = Transcript::default();
        transcript.push(Entry::Step(step));

        let rows = rows(&mut transcript);
        assert_eq!(select((1, 0), (3, 40)).text(rows), "first\n\nsecond");
    }

    #[test]
    fn list_wrap_copy() {
        let message = Item::Message {
            text: "- 🍕🍕 🚀🚀 🎉🎉".to_owned(),
            answer: false,
        };
        let step = Step {
            items: vec![message],
            ..Step::default()
        };
        let mut transcript = Transcript::default();
        transcript.push(Entry::Step(step));

        let rows = rows(&mut transcript);
        assert_eq!(rows.len(), 3);
        assert_eq!(select((1, 0), (2, 40)).text(rows), "• 🍕🍕 🚀🚀 🎉🎉");
    }

    #[test]
    fn agent_status_skipped() {
        let helper = |title: &str, status: ToolCallStatus, text: &str| Helper {
            id: "call_1.1".to_owned(),
            title: title.to_owned(),
            status,
            text: text.to_owned(),
            preview: String::new(),
            interrupted: false,
            started: None,
            color: 0,
            usage: None,
        };
        let counting = Helper {
            preview: "print(len(cafés))".to_owned(),
            ..helper("Count the cafés", ToolCallStatus::InProgress, "thinking")
        };
        let call = Call {
            id: "call_1".to_owned(),
            title: "eval".to_owned(),
            status: ToolCallStatus::Completed,
            content: vec!["```python\nagent(tasks)\n```".to_owned()],
            attachments: Vec::new(),
            retries: Vec::new(),
            helpers: vec![
                counting,
                helper(
                    "Spell naïve",
                    ToolCallStatus::Completed,
                    "It has a diaeresis.",
                ),
            ],
        };
        let step = Step {
            items: vec![Item::Call(call)],
            ..Step::default()
        };
        let mut transcript = Transcript::default();
        transcript.push(Entry::Step(step));

        let rows = transcript.rows(40, false);
        let all = select((0, 0), (rows.len(), 40)).text(rows);
        let expected = "\nagent(tasks)\nCount the cafés\nprint(len(cafés))\nSpell naïve";
        assert_eq!(all, expected);
        for (row, to) in [(2, 40), (5, 40), (6, 7)] {
            assert_eq!(select((row, 0), (row, to)).text(rows), "", "{row}");
        }
    }
}
