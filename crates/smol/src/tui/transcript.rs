use crate::conversation::{Conversation, Entry};
use crate::tui::render::{Render, Row};
use ratatui::layout::{Position, Rect};
use std::path::PathBuf;

const UNSENT_HINT: &str = "Goes to the model with your next message";

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

/// Which rows of the conversation the screen shows, kept from frame to frame
#[derive(Debug, Default)]
pub(crate) struct Viewport {
    /// Rows scrolled back from the bottom
    pub(crate) back: usize,
    /// The most rows the last frame could scroll back
    bottom: usize,
    /// Where the last frame drew the scrollbar, empty without one
    pub(crate) bar: Rect,
}

impl Viewport {
    pub(crate) fn top(&self) -> usize {
        self.bottom.saturating_sub(self.back)
    }

    pub(crate) fn scroll_up(&mut self, rows: usize) {
        self.back = self.back.saturating_add(rows);
    }

    pub(crate) fn scroll_down(&mut self, rows: usize) {
        self.back = self.back.saturating_sub(rows);
    }

    /// Inverts where `place_bar` puts the thumb, so it is drawn on the cell it was dragged to
    pub(crate) fn drag_to(&mut self, row: u16) {
        let last = usize::from(self.bar.height.saturating_sub(1));
        let offset = usize::from(row.saturating_sub(self.bar.top()));
        let top = (offset.min(last) * self.bottom + last / 2).checked_div(last);
        let top = top.unwrap_or(self.bottom);
        self.back = self.bottom - top.min(self.bottom);
    }

    /// Shows `total` rows `height` at a time, where a view scrolled back stays on what it showed,
    /// at the row `anchored` once everything was laid out again, and returns the first row shown
    pub(crate) fn settle(&mut self, total: usize, height: usize, anchored: Option<usize>) -> usize {
        let bottom = total.saturating_sub(height);
        match anchored {
            Some(top) => self.back = bottom.saturating_sub(top),
            // While scrolled back, content growing or shrinking must not shift what is being read
            None if self.back > 0 => {
                let moved = self.back.saturating_add(bottom);
                self.back = moved.saturating_sub(self.bottom);
            }
            None => {}
        }

        self.back = self.back.min(bottom);
        self.bottom = bottom;
        self.bar = Rect::default();
        self.top()
    }

    /// Puts the scrollbar in the last column of `track` and returns where its thumb goes,
    /// or None when there is nothing to scroll
    pub(crate) fn place_bar(&mut self, track: Rect) -> Option<Position> {
        // The thumb's row below divides by the rows to scroll and needs a row to move in
        if self.bottom == 0 || track.is_empty() {
            return None;
        }

        let column = track.right().saturating_sub(1);
        self.bar = Rect::new(column, track.top(), 1, track.height);
        let last = usize::from(track.height - 1);
        let offset = (self.top() * last + self.bottom / 2) / self.bottom;
        let row = track.top() + offset.min(last) as u16;
        Some(Position::new(column, row))
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
    pub(crate) loaded: u64,
    /// The session's directory, where relative paths start
    cwd: PathBuf,
}

impl Transcript {
    pub(crate) fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            ..Self::default()
        }
    }

    /// Before a layout change, records which entry `row` belongs to and its offset there
    /// for `Rows::row_at` to find after relayout
    pub(crate) fn anchor(&self, row: usize, width: u16, expanded: bool) -> Option<(usize, usize)> {
        if self.layout == (width, expanded, self.loaded) {
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

    pub(crate) fn rows(&mut self, talk: &Conversation, width: u16, expanded: bool) -> Rows<'_> {
        // A picture that finished loading changes the height of its rows
        let layout = (width, expanded, self.loaded);
        let relaid = self.layout != layout;
        if relaid {
            self.layout = layout;
            self.settled.clear();
            self.starts.clear();
        }

        let mut render = Render::new(width, expanded);
        render.cwd = self.cwd.clone();
        let rendered = self.starts.len();
        for entry in talk.entries().iter().skip(rendered) {
            let start = self.settled.len() + render.rows.len();
            self.starts.push(start);
            render.entry(entry);
        }
        self.settled.append(&mut render.rows);

        if relaid || self.drawn != talk.revision() {
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
            self.held = render.rows;
        }

        Rows {
            settled: &self.settled,
            held: &self.held,
            starts: &self.starts,
        }
    }
}
