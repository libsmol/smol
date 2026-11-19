use crate::tui::render::Row;
use crate::tui::theme::LINE;
use crate::tui::transcript::Rows;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use std::ops::Range;
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

            let end = columns.end.min(width).min(row.line.width());
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
