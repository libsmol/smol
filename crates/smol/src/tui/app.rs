use crate::chat::{opens_mention, resolved, List, Output, Session};
use crate::client::{models, McpState, Pick};
use crate::tui::render::Target;
use crate::tui::select::{self, Point, Selection};
use crate::tui::transcript::{Entry, Outcome, Transcript};
use crate::tui::ui::input_box;
use crate::tui::{browser, diagram};
use ignore::WalkBuilder;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;
use ratatui_textarea::{CursorMove, DataCursor, TextArea};
use std::collections::VecDeque;
use std::fs;
use std::mem;
use std::path::{is_separator, MAIN_SEPARATOR};
use std::time::{Duration, Instant};

const FLASH_WAIT: Duration = Duration::from_secs(2);
const DOUBLE_CLICK_WAIT: Duration = Duration::from_millis(500);
const ANIMATE_EVERY: Duration = Duration::from_millis(100);
const RESUME_PREFIX: &str = ":resume ";
const MODEL_PREFIX: &str = ":model ";
const MCP_PREFIX: &str = ":mcp ";
const SHELL_USAGE: &str = "!<command>";
const MAX_UNFOLDED_LINES: usize = 5;
/// On Windows, characters typed with AltGr carry Ctrl and Alt
const ALTGR: KeyModifiers = KeyModifiers::CONTROL.union(KeyModifiers::ALT);
/// Keeps typing @ responsive even in the home directory
const MAX_PATHS: usize = 20_000;
const MAX_MATCHES: usize = 200;

pub(crate) struct App {
    pub(crate) session: Session,
    pub(crate) transcript: Transcript,
    pub(crate) run: Option<Run>,
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
    choices_closed: bool,
    pub(crate) choices_area: Rect,
    session_rows: Option<Vec<Pick>>,
    /// The first item is the listed directory, or empty for the whole cwd
    paths: Option<(String, Vec<String>)>,
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
    pub(crate) selection: Option<Selection>,
    pressed: Option<Point>,
    dragging: bool,
    last_click: Option<(Point, Instant)>,
    /// `Err` holds the reason it failed
    pub(crate) flash: Option<(Result<String, String>, Instant)>,
    pub(crate) quit: bool,
    dirty: bool,
}

pub(crate) struct Run {
    pub(crate) step_started: Instant,
    pub(crate) interrupting: bool,
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
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    New,
    List(List),
    Resume(String),
    Model(String),
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
        if !line.starts_with(':') {
            return None;
        }

        let (name, arg) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let parsed = match (name, arg.trim()) {
            (":new", "") => Some(Self::New),
            (":resume", "") => Some(Self::List(List::Sessions)),
            (":resume", id) => Some(Self::Resume(id.to_owned())),
            (":model", "") => Some(Self::List(List::Models)),
            (":model", model) => Some(Self::Model(model.to_owned())),
            (":mcp", "") => Some(Self::List(List::Servers)),
            (":mcp", arg) => match arg.split_once(char::is_whitespace) {
                Some((switch @ ("on" | "off"), server)) => {
                    let server = server.trim().to_owned();
                    Some(Self::Mcp(switch == "on", server))
                }
                _ => None,
            },
            (":quit", "") => Some(Self::Quit),
            _ => None,
        };
        if parsed.is_some() {
            return parsed;
        }

        let known = HINTS.iter().find(|hint| hint.name() == name);
        let text = match known {
            Some(hint) => format!("usage: {}", hint.usage),
            None => {
                let mut usages: Vec<&str> = HINTS.iter().map(|hint| hint.usage).collect();
                usages.push(SHELL_USAGE);
                let commands = usages.join(", ");
                format!("command: {name}: unknown (commands: {commands})")
            }
        };
        Some(Self::Invalid(text))
    }
}

struct Hint {
    usage: &'static str,
    summary: &'static str,
}

impl Hint {
    fn name(&self) -> &'static str {
        self.usage.split(' ').next().unwrap_or(self.usage)
    }
}

