use crate::chat::{image_placeholder, mention, resolved, typing_mention, Output, Session};
use crate::client::{mcp_line, mcp_state, Selector};
use crate::conversation::{Entry, Outcome, Said, ShellEnd};
use crate::tui::desktop::Desktop;
use crate::tui::draft::Draft;
use crate::tui::picture::{self, Source};
use crate::tui::render::Target;
use crate::tui::select::{Point, Selection};
use crate::tui::transcript::{Transcript, Viewport};
use crate::tui::{clipped, markdown};
use anyhow::Result;
use chrono::{DateTime, Local};
use ignore::WalkBuilder;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;
use ratatui_textarea::{CursorMove, DataCursor, TextArea};
use reaper::wire::McpState;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::mem;
use std::path::{is_separator, Path, PathBuf, MAIN_SEPARATOR};
use std::time::{Duration, Instant};

const FLASH_WAIT: Duration = Duration::from_secs(2);
const MAX_ALERT_COLUMNS: usize = 160;
const DOUBLE_CLICK_WAIT: Duration = Duration::from_millis(500);
const ANIMATE_EVERY: Duration = Duration::from_millis(100);
/// How often the sidebar's timers of the programs the code left running advance
const TICK_EVERY: Duration = Duration::from_secs(1);

const SHELL_USAGE: &str = "!<command>";

/// On Windows, characters typed with AltGr carry Ctrl and Alt
pub(crate) const ALTGR: KeyModifiers = KeyModifiers::CONTROL.union(KeyModifiers::ALT);

/// Keeps typing @ responsive even in the home directory
const MAX_PATHS: usize = 20_000;
/// The page's app.js scrolls this many rows too
const SCROLL_ROWS: usize = 3;
const MAX_MATCHES: usize = 200;

pub(crate) struct App {
    pub(crate) session: Session,
    desktop: Box<dyn Desktop>,
    pub(crate) transcript: Transcript,
    pub(crate) queued: VecDeque<String>,
    pub(crate) draft: Draft,
    history: History,
    pub(crate) choices: Vec<Choice>,
    /// None after the input changes, until `refresh` highlights the exact, current or first choice
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
    pub(crate) viewport: Viewport,
    pub(crate) messages_area: Rect,
    pub(crate) pinned_area: Rect,
    pub(crate) input_area: Rect,
    /// The pictures the last frame drew
    pub(crate) pictures: Vec<Source>,
    pub(crate) selection: Option<Selection>,
    pressed: Option<Point>,
    dragging: bool,
    last_click: Option<(Point, Instant)>,
    /// `Err` holds the reason it failed
    pub(crate) flash: Option<(Result<String, String>, Instant)>,
    /// The end of a turn nothing runs after, for the host to tell a user who looked away
    pub(crate) alert: Option<Alert>,
    pub(crate) quit: bool,
    dirty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Alert {
    /// The session's title, empty until it has one
    pub(crate) title: String,
    pub(crate) body: String,
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
        let text = mention(path);
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
            draft: Draft::default(),
            history: History::default(),
            choices: Vec::new(),
            highlight: ListState::default(),
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
            viewport: Viewport::default(),
            messages_area: Rect::default(),
            pinned_area: Rect::default(),
            input_area: Rect::default(),
            pictures: Vec::new(),
            selection: None,
            pressed: None,
            dragging: false,
            last_click: None,
            flash: None,
            alert: None,
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
        if loaded != self.transcript.loaded {
            self.transcript.loaded = loaded;
            self.dirty = true;
        }

        let animated = self.animating && self.drawn.elapsed() >= ANIMATE_EVERY;
        let scrolled = self.scroll_at.is_some_and(|at| at <= Instant::now());
        let busy = self.session.run.is_some();
        let ticked = !self.session.programs.is_empty() && self.drawn.elapsed() >= TICK_EVERY;
        let dirty = mem::take(&mut self.dirty) || busy || animated || scrolled || ticked;
        if dirty {
            self.drawn = Instant::now();
        }
        dirty
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
            self.viewport.back = 0;
            self.session.prompt(text);
            return;
        };

