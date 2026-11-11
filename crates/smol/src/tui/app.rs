use crate::chat::{opens_mention, resolved, Output, Session};
use crate::client::{McpState, Selector};
use crate::conversation::{Entry, Outcome};
use crate::tui::desktop::Desktop;
use crate::tui::picture::{self, Source};
use crate::tui::render::Target;
use crate::tui::select::{Point, Selection};
use crate::tui::transcript::Transcript;
use crate::tui::ui::input_box;
use anyhow::Result;
use chrono::{DateTime, Local};
use ignore::WalkBuilder;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;
use ratatui_textarea::{CursorMove, DataCursor, TextArea};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::mem;
use std::path::{is_separator, Path, PathBuf, MAIN_SEPARATOR};
use std::time::{Duration, Instant};

const FLASH_WAIT: Duration = Duration::from_secs(2);
const DOUBLE_CLICK_WAIT: Duration = Duration::from_millis(500);
const ANIMATE_EVERY: Duration = Duration::from_millis(100);

const SHELL_USAGE: &str = "!<command>";

const MAX_UNFOLDED_LINES: usize = 5;
const MAX_UNFOLDED_CHARS: usize = 1000;
/// On Windows, characters typed with AltGr carry Ctrl and Alt
const ALTGR: KeyModifiers = KeyModifiers::CONTROL.union(KeyModifiers::ALT);

/// Keeps typing @ responsive even in the home directory
const MAX_PATHS: usize = 20_000;
const MAX_MATCHES: usize = 200;

pub(crate) struct App {
    pub(crate) session: Session,
    desktop: Box<dyn Desktop>,
    pub(crate) transcript: Transcript,
    pub(crate) queued: VecDeque<String>,
    pub(crate) input: TextArea<'static>,
    /// The leading ! is not in `input`, and `draft()` gives the whole text
    pub(crate) shell: bool,
    /// Must be reset whenever the input box is replaced
    pub(crate) input_top: u16,
    /// The k-th item is the original text of placeholder #k
    pastes: Vec<String>,
    history: History,
    pub(crate) choices: Vec<Choice>,
    pub(crate) highlight: ListState,
    /// The highlighted choice's label and when the highlight landed on it
    pub(crate) marquee: Option<(String, Instant)>,
    /// When the highlighted label, too long for the list, moves on next
    pub(crate) scroll_at: Option<Instant>,
    /// The memory Delete marked, which the next event clears unless it is Enter
    forgetting: Option<i64>,
    choices_closed: bool,
    pub(crate) choices_area: Rect,
    /// The list a command opened, asked of the agent once while its prefix stays typed
    opened: Option<(List, Vec<Pick>)>,
    /// Paths offered after an @ until the next submit, by listed directory or empty for the cwd
    paths: HashMap<String, Vec<String>>,
    pub(crate) expanded: bool,
    pub(crate) animating: bool,
    drawn: Instant,
    pub(crate) scrolled_back: usize,
    /// The most rows the previous frame could scroll back
    pub(crate) bottom: usize,
    /// `scrolled_back` as the previous frame left it, which only scrolling changes before the next
    pub(crate) drawn_back: usize,
    pub(crate) messages_area: Rect,
    pub(crate) pinned_area: Rect,
    pub(crate) scrollbar: Rect,
    pub(crate) input_area: Rect,
    /// The pictures the last frame drew
    pub(crate) shown: Vec<Source>,
    pub(crate) selection: Option<Selection>,
    pressed: Option<Point>,
    dragging: bool,
    last_click: Option<(Point, Instant)>,
    /// `Err` holds the reason it failed
    pub(crate) flash: Option<(Result<String, String>, Instant)>,
    pub(crate) quit: bool,
    dirty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum List {
    Sessions,
    Models,
    Efforts,
    Servers,
    Memories,
}

/// A row of a list, which takes exactly one line
#[derive(Clone)]
struct Pick {
    /// What `listed` writes first and the query matches
    value: String,
    label: String,
    detail: String,
    current: bool,
    completion: Completion,
}

#[derive(Clone)]
pub(crate) struct Choice {
    pub(crate) label: String,
    pub(crate) detail: String,
    completion: Completion,
}

#[derive(Clone)]
enum Completion {
    /// Text ending in a space still needs an argument, so Enter only fills it in without submitting
    Input(String),
    Mention {
        typed: usize,
        text: String,
    },
    /// A memory's id, which fills in nothing
    Memory(i64),
}

impl Choice {
    fn input(label: String, detail: &str, text: String) -> Self {
        Self {
            label,
            detail: detail.to_owned(),
            completion: Completion::Input(text),
        }
    }

    fn path(path: &str, typed: usize) -> Self {
        let quoted = path.contains(char::is_whitespace);
        let text = match (quoted, path.ends_with('/')) {
            (false, false) => format!("@{path} "),
            (false, true) => format!("@{path}"),
            (true, false) => format!("@\"{path}\" "),
            (true, true) => format!("@\"{path}"),
        };

        Self {
            label: path.to_owned(),
            detail: String::new(),
            completion: Completion::Mention { typed, text },
        }
    }

    pub(crate) fn submits(&self) -> bool {
        matches!(&self.completion, Completion::Input(text) if !text.ends_with(' '))
    }

