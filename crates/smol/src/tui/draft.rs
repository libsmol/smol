use crate::chat::image_placeholder;
use crate::tui::{DIM, SELECTED};
use ratatui::style::Style;
use ratatui_textarea::{CursorMove, DataCursor, TextArea, WrapMode};

const MAX_UNFOLDED_LINES: usize = 5;
pub(crate) const MAX_UNFOLDED_CHARS: usize = 1000;

/// What the user writes in the input box
pub(crate) struct Draft {
    pub(crate) area: TextArea<'static>,
    /// The leading ! is not in `area`, and `whole` gives it back
    pub(crate) shell: bool,
    /// The first screen row the box shows, which goes back to 0 whenever `area` is replaced
    pub(crate) top: u16,
    /// The k-th item is the original text of placeholder #k
    pastes: Vec<String>,
}

impl Default for Draft {
    fn default() -> Self {
        Self {
            area: input_box(""),
            shell: false,
            top: 0,
            pastes: Vec::new(),
        }
    }
}

impl Draft {
    /// Without the leading ! of a shell command
    pub(crate) fn text(&self) -> String {
        self.area.lines().join("\n")
    }

    pub(crate) fn whole(&self) -> String {
        let text = self.text();
        match self.shell {
            true => format!("!{text}"),
            false => text,
        }
    }

    /// The whole text with each paste in place of its placeholder
    pub(crate) fn expanded(&self) -> String {
        let mut text = self.whole();
        // A later paste may contain the placeholder of an earlier one
        for (index, paste) in self.pastes.iter().enumerate().rev() {
            let shown = placeholder(index + 1, paste);
            text = text.replace(&shown, paste);
        }
        text
    }

    /// Where the cursor is in `text`, where a newline counts as one character
    pub(crate) fn cursor(&self) -> usize {
        let DataCursor(row, col) = self.area.cursor();
        let above = self.area.lines()[..row].iter();
        above.map(|line| line.chars().count() + 1).sum::<usize>() + col
    }

    /// A leading ! makes it a shell command
    pub(crate) fn replace(&mut self, text: &str) {
        let command = shell_command(text);
        self.shell = command.is_some();
        let text = command.unwrap_or(text);
        self.rewrite(text, None);
    }

    /// Takes a ! typed or pasted at the start as the start of a shell command
    pub(crate) fn take_bang(&mut self) {
        let text = self.text();
        let Some(command) = shell_command(&text).filter(|_| !self.shell) else {
            return;
        };

        let taken = text.chars().count() - command.chars().count();
        let cursor = self.cursor().saturating_sub(taken);
        self.shell = true;
        self.rewrite(command, Some(cursor));
    }

    /// A partly edited placeholder can't be expanded back, so a deletion removes it whole and an
    /// insertion inside moves after it, for pastes and for the `images` pasted by number
    pub(crate) fn edit<'a>(
        &mut self,
        change: impl FnOnce(&mut TextArea<'static>) -> bool,
        images: impl Iterator<Item = &'a usize>,
    ) -> bool {
        let before = self.text();
        let old_cursor = self.cursor();
        if !change(&mut self.area) {
            return false;
        }

        let after = self.text();
        let new_cursor = self.cursor();

        // The edit lies between the unchanged ends,
        // and repeated characters can carry the common prefix past it
        let old: Vec<char> = before.chars().collect();
        let new: Vec<char> = after.chars().collect();
        let same = |(a, b): &(&char, &char)| a == b;
        let head = old.iter().zip(&new).take_while(same).count();
        let head = head.min(old_cursor).min(new_cursor);
        let rest = old.len().min(new.len()) - head;
        let ends = old.iter().rev().zip(new.iter().rev());
        let tail = ends.take(rest).take_while(same).count();
        let edited = (head, old.len() - tail);
        let (mut start, mut end) = edited;

        let mut placeholders = Vec::new();
        for (index, paste) in self.pastes.iter().enumerate() {
            let shown = placeholder(index + 1, paste);
            placeholders.push(shown);
        }
        for &number in images {
            let shown = image_placeholder(number);
            placeholders.push(shown);
        }
        for shown in &placeholders {
            for (at, _) in before.match_indices(shown.as_str()) {
                let from = before[..at].chars().count();
                let to = from + shown.chars().count();
                if start < end && start < to && end > from {
                    start = start.min(from);
                    end = end.max(to);
                } else if start == end && from < start && start < to {
                    start = to;
                    end = to;
                }
            }
        }

        if (start, end) == edited {
            return true;
        }

        let inserted = &new[head..new.len() - tail];
        let kept = old[..start].iter().chain(inserted).chain(&old[end..]);
        let text: String = kept.collect();
        let cursor = start + inserted.len();
        self.rewrite(&text, Some(cursor));

        true
    }

    /// Long text as the placeholder it goes in the draft as, kept to expand on submit
    pub(crate) fn folded(&mut self, text: String) -> String {
        let short = text.chars().count() <= MAX_UNFOLDED_CHARS;
        if short && text.lines().count() <= MAX_UNFOLDED_LINES {
            return text;
        }

        // A ! hidden in the placeholder would still run the paste as a shell command
        let (marker, text) = match shell_command(&text) {
            Some(command) => ("!", command.to_owned()),
            None => ("", text),
        };
        let number = self.pastes.len() + 1;
        let shown = placeholder(number, &text);
        self.pastes.push(text);
        format!("{marker}{shown}")
    }

    /// Keeps `shell`, with the cursor at the end or at `cursor`
    fn rewrite(&mut self, text: &str, cursor: Option<usize>) {
        self.area = input_box(text);
        self.top = 0;
        let Some(cursor) = cursor else {
            return;
        };

        let placed: String = text.chars().take(cursor).collect();
        let row = placed.matches('\n').count();
        let line = placed.rsplit('\n').next().unwrap_or_default();
        let col = line.chars().count();
        let row = u16::try_from(row).unwrap_or(u16::MAX);
        let col = u16::try_from(col).unwrap_or(u16::MAX);
        self.area.move_cursor(CursorMove::Jump(row, col));
    }
}

pub(crate) fn input_box(text: &str) -> TextArea<'static> {
    let lines = text.split('\n').map(str::to_owned).collect();
    let mut input = TextArea::new(lines);
    input.move_cursor(CursorMove::Bottom);
    input.move_cursor(CursorMove::End);

    input.set_wrap_mode(WrapMode::Glyph);
    input.set_placeholder_style(DIM);
    input.set_selection_style(SELECTED);
    input.set_cursor_line_style(Style::new());
    // Input methods place their candidate window at the hardware cursor,
    // so the textarea cursor is not drawn
    input.set_cursor_style(Style::new());

    input
}

fn placeholder(number: usize, text: &str) -> String {
    let lines = text.lines().count();
    if lines > 1 {
        return format!("[paste #{number}: {lines} lines]");
    }

    let chars = text.chars().count();
    format!("[paste #{number}: {chars} characters]")
}

/// Strips leading whitespace first, as submitting does
fn shell_command(text: &str) -> Option<&str> {
    let command = text.trim_start().strip_prefix('!')?;
    Some(command.trim_start())
}