        // A queued command may run while a draft sits in the input box,
        // and then the list goes into the transcript
        let drafting = !self.draft.whole().is_empty();
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
                let effort = self.session.effort().unwrap_or_else(|| level.clone());
                switched.map(|()| format!("effort: {effort}"))
            }
            Command::Mcp(on, name) => {
                let switched = self.session.set_mcp(*on, name);
                switched.map(|status| mcp_line(&status))
            }
            Command::Shell(command) => {
                self.viewport.back = 0;
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
                let alt = key.modifiers.contains(KeyModifiers::ALT);
                let plain = key.modifiers.is_empty();
                let running = self.session.running();
                let idle = self.session.run.is_none() && self.draft.whole().is_empty();
                let page = usize::from(self.messages_area.height);
                // The input box wraps, so this counts screen rows
                let top = self.draft.area.screen_cursor().row == 0;
                let first = self.draft.area.cursor() == DataCursor(0, 0);
                let last = self.draft.cursor() == self.draft.text().chars().count();

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
                            self.draft.replace("");
                            self.history = History::default();
                        }
                    },
                    (_, KeyCode::Char('d')) if ctrl && idle => self.quit = true,
                    (_, KeyCode::Char('o')) if ctrl => self.expanded = !self.expanded,
                    // Windows Terminal keeps Ctrl+V for its own paste, which carries no image
                    (_, KeyCode::Char('v')) if ctrl || alt => self.paste_image(None),
                    (_, KeyCode::Char('j')) if ctrl => {
                        self.edit(|input| input.insert_str("\n"));
                    }
                    (_, KeyCode::Enter) if plain => self.submit(),
                    (_, KeyCode::Enter) => {
                        self.edit(|input| input.insert_str("\n"));
                    }
                    (_, KeyCode::Esc) if self.viewport.back > 0 => self.viewport.back = 0,
                    (_, KeyCode::Esc) if running => self.session.cancel(),
                    // An invisible ! sits before the cursor
                    (_, KeyCode::Backspace) if self.draft.shell && first => {
                        self.draft.shell = false;
                    }
                    (_, KeyCode::PageUp) => self.viewport.scroll_up(page),
                    (_, KeyCode::PageDown) => self.viewport.scroll_down(page),
                    // Must come before the history arm
                    (_, KeyCode::Up) if top && !self.queued.is_empty() => {
                        if let Some(text) = self.queued.pop_back() {
                            let mut text = self.draft.folded(text);
                            let draft = self.draft.whole();
                            if !draft.is_empty() {
                                text = format!("{text}\n{draft}");
                            }
                            self.draft.replace(&text);
                            // The next Up must still find the cursor on the first screen row
                            self.draft.area.move_cursor(CursorMove::Top);
                            self.draft.area.move_cursor(CursorMove::Head);
                            self.choices_closed = true;
                        }
                    }
                    (_, KeyCode::Up) if top => {
                        let current = self.draft.whole();
                        let sent = self.session.conversation.sent();
                        if let Some(text) = self.history.back(&sent, current) {
                            let text = self.draft.folded(text);
                            self.draft.replace(&text);
                            self.choices_closed = true;
                        }
                    }
                    (_, KeyCode::Down) => {
                        let before = self.draft.area.screen_cursor().row;
                        self.draft.area.move_cursor(CursorMove::Down);
                        // Only a cursor that can't move down is on the last screen row
                        if self.draft.area.screen_cursor().row == before {
                            let sent = self.session.conversation.sent();
                            if let Some((text, draft)) = self.history.forward(&sent) {
                                // The user's own draft comes back exactly as typed
                                let text = match draft {
                                    true => text,
                                    false => self.draft.folded(text),
                                };
                                self.draft.replace(&text);
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
                            self.highlight.select(None);
                            self.choices_closed = false;
                        }
                    }
                }
            }

            Event::Paste(text) => {
                self.selection = None;
                let text = text.replace("\r\n", "\n").replace('\r', "\n");
                let text = self.draft.folded(text);
                if self.edit(|input| input.insert_str(text)) {
                    self.highlight.select(None);
                    self.choices_closed = false;
                }
            }

            Event::Mouse(mouse) => {
                let selected = self.selection;
                let position = Position::new(mouse.column, mouse.row);
                let on_choices = self.choices_area.contains(position);
                let area = self.messages_area;
                let inside = area.contains(position);
                let scrolls = inside || self.pinned_area.contains(position);
                let on_bar = self.viewport.bar.contains(position);

                match mouse.kind {
                    // A drag of the scrollbar ends wherever the button is let go
                    MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                        self.viewport.drag_to(mouse.row);
                    }
                    MouseEventKind::Up(MouseButton::Left) if self.dragging => {
                        self.dragging = false;
                    }

                    MouseEventKind::ScrollUp if on_choices => self.highlight.select_previous(),
                    MouseEventKind::ScrollDown if on_choices => self.highlight.select_next(),
                    _ if on_choices => {}

                    MouseEventKind::ScrollUp if scrolls => self.viewport.scroll_up(SCROLL_ROWS),
                    MouseEventKind::ScrollDown if scrolls => self.viewport.scroll_down(SCROLL_ROWS),

                    MouseEventKind::Down(MouseButton::Left) if on_bar => {
                        self.dragging = true;
                        self.selection = None;
                        self.viewport.drag_to(mouse.row);
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
                                self.viewport.scroll_up(1);
                            } else if mouse.row >= area.bottom() {
                                self.viewport.scroll_down(1);
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

                let grown = self
                    .selection
                    .filter(|selection| Some(*selection) != selected);
                if let Some(selection) = grown {
                    let talk = &self.session.conversation;
                    let rows = self.transcript.rows(talk, area.width, self.expanded);
                    let text = selection.text(rows);
                    self.desktop.selecting(&text);
                }
            }

            Event::Resize(..) => self.selection = None,
            _ => {}
        }

        if typed {
            self.draft.take_bang();
            self.refresh();
        }
    }

    pub(crate) fn pump(&mut self) {
        let mut changed = false;
        // Queued messages go out only once every output is drained
        let mut ended = None;
        // The session whose programs ended, which the queue may replace with another
        let mut background = None;
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

                Output::Background => background = Some(self.session.id.clone()),

                Output::Done(outcome) => {
                    ended = Some(outcome);
                    changed = true;
                }
            }
        }

        // Worded before the queue runs, which may open another session
        let said = match &ended {
            Some(Outcome::Answered) => {
                // A command may hold secrets, which notification centers keep
                let first = match self.session.conversation.said() {
                    Said::Answer(answer) => markdown::first_line(&answer),
                    Said::Shell(shell) => match &shell.end {
                        ShellEnd::Failed(reason) => format!("Failed: {reason}"),
                        _ => String::new(),
                    },
                };
                match first.is_empty() {
                    true => Some("Done".to_owned()),
                    false => Some(first),
                }
            }
            Some(Outcome::Failed(error)) => {
                let first = error.trim().lines().next().unwrap_or_default();
                Some(format!("Failed: {first}"))
            }
            Some(Outcome::Cancelled) | None => None,
        };
        let title = &self.session.title;
        let alerted = |said: String| {
            let title = title.clone();
            let body = clipped(&said, MAX_ALERT_COLUMNS);
            Alert { title, body }
        };
        let alert = said.map(alerted);

        if ended.is_some() {
            while self.session.run.is_none() && !self.quit {
                let Some(next) = self.queued.pop_front() else {
                    break;
                };
                self.submit_text(&next);
            }
        }
        // A task the queue started takes the programs that ended along with it
        let kept = background.as_ref() == Some(&self.session.id);
        if kept && self.session.run.is_none() && !self.quit {
            self.session.background();
        }
        if changed {
            self.refresh();
        }

        // A turn that the queue or programs that ended started is still at work
        if alert.is_some() && self.session.run.is_none() {
            self.alert = alert;
        }
    }

    /// Puts the image `given`, or else the clipboard's, in the input as its placeholder, or flashes
    /// why it can't, reading the clipboard only once the image could go in, as a terminal may ask
    /// the user first
    pub(crate) fn paste_image(&mut self, given: Option<Vec<u8>>) {
        self.dirty = true;
        let pasted = match (self.draft.shell, self.session.client.image_prompts) {
            (true, _) => Err("image: shell commands take none".to_owned()),
            (false, false) => {
                let name = &self.session.client.agent.name;
                Err(format!("image: {name} takes no images"))
            }
            (false, true) => {
                let read = match given {
                    Some(data) => Ok(data),
                    None => self.desktop.image(),
                };
                read.and_then(|data| self.session.paste_image(data))
            }
        };
        let number = match pasted {
            Ok(number) => number,
            Err(reason) => {
                self.flash = Some((Err(reason), Instant::now()));
                return;
            }
        };

        self.selection = None;
        let shown = image_placeholder(number);
        if self.edit(|input| input.insert_str(shown)) {
            self.highlight.select(None);
            self.choices_closed = false;
        }
        self.refresh();
    }

    /// The replayed history arrives before the `session/load` reply
    pub(crate) fn show_session(&mut self) {
        self.viewport.back = 0;
        self.pump();
        self.session.conversation.finish_turn(Outcome::Answered);

        let text = format!("resumed session {}", self.session.id);
        self.info(text);
    }

    /// Must be called whenever the input, status or options change
    fn refresh(&mut self) {
        let fresh = self.highlight.selected().is_none();
        if fresh {
            self.highlight.select_first();
        }

        if self.draft.shell {
            self.choices.clear();
            return;
        }

        let mut listing = None;
        if let [line] = self.draft.area.lines() {
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
        // Only running the command reports a list that is empty or failed,
        // since a prefix may be typed again and again
        if let Some(list) = listing.filter(|_| opened != listing) {
            let picks = self.picks(list).unwrap_or_default();
            self.opened = Some((list, picks));
        }

        let DataCursor(row, col) = self.draft.area.cursor();
        let line = self.draft.area.lines().get(row).map_or("", String::as_str);
        let before: String = line.chars().take(col).collect();
        let typing = typing_mention(&before);
        let query = typing.map_or("", |(_, query)| query);
        let scope = outside_dir(query).unwrap_or_default();
        if typing.is_some() && !self.paths.contains_key(scope) {
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

        if let (Some((typed, query)), Some(paths)) = (typing, self.paths.get(scope)) {
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

        let [line] = self.draft.area.lines() else {
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
                let chosen = fresh && lower == query;
                offer(&mut self.choices, &mut self.highlight, choice, chosen);
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
            let chosen = fresh && (exact || current);
            offer(&mut self.choices, &mut self.highlight, choice, chosen);
        }
    }

    /// Reports a list that is empty or failed to arrive, and returns whether it has rows
    fn open_list(&mut self, list: List) -> bool {
        let asked = self.picks(list);
        match &asked {
            Ok(picks) if picks.is_empty() => {
                let text = empty_text(list);
                self.info(text);
            }
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

            List::Models => return Ok(self.option_picks(Selector::Model, &prefix)),
            List::Efforts => return Ok(self.option_picks(Selector::Effort, &prefix)),

            List::Servers => {
                for server in &self.session.mcp {
                    let state = mcp_state(&server.state);
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
    fn option_picks(&self, selector: Selector, prefix: &str) -> Vec<Pick> {
        let mut picks = Vec::new();
        for offering in self.session.offerings(selector) {
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
        let row = self.viewport.top() + usize::from(y - top);
        let col = usize::from(x - left);
        Some(Point { row, col })
    }

    fn fill(&mut self, completion: &Completion) {
        match completion {
            Completion::Input(text) => self.complete(text),
            Completion::Mention { typed, text } => {
                for _ in 0..*typed {
                    self.draft.area.delete_char();
                }
                self.draft.area.insert_str(text);
                self.highlight.select(None);
            }
            Completion::Memory(_) => {}
        }
    }

    fn complete(&mut self, text: &str) {
        self.draft.replace(text);
        self.highlight.select(None);
        self.choices_closed = false;
        self.refresh();
    }

    /// The pasted images' placeholders stay whole like those of pastes
    fn edit(&mut self, change: impl FnOnce(&mut TextArea<'static>) -> bool) -> bool {
        let images = self.session.images.keys();
        self.draft.edit(change, images)
    }

    fn submit(&mut self) {
        let text = self.draft.expanded();
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }

        let command = Command::parse(&text);
        if let Some(Command::Invalid(message)) = &command {
            return self.failure(message);
        }

        self.history = History::default();
        self.draft = Draft::default();
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

/// Queued text the running turn can take, as commands wait for it to end
fn plain(text: &str) -> bool {
    Command::parse(text).is_none()
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

fn offer(choices: &mut Vec<Choice>, highlight: &mut ListState, choice: Choice, chosen: bool) {
    if chosen {
        highlight.select(Some(choices.len()));
    }
    choices.push(choice);
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
    use crate::tui::app::*;
    use crate::tui::tests::{app, written};
    use ratatui::crossterm::event::KeyEvent;
    use std::thread;

    fn settle(app: &mut App, done: impl Fn(&mut App) -> bool) {
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

    /// A clipboard that Ctrl+V and Alt+V read
    struct Clipped(Result<Vec<u8>, String>);

    impl Desktop for Clipped {
        fn copy(&mut self, _: &str) {}

        fn open(&mut self, _: &Target) -> Result<(), String> {
            Ok(())
        }

        fn image(&mut self) -> Result<Vec<u8>, String> {
            self.0.clone()
        }
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

        // The update joins the turn, and the command waits for its end
        let mut asked = Vec::new();
        for entry in app.session.conversation.entries() {
            match entry {
                Entry::Task(text) => asked.push(text.as_str()),
                Entry::Info(_) => asked.push(":mcp"),
                _ => {}
            }
        }
        assert_eq!(asked, ["queue", "update", ":mcp", "later"]);
        assert_eq!(app.session.conversation.stats().turns, 2);
    }

    /// An image goes in as its placeholder, which goes whole and never gets its number again,
    /// and goes out with the task
    #[test]
    fn image_pasted() {
        let mut app = app();
        let path = written("pasted", 2, 2);
        let png = fs::read(&path).expect("png");
        fs::remove_dir_all(path.parent().expect("dir")).ok();
        app.desktop = Box::new(Clipped(Ok(png.clone())));
        app.draft.replace("Look at ");
        let ctrl_v = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL);
        app.on_event(Event::Key(ctrl_v));
        assert_eq!(app.draft.area.lines(), ["Look at [image #1]"]);
        key(&mut app, KeyCode::Backspace);
        assert_eq!(app.draft.area.lines(), ["Look at "]);

        let alt_v = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT);
        app.on_event(Event::Key(alt_v));
        key(&mut app, KeyCode::Left);
        key(&mut app, KeyCode::Char('🐍'));
        assert_eq!(app.draft.area.lines(), ["Look at [image #2]🐍"]);
        key(&mut app, KeyCode::Enter);
        settle(&mut app, |app| app.session.run.is_none());
        let rows = shown(&mut app);
        assert!(
            rows.iter().any(|row| row.contains("Seen image/png.")),
            "{rows:?}"
        );

        app.desktop = Box::new(Clipped(Err("clipboard: no image".to_owned())));
        app.on_event(Event::Key(ctrl_v));
        let flash = app.flash.as_ref().map(|(text, _)| text.clone());
        assert_eq!(flash, Some(Err("clipboard: no image".to_owned())));
        app.flash = None;
        app.paste_image(Some("café".as_bytes().to_vec()));
        let refused = matches!(app.flash, Some((Err(_), _)));
        assert!(refused);
        assert!(app.draft.area.lines().iter().all(String::is_empty));

        // Neither a shell command nor an agent without images reads the clipboard
        app.draft.replace("! ls ");
        app.desktop = Box::new(Clipped(Ok(png)));
        app.flash = None;
        app.on_event(Event::Key(ctrl_v));
        let refused = matches!(app.flash, Some((Err(_), _)));
        assert!(refused);
        app.draft.replace("");
        app.session.client.image_prompts = false;
        app.flash = None;
        app.on_event(Event::Key(ctrl_v));
        let refused = matches!(app.flash, Some((Err(_), _)));
        assert!(refused);
        assert!(app.draft.area.lines().iter().all(String::is_empty));
        assert_eq!(app.session.images.len(), 2);
    }
}