    pub(crate) fn forgets(&self) -> bool {
        matches!(self.completion, Completion::Memory(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    New,
    List(List),
    Resume(String),
    Model(String),
    Effort(String),
    Mcp(bool, String),
    Shell(String),
    Quit,
    Invalid(String),
}

impl Command {
    fn parse(line: &str) -> Option<Self> {
        if let Some(command) = line.strip_prefix('!') {
            let command = command.trim();
            let parsed = match command.is_empty() {
                true => Self::Invalid(format!("usage: {SHELL_USAGE}")),
                false => Self::Shell(command.to_owned()),
            };
            return Some(parsed);
        }

        // Only `:` and letters name a command, and other lines starting with `:` are messages
        let (name, arg) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let word = name.strip_prefix(':').unwrap_or_default();
        if word.is_empty() || !word.chars().all(|c| c.is_ascii_alphabetic()) {
            return None;
        }

        let Some(spec) = COMMANDS.iter().find(|spec| spec.name == name) else {
            let mut usages: Vec<String> = COMMANDS.iter().map(Spec::usage).collect();
            usages.push(SHELL_USAGE.to_owned());
            let commands = usages.join(", ");
            let text = format!("command: {name}: unknown (commands: {commands})");
            return Some(Self::Invalid(text));
        };

        // An argument is picked from the list the bare command opens
        let parsed = match (&spec.bare, arg.trim()) {
            (bare, "") => Some(bare.clone()),
            (Self::List(List::Sessions), id) => Some(Self::Resume(id.to_owned())),
            (Self::List(List::Models), model) => Some(Self::Model(model.to_owned())),
            (Self::List(List::Efforts), effort) => Some(Self::Effort(effort.to_owned())),
            (Self::List(List::Servers), arg) => match arg.split_once(char::is_whitespace) {
                Some((switch @ ("on" | "off"), server)) => {
                    let server = server.trim().to_owned();
                    Some(Self::Mcp(switch == "on", server))
                }
                _ => None,
            },
            _ => None,
        };
        if parsed.is_some() {
            return parsed;
        }

        let usage = spec.usage();
        Some(Self::Invalid(format!("usage: {usage}")))
    }
}

struct Spec {
    name: &'static str,
    args: &'static str,
    summary: &'static str,
    /// What the command means without an argument
    bare: Command,
}

impl Spec {
    fn usage(&self) -> String {
        format!("{}{}", self.name, self.args)
    }

    /// The command with the space before its argument
    fn prefix(&self) -> String {
        format!("{} ", self.name)
    }
}

static COMMANDS: [Spec; 7] = [
    Spec {
        name: ":new",
        args: "",
        summary: "start a new session",
        bare: Command::New,
    },
    Spec {
        name: ":resume",
        args: " <session>",
        summary: "resume an earlier session",
        bare: Command::List(List::Sessions),
    },
    Spec {
        name: ":model",
        args: " <name>",
        summary: "switch the model",
        bare: Command::List(List::Models),
    },
    Spec {
        name: ":effort",
        args: " <level>",
        summary: "switch the reasoning effort",
        bare: Command::List(List::Efforts),
    },
    Spec {
        name: ":mcp",
        args: " <on|off> <server>",
        summary: "turn an MCP server on (again) or off",
        bare: Command::List(List::Servers),
    },
    Spec {
        name: ":memory",
        args: "",
        summary: "list the memories, and forget one with del then enter",
        bare: Command::List(List::Memories),
    },
    Spec {
        name: ":quit",
        args: "",
        summary: "quit smol",
        bare: Command::Quit,
    },
];

impl App {
    pub(crate) fn new(session: Session, desktop: Box<dyn Desktop>) -> Self {
        let cwd = session.client.cwd.clone();
        Self {
            session,
            desktop,
            transcript: Transcript::new(cwd),
            queued: VecDeque::new(),
            input: input_box(""),
            shell: false,
            input_top: 0,
            pastes: Vec::new(),
            history: History::default(),
            choices: Vec::new(),
            highlight: ListState::default().with_selected(Some(0)),
            marquee: None,
            scroll_at: None,
            forgetting: None,
            choices_closed: false,
            choices_area: Rect::default(),
            opened: None,
            paths: HashMap::new(),
            expanded: false,
            animating: false,
            drawn: Instant::now(),
            scrolled_back: 0,
            bottom: 0,
            drawn_back: 0,
            messages_area: Rect::default(),
            pinned_area: Rect::default(),
            scrollbar: Rect::default(),
            input_area: Rect::default(),
            shown: Vec::new(),
            selection: None,
            pressed: None,
            dragging: false,
            last_click: None,
            flash: None,
            quit: false,
            dirty: true,
        }
    }

    pub(crate) fn take_dirty(&mut self) -> bool {
        let flash = self.flash.as_ref();
        let faded = flash.is_some_and(|(_, at)| at.elapsed() > FLASH_WAIT);
        if faded {
            self.flash = None;
            self.dirty = true;
        }
        let loaded = picture::loaded();
        if loaded != self.transcript.pictures {
            self.transcript.pictures = loaded;
            self.dirty = true;
        }

        let animated = self.animating && self.drawn.elapsed() >= ANIMATE_EVERY;
        let scrolled = self.scroll_at.is_some_and(|at| at <= Instant::now());
        let busy = self.session.run.is_some();
        let dirty = mem::take(&mut self.dirty) || busy || animated || scrolled;
        if dirty {
            self.drawn = Instant::now();
        }
        dirty
    }

    pub(crate) fn draft(&self) -> String {
        let text = self.input.lines().join("\n");
        match self.shell {
            true => format!("!{text}"),
            false => text,
        }
    }

    pub(crate) fn view_top(&self) -> usize {
        self.bottom.saturating_sub(self.scrolled_back)
    }

    pub(crate) fn highlighted(&self) -> Option<usize> {
        let last = self.choices.len().checked_sub(1)?;
        let index = self.highlight.selected().unwrap_or(0);
        Some(index.min(last))
    }

    /// Whether Enter now forgets the highlighted memory
    pub(crate) fn marked(&self) -> bool {
        let Some(index) = self.highlighted() else {
            return false;
        };
        let choice = &self.choices[index];
        match (self.forgetting, &choice.completion) {
            (Some(marked), Completion::Memory(id)) => marked == *id,
            _ => false,
        }
    }

    pub(crate) fn submit_text(&mut self, text: &str) {
        let Some(command) = Command::parse(text) else {
            self.scrolled_back = 0;
            self.session.prompt(text);
            return;
        };

        // A queued command may run while a draft sits in the input box,
        // and then the list goes into the transcript
        let drafting = !self.draft().is_empty();
        let result = match &command {
            Command::List(list) if drafting => {
                let picks = self.picks(*list);
                picks.map(|picks| listed(*list, picks))
            }
            Command::List(list) => {
                if self.open_list(*list) {
                    let prefix = prefix(*list);
                    self.complete(&prefix);
                }
                return;
            }
            Command::Quit => {
                self.quit = true;
                return;
            }
            Command::New => return self.open(None),
            Command::Resume(id) => return self.open(Some(id)),
            Command::Model(name) => {
                let switched = self.session.choose(Selector::Model, name);
                let model = switched.map(|()| self.session.model());
                model.map(|model| format!("model: {model}"))
            }
            Command::Effort(level) => {
                let switched = self.session.choose(Selector::Effort, level);
                let effort = switched.map(|()| self.session.effort().unwrap_or_default());
                effort.map(|effort| format!("effort: {effort}"))
            }
            Command::Mcp(on, name) => {
                let switched = self.session.set_mcp(*on, name);
                switched.map(|status| status.to_string())
            }
            Command::Shell(command) => {
                self.scrolled_back = 0;
                return self.session.shell(command);
            }
            Command::Invalid(text) => return self.failure(text),
        };

        match result {
            Ok(text) => self.info(text),
            Err(error) => {
                let text = format!("{error:#}");
                self.failure(text);
            }
        }
    }

    fn open(&mut self, resume: Option<&str>) {
        if let Err(error) = self.session.open(resume) {
            let text = format!("{error:#}");
            return self.failure(text);
        }

        let cwd = self.session.client.cwd.clone();
        self.transcript = Transcript::new(cwd);
        picture::clear();
        self.history = History::default();
        self.paths.clear();
        // A queued command runs with no key press to clear the selection,
        // which would land on the new transcript by row number
        self.selection = None;
        self.pressed = None;

        if resume.is_some() {
            self.show_session();
        }
    }

    pub(crate) fn on_event(&mut self, event: Event) {
        let moved = matches!(&event, Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved);
        // Windows reports every key once more on release
        let released = matches!(&event, Event::Key(key) if key.kind == KeyEventKind::Release);
        if moved || released || matches!(event, Event::FocusGained | Event::FocusLost) {
            return;
        }

        self.dirty = true;
        let typed = matches!(event, Event::Key(_) | Event::Paste(_));
        let forgetting = self.forgetting.take();

        match event {
            Event::Key(key) => {
                self.selection = None;

                let count = self.choices.len();
                let index = self.highlighted().unwrap_or(0);
                let choice = self.choices.get(index).cloned();

                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let plain = key.modifiers.is_empty();
                let running = self.session.running();
                let idle = self.session.run.is_none() && self.draft().is_empty();
                let page = usize::from(self.messages_area.height);
                // The input box wraps, so this counts screen rows
                let top = self.input.screen_cursor().row == 0;
                let first = self.input.cursor() == DataCursor(0, 0);
                let text = self.input.lines().join("\n");
                let last = offset(&self.input) == text.chars().count();

                match (choice, key.code) {
                    (Some(_), KeyCode::Up) => {
                        let previous = (index + count - 1) % count;
                        self.highlight.select(Some(previous));
                    }
                    (Some(_), KeyCode::Down) => {
                        let next = (index + 1) % count;
                        self.highlight.select(Some(next));
                    }
                    (Some(choice), KeyCode::Tab) => self.fill(&choice.completion),
                    (
                        Some(Choice {
                            completion: Completion::Memory(id),
                            ..
                        }),
                        KeyCode::Enter,
                    ) if plain && forgetting == Some(id) => {
                        match self.session.forget(id) {
                            Ok(()) => {
                                let text = format!(
                                    "forgot memory {id}; the system prompt drops it next session"
                                );
                                self.info(text);
                            }
                            Err(error) => {
                                let text = format!("{error:#}");
                                self.failure(text);
                            }
                        }
                        // Asked again on the refresh below, without the memory
                        self.opened = None;
                    }
                    (Some(choice), KeyCode::Enter) if plain => {
                        self.fill(&choice.completion);
                        if choice.submits() {
                            self.submit();
                        }
                    }
                    // Taking the mark above already dropped it, and the list stays open
                    (Some(_), KeyCode::Esc) if forgetting.is_some() => {}
                    (Some(_), KeyCode::Esc) => self.choices_closed = true,
                    (
                        Some(Choice {
                            completion: Completion::Memory(id),
                            ..
                        }),
                        KeyCode::Delete,
                    ) if plain && last => self.forgetting = Some(id),

                    (_, KeyCode::Char('c')) if ctrl => match self.session.run {
                        Some(_) if running => self.session.cancel(),
                        // The way out when an interrupt is stuck
                        Some(_) => self.quit = true,
                        None => {
                            self.set_input("");
                            self.history = History::default();
                        }
                    },
                    (_, KeyCode::Char('d')) if ctrl && idle => self.quit = true,
                    (_, KeyCode::Char('o')) if ctrl => self.expanded = !self.expanded,
                    (_, KeyCode::Char('j')) if ctrl => {
                        self.edit(|input| input.insert_str("\n"));
                    }
                    (_, KeyCode::Enter) if plain => self.submit(),
                    (_, KeyCode::Enter) => {
                        self.edit(|input| input.insert_str("\n"));
                    }
                    (_, KeyCode::Esc) if self.scrolled_back > 0 => self.scrolled_back = 0,
                    (_, KeyCode::Esc) if running => self.session.cancel(),
                    // An invisible ! sits before the cursor
                    (_, KeyCode::Backspace) if self.shell && first => self.shell = false,
                    (_, KeyCode::PageUp) => self.scroll_up(page),
                    (_, KeyCode::PageDown) => self.scroll_down(page),
                    // Must come before the history arm
                    (_, KeyCode::Up) if top && !self.queued.is_empty() => {
                        if let Some(text) = self.queued.pop_back() {
                            let mut text = self.folded(text);
                            let draft = self.draft();
                            if !draft.is_empty() {
                                text = format!("{text}\n{draft}");
                            }
                            self.set_input(&text);
                            // The next Up must still find the cursor on the first screen row
                            self.input.move_cursor(CursorMove::Top);
                            self.input.move_cursor(CursorMove::Head);
                            self.choices_closed = true;
                        }
                    }
                    (_, KeyCode::Up) if top => {
                        let current = self.draft();
                        let sent = self.session.conversation.sent();
                        if let Some(text) = self.history.back(&sent, current) {
                            let text = self.folded(text);
                            self.set_input(&text);
                            self.choices_closed = true;
                        }
                    }
                    (_, KeyCode::Down) => {
                        let before = self.input.screen_cursor().row;
                        self.input.move_cursor(CursorMove::Down);
                        // Only a cursor that can't move down is on the last screen row
                        if self.input.screen_cursor().row == before {
                            let sent = self.session.conversation.sent();
                            if let Some((text, draft)) = self.history.forward(&sent) {
                                // The user's own draft comes back exactly as typed
                                let text = match draft {
                                    true => text,
                                    false => self.folded(text),
                                };
                                self.set_input(&text);
                                self.choices_closed = true;
                            }
                        }
                    }
                    _ => {
                        // The textarea drops characters carrying Ctrl+Alt as shortcuts
                        let mut key = key;
                        if matches!(key.code, KeyCode::Char(_)) && key.modifiers.contains(ALTGR) {
                            key.modifiers.remove(ALTGR);
                        }

                        if self.edit(|input| input.input(key)) {
                            self.highlight.select_first();
                            self.choices_closed = false;
                        }
                    }
                }
            }

            Event::Paste(text) => {
                self.selection = None;
                let text = text.replace("\r\n", "\n").replace('\r', "\n");
                let text = self.folded(text);
                if self.edit(|input| input.insert_str(text)) {
                    self.highlight.select_first();
                    self.choices_closed = false;
                }
            }

            Event::Mouse(mouse) => {
                let position = Position::new(mouse.column, mouse.row);
                let on_choices = self.choices_area.contains(position);
                let area = self.messages_area;
                let inside = area.contains(position);
                let scrolls = inside || self.pinned_area.contains(position);
                let on_bar = self.scrollbar.contains(position);

                match mouse.kind {
                    // A drag of the scrollbar ends wherever the button is let go
                    MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                        self.drag_to(mouse.row);
                    }
                    MouseEventKind::Up(MouseButton::Left) if self.dragging => {
                        self.dragging = false;
                    }

                    MouseEventKind::ScrollUp if on_choices => self.highlight.select_previous(),
                    MouseEventKind::ScrollDown if on_choices => self.highlight.select_next(),
                    _ if on_choices => {}

                    MouseEventKind::ScrollUp if scrolls => self.scroll_up(3),
                    MouseEventKind::ScrollDown if scrolls => self.scroll_down(3),

                    MouseEventKind::Down(MouseButton::Left) if on_bar => {
                        self.dragging = true;
                        self.selection = None;
                        self.drag_to(mouse.row);
                    }

                    MouseEventKind::Down(MouseButton::Left) => {
                        let point = self.point(mouse).filter(|_| inside);
                        let recent = |(at, time): (Point, Instant)| {
                            Some(at) == point && time.elapsed() < DOUBLE_CLICK_WAIT
                        };
                        let double = self.last_click.is_some_and(recent);

                        // A third click is not another double click and starts over
                        let single = point.filter(|_| !double);
                        self.last_click = single.map(|point| (point, Instant::now()));
                        self.pressed = single;
                        self.selection = None;

                        if let Some(point) = point.filter(|_| double) {
                            let talk = &self.session.conversation;
                            let rows = self.transcript.rows(talk, area.width, self.expanded);
                            self.selection = Selection::word(rows, point);
                        }
                    }

                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let (Some(anchor), Some(cursor)) = (self.pressed, self.point(mouse)) {
                            if mouse.row < area.top() {
                                self.scroll_up(1);
                            } else if mouse.row >= area.bottom() {
                                self.scroll_down(1);
                            }

                            if cursor != anchor || self.selection.is_some() {
                                self.selection = Some(Selection { anchor, cursor });
                            }
                        }
                    }

                    MouseEventKind::Up(MouseButton::Left) => {
                        let pressed = self.pressed.take();
                        let talk = &self.session.conversation;
                        let Some(selection) = self.selection else {
                            // No drag means it was a click
                            let rows = self.transcript.rows(talk, area.width, self.expanded);
                            let Some(point) = pressed else {
                                return;
                            };
                            let row = rows.get(point.row);
                            let mut targets = row.into_iter().flat_map(|row| &row.targets);
                            let hit = targets.find(|(columns, _)| columns.contains(&point.col));
                            let Some((_, target)) = hit.cloned() else {
                                return;
                            };

                            let opening = match target {
                                Target::Url(_) => "opening the link in a browser",
                                Target::Diagram(_) => "opening the diagram in a browser",
                                Target::Picture(_) => "opening the image",
                            };
                            let opened = self.desktop.open(&target);
                            let told = opened.map(|()| opening.to_owned());
                            self.flash = Some((told, Instant::now()));
                            return;
                        };

                        let rows = self.transcript.rows(talk, area.width, self.expanded);
                        let text = selection.text(rows);
                        if !text.trim().is_empty() {
                            self.desktop.copy(&text);
                            let count = text.chars().count();
                            let copied = format!("copied {count} characters");
                            self.flash = Some((Ok(copied), Instant::now()));
                        }
                    }

                    _ => {}
                }
            }

            Event::Resize(..) => self.selection = None,
            _ => {}
        }

        if typed {
            // A ! typed or pasted at the start is removed by `set_input`
            let text = self.input.lines().join("\n");
            if let Some(command) = shell_command(&text).filter(|_| !self.shell) {
                let taken = text.chars().count() - command.chars().count();
                let cursor = offset(&self.input).saturating_sub(taken);
                self.set_input(&text);
                jump(&mut self.input, cursor);
            }
            self.refresh();
        }
    }

    pub(crate) fn pump(&mut self) {
        let mut changed = false;
        // Queued messages go out only once every output is drained
        let mut finished = false;
        while let Some(output) = self.session.poll() {
            self.dirty = true;

            match output {
                Output::Shown => {}
                Output::Servers => changed = true,

                // A command waits for the turn to end, and so does everything queued behind it
                Output::Queued(asked) => {
                    let count = match self.session.running() {
                        true => self.queued.iter().take_while(|text| plain(text)).count(),
                        false => 0,
                    };
                    let texts = self.queued.drain(..count);
                    self.session.answer(asked, texts);
                }

                Output::Done => {
                    finished = true;
                    changed = true;
                }
            }
        }

        if finished {
            while self.session.run.is_none() && !self.quit {
                let Some(next) = self.queued.pop_front() else {
                    break;
                };
                self.submit_text(&next);
            }
        }
        if changed {
            self.refresh();
        }
    }

    /// The replayed history arrives before the `session/load` reply
    pub(crate) fn show_session(&mut self) {
        self.scrolled_back = 0;
        self.pump();
        self.session.conversation.finish_turn(Outcome::Answered);

        let text = format!("resumed session {}", self.session.id);
        self.info(text);
    }

    /// Must be called whenever the input, status or options change
    fn refresh(&mut self) {
        if self.shell {
            self.choices.clear();
            return;
        }

        let mut listing = None;
        if let [line] = self.input.lines() {
            for spec in &COMMANDS {
                let Command::List(list) = spec.bare else {
                    continue;
                };
                if line.starts_with(&spec.prefix()) {
                    listing = Some(list);
                    break;
                }
            }
        }

        // A list opens once its prefix is typed, which is when sessions and memories are asked for
        let opened = self.opened.as_ref().map(|(list, _)| *list);
        if listing.is_none() {
            self.opened = None;
        }
        if let Some(list) = listing.filter(|_| opened != listing) {
            self.open_list(list);
        }

        let DataCursor(row, col) = self.input.cursor();
        let line = self.input.lines().get(row).map_or("", String::as_str);
        let before: String = line.chars().take(col).collect();
        let mention = typing_mention(&before);
        let query = mention.map_or("", |(_, query)| query);
        let scope = outside_dir(query).unwrap_or_default();
        if mention.is_some() && !self.paths.contains_key(scope) {
            let cwd = &self.session.client.cwd;
            let paths = if scope.is_empty() {
                walk(cwd, MAX_PATHS)
            } else {
                // Outside the cwd only one level is listed,
                // which stays fast even in the home or root directory
                let mut paths = Vec::new();
                let dir = resolved(scope, cwd);
                if let Ok(entries) = fs::read_dir(dir) {
                    for entry in entries.flatten().take(MAX_PATHS) {
                        // A name that doesn't convert back could not be found once picked
                        let Ok(name) = entry.file_name().into_string() else {
                            continue;
                        };

                        let mut path = format!("{scope}{name}");
                        if entry.path().is_dir() {
                            path.push('/');
                        }
                        paths.push(path);
                    }
                }
                paths
            };
            self.paths.insert(scope.to_owned(), paths);
        }

        self.choices.clear();
        if self.choices_closed {
            return;
        }

        if let (Some((typed, query)), Some(paths)) = (mention, self.paths.get(scope)) {
            // Directories outside the cwd list hidden entries only after a typed .,
            // and they must go before sorting and truncation
            let name = query.rsplit(is_separator).next().unwrap_or_default();
            let dotted = scope.is_empty() || name.starts_with('.');
            let shown = |path: &&String| {
                let entry = path.strip_prefix(scope).unwrap_or(path);
                dotted || !entry.starts_with('.')
            };
            let candidates = paths.iter().filter(shown);
            for path in matching_paths(candidates, query) {
                let choice = Choice::path(path, typed);
                self.choices.push(choice);
            }
            return;
        }

        let [line] = self.input.lines() else {
            return;
        };
        let Some(list) = listing else {
            for spec in hints(line) {
                // A trailing space makes Enter fill without submitting,
                // and only commands that open a list get one
                let completion = match spec.bare {
                    Command::List(_) => spec.prefix(),
                    _ => spec.name.to_owned(),
                };
                let label = spec.usage();
                let choice = Choice::input(label, spec.summary, completion);
                self.choices.push(choice);
            }
            return;
        };

        let prefix = prefix(list);
        let arg = &line[prefix.len()..];
        if list == List::Servers {
            let mut offers = Vec::new();
            for server in &self.session.mcp {
                let switches: &[(&str, &str)] = match server.state {
                    McpState::Off => &[("on", "turn on")],
                    McpState::Connecting => &[("off", "turn off")],
                    _ => &[("on", "reconnect"), ("off", "turn off")],
                };
                for &(switch, effect) in switches {
                    offers.push((switch, &server.name, effect));
                }
            }

            let arg = arg.trim_start();
            let Some((switch, query)) = arg.split_once(char::is_whitespace) else {
                for (switch, detail) in [("on", "turn on or reconnect"), ("off", "turn off")] {
                    let offered = offers.iter().any(|&(offered, _, _)| offered == switch);
                    if !offered || !switch.starts_with(arg) {
                        continue;
                    }

                    let completion = format!("{prefix}{switch} ");
                    let label = switch.to_owned();
                    let choice = Choice::input(label, detail, completion);
                    self.choices.push(choice);
                }
                return;
            };

            let query = query.trim().to_lowercase();
            for (offered, name, effect) in offers {
                let lower = name.to_lowercase();
                if offered != switch || !lower.contains(&query) {
                    continue;
                }

                let completion = format!("{prefix}{switch} {name}");
                let label = name.clone();
                let choice = Choice::input(label, effect, completion);
                offer(&mut self.choices, choice, lower == query);
            }
            return;
        }

        // Lists from the agent use only what was asked when the list opened, since asking again
        // here would freeze the UI, while models and efforts are read live to keep the current
        // one marked
        let live = matches!(list, List::Models | List::Efforts);
        let picks = match &self.opened {
            Some((_, picks)) if !live => picks.clone(),
            _ => self.picks(list).unwrap_or_default(),
        };
        let query = arg.trim().to_lowercase();
        for pick in picks {
            let label = pick.label.to_lowercase();
            let value = pick.value.to_lowercase();
            if !label.contains(&query) && !value.contains(&query) {
                continue;
            }

            let exact = label == query || value == query;
            let current = pick.current && query.is_empty() && live;
            let detail = match pick.current {
                true => "current",
                false => &pick.detail,
            };
            let choice = Choice {
                label: pick.label,
                detail: detail.to_owned(),
                completion: pick.completion,
            };
            offer(&mut self.choices, choice, exact || current);
        }
    }

    /// Reports a list that is empty or failed to arrive, and returns whether it has rows
    fn open_list(&mut self, list: List) -> bool {
        let asked = self.picks(list);
        match &asked {
            Ok(picks) if picks.is_empty() => self.info(empty_text(list)),
            Ok(_) => {}
            Err(error) => {
                let text = format!("{error:#}");
                self.failure(text);
            }
        }

        let picks = asked.unwrap_or_default();
        let filled = !picks.is_empty();
        self.opened = Some((list, picks));
        filled
    }

    fn picks(&self, list: List) -> Result<Vec<Pick>> {
        let prefix = prefix(list);
        let mut picks = Vec::new();
        match list {
            List::Sessions => {
                for info in self.session.sessions()? {
                    let id = info.session_id.0.to_string();
                    let title = info.title.unwrap_or_default();
                    let words: Vec<&str> = title.split_whitespace().collect();
                    let label = match words.is_empty() {
                        true => id.clone(),
                        false => words.join(" "),
                    };
                    let updated = info.updated_at.as_deref().map(stamp);
                    let updated = updated.unwrap_or_default();
                    let completion = Completion::Input(format!("{prefix}{id}"));
                    picks.push(Pick {
                        value: id,
                        label,
                        detail: updated,
                        current: false,
                        completion,
                    });
                }
            }

            List::Models => picks = self.offered(Selector::Model, &prefix),
            List::Efforts => picks = self.offered(Selector::Effort, &prefix),

            List::Servers => {
                for server in &self.session.mcp {
                    let state = server.state.to_string();
                    let completion = Completion::Input(format!("{prefix}{}", server.name));
                    picks.push(Pick {
                        value: server.name.clone(),
                        label: server.name.clone(),
                        detail: state,
                        current: false,
                        completion,
                    });
                }
            }

            List::Memories => {
                for memory in self.session.memories()? {
                    let words: Vec<&str> = memory.text.split_whitespace().collect();
                    let scope = match memory.project {
                        true => "project",
                        false => "global",
                    };
                    picks.push(Pick {
                        value: memory.id.to_string(),
                        label: words.join(" "),
                        detail: format!("{scope} #{}", memory.id),
                        current: false,
                        completion: Completion::Memory(memory.id),
                    });
                }
            }
        }
        Ok(picks)
    }

    /// The choices of a select option, each completing `prefix`
    fn offered(&self, selector: Selector, prefix: &str) -> Vec<Pick> {
        let mut picks = Vec::new();
        for offering in self.session.choices(selector) {
            let completion = Completion::Input(format!("{prefix}{}", offering.value));
            picks.push(Pick {
                value: offering.value,
                label: offering.name,
                detail: offering.description.unwrap_or_default(),
                current: offering.current,
                completion,
            });
        }
        picks
    }

    /// Sticks to the edge when a drag leaves the messages area
    fn point(&self, mouse: MouseEvent) -> Option<Point> {
        let area = self.messages_area;
        if area.is_empty() {
            return None;
        }

        let top = area.top();
        let left = area.left();
        let bottom = area.bottom() - 1;
        let right = area.right() - 1;
        let y = mouse.row.clamp(top, bottom);
        let x = mouse.column.clamp(left, right);
        let row = self.view_top() + usize::from(y - top);
        let col = usize::from(x - left);
        Some(Point { row, col })
    }

    fn fill(&mut self, completion: &Completion) {
        match completion {
            Completion::Input(text) => self.complete(text),
            Completion::Mention { typed, text } => {
                for _ in 0..*typed {
                    self.input.delete_char();
                }
                self.input.insert_str(text);
                self.highlight.select_first();
            }
            Completion::Memory(_) => {}
        }
    }

    fn complete(&mut self, text: &str) {
        self.set_input(text);
        self.highlight.select_first();
        self.choices_closed = false;
        self.refresh();
    }

    fn set_input(&mut self, text: &str) {
        let command = shell_command(text);
        self.shell = command.is_some();
        let text = command.unwrap_or(text);
        self.input = input_box(text);
        self.input_top = 0;
    }

    /// A partly edited placeholder can't be expanded back,
    /// so a deletion removes it whole and an insertion inside moves after it
    fn edit(&mut self, change: impl FnOnce(&mut TextArea<'static>) -> bool) -> bool {
        let before = self.input.lines().join("\n");
        let old_cursor = offset(&self.input);
        if !change(&mut self.input) {
            return false;
        }

        let after = self.input.lines().join("\n");
        let new_cursor = offset(&self.input);

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

        for (index, paste) in self.pastes.iter().enumerate() {
            let shown = placeholder(index + 1, paste);
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

        // Not `set_input`, which would change `shell`
        self.input = input_box(&text);
        self.input_top = 0;
        jump(&mut self.input, cursor);

        true
    }

    fn folded(&mut self, text: String) -> String {
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

    fn submit(&mut self) {
        let mut text = self.draft();
        // A later paste may contain the placeholder of an earlier one
        for (index, paste) in self.pastes.iter().enumerate().rev() {
            let shown = placeholder(index + 1, paste);
            text = text.replace(&shown, paste);
        }

        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }

        let command = Command::parse(&text);
        if let Some(Command::Invalid(message)) = &command {
            return self.failure(message);
        }

        self.history = History::default();
        self.set_input("");
        self.pastes.clear();
        self.paths.clear();

        if self.session.run.is_none() {
            return self.submit_text(&text);
        }
        self.queued.push_back(text);
        // A turn waiting on agents takes it now, unless a command ahead of it waits for the turn to end
        if self.queued.iter().all(|text| plain(text)) {
            self.session.notify_queued();
        }
    }

    /// Inverts `ui::thumb`, so the thumb is drawn on the cell it was dragged to
    fn drag_to(&mut self, row: u16) {
        let last = usize::from(self.scrollbar.height.saturating_sub(1));
        let offset = usize::from(row.saturating_sub(self.scrollbar.top()));
        let top = (offset.min(last) * self.bottom + last / 2).checked_div(last);
        let top = top.unwrap_or(self.bottom);
        self.scrolled_back = self.bottom - top.min(self.bottom);
    }

    fn scroll_up(&mut self, lines: usize) {
        self.scrolled_back = self.scrolled_back.saturating_add(lines);
    }

    fn scroll_down(&mut self, lines: usize) {
        self.scrolled_back = self.scrolled_back.saturating_sub(lines);
    }

    fn info(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.session.conversation.push(Entry::Info(text));
    }

    fn failure(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.session.conversation.push(Entry::Failure(text));
    }
}

#[derive(Default)]
struct History {
    cursor: Option<usize>,
    draft: String,
}

impl History {
    fn back(&mut self, sent: &[String], current: String) -> Option<String> {
        let index = match self.cursor {
            None => {
                let last = sent.len().checked_sub(1)?;
                self.draft = current;
                last
            }
            Some(index) => index.checked_sub(1)?,
        };

        self.cursor = Some(index);
        sent.get(index).cloned()
    }

    /// The second item is true when moving past the newest entry back to the draft
    fn forward(&mut self, sent: &[String]) -> Option<(String, bool)> {
        let next = self.cursor? + 1;
        if next < sent.len() {
            self.cursor = Some(next);
            return Some((sent[next].clone(), false));
        }

        self.cursor = None;
        let draft = mem::take(&mut self.draft);
        Some((draft, true))
    }
}

/// Windows sends a paste as one batch of key presses with newlines as Enter,
/// and with `stalled` the batch may be keys typed during a freeze
pub(crate) fn pasted(events: &[Event], stalled: bool) -> Option<String> {
    let mut text = String::new();
    for event in events {
        let key = match event {
            Event::Key(key) => key,
            // The mouse moved during the paste
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved => continue,
            _ => return None,
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }

        let modifiers = key.modifiers.difference(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char(c) if modifiers.is_empty() || modifiers == ALTGR => text.push(c),
            KeyCode::Enter if key.modifiers.is_empty() => text.push('\n'),
            KeyCode::Tab if key.modifiers.is_empty() => text.push('\t'),
            _ => return None,
        }
    }

    // A long line is a paste too, since typing it in key by key diffs the input each time
    let count = text.chars().count();
    let burst = count > 1;
    let long = count > MAX_UNFOLDED_CHARS;
    let typed = stalled && text.ends_with('\n') && text.matches('\n').count() == 1;
    match burst && (text.contains('\n') || long) && !typed {
        true => Some(text),
        false => None,
    }
}

fn placeholder(number: usize, text: &str) -> String {
    let lines = text.lines().count();
    if lines > 1 {
        return format!("[paste #{number}: {lines} lines]");
    }

    let chars = text.chars().count();
    format!("[paste #{number}: {chars} characters]")
}

/// Queued text the running turn can take, as commands wait for it to end
fn plain(text: &str) -> bool {
    Command::parse(text).is_none()
}

/// Strips leading whitespace first, as submitting does
fn shell_command(text: &str) -> Option<&str> {
    let command = text.trim_start().strip_prefix('!')?;
    Some(command.trim_start())
}

/// A newline counts as one character
fn offset(input: &TextArea) -> usize {
    let DataCursor(row, col) = input.cursor();
    let above = input.lines()[..row].iter();
    above.map(|line| line.chars().count() + 1).sum::<usize>() + col
}

fn jump(input: &mut TextArea, cursor: usize) {
    let text = input.lines().join("\n");
    let placed: String = text.chars().take(cursor).collect();
    let row = placed.matches('\n').count();
    let line = placed.rsplit('\n').next().unwrap_or_default();
    let col = line.chars().count();
    let row = u16::try_from(row).unwrap_or(u16::MAX);
    let col = u16::try_from(col).unwrap_or(u16::MAX);
    input.move_cursor(CursorMove::Jump(row, col));
}

/// Returns the character count including the @, and the path typed so far
fn typing_mention(before: &str) -> Option<(usize, &str)> {
    let mut ats = before.rmatch_indices('@').map(|(at, _)| at);
    let at = ats.find(|&at| opens_mention(&before[..at]))?;

    let rest = &before[at + 1..];
    let path = match rest.strip_prefix('"') {
        Some(quoted) if quoted.contains('"') => return None,
        Some(quoted) => quoted,
        None if rest.contains(char::is_whitespace) => return None,
        None => rest,
    };

    let count = before[at..].chars().count();
    Some((count, path))
}

/// At most `limit` paths in `cwd` that its ignore files leave, a level at a time,
/// so every level above the last one listed is complete
fn walk(cwd: &Path, limit: usize) -> Vec<String> {
    let mut builder = WalkBuilder::new(cwd);
    builder.hidden(false).require_git(false);
    let mut matchers = builder.build_matchers();
    let Some(matcher) = matchers.first_mut() else {
        return Vec::new();
    };

    let mut paths = Vec::new();
    let mut folders = VecDeque::from([PathBuf::new()]);
    while let Some(folder) = folders.pop_front() {
        let Ok(entries) = fs::read_dir(cwd.join(&folder)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let relative = folder.join(&name);
            // `file_type` doesn't follow links, which keeps a link to a folder above from looping
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if name == ".git" || matcher.matched(&relative, is_dir).is_ignore() {
                continue;
            }

            // Completions always use /, which Windows accepts too
            let mut path = relative.to_string_lossy().replace(MAIN_SEPARATOR, "/");
            if is_dir {
                path.push('/');
                folders.push_back(relative);
            }
            paths.push(path);
            if paths.len() == limit {
                return paths;
            }
        }
    }
    paths
}

/// A path outside the working directory, cut after its last separator
fn outside_dir(query: &str) -> Option<&str> {
    let unified = query.replace(MAIN_SEPARATOR, "/");
    let outside = ["~/", "/", "../"];
    let mut chars = unified.chars();
    let drive = cfg!(windows)
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.next() == Some(':')
        && chars.next() == Some('/');
    if !drive && !outside.iter().any(|start| unified.starts_with(start)) {
        return None;
    }

    let end = query.rfind(is_separator)? + 1;
    Some(&query[..end])
}

/// Typed and listed paths may both use \ on Windows, so they are compared with /
fn matching_paths<'a>(paths: impl IntoIterator<Item = &'a String>, query: &str) -> Vec<&'a String> {
    let exact = query.replace(MAIN_SEPARATOR, "/");
    let query = exact.to_lowercase();
    let mut found = Vec::new();
    for path in paths {
        let unified = path.replace(MAIN_SEPARATOR, "/");
        // A fully typed path gets no suggestions, or Enter would swap it for a longer one
        if unified.trim_end_matches('/') == exact {
            return Vec::new();
        }

        let lower = unified.to_lowercase();
        if !lower.contains(&query) || lower == query {
            continue;
        }

        let trimmed = lower.trim_end_matches('/');
        let name = trimmed.rsplit('/').next().unwrap_or_default();
        let depth = trimmed.matches('/').count();
        let rank = if lower.starts_with(&query) {
            0
        } else if name.starts_with(&query) {
            1
        } else if name.contains(&query) {
            2
        } else {
            3
        };
        found.push(((rank, depth, lower), path));
    }

    found.sort();
    found.truncate(MAX_MATCHES);
    found.into_iter().map(|(_, path)| path).collect()
}

fn offer(choices: &mut Vec<Choice>, choice: Choice, exact: bool) {
    match exact {
        true => choices.insert(0, choice),
        false => choices.push(choice),
    }
}

fn hints(input: &str) -> Vec<&'static Spec> {
    if !input.starts_with(':') || input.contains(char::is_whitespace) {
        return Vec::new();
    }

