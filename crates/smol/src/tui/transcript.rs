use crate::conversation::{Conversation, Entry, Step};
use crate::tui::render::{Render, Row};
use crate::tui::SPIN_EVERY;
use ratatui::layout::{Position, Rect};
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

/// Which rows of the conversation the screen shows, kept from frame to frame
#[derive(Debug, Default)]
pub(crate) struct Viewport {
    /// Rows scrolled back from the bottom
    pub(crate) back: usize,
    /// The most rows the last frame could scroll back
    bottom: usize,
    /// `back` as the last frame left it, which only scrolling changes before the next
    drawn_back: usize,
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

    /// Inverts `thumb`, so the thumb is drawn on the cell it was dragged to
    pub(crate) fn drag_to(&mut self, row: u16) {
        let last = usize::from(self.bar.height.saturating_sub(1));
        let offset = usize::from(row.saturating_sub(self.bar.top()));
        let top = (offset.min(last) * self.bottom + last / 2).checked_div(last);
        let top = top.unwrap_or(self.bottom);
        self.back = self.bottom - top.min(self.bottom);
    }

    /// Shows `total` rows `height` at a time, where a view scrolled back stays on what it showed,
    /// at the row `anchored` once everything was laid out again, and returns the first row shown
    /// and whether scrolling moved it since the last frame
    pub(crate) fn settle(
        &mut self,
        total: usize,
        height: usize,
        anchored: Option<usize>,
    ) -> (usize, bool) {
        let scrolled = self.back != self.drawn_back;
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
        self.drawn_back = self.back;
        self.bar = Rect::default();
        (self.top(), scrolled)
    }

    /// Puts the scrollbar in the last column of `track` and returns where its thumb goes,
    /// or None when there is nothing to scroll
    pub(crate) fn place_bar(&mut self, track: Rect) -> Option<Position> {
        // Without a track the thumb would land on the corner
        if self.bottom == 0 || track.is_empty() {
            return None;
        }

        let column = track.right().saturating_sub(1);
        self.bar = Rect::new(column, track.top(), 1, track.height);
        let row = track.top() + thumb(self.top(), self.bottom, track.height);
        Some(Position::new(column, row))
    }
}

pub(crate) fn thumb(top: usize, bottom: usize, length: u16) -> u16 {
    let last = usize::from(length.saturating_sub(1));
    let row = (top * last + bottom / 2).checked_div(bottom).unwrap_or(0);
    row.min(last) as u16
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

    fn render(&self, width: u16, expanded: bool) -> Render {
        let mut render = Render::new(width, expanded);
        render.cwd = self.cwd.clone();
        render
    }

    pub(crate) fn rows(&mut self, talk: &Conversation, width: u16, expanded: bool) -> Rows<'_> {
        // A picture that finished loading changes the height of its rows
        let layout = (width, expanded, self.loaded);
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
            let rows = self.placed(group);
            let hidden = group.entry.is_some() && !self.on_screen(rows);
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
                // Hiding the area moves these rows again, so only scrolling takes agents back out of it
                let kept = self.pins.contains(&call.id) && !self.scrolled;
                let rows = self.placed(group);
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

    /// Where a group's rows are among all rows
    fn placed(&self, group: &Group) -> Range<usize> {
        let start = match group.entry {
            Some(index) => self.starts[index],
            None => self.settled.len(),
        };
        start + group.rows.start..start + group.rows.end
    }

    /// Before the screen is known every row counts as on it
    fn on_screen(&self, rows: Range<usize>) -> bool {
        match &self.view {
            Some(view) => rows.start < view.end && view.start < rows.end,
            None => true,
        }
    }
}