const HINTS: [Hint; 5] = [
    Hint {
        usage: ":new",
        summary: "start a new session",
    },
    Hint {
        usage: ":resume <session>",
        summary: "resume an earlier session",
    },
    Hint {
        usage: ":model <name>",
        summary: "switch the model",
    },
    Hint {
        usage: ":mcp <on|off> <server>",
        summary: "turn an MCP server on (again) or off",
    },
    Hint {
        usage: ":quit",
        summary: "quit smol",
    },
];

impl App {
    pub(crate) fn new(session: Session) -> Self {
        let cwd = session.client.cwd.clone();
        Self {
            session,
            transcript: Transcript::new(cwd),
            run: None,
            queued: VecDeque::new(),
            input: input_box(""),
            shell: false,
            input_top: 0,
            pastes: Vec::new(),
            history: History::default(),
            choices: Vec::new(),
            highlight: ListState::default().with_selected(Some(0)),
            choices_closed: false,
            choices_area: Rect::default(),
            session_rows: None,
            paths: None,
            expanded: false,
            animating: false,
            drawn: Instant::now(),
            scrolled_back: 0,
            bottom: 0,
            drawn_back: 0,
            messages_area: Rect::default(),
            pinned_area: Rect::default(),
            scrollbar: Rect::default(),
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

        let animated = self.animating && self.drawn.elapsed() >= ANIMATE_EVERY;
        let dirty = mem::take(&mut self.dirty) || self.run.is_some() || animated;
        if dirty {
            self.drawn = Instant::now();
        }
        dirty
    }

    pub(crate) fn running(&self) -> bool {
        self.run.as_ref().is_some_and(|run| !run.interrupting)
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

    pub(crate) fn submit_text(&mut self, text: &str) {
        let Some(command) = Command::parse(text) else {
            return self.send(text);
        };

        // A queued command may run while a draft sits in the input box,
        // and then the list goes into the transcript
        let drafting = !self.draft().is_empty();
        let result = match &command {
            Command::List(list) if drafting => self.session.list(*list),
            Command::List(List::Sessions) => return self.complete(RESUME_PREFIX),
            Command::List(List::Models) if models(&self.session.options).is_empty() => {
                return self.info(List::Models.empty_text())
            }
            Command::List(List::Models) => return self.complete(MODEL_PREFIX),
            Command::List(List::Servers) if self.session.mcp.is_empty() => {
                return self.info(List::Servers.empty_text())
            }
            Command::List(List::Servers) => return self.complete(MCP_PREFIX),
            Command::Quit => {
                self.quit = true;
                return;
            }
            Command::New => return self.open(None),
            Command::Resume(id) => return self.open(Some(id)),
            Command::Model(name) => self.session.set_model(name),
            Command::Mcp(on, name) => self.session.set_mcp(*on, name),
            Command::Shell(command) => {
                self.begin();
                return self.session.client.shell(&self.session.id, command);
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
        self.history = History::default();
        // A queued command runs with no key press to clear the selection,
        // which would land on the new transcript by row number
        self.selection = None;
        self.pressed = None;

        if resume.is_some() {
            self.show_session();
        }
    }

    fn send(&mut self, task: &str) {
        self.transcript.task(task);
        self.begin();
        self.session.prompt(task);
    }

    fn begin(&mut self) {
        self.scrolled_back = 0;
        self.run = Some(Run {
            step_started: Instant::now(),
            interrupting: false,
        });
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

        match event {
            Event::Key(key) => {
                self.selection = None;

                let count = self.choices.len();
                let index = self.highlighted().unwrap_or(0);
                let choice = self.choices.get(index).cloned();

                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let plain = key.modifiers.is_empty();
                let running = self.running();
                let idle = self.run.is_none() && self.draft().is_empty();
                let page = usize::from(self.messages_area.height);
                // The input box wraps, so this counts screen rows
                let top = self.input.screen_cursor().row == 0;
                let first = self.input.cursor() == DataCursor(0, 0);

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
                    (Some(choice), KeyCode::Enter) if plain => {
                        self.fill(&choice.completion);
                        if choice.submits() {
                            self.submit();
                        }
                    }
                    (Some(_), KeyCode::Esc) => self.choices_closed = true,

                    (_, KeyCode::Char('c')) if ctrl => match self.run {
                        Some(_) if running => self.interrupt(),
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
                    (_, KeyCode::Esc) if running => self.interrupt(),
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
                        let sent = self.transcript.sent();
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
                            let sent = self.transcript.sent();
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
                    MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                        self.drag_to(mouse.row);
                    }
                    MouseEventKind::Up(MouseButton::Left) if self.dragging => {
                        self.dragging = false;
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
                            let rows = self.transcript.rows(area.width, self.expanded);
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
                        let Some(selection) = self.selection else {
                            // No drag means it was a click
                            let rows = self.transcript.rows(area.width, self.expanded);
                            let Some(point) = pressed else {
                                return;
                            };
                            let row = rows.get(point.row);
                            let mut targets = row.into_iter().flat_map(|row| &row.targets);
                            let hit = targets.find(|(columns, _)| columns.contains(&point.col));
                            let Some((_, target)) = hit.cloned() else {
                                return;
                            };

                            let told = match target {
                                Target::Url(url) => {
                                    let opened = browser::open(&url);
                                    opened.map(|()| "opening the link in a browser".to_owned())
                                }
                                Target::Diagram(code) => {
                                    let opened = diagram::open(&code);
                                    opened.map(|()| "opening the diagram in a browser".to_owned())
                                }
                            };
                            self.flash = Some((told, Instant::now()));
                            return;
                        };

                        let rows = self.transcript.rows(area.width, self.expanded);
                        let text = selection.text(rows);
                        if !text.trim().is_empty() {
                            select::copy(&text);
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
        while let Some(outputs) = self.session.poll() {
            self.dirty = true;

            for output in outputs {
                match output {
                    Output::Update(update) => {
                        let ended = self.transcript.update(&update);
                        if let Some(run) = self.run.as_mut().filter(|_| ended) {
                            run.step_started = Instant::now();
                        }
                    }

                    Output::Notice(notice) => self.transcript.notice(&notice),
                    Output::Info(text) => self.info(text),
                    Output::Failure(text) => self.failure(text),
                    Output::Status => changed = true,

                    // A command waits for the turn to end, and so does everything queued behind it
                    Output::Queued(asked) => {
                        let count = match self.running() {
                            true => self.queued.iter().take_while(|text| plain(text)).count(),
                            false => 0,
                        };

                        let mut prompts = Vec::new();
                        for text in self.queued.drain(..count) {
                            self.transcript.amend(&text);
                            let blocks = self.session.blocks(&text);
                            prompts.push(blocks);
                        }
                        asked.answer(prompts);
                    }

                    Output::Done(stop) => {
                        self.run = None;
                        self.transcript.done(stop);
                        finished = true;
                        changed = true;
                    }
                }
            }
        }

        if finished {
            while self.run.is_none() && !self.quit {
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
        self.transcript.finish_turn(Outcome::Answered);

        let text = format!("resumed session {}", self.session.id);
        self.info(text);
    }

    /// Must be called whenever the input, status or options change
    fn refresh(&mut self) {
        if self.shell {
            self.choices.clear();
            return;
        }

        let lists = [
            (RESUME_PREFIX, List::Sessions),
            (MODEL_PREFIX, List::Models),
        ];
        let listing = match self.input.lines() {
            [line] => lists
                .into_iter()
                .find(|(prefix, _)| line.starts_with(prefix)),
            _ => None,
        };

        // Sessions come from the agent and are asked for once when the prefix is typed
        let asking = matches!(listing, Some((_, List::Sessions)));
        if !asking {
            self.session_rows = None;
        } else if self.session_rows.is_none() {
            let asked = self.session.rows(List::Sessions);
            match &asked {
                Ok(rows) if rows.is_empty() => self.info(List::Sessions.empty_text()),
                Ok(_) => {}
                Err(error) => {
                    let text = format!("{error:#}");
                    self.failure(text);
                }
            }

            let rows = asked.unwrap_or_default();
            self.session_rows = Some(rows);
        }

        let DataCursor(row, col) = self.input.cursor();
        let line = self.input.lines().get(row).map_or("", String::as_str);
        let before: String = line.chars().take(col).collect();
        let mention = typing_mention(&before);
        let query = mention.map_or("", |(_, query)| query);
        let scope = outside_dir(query).unwrap_or_default();
        let cached = self.paths.as_ref().map(|(dir, _)| dir.as_str());
        if mention.is_none() {
            self.paths = None;
        } else if cached != Some(scope) {
            let cwd = &self.session.client.cwd;
            let mut paths = Vec::new();
            if scope.is_empty() {
                let mut walk = WalkBuilder::new(cwd);
                walk.hidden(false)
                    .require_git(false)
                    .filter_entry(|entry| entry.file_name() != ".git");

                for entry in walk.build().flatten().take(MAX_PATHS) {
                    let full = entry.path();
                    let relative = full.strip_prefix(cwd).unwrap_or(full);
                    if relative.as_os_str().is_empty() {
                        continue;
                    }

                    // Completions always use /, which Windows accepts too
                    let mut path = relative.to_string_lossy().replace(MAIN_SEPARATOR, "/");
                    if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                        path.push('/');
                    }
                    paths.push(path);
                }
            } else {
                // Outside the cwd only one level is listed,
                // which stays fast even in the home or root directory
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
            }

            self.paths = Some((scope.to_owned(), paths));
        }

        self.choices.clear();
        if self.choices_closed {
            return;
        }

        if let (Some((typed, query)), Some((_, paths))) = (mention, &self.paths) {
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

        if let Some(arg) = line.strip_prefix(MCP_PREFIX) {
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

                    let completion = format!("{MCP_PREFIX}{switch} ");
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

                let completion = format!("{MCP_PREFIX}{switch} {name}");
                let label = name.clone();
                let choice = Choice::input(label, effect, completion);
                offer(&mut self.choices, choice, lower == query);
            }
            return;
        }

        let Some((prefix, list)) = listing else {
            for hint in hints(line) {
                let name = hint.name();
                // A trailing space makes Enter fill without submitting,
                // and only commands that open a list get one
                let spaced = format!("{name} ");
                let opens = [RESUME_PREFIX, MODEL_PREFIX, MCP_PREFIX].contains(&spaced.as_str());
                let completion = match opens {
                    true => spaced,
                    false => name.to_owned(),
                };
                let label = hint.usage.to_owned();
                let choice = Choice::input(label, hint.summary, completion);
                self.choices.push(choice);
            }
            return;
        };

        // Sessions use only what was already fetched, since asking again here would freeze the UI
        let rows = match asking {
            true => self.session_rows.clone(),
            false => self.session.rows(list).ok(),
        };
        let query = line[prefix.len()..].trim().to_lowercase();
        for row in rows.unwrap_or_default() {
            let label = row.label.to_lowercase();
            let value = row.value.to_lowercase();
            if !label.contains(&query) && !value.contains(&query) {
                continue;
            }

            let exact = label == query || value == query;
            let current = row.current && query.is_empty() && list == List::Models;
            let detail = match row.current {
                true => "current",
                false => &row.description,
            };
            let completion = format!("{prefix}{}", row.value);
            let choice = Choice::input(row.label, detail, completion);
            offer(&mut self.choices, choice, exact || current);
        }
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
        if text.lines().count() <= MAX_UNFOLDED_LINES {
            return text;
        }

        let number = self.pastes.len() + 1;
        let shown = placeholder(number, &text);
        self.pastes.push(text);
        shown
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

        if self.run.is_none() {
            return self.submit_text(&text);
        }
        self.queued.push_back(text);
        // A turn waiting on agents takes it now, unless a command ahead of it waits for the turn to end
        if self.queued.iter().all(|text| plain(text)) {
            self.session.enqueued();
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

    fn interrupt(&mut self) {
        if let Some(run) = &mut self.run {
            run.interrupting = true;
            self.session.client.cancel(&self.session.id);
        }
    }

    fn info(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.transcript.push(Entry::Info(text));
    }

    fn failure(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.transcript.push(Entry::Failure(text));
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

    let burst = text.chars().count() > 1;
    let typed = stalled && text.ends_with('\n') && text.matches('\n').count() == 1;
    match burst && text.contains('\n') && !typed {
        true => Some(text),
        false => None,
    }
}

fn placeholder(number: usize, text: &str) -> String {
    let lines = text.lines().count();
    format!("[paste #{number}: {lines} lines]")
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

fn hints(input: &str) -> Vec<&'static Hint> {
    if !input.starts_with(':') || input.contains(char::is_whitespace) {
        return Vec::new();
    }

    let mut found = Vec::new();
    for hint in &HINTS {
        if hint.name().starts_with(input) {
            found.push(hint);
        }
    }
    found
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
        let rows = app.transcript.rows(60, false);
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
        settle(&mut app, |app| app.queued.is_empty() && app.run.is_none());

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
            app.transcript.sent(),
            ["first task", "! ls docs", "second task"]
        );
    }

    #[test]
    fn queue_joins_turn() {
        let mut app = app();
        app.submit_text("queue");
        app.queued
            .extend(["update", ":mcp", "later"].map(str::to_owned));
        settle(&mut app, |app| app.queued.is_empty() && app.run.is_none());

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
        assert_eq!(app.transcript.stats().turns, 2);
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
        settle(&mut app, |app| app.run.is_none());

        app.submit_text(":resume nope");
        let expected = ["", "❯ first task", "", "✗ unknown session"];
        assert_eq!(shown(&mut app), expected);

        app.submit_text(":resume s1");
        let expected = ["", "❯ old task", "", "  resumed session s1"];
        assert_eq!(shown(&mut app), expected);
    }

    #[test]
    fn resume_lists_cwd() {
        let mut app = app();
        app.session.open(None).expect("open");
        app.submit_text(":resume");

        let labels: Vec<&str> = app
            .choices
            .iter()
            .map(|choice| choice.label.as_str())
            .collect();
        assert_eq!(labels, ["old session"]);
    }

    #[test]
    fn no_models_shown() {
        let mut app = app();
        app.session.options.clear();
        app.submit_text(":model");
        assert_eq!(shown(&mut app), ["", "  no models"]);
    }

    #[test]
    fn current_model_first() {
        let mut app = app();
        app.session.set_model("m2-café").expect("switch");
        assert_eq!(labels(&mut app, ":model "), ["m2-café", "m1"]);
        assert_eq!(labels(&mut app, ":model m"), ["m1", "m2-café"]);
    }

    #[test]
    fn refused_not_counted() {
        let mut app = app();
        app.submit_text("huge");
        settle(&mut app, |app| app.run.is_none());

        assert_eq!(app.transcript.stats().turns, 0);
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
        assert!(!app.run.as_ref().is_some_and(|run| run.interrupting));

        key(&mut app, KeyCode::Esc);
        assert!(app.run.as_ref().is_some_and(|run| run.interrupting));
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
        assert!(app.queued.is_empty() && app.running());
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
        let sent = app.transcript.sent();
        let expected = format!("Résumé: {pasted}\nshort");
        assert_eq!(sent.last(), Some(&expected));

        key(&mut app, KeyCode::Up);
        assert_eq!(app.input.lines(), ["[paste #1: 8 lines]"]);
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
        settle(&mut app, |app| app.queued.is_empty() && app.run.is_none());
        assert_eq!(app.transcript.sent(), ["wait", "try another way"]);
    }

    #[test]
    fn quit_stops_queue() {
        let mut app = app();
        app.submit_text("first task");
        app.queued.extend([":quit", "later"].map(str::to_owned));
        settle(&mut app, |app| app.quit);

        assert!(app.run.is_none());
        assert_eq!(app.queued, ["later"]);
    }

    #[test]
    fn invalid_command_kept() {
        let mut app = app();
        app.set_input(": what does this mean");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.input.lines(), [": what does this mean"]);

        app.submit_text(":undo");
        assert!(app.run.is_none());
        let shown = shown(&mut app);
        let failed: Vec<&String> = shown.iter().filter(|row| row.starts_with('✗')).collect();
        assert_eq!(failed.len(), 2, "{shown:?}");
        assert!(failed[0].starts_with("✗ command: :: unknown"), "{shown:?}");
        assert!(
            failed[1].starts_with("✗ command: :undo: unknown"),
            "{shown:?}"
        );
    }

    #[test]
    fn path_completions() {
        let text = |path| match Choice::path(path, 1).completion {
            Completion::Mention { text, .. } | Completion::Input(text) => text,
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
        assert_eq!(Command::parse(":new"), Some(Command::New));
        let shell = Command::parse("! ls docs\n");
        assert_eq!(shell, Some(Command::Shell("ls docs".to_owned())));
        let usage = Some(Command::Invalid("usage: !<command>".to_owned()));
        assert_eq!(Command::parse("!  "), usage);
        let sessions = Some(Command::List(List::Sessions));
        assert_eq!(Command::parse(":resume"), sessions);

        let resume = Command::parse(":resume  abc-123 ");
        assert_eq!(resume, Some(Command::Resume("abc-123".to_owned())));
        assert_eq!(Command::parse(":quit"), Some(Command::Quit));

        let models = Some(Command::List(List::Models));
        assert_eq!(Command::parse(":model"), models);
        let model = Command::parse(":model tiny");
        assert_eq!(model, Some(Command::Model("tiny".to_owned())));

        let servers = Some(Command::List(List::Servers));
        assert_eq!(Command::parse(":mcp"), servers);
        let on = Command::parse(":mcp on fake");
        assert_eq!(on, Some(Command::Mcp(true, "fake".to_owned())));
        let off = Command::parse(":mcp off  fake ");
        assert_eq!(off, Some(Command::Mcp(false, "fake".to_owned())));
        let wide = Command::parse(":mcp on\u{3000}web");
        assert_eq!(wide, Some(Command::Mcp(true, "web".to_owned())));

        for line in ["hello", "quit", "q", "", " :new"] {
            assert_eq!(Command::parse(line), None, "{line}");
        }

        let usage = |text: &str| Some(Command::Invalid(text.to_owned()));
        assert_eq!(Command::parse(":new please"), usage("usage: :new"));
        let mcp = usage("usage: :mcp <on|off> <server>");
        for line in [":mcp fake", ":mcp toggle fake", ":mcp on"] {
            assert_eq!(Command::parse(line), mcp, "{line}");
        }

        let commands =
            ":new, :resume <session>, :model <name>, :mcp <on|off> <server>, :quit, !<command>";
        for name in [":help", ":", ":)"] {
            let unknown = format!("command: {name}: unknown (commands: {commands})");
            assert_eq!(Command::parse(name), usage(&unknown), "{name}");
        }
        let goal = format!("command: :goal: unknown (commands: {commands})");
        let parsed = Command::parse(":goal fix the café parser");
        assert_eq!(parsed, usage(&goal));
    }

    #[test]
    fn hints_match_prefix() {
        let names = |input| {
            let found = hints(input);
            found.iter().map(|hint| hint.name()).collect::<Vec<_>>()
        };

        let all = [":new", ":resume", ":model", ":mcp", ":quit"];
        assert_eq!(names(":"), all);
        assert_eq!(names(":res"), [":resume"]);
        assert_eq!(names(":m"), [":model", ":mcp"]);
        assert!(names(":resume abc").is_empty() && names("new").is_empty());

        for hint in &HINTS {
            let parsed = Command::parse(hint.name());
            assert!(!matches!(parsed, Some(Command::Invalid(_))), "{parsed:?}");
        }

        for prefix in [RESUME_PREFIX, MODEL_PREFIX, MCP_PREFIX] {
            let named = HINTS
                .iter()
                .any(|hint| format!("{} ", hint.name()) == prefix);
            assert!(named, "{prefix}");
        }
    }
}