    let mut found = Vec::new();
    for spec in &COMMANDS {
        if spec.name.starts_with(input) {
            found.push(spec);
        }
    }
    found
}

/// The command that opens `list`, with the space before its argument
fn prefix(list: List) -> String {
    let opens = |spec: &&Spec| spec.bare == Command::List(list);
    let spec = COMMANDS.iter().find(opens).expect("command");
    spec.prefix()
}

fn empty_text(list: List) -> &'static str {
    match list {
        List::Sessions => "no other sessions",
        List::Models => "no models",
        List::Efforts => "no effort levels",
        List::Servers => "no MCP servers",
        List::Memories => "no memories",
    }
}

fn listed(list: List, picks: Vec<Pick>) -> String {
    if picks.is_empty() {
        return empty_text(list).to_owned();
    }

    let mut lines = Vec::new();
    for pick in picks {
        let mut line = pick.value;
        if pick.label != line {
            line += &format!("  {}", pick.label);
        }
        if !pick.detail.is_empty() {
            line += &format!("  {}", pick.detail);
        }
        if pick.current {
            line += "  (current)";
        }
        lines.push(line);
    }
    lines.join("\n")
}

fn stamp(rfc3339: &str) -> String {
    let Ok(time) = DateTime::parse_from_rfc3339(rfc3339) else {
        let spaced = rfc3339.replacen('T', " ", 1);
        return spaced.chars().take(16).collect();
    };
    let local = time.with_timezone(&Local);
    local.format("%Y-%m-%d %H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use crate::client::McpStatus;
    use crate::tui::app::*;
    use crate::tui::tests::{app, draw, fill};
    use crate::tui::{theme, ui, ACCENT};
    use ratatui::crossterm::event::KeyEvent;
    use std::env;
    use std::process;
    use std::thread;

    fn settle(app: &mut App, done: impl Fn(&App) -> bool) {
        for _ in 0..500 {
            app.pump();
            if done(app) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("not settled");
    }

    fn shown(app: &mut App) -> Vec<String> {
        let rows = app.transcript.rows(&app.session.conversation, 60, false);
        rows.iter().map(|row| row.line.to_string()).collect()
    }

    fn key(app: &mut App, code: KeyCode) {
        let key = KeyEvent::from(code);
        app.on_event(Event::Key(key));
    }

    fn mouse(app: &mut App, kind: MouseEventKind, column: u16, row: u16) {
        let mouse = MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.on_event(Event::Mouse(mouse));
    }

    fn labels(app: &mut App, input: &str) -> Vec<String> {
        app.set_input(input);
        app.refresh();
        let labels = app.choices.iter().map(|choice| choice.label.clone());
        labels.collect()
    }

    #[test]
    fn queue_runs_shell() {
        let mut app = app();
        app.submit_text("first task");
        app.queued
            .extend(["! ls docs", "second task"].map(str::to_owned));
        settle(&mut app, |app| {
            app.queued.is_empty() && app.session.run.is_none()
        });

        let expected = [
            "",
            "❯ first task",
            "",
            "! ls docs",
            "  ├ alpha",
            "  │ beta",
            "  ✗ exit 2",
            "",
            "❯ second task",
        ];
        assert_eq!(shown(&mut app), expected);
        assert_eq!(
            app.session.conversation.sent(),
            ["first task", "! ls docs", "second task"]
        );
    }

    #[test]
    fn queue_joins_turn() {
        let mut app = app();
        app.submit_text("queue");
        app.queued
            .extend(["update", ":mcp", "later"].map(str::to_owned));
        settle(&mut app, |app| {
            app.queued.is_empty() && app.session.run.is_none()
        });

        let expected = [
            "",
            "❯ queue",
            "",
            "❯ update",
            "",
            "  no MCP servers",
            "",
            "❯ later",
        ];
        assert_eq!(shown(&mut app), expected);
        assert_eq!(app.session.conversation.stats().turns, 2);
    }

    #[test]
    fn queued_keeps_draft() {
        let mut app = app();
        let server = McpStatus {
            name: "web".to_owned(),
            state: McpState::Off,
        };
        app.session.mcp.push(server);
        app.submit_text("first task");
        app.queued.push_back(":mcp".to_owned());
        app.set_input("draft");
        settle(&mut app, |app| app.queued.is_empty());

        assert_eq!(app.input.lines(), ["draft"]);
        let expected = ["", "❯ first task", "", "  web  off"];
        assert_eq!(shown(&mut app), expected);
    }

    #[test]
    fn exact_hint_first() {
        let mut app = app();
        for name in ["search engine", "search"] {
            let server = McpStatus {
                name: name.to_owned(),
                state: McpState::Off,
            };
            app.session.mcp.push(server);
        }
        assert_eq!(
            labels(&mut app, ":mcp on\u{3000}search"),
            ["search", "search engine"]
        );
    }

    #[test]
    fn resume_failure_kept() {
        let mut app = app();
        app.submit_text("first task");
        settle(&mut app, |app| app.session.run.is_none());

        app.submit_text(":resume nope");
        let expected = ["", "❯ first task", "", "✗ unknown session"];
        assert_eq!(shown(&mut app), expected);

        app.submit_text(":resume s1");
        let expected = ["", "❯ old task", "", "  resumed session s1"];
        assert_eq!(shown(&mut app), expected);
    }

    #[test]
    fn lists_formatted() {
        let mut app = app();
        let list = |app: &App, list: List| {
            let picks = app.picks(list).expect("list");
            listed(list, picks)
        };

        assert_eq!(list(&app, List::Models), "m1  (current)\nm2-café");
        assert_eq!(list(&app, List::Efforts), "low\nhigh  (current)");
        assert_eq!(list(&app, List::Sessions), "no other sessions");
        assert_eq!(list(&app, List::Servers), "no MCP servers");

        app.session.open(None).expect("open");
        assert_eq!(list(&app, List::Sessions), "s1  old session");
    }

    #[test]
    fn memories_picked() {
        let mut app = app();
        app.submit_text(":memory");
        assert_eq!(app.input.lines(), [":memory "]);
        let picked: Vec<(&str, &str)> = app
            .choices
            .iter()
            .map(|choice| (choice.label.as_str(), choice.detail.as_str()))
            .collect();
        let expected = [
            ("Tests run with pytest", "project #12"),
            ("Prefers résumé over CV and naïve tests", "global #99"),
        ];
        assert_eq!(picked, expected);
        let status = |app: &mut App| {
            let terminal = draw(app, 80, 12);
            let buffer = terminal.backend().buffer();
            (0..80)
                .map(|x| buffer[(x, 11)].symbol())
                .collect::<String>()
        };
        let listing = status(&mut app);
        assert!(
            listing.contains("↑↓ select · del forget · esc close"),
            "{listing}"
        );

        // A held key repeats Delete, which only marks the memory again
        key(&mut app, KeyCode::Delete);
        key(&mut app, KeyCode::Delete);
        assert!(app.marked());
        let marked = status(&mut app);
        assert!(marked.contains("enter forget · esc keep"), "{marked}");
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Enter);

        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Delete);
        key(&mut app, KeyCode::Esc);
        assert!(!app.marked() && !app.choices.is_empty());
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Delete);
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Delete);
        key(&mut app, KeyCode::Enter);
        let expected = [
            "",
            "  forgot memory 12; the system prompt drops it next session",
            "",
            "✗ memory: 99: not found",
        ];
        assert_eq!(shown(&mut app), expected);
        assert_eq!(app.choices.len(), 2);
        let found = labels(&mut app, ":memory naïve");
        assert_eq!(found, ["Prefers résumé over CV and naïve tests"]);

        key(&mut app, KeyCode::Left);
        key(&mut app, KeyCode::Delete);
        assert_eq!(app.input.lines(), [":memory naïv"]);
        assert!(!app.marked());
        assert_eq!(shown(&mut app), expected);
    }

    #[test]
    fn short_stamp() {
        assert_eq!(stamp("2026-10-04T08:09:10"), "2026-10-04 08:09");
    }

    #[test]
    fn current_model_first() {
        let mut app = app();
        app.session
            .choose(Selector::Model, "m2-café")
            .expect("switch");
        assert_eq!(labels(&mut app, ":model "), ["m2-café", "m1"]);
        assert_eq!(labels(&mut app, ":model m"), ["m1", "m2-café"]);
    }

    #[test]
    fn effort_switched() {
        let mut app = app();
        assert_eq!(labels(&mut app, ":effort "), ["high", "low"]);

        app.submit_text(":effort low");
        assert_eq!(shown(&mut app), ["", "  effort: low"]);
        assert_eq!(labels(&mut app, ":effort "), ["low", "high"]);
    }

    #[test]
    fn refused_not_counted() {
        let mut app = app();
        app.submit_text("huge");
        settle(&mut app, |app| app.session.run.is_none());

        assert_eq!(app.session.conversation.stats().turns, 0);
        assert_eq!(shown(&mut app), ["", "❯ huge", "", "✗ task: too large"]);
    }

    #[test]
    fn ctrl_d_quits() {
        let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        let ctrl_d = Event::Key(ctrl_d);
        let mut idle = app();
        idle.on_event(ctrl_d.clone());
        assert!(idle.quit);

        let mut app = app();
        app.set_input("éclair");
        key(&mut app, KeyCode::Home);
        app.on_event(ctrl_d.clone());
        assert_eq!(app.input.lines(), ["clair"]);

        app.set_input("");
        app.submit_text("wait");
        app.on_event(ctrl_d);
        assert!(!app.quit);
    }

    #[test]
    fn hints_run() {
        let mut app = app();
        for typed in ":mo".chars() {
            key(&mut app, KeyCode::Char(typed));
        }
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.input.lines(), [":model "]);

        app.set_input("");
        for typed in ":qu".chars() {
            key(&mut app, KeyCode::Char(typed));
        }
        key(&mut app, KeyCode::Enter);
        assert!(app.quit);
    }

    #[test]
    fn tiny_area_clicks() {
        let mut app = app();
        draw(&mut app, 12, 3);

        app.messages_area = Rect::new(2, 1, 0, 0);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            mouse(&mut app, kind, 3, 1);
        }
        assert_eq!((app.pressed, app.selection), (None, None));
    }

    /// A drag of the scrollbar let go over the completions still ends
    #[test]
    fn drag_released_anywhere() {
        let mut app = app();
        fill(&mut app, 15);
        draw(&mut app, 60, 20);
        let bar = app.scrollbar;
        let left = MouseButton::Left;
        mouse(&mut app, MouseEventKind::Down(left), bar.left(), bar.top());
        assert!(app.dragging);

        app.choices_area = Rect::new(0, 0, 10, 5);
        mouse(&mut app, MouseEventKind::Drag(left), 2, 2);
        mouse(&mut app, MouseEventKind::Up(left), 2, 2);
        assert!(!app.dragging);
    }

    #[test]
    fn scrollbar_drags() {
        let mut app = app();
        // Fewer rows than twice the track keep the rounding in `drag_to` visible
        fill(&mut app, 15);
        draw(&mut app, 60, 20);
        let bar = app.scrollbar;
        let bottom = app.bottom;
        let column = bar.left();
        let on_bar = |app: &mut App, kind, row| mouse(app, kind, column, row);

        let left = MouseButton::Left;
        on_bar(&mut app, MouseEventKind::Down(left), bar.bottom() - 1);
        on_bar(&mut app, MouseEventKind::Drag(left), bar.top());
        assert_eq!((app.view_top(), app.scrolled_back), (0, bottom));
        on_bar(&mut app, MouseEventKind::Up(left), bar.top());
        assert!(!app.dragging);

        for pressed in bar.top()..bar.bottom() {
            on_bar(&mut app, MouseEventKind::Down(left), pressed);
            let row = ui::thumb(app.view_top(), bottom, bar.height);
            assert_eq!(bar.top() + row, pressed);
        }

        on_bar(&mut app, MouseEventKind::Down(left), bar.bottom() - 1);
        assert_eq!((app.view_top(), app.scrolled_back), (bottom, 0));
    }

    #[test]
    fn shell_input_marked() {
        let mut app = app();
        let input_row = |app: &mut App| {
            let terminal = draw(app, 80, 12);
            let buffer = terminal.backend().buffer();
            let row: String = (0..80).map(|x| buffer[(x, 9)].symbol()).collect();
            (row, buffer[(0, 8)].style().fg)
        };

        for typed in "  !".chars() {
            key(&mut app, KeyCode::Char(typed));
        }
        let (row, corner) = input_row(&mut app);
        assert!(row.contains("backspace goes back"), "{row}");
        assert_eq!(corner, Some(theme::PINK));

        key(&mut app, KeyCode::Char(':'));
        assert!(app.choices.is_empty());
        for typed in "ls docs".chars() {
            key(&mut app, KeyCode::Char(typed));
        }
        assert_eq!(app.input.lines(), [":ls docs"]);

        key(&mut app, KeyCode::Home);
        key(&mut app, KeyCode::Backspace);
        assert_eq!(app.input.lines(), [":ls docs"]);
        let (_, corner) = input_row(&mut app);
        assert_eq!(corner, ACCENT.fg);

        key(&mut app, KeyCode::Char('!'));
        key(&mut app, KeyCode::Char('!'));
        key(&mut app, KeyCode::Home);
        key(&mut app, KeyCode::Backspace);
        assert_eq!(app.draft(), "!:ls docs");

        app.set_input("résumé");
        key(&mut app, KeyCode::Home);
        app.on_event(Event::Paste("! café\n!naïve ".to_owned()));
        assert_eq!(app.input.lines(), ["café", "!naïve résumé"]);
        assert_eq!(app.input.cursor(), DataCursor(1, 7));
    }

    #[test]
    fn shell_draft_kept() {
        let mut app = app();
        app.submit_text("wait");
        key(&mut app, KeyCode::Char('!'));
        for typed in "ls docs".chars() {
            key(&mut app, KeyCode::Char(typed));
        }
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.queued, ["!ls docs"]);
        assert!(!app.shell);

        key(&mut app, KeyCode::Up);
        assert!(app.shell);
        assert_eq!(app.input.lines(), ["ls docs"]);
    }

    #[test]
    fn esc_follows_first() {
        let mut app = app();
        app.submit_text("wait");
        app.scrolled_back = 5;

        key(&mut app, KeyCode::Esc);
        assert_eq!(app.scrolled_back, 0);
        assert!(app.session.running());

        key(&mut app, KeyCode::Esc);
        let run = app.session.run.as_ref();
        assert!(run.is_some_and(|run| run.interrupting));
    }

    #[test]
    fn up_withdraws_queued() {
        let mut app = app();
        app.submit_text("wait");
        for text in ["one", "two"] {
            app.set_input(text);
            key(&mut app, KeyCode::Enter);
        }
        assert_eq!(app.queued, ["one", "two"]);

        app.set_input("draft");
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Up);
        assert_eq!(app.input.lines(), ["one", "two", "draft"]);
        assert!(app.queued.is_empty() && app.session.running());
    }

    #[test]
    fn long_paste_folded() {
        let mut app = app();
        app.set_input("Résumé: ");
        let lines: Vec<String> = (1..=7).map(|n| format!("line {n}")).collect();
        let pasted = lines.join("\n");
        app.on_event(Event::Paste(pasted.clone()));
        assert_eq!(app.input.lines(), ["Résumé: [paste #1: 7 lines]"]);

        key(&mut app, KeyCode::Backspace);
        assert_eq!(app.input.lines(), ["Résumé: "]);

        app.on_event(Event::Paste(pasted.clone()));
        app.on_event(Event::Paste("\nshort".to_owned()));
        assert_eq!(app.input.lines(), ["Résumé: [paste #2: 7 lines]", "short"]);
        key(&mut app, KeyCode::Enter);
        let sent = app.session.conversation.sent();
        let expected = format!("Résumé: {pasted}\nshort");
        assert_eq!(sent.last(), Some(&expected));

        key(&mut app, KeyCode::Up);
        assert_eq!(app.input.lines(), ["[paste #1: 8 lines]"]);

        // One long line folds too, counted in characters rather than bytes
        settle(&mut app, |app| app.session.run.is_none());
        app.set_input("");
        let line = "café 🐍 ".repeat(200);
        app.on_event(Event::Paste(line.clone()));
        assert_eq!(app.input.lines(), ["[paste #2: 1400 characters]"]);
        key(&mut app, KeyCode::Enter);
        let sent = app.session.conversation.sent();
        assert_eq!(sent.last().map(String::as_str), Some(line.trim()));
    }

    #[test]
    fn long_shell_folded() {
        let mut app = app();
        app.submit_text("wait");
        let lines: Vec<String> = (1..=7).map(|n| format!("echo café {n}")).collect();
        let command = lines.join("\n");
        app.on_event(Event::Paste(format!("  ! {command}")));
        assert!(app.shell);
        assert_eq!(app.input.lines(), ["[paste #1: 7 lines]"]);

        key(&mut app, KeyCode::Enter);
        assert_eq!(app.queued, [format!("!{command}")]);
        key(&mut app, KeyCode::Up);
        assert!(app.shell);
        assert_eq!(app.input.lines(), ["[paste #1: 7 lines]"]);

        key(&mut app, KeyCode::Home);
        key(&mut app, KeyCode::Backspace);
        assert!(!app.shell);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.queued, [command]);
    }

    #[test]
    fn placeholder_atomic() {
        let mut app = app();
        let lines: Vec<String> = (1..=7).map(|n| format!("line {n}")).collect();
        let pasted = lines.join("\n");
        app.set_input("Résumé: ");
        app.on_event(Event::Paste(pasted.clone()));
        let ctrl_w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        app.on_event(Event::Key(ctrl_w));
        assert_eq!(app.input.lines(), ["Résumé: "]);

        app.on_event(Event::Paste(pasted));
        for _ in 0..3 {
            key(&mut app, KeyCode::Left);
        }
        key(&mut app, KeyCode::Char('🚀'));
        assert_eq!(app.input.lines(), ["Résumé: [paste #2: 7 lines]🚀"]);

        key(&mut app, KeyCode::Home);
        for _ in 0..8 {
            key(&mut app, KeyCode::Right);
        }
        key(&mut app, KeyCode::Delete);
        assert_eq!(app.input.lines(), ["Résumé: 🚀"]);
    }

    #[test]
    fn brackets_beside_placeholder() {
        let mut app = app();
        let lines: Vec<String> = (1..=7).map(|n| format!("line {n}")).collect();
        app.set_input("[");
        app.on_event(Event::Paste(lines.join("\n")));

        key(&mut app, KeyCode::Home);
        key(&mut app, KeyCode::Right);
        key(&mut app, KeyCode::Char('['));
        assert_eq!(app.input.lines(), ["[[[paste #1: 7 lines]"]);
        key(&mut app, KeyCode::Backspace);
        key(&mut app, KeyCode::Backspace);
        assert_eq!(app.input.lines(), ["[paste #1: 7 lines]"]);

        key(&mut app, KeyCode::End);
        key(&mut app, KeyCode::Char(']'));
        key(&mut app, KeyCode::Left);
        key(&mut app, KeyCode::Backspace);
        assert_eq!(app.input.lines(), ["]"]);
    }

    #[test]
    fn interrupt_sends_queued() {
        let mut app = app();
        app.submit_text("wait");
        app.set_input("try another way");
        key(&mut app, KeyCode::Enter);

        key(&mut app, KeyCode::Esc);
        settle(&mut app, |app| {
            app.queued.is_empty() && app.session.run.is_none()
        });
        assert_eq!(app.session.conversation.sent(), ["wait", "try another way"]);
    }

    #[test]
    fn quit_stops_queue() {
        let mut app = app();
        app.submit_text("first task");
        app.queued.extend([":quit", "later"].map(str::to_owned));
        settle(&mut app, |app| app.quit);

        assert!(app.session.run.is_none());
        assert_eq!(app.queued, ["later"]);
    }

    #[test]
    fn invalid_command_kept() {
        let mut app = app();
        app.set_input(":undo");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.input.lines(), [":undo"]);
        assert!(app.session.run.is_none());
        let rows = shown(&mut app);
        assert!(rows[1].starts_with("✗ command: :undo: unknown"), "{rows:?}");

        app.set_input(": what does this mean");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.input.lines(), [""]);
        let rows = shown(&mut app);
        assert!(
            rows.contains(&"❯ : what does this mean".to_owned()),
            "{rows:?}"
        );
    }

    #[test]
    fn path_completions() {
        let text = |path| {
            let Completion::Mention { text, .. } = Choice::path(path, 1).completion else {
                panic!("{path}");
            };
            text
        };
        assert_eq!(text("src/main.rs"), "@src/main.rs ");
        assert_eq!(text("src/"), "@src/");
        assert_eq!(text("my notes.txt"), "@\"my notes.txt\" ");
        assert_eq!(text("my docs/"), "@\"my docs/");
    }

    #[test]
    fn history_keeps_draft() {
        let mut history = History::default();
        assert_eq!(history.back(&[], "draft".to_owned()), None);

        let sent = ["first".to_owned(), "second".to_owned()];
        let back = history.back(&sent, "draft".to_owned());
        assert_eq!(back.as_deref(), Some("second"));
        assert_eq!(history.back(&sent, String::new()).as_deref(), Some("first"));
        assert_eq!(history.back(&sent, String::new()), None);
        let second = ("second".to_owned(), false);
        assert_eq!(history.forward(&sent), Some(second));
        let draft = ("draft".to_owned(), true);
        assert_eq!(history.forward(&sent), Some(draft));
        assert_eq!(history.forward(&sent), None);
    }

    #[test]
    fn key_burst_pasted() {
        let pressed = |code| Event::Key(KeyEvent::from(code));
        let first = pressed(KeyCode::Char('é'));
        let enter = pressed(KeyCode::Enter);
        let second = pressed(KeyCode::Char('ñ'));
        let shifted = KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT);
        let moved = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 3,
            row: 4,
            modifiers: KeyModifiers::NONE,
        });
        let burst = [
            first.clone(),
            enter.clone(),
            moved,
            Event::Key(shifted),
            second.clone(),
        ];
        assert_eq!(pasted(&burst, false).as_deref(), Some("é\nAñ"));
        assert_eq!(pasted(&burst, true).as_deref(), Some("é\nAñ"));

        // A line typed during a freeze, then Enter
        let line = [first.clone(), second.clone(), enter.clone()];
        assert_eq!(pasted(&line, true), None);
        assert_eq!(pasted(&line, false).as_deref(), Some("éñ\n"));

        // One long line too, which then folds
        let long = vec![first.clone(); MAX_UNFOLDED_CHARS + 1];
        let text = "é".repeat(MAX_UNFOLDED_CHARS + 1);
        assert_eq!(pasted(&long, true), Some(text));
        assert_eq!(pasted(&long[1..], false), None);

        let up = pressed(KeyCode::Up);
        let typed = [
            vec![enter.clone()],
            vec![first.clone(), second],
            vec![first, up, enter],
        ];
        for events in typed {
            assert_eq!(pasted(&events, false), None, "{events:?}");
        }
    }

    #[test]
    fn typing_mentions() {
        assert_eq!(typing_mention("café @src/ma"), Some((7, "src/ma")));
        assert_eq!(typing_mention("👉@"), Some((1, "")));
        assert_eq!(typing_mention("@a.md and @\"my fi"), Some((7, "my fi")));
        assert_eq!(typing_mention("see @café/@ty"), Some((9, "café/@ty")));

        let none = [
            "me@x",
            "name_@x",
            "@a.md again",
            "@\"a b\" again",
            "no mention",
        ];
        for before in none {
            assert_eq!(typing_mention(before), None, "{before}");
        }
    }

    #[test]
    fn outside_scoped() {
        assert_eq!(outside_dir("~/Résumés/dra"), Some("~/Résumés/"));
        assert_eq!(outside_dir("~/"), Some("~/"));
        assert_eq!(outside_dir("/"), Some("/"));
        assert_eq!(outside_dir("/etc/ho"), Some("/etc/"));
        assert_eq!(outside_dir("../.."), Some("../"));
        for inside in ["", "~", "~josé/a", "src/ma", "./src/", "..", "café/menu"] {
            assert_eq!(outside_dir(inside), None, "{inside}");
        }

        #[cfg(windows)]
        {
            assert_eq!(
                outside_dir("C:\\Users\\José\\do"),
                Some("C:\\Users\\José\\")
            );
            assert_eq!(outside_dir("d:/données/"), Some("d:/données/"));
            assert_eq!(outside_dir("~\\Résumés\\dra"), Some("~\\Résumés\\"));
            assert_eq!(outside_dir("C:"), None);
        }
    }

    #[test]
    fn outside_paths_listed() {
        let name = format!("smol-outside-{}", process::id());
        let dir = env::temp_dir().join(name);
        fs::create_dir_all(dir.join("piñatas")).expect("mkdir");
        for file in [
            "résumé.txt",
            "café menu.md",
            ".hidden",
            "piñatas/inside.txt",
        ] {
            fs::write(dir.join(file), "").expect("write");
        }
        let shown = dir.display().to_string();
        // The Windows temp directory is under the user profile, whose name may contain spaces
        let quote = match shown.contains(' ') {
            true => "\"",
            false => "",
        };
        let mut app = app();

        let input = format!("see @{quote}{shown}/");
        let listed = labels(&mut app, &input);
        let names = ["café menu.md", "piñatas/", "résumé.txt"];
        let expected = names.map(|name| format!("{shown}/{name}"));
        assert_eq!(listed, expected);

        let input = format!("see @{quote}{shown}/.");
        let hidden = labels(&mut app, &input);
        assert_eq!(hidden, [format!("{shown}/.hidden")]);

        let input = format!("see @\"{shown}/piñatas/");
        let inner = labels(&mut app, &input);
        assert_eq!(inner, [format!("{shown}/piñatas/inside.txt")]);

        let input = format!("see @{quote}{shown}/missing/");
        let missing = labels(&mut app, &input);
        assert!(missing.is_empty(), "{missing:?}");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn walk_breadth_first() {
        let name = format!("smol-walk-{}", process::id());
        let dir = env::temp_dir().join(name);
        for folder in ["piñatas", "ignored", ".git"] {
            fs::create_dir_all(dir.join(folder)).expect("mkdir");
        }
        for n in 1..=10 {
            let file = format!("piñatas/{n}.txt");
            fs::write(dir.join(file), "").expect("write");
        }
        for file in ["résumé.md", ".env", "ignored/café.md", ".git/HEAD"] {
            fs::write(dir.join(file), "").expect("write");
        }
        fs::write(dir.join(".gitignore"), "ignored/\n").expect("write");
        fs::write(dir.join("piñatas/.gitignore"), "3.txt\n").expect("write");

        let mut walked = walk(&dir, 4);
        walked.sort();
        assert_eq!(walked, [".env", ".gitignore", "piñatas/", "résumé.md"]);
        let all = walk(&dir, 100);
        assert_eq!(all.len(), 14, "{all:?}");
        assert!(all.contains(&"piñatas/10.txt".to_owned()), "{all:?}");
        assert!(!all.contains(&"piñatas/3.txt".to_owned()), "{all:?}");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn paths_ranked() {
        let paths = [
            "README.md",
            "docs/",
            "docs/main.md",
            "domain.rs",
            "src/",
            "src/main.rs",
            "crates/x/src/main.rs",
            "notes/café.md",
        ];
        let paths = paths.map(str::to_owned);
        let ranked = |query| {
            let found = matching_paths(&paths, query);
            found.into_iter().map(String::as_str).collect::<Vec<_>>()
        };

        let top = ["docs/", "domain.rs", "README.md", "src/"];
        assert_eq!(ranked("")[..4], top);
        assert_eq!(ranked("src/"), ["src/main.rs", "crates/x/src/main.rs"]);
        let main = [
            "docs/main.md",
            "src/main.rs",
            "crates/x/src/main.rs",
            "domain.rs",
        ];
        assert_eq!(ranked("MAIN"), main);

        for typed in ["src/main.rs", "src", "notes/café.md"] {
            assert!(ranked(typed).is_empty(), "{typed}");
        }
    }

    #[test]
    fn commands_parse() {
        let shell = Command::parse("! ls docs\n");
        assert_eq!(shell, Some(Command::Shell("ls docs".to_owned())));
        let usage = Some(Command::Invalid("usage: !<command>".to_owned()));
        assert_eq!(Command::parse("!  "), usage);

        let resume = Command::parse(":resume  abc-123 ");
        assert_eq!(resume, Some(Command::Resume("abc-123".to_owned())));
        let model = Command::parse(":model tiny");
        assert_eq!(model, Some(Command::Model("tiny".to_owned())));
        let effort = Command::parse(":effort  high ");
        assert_eq!(effort, Some(Command::Effort("high".to_owned())));

        let on = Command::parse(":mcp on fake");
        assert_eq!(on, Some(Command::Mcp(true, "fake".to_owned())));
        let off = Command::parse(":mcp off  fake ");
        assert_eq!(off, Some(Command::Mcp(false, "fake".to_owned())));
        let wide = Command::parse(":mcp on\u{3000}web");
        assert_eq!(wide, Some(Command::Mcp(true, "web".to_owned())));

        let messages = ["hello", " :new", ":", ":)", ":) thanks", "::1", ":Résumé"];
        for line in messages {
            assert_eq!(Command::parse(line), None, "{line}");
        }

        let usage = |text: &str| Some(Command::Invalid(text.to_owned()));
        assert_eq!(Command::parse(":new please"), usage("usage: :new"));
        let mcp = usage("usage: :mcp <on|off> <server>");
        for line in [":mcp toggle fake", ":mcp on"] {
            assert_eq!(Command::parse(line), mcp, "{line}");
        }

        let commands = ":new, :resume <session>, :model <name>, :effort <level>, \
                        :mcp <on|off> <server>, :memory, :quit, !<command>";
        let unknown = format!("command: :help: unknown (commands: {commands})");
        assert_eq!(Command::parse(":help"), usage(&unknown));
    }

    #[test]
    fn hints_match_prefix() {
        let names = |input| {
            let found = hints(input);
            found.iter().map(|spec| spec.name).collect::<Vec<_>>()
        };

        let all = [
            ":new", ":resume", ":model", ":effort", ":mcp", ":memory", ":quit",
        ];
        assert_eq!(names(":"), all);
        assert_eq!(names(":res"), [":resume"]);
        assert_eq!(names(":m"), [":model", ":mcp", ":memory"]);
        assert!(names(":resume abc").is_empty() && names("new").is_empty());

        for spec in &COMMANDS {
            let parsed = Command::parse(spec.name);
            assert_eq!(parsed.as_ref(), Some(&spec.bare));
        }

        let lists = [
            List::Sessions,
            List::Models,
            List::Efforts,
            List::Servers,
            List::Memories,
        ];
        let prefixes = lists.map(prefix);
        let expected = [":resume ", ":model ", ":effort ", ":mcp ", ":memory "];
        assert_eq!(prefixes, expected);
    }

    /// A command whose list is empty says so once and leaves the input empty
    #[test]
    fn empty_list_closed() {
        let mut app = app();
        app.submit_text(":resume");
        assert_eq!(app.input.lines(), [""]);
        assert_eq!(shown(&mut app), ["", "  no other sessions"]);

        app.session.options.clear();
        for typed in ":model ".chars() {
            key(&mut app, KeyCode::Char(typed));
        }
        key(&mut app, KeyCode::Char('m'));
        assert_eq!(app.input.lines(), [":model m"]);
        let expected = ["", "  no other sessions", "", "  no models"];
        assert_eq!(shown(&mut app), expected);
    }
}
