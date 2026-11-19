use crate::conversation::Entry;
use crate::tui::app::App;
use crate::tui::clipboard::{self, UNREACHABLE};
use crate::tui::desktop::{opened, Desktop, Local, Opened};
use crate::tui::picture::{self, Pictures};
use crate::tui::render::Target;
use crate::tui::screen::{self, Screen};
use crate::tui::web::grid::{encode, Changes, Grid};
use crate::tui::web::input::Input;
use crate::tui::web::serve::{Served, LEFT};
use crate::tui::{drive, finish, start, theme, ui, Ending, Host, POLL_EVERY};
use anyhow::{bail, Context, Result};
use ratatui::backend::{Backend, ClearType};
use ratatui::buffer::{self, CellWidth};
use ratatui::crossterm::event::{Event, MouseEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{self, BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::layout::{Position, Rect, Size};
use ratatui::Terminal;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::{self, stdout};
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

mod grid;
mod input;
mod serve;

/// The screen's size until a device says its own
const FIRST_SIZE: Size = Size::new(80, 24);
/// How long the browsers get to hear that smol web ended
const PARTING_WAIT: Duration = Duration::from_millis(500);
/// Each guess at a token takes a request, which nothing limits
const MIN_TOKEN_CHARS: usize = 16;

/// Where input came from, which also decides where what it copies or opens goes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Who {
    /// The terminal `smol web` runs in
    Here,
    Browser(u64),
}

pub(crate) enum Arrival {
    Joined {
        id: u64,
        outbox: SyncSender<String>,
        /// The closing code its socket sends once the outbox goes
        parting: Arc<AtomicU16>,
    },
    Left(u64),
    Input(Who, Input),
}

/// A screen the user looks at the TUI on
struct Device {
    /// Unknown for a browser until its page says it
    size: Option<Size>,
    /// Whether the user looks at it, as it last said
    focused: bool,
}

struct Browser {
    device: Device,
    outbox: SyncSender<String>,
    parting: Arc<AtomicU16>,
    /// Missed frames, so the next one it gets is the whole screen
    behind: bool,
}

/// The terminal `smol web` runs in, which shows the shared screen cut to its own size
struct Mirror {
    screen: Screen,
    device: Device,
    /// The picture store's Kitty images are for browsers, which this terminal may not draw
    kitty: bool,
    /// Drawn whole on the next show, as at the start and after its own size changed
    stale: bool,
    stop: Arc<AtomicBool>,
}

/// Sends what the user copies or opens to the device whose input asked for it
struct Relay {
    asker: Rc<Cell<Who>>,
    told: Rc<RefCell<Vec<(u64, String)>>>,
    served: Arc<Mutex<Served>>,
}

struct Web {
    terminal: Terminal<Grid>,
    browsers: HashMap<u64, Browser>,
    mirror: Option<Mirror>,
    /// The device that acted last, whose size the screen takes
    latest: Option<Who>,
    asker: Rc<Cell<Who>>,
    told: Rc<RefCell<Vec<(u64, String)>>>,
    arrivals: Receiver<Arrival>,
    /// The whole screen as a frame for browsers that fell behind, until the next draw
    whole: Option<String>,
}

impl Relay {
    fn tell(&self, id: u64, message: Value) {
        self.told.borrow_mut().push((id, message.to_string()));
    }
}

impl Desktop for Relay {
    fn selecting(&mut self, text: &str) {
        if let Who::Browser(id) = self.asker.get() {
            let message = json!({ "t": "selection", "text": text });
            self.tell(id, message);
        }
    }

    fn copy(&mut self, text: &str) {
        let Who::Browser(id) = self.asker.get() else {
            return Local.copy(text);
        };
        let message = json!({ "t": "copy", "text": text });
        self.tell(id, message);
    }

    fn open(&mut self, target: &Target) -> Result<(), String> {
        let Who::Browser(id) = self.asker.get() else {
            return Local.open(target);
        };

        let url = match opened(target)? {
            Opened::Url(url) => url,
            Opened::File { mime, bytes, .. } => {
                let mut served = self.served.lock().expect("served");
                let number = served.open(bytes, mime);
                format!("open/{number}")
            }
        };
        let message = json!({ "t": "open", "url": url });
        self.tell(id, message);
        Ok(())
    }

    /// The terminal can't be asked for its clipboard while a thread reads its input, and a page
    /// sends its images as they are pasted
    fn image(&mut self) -> Result<Vec<u8>, String> {
        let read = match self.asker.get() {
            Who::Here => clipboard::system(),
            Who::Browser(_) => None,
        };
        read.unwrap_or_else(|| Err(UNREACHABLE.to_owned()))
    }
}

impl Host for Web {
    /// Quitting in a browser only lets it go, and a `:quit` queued during a turn, which runs after
    /// it, counts as the device's that acted last
    fn quits(&mut self, app: &mut App) -> bool {
        let Who::Browser(id) = self.asker.get() else {
            return true;
        };
        app.quit = false;
        self.let_go(app, id);
        false
    }

    fn show(&mut self, app: &mut App) -> Result<()> {
        let mirror = self.mirror.as_ref();
        let browsed = self.browsers.values().any(|browser| browser.device.focused);
        let looked = browsed || mirror.is_some_and(|mirror| mirror.device.focused);
        let alert = app.alert.take();
        if let Some(alert) = alert.filter(|_| !looked) {
            if let Some(mirror) = mirror {
                mirror.screen.alert(&alert)?;
            }
            let message = json!({ "t": "alert", "title": alert.title, "body": alert.body });
            let message = message.to_string();
            for browser in self.browsers.values() {
                browser.outbox.try_send(message.clone()).ok();
            }
        }

        if mirror.is_some_and(|mirror| mirror.kitty) {
            picture::send(&mut stdout(), &app.pictures)?;
        }

        // A busy app draws on every turn of the loop, mostly the same screen again
        let mut drawn = None;
        if app.take_dirty() {
            self.terminal.draw(|frame| ui::draw(frame, app))?;
            let changes = self.terminal.backend_mut().take();
            drawn = Some(changes).filter(|changes| !changes.is_empty());
        }

        let grid = self.terminal.backend();
        if let Some(mirror) = &mut self.mirror {
            let whole = mirror.stale.then(|| grid.whole());
            mirror.stale = false;
            if let Some(changes) = whole.as_ref().or(drawn.as_ref()) {
                mirror.show(grid, changes)?;
            }
        }

        if drawn.is_some() {
            self.whole = None;
        }
        if self.browsers.is_empty() {
            return Ok(());
        }
        let input = app.input_area;
        let encoded = |changes: &Changes| encode(grid.screen(), changes, grid.cursor(), input);
        let diff = drawn.as_ref().map(encoded);
        let behind = self.browsers.values().any(|browser| browser.behind);
        if behind && self.whole.is_none() {
            self.whole = Some(encoded(&grid.whole()));
        }

        for browser in self.browsers.values_mut() {
            let frame = match (browser.behind, &self.whole, &diff) {
                (true, Some(frame), _) => frame,
                (false, _, Some(frame)) => frame,
                _ => continue,
            };
            let sent = browser.outbox.try_send(frame.clone());
            browser.behind = sent.is_err();
        }
        Ok(())
    }

    fn listen(&mut self, app: &mut App, wait: Duration) -> Result<()> {
        let first = match self.arrivals.recv_timeout(wait) {
            Ok(arrival) => arrival,
            Err(RecvTimeoutError::Timeout) => return Ok(()),
            Err(RecvTimeoutError::Disconnected) => bail!("web: the server stopped"),
        };
        let mut arrivals = vec![first];
        arrivals.extend(self.arrivals.try_iter());

        for arrival in arrivals {
            match arrival {
                Arrival::Joined {
                    id,
                    outbox,
                    parting,
                } => {
                    let device = Device {
                        size: None,
                        focused: false,
                    };
                    let browser = Browser {
                        device,
                        outbox,
                        parting,
                        behind: true,
                    };
                    self.browsers.insert(id, browser);
                }
                Arrival::Left(id) => {
                    self.browsers.remove(&id);
                    self.left(app, id);
                }
                // A browser let go after it quit may still have input on the way
                Arrival::Input(Who::Browser(id), _) if !self.browsers.contains_key(&id) => {}
                Arrival::Input(who, Input::Size(size)) => {
                    if let Some(device) = self.device(who) {
                        device.size = Some(size);
                    }
                    if let (Who::Here, Some(mirror)) = (who, &mut self.mirror) {
                        mirror.stale = true;
                    }
                    self.latest = Some(who);
                    self.fit(app);
                }
                Arrival::Input(who, Input::Focus(focused)) => {
                    if let Some(device) = self.device(who) {
                        device.focused = focused;
                    }
                }
                Arrival::Input(who, Input::Image(data)) => {
                    self.asker.set(who);
                    app.paste_image(Some(data));
                    self.latest = Some(who);
                    self.fit(app);
                }
                Arrival::Input(who, Input::Events(events)) => {
                    // Clicks and drags fall on the screen as this device last saw it,
                    // so the screen takes its size only after them
                    self.asker.set(who);
                    for event in events {
                        app.on_event(event);
                    }
                    // Taken now, as input from the next device would make the quit look like its
                    if app.quit {
                        if let Who::Browser(id) = who {
                            app.quit = false;
                            self.let_go(app, id);
                            continue;
                        }
                    }
                    self.latest = Some(who);
                    self.fit(app);
                }
            }
        }

        let told: Vec<(u64, String)> = self.told.borrow_mut().drain(..).collect();
        for (id, message) in told {
            if let Some(browser) = self.browsers.get(&id) {
                browser.outbox.try_send(message).ok();
            }
        }
        Ok(())
    }
}

impl Web {
    fn device(&mut self, who: Who) -> Option<&mut Device> {
        match who {
            Who::Here => self.mirror.as_mut().map(|mirror| &mut mirror.device),
            Who::Browser(id) => {
                let browser = self.browsers.get_mut(&id);
                browser.map(|browser| &mut browser.device)
            }
        }
    }

    /// Closes a browser that quit, which then hears it left
    fn let_go(&mut self, app: &mut App, id: u64) {
        if let Some(browser) = self.browsers.remove(&id) {
            browser.parting.store(LEFT, Ordering::Relaxed);
        }
        self.left(app, id);
    }

    /// Makes the screen the size of the device that acted last
    fn fit(&mut self, app: &mut App) {
        let Some(latest) = self.latest else {
            return;
        };
        let size = self.device(latest).and_then(|device| device.size);
        let Some(size) = size else {
            return;
        };

        let grid = self.terminal.backend_mut();
        if grid.screen().area.as_size() == size {
            return;
        }
        grid.resize(size);
        app.on_event(Event::Resize(size.width, size.height));
    }

    /// The screen goes back to the terminal smol web runs in when the device it fit leaves
    fn left(&mut self, app: &mut App, id: u64) {
        if self.latest != Some(Who::Browser(id)) {
            return;
        }
        self.latest = self.mirror.as_ref().map(|_| Who::Here);
        self.fit(app);
    }
}

impl Mirror {
    fn show(&mut self, grid: &Grid, changes: &Changes) -> io::Result<()> {
        let truecolor = self.screen.truecolor();
        let backend = self.screen.backend();
        execute!(stdout(), BeginSynchronizedUpdate)?;
        if changes.cleared {
            backend.clear_region(ClearType::All)?;
        }

        let size = self.device.size.unwrap_or(FIRST_SIZE);
        let area = Rect::from((Position::ORIGIN, size));
        let mut cells = Vec::new();
        for &at in changes.cells.iter().filter(|at| area.contains(**at)) {
            let mut cell = grid.screen()[at].clone();
            // The terminal would wrap a wide symbol cut by its edge
            let end = at.x.saturating_add(cell.symbol().cell_width());
            let cut = end > area.right();
            if cut || (!self.kitty && picture::is_tile(&cell)) {
                let bg = cell.bg;
                cell = buffer::Cell::EMPTY;
                cell.bg = bg;
            }
            if !truecolor {
                theme::downgrade(&mut cell);
            }
            cells.push((at.x, at.y, cell));
        }
        let content = cells.iter().map(|(x, y, cell)| (*x, *y, cell));
        backend.draw(content)?;

        match grid.cursor().filter(|at| area.contains(*at)) {
            Some(at) => {
                backend.set_cursor_position(at)?;
                backend.show_cursor()?;
            }
            None => backend.hide_cursor()?,
        }
        Backend::flush(backend)?;
        execute!(stdout(), EndSynchronizedUpdate)?;
        Ok(())
    }
}

impl Drop for Mirror {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Checks a token from `--token`, which goes unchanged into an address and a cookie
pub(crate) fn fixed_token(token: &str) -> Result<String, String> {
    let plain = |c: char| c.is_ascii_alphanumeric() || "-._~".contains(c);
    if token.len() < MIN_TOKEN_CHARS || !token.chars().all(plain) {
        return Err(format!(
            "at least {MIN_TOKEN_CHARS} letters, digits or `-._~`"
        ));
    }
    Ok(token.to_owned())
}

pub(crate) fn run(
    command: &[OsString],
    model: Option<String>,
    resume: Option<&str>,
    address: SocketAddr,
    token: Option<String>,
    here: bool,
) -> Result<()> {
    let token = match token {
        Some(token) => token,
        None => {
            let mut bytes = [0; 16];
            getrandom::fill(&mut bytes).context("web: token")?;
            bytes.iter().map(|byte| format!("{byte:02x}")).collect()
        }
    };

    // A taken address fails before the agent starts
    let served = Arc::default();
    let (arrive, arrivals) = mpsc::channel();
    let pictures = Pictures::here();
    let bound = serve::listen(
        address,
        token.clone(),
        Arc::clone(&served),
        pictures,
        arrive.clone(),
    );
    let bound = bound.with_context(|| format!("web: {address}"))?;

    let asker = Rc::new(Cell::new(Who::Here));
    let told = Rc::default();
    let relay = Relay {
        asker: Rc::clone(&asker),
        told: Rc::clone(&told),
        served,
    };
    // Browsers draw every picture from its Kitty placeholders, whatever this terminal does
    let mut app = start(command, model, resume, true, Box::new(relay))?;

    let host = match bound.ip() {
        ip if ip.is_unspecified() => "localhost".to_owned(),
        ip if ip.is_ipv6() => format!("[{ip}]"),
        ip => ip.to_string(),
    };
    let url = format!("http://{host}:{}/?token={token}", bound.port());
    println!("smol web: {url}");
    if !bound.ip().is_loopback() {
        eprintln!("smol web: the token goes over plain HTTP, so reach this address only through TLS or a private network");
    }

    let shown = format!("web: {url}");
    app.session.conversation.push(Entry::Info(shown));

    let ending = Ending::catch();
    let mirror = match here {
        true => {
            let screen = Screen::enter()?;
            let (columns, rows) = terminal::size()?;
            let stop = Arc::new(AtomicBool::new(false));

            let stopped = Arc::clone(&stop);
            let arrive = arrive.clone();
            thread::spawn(move || {
                while !stopped.load(Ordering::Relaxed) {
                    let Ok(events) = screen::read(POLL_EVERY, false) else {
                        break;
                    };

                    // Only acting makes this terminal's size the screen's, which hovering and
                    // focusing are not
                    let mut sized = None;
                    let mut focused = None;
                    let mut acted = Vec::new();
                    for event in events {
                        match event {
                            Event::Resize(width, height) => {
                                let size = Size::new(width, height);
                                sized = Some(Input::Size(size));
                            }
                            Event::FocusGained => focused = Some(Input::Focus(true)),
                            Event::FocusLost => focused = Some(Input::Focus(false)),
                            Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved => {}
                            event => acted.push(event),
                        }
                    }
                    let acted = (!acted.is_empty()).then_some(Input::Events(acted));

                    for input in sized.into_iter().chain(focused).chain(acted) {
                        let arrival = Arrival::Input(Who::Here, input);
                        if arrive.send(arrival).is_err() {
                            return;
                        }
                    }
                }
            });

            // A terminal that keeps quiet about focus is taken to be looked at
            let device = Device {
                size: Some(Size::new(columns, rows)),
                focused: true,
            };
            Some(Mirror {
                screen,
                device,
                kitty: picture::kitty_here(),
                stale: true,
                stop,
            })
        }
        false => None,
    };
    drop(arrive);

    let size = mirror.as_ref().and_then(|mirror| mirror.device.size);
    let size = size.unwrap_or(FIRST_SIZE);
    let grid = Grid::new(size);
    let terminal = Terminal::new(grid)?;
    let latest = mirror.as_ref().map(|_| Who::Here);
    let mut web = Web {
        terminal,
        browsers: HashMap::new(),
        mirror,
        latest,
        asker,
        told,
        arrivals,
        whole: None,
    };
    drive(&mut app, &mut web, &ending)?;

    // A browser hears that smol web ended once its outbox goes, before smol is gone
    let mut leaving: HashSet<u64> = web.browsers.drain().map(|(id, _)| id).collect();
    let deadline = Instant::now() + PARTING_WAIT;
    while !leaving.is_empty() {
        let left = deadline.saturating_duration_since(Instant::now());
        match web.arrivals.recv_timeout(left) {
            Ok(Arrival::Left(id)) => {
                leaving.remove(&id);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    let kitty = web.mirror.as_ref().is_some_and(|mirror| mirror.kitty);
    finish(app, web, kitty, &ending);
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::chat::Session;
    use crate::tests::fake;
    use crate::tui::app::{Alert, App};
    use crate::tui::clipboard::UNREACHABLE;
    use crate::tui::tests::{fill, written};
    use crate::tui::web::grid::Grid;
    use crate::tui::web::input::Input;
    use crate::tui::web::serve::LEFT;
    use crate::tui::web::{fixed_token, Arrival, Relay, Web, Who, FIRST_SIZE};
    use crate::tui::Host;
    use ratatui::crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::layout::Size;
    use ratatui::Terminal;
    use serde_json::Value;
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::fs;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
    use std::sync::Arc;
    use std::time::Duration;

    /// A host with browsers as channels and no server
    fn hosted() -> (App, Web, Sender<Arrival>) {
        let agent = fake();
        let session = Session::start(&agent, None, None, true).expect("start");
        let asker = Rc::new(Cell::new(Who::Here));
        let told = Rc::default();
        let served = Arc::default();
        let relay = Relay {
            asker: Rc::clone(&asker),
            told: Rc::clone(&told),
            served,
        };
        let app = App::new(session, Box::new(relay));

        let (arrive, arrivals) = mpsc::channel();
        let grid = Grid::new(FIRST_SIZE);
        let web = Web {
            terminal: Terminal::new(grid).expect("terminal"),
            browsers: HashMap::new(),
            mirror: None,
            latest: None,
            asker,
            told,
            arrivals,
            whole: None,
        };
        (app, web, arrive)
    }

    fn joined(id: u64, outbox: SyncSender<String>, parting: Arc<AtomicU16>) -> Arrival {
        Arrival::Joined {
            id,
            outbox,
            parting,
        }
    }

    fn sized(who: Who, width: u16, height: u16) -> Arrival {
        let size = Size::new(width, height);
        Arrival::Input(who, Input::Size(size))
    }

    fn step(app: &mut App, web: &mut Web) {
        web.listen(app, Duration::from_millis(50)).expect("listen");
        web.show(app).expect("show");
    }

    /// The size of the last frame sent and the screen those frames draw
    fn seen(frames: &Receiver<String>) -> ((u64, u64), String) {
        let mut size = (0, 0);
        let mut rows: Vec<Vec<String>> = Vec::new();
        while let Ok(text) = frames.try_recv() {
            let frame: Value = serde_json::from_str(&text).expect("json");
            let (width, height) = (
                frame["w"].as_u64().expect("w"),
                frame["h"].as_u64().expect("h"),
            );
            if frame["clear"] == Value::Bool(true) || (width, height) != size {
                size = (width, height);
                rows = vec![vec![" ".to_owned(); width as usize]; height as usize];
            }
            for run in frame["runs"].as_array().expect("runs") {
                let (x, y) = (run[0].as_u64().expect("x"), run[1].as_u64().expect("y"));
                for (at, symbol) in (x as usize..).zip(run[3].as_array().expect("symbols")) {
                    rows[y as usize][at] = symbol.as_str().unwrap_or_default().to_owned();
                }
            }
        }
        let text: Vec<String> = rows.iter().map(|row| row.concat()).collect();
        (size, text.join("\n"))
    }

    #[test]
    fn tokens_checked() {
        assert!(fixed_token("0123456789abcdef").is_ok());
        assert!(fixed_token("my-smol_token.v2~").is_ok());
        assert!(fixed_token("0123456789abcde").is_err());
        assert!(fixed_token("café-0123456789abcdef").is_err());
        assert!(fixed_token("0123456789 abcdef").is_err());
        assert!(fixed_token("0123456789abcdef&next=1").is_err());
    }

    #[test]
    fn screen_shared() {
        let (mut app, mut web, arrive) = hosted();
        let (first, to_first) = mpsc::sync_channel(64);
        let (second, to_second) = mpsc::sync_channel(64);
        let parting = Arc::new(AtomicU16::new(0));
        let one = Who::Browser(1);
        let two = Who::Browser(2);

        arrive
            .send(joined(1, first, Arc::clone(&parting)))
            .expect("join");
        arrive.send(sized(one, 60, 20)).expect("size");
        step(&mut app, &mut web);
        let (size, screen) = seen(&to_first);
        assert_eq!(size, (60, 20));
        assert!(screen.contains("Ask anything"), "{screen}");

        // The one who joins later gets the whole screen, now at its size, and so does the first
        arrive
            .send(joined(2, second, Arc::default()))
            .expect("join");
        arrive.send(sized(two, 90, 30)).expect("size");
        step(&mut app, &mut web);
        assert_eq!(seen(&to_second).0, (90, 30));
        assert_eq!(seen(&to_first).0, (90, 30));

        let typed = Input::Events(vec![Event::Paste("crème brûlée".to_owned())]);
        arrive.send(Arrival::Input(one, typed)).expect("type");
        step(&mut app, &mut web);
        let (size, screen) = seen(&to_second);
        assert_eq!(size, (60, 20));
        assert!(screen.contains("crème brûlée"), "{screen}");

        // Quitting in a browser only lets that browser go, though another acts right after
        let control = |c| Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
        let quit = Input::Events(vec![control('c'), control('d')]);
        arrive.send(Arrival::Input(one, quit)).expect("quit");
        arrive.send(sized(two, 90, 30)).expect("size");
        step(&mut app, &mut web);
        assert!(!app.quit);
        assert_eq!(parting.load(Ordering::Relaxed), LEFT);
        while to_first.try_recv().is_ok() {}
        assert_eq!(to_first.try_recv(), Err(TryRecvError::Disconnected));
        assert!(!web.browsers.contains_key(&1) && web.browsers.contains_key(&2));
    }

    /// A browser's image goes in as its placeholder, while its Alt+V can't read the clipboard
    #[test]
    fn image_arrived() {
        let (mut app, mut web, arrive) = hosted();
        let (outbox, _sent) = mpsc::sync_channel(64);
        arrive
            .send(joined(1, outbox, Arc::default()))
            .expect("join");
        let path = written("browser", 2, 2);
        let png = fs::read(&path).expect("png");
        fs::remove_dir_all(path.parent().expect("dir")).ok();
        let pasted = Arrival::Input(Who::Browser(1), Input::Image(png));
        arrive.send(pasted).expect("image");
        step(&mut app, &mut web);
        assert_eq!(app.draft.text(), "[image #1]");

        let alt_v = Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT));
        let pressed = Arrival::Input(Who::Browser(1), Input::Events(vec![alt_v]));
        arrive.send(pressed).expect("key");
        step(&mut app, &mut web);
        let flash = app.flash.as_ref().map(|(text, _)| text.clone());
        assert_eq!(flash, Some(Err(UNREACHABLE.to_owned())));
    }

    /// A browser hears the text selected so far while it drags, for its release to copy, and the
    /// copy that follows repeats the last of it
    #[test]
    fn selection_sent() {
        let (mut app, mut web, arrive) = hosted();
        let (outbox, sent) = mpsc::sync_channel(64);
        arrive
            .send(joined(1, outbox, Arc::default()))
            .expect("join");
        arrive.send(sized(Who::Browser(1), 60, 20)).expect("size");
        fill(&mut app, 40);
        step(&mut app, &mut web);

        let mouse = |kind, column, row| {
            let modifiers = KeyModifiers::NONE;
            Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers,
            })
        };
        let left = MouseButton::Left;
        let dragged = vec![
            mouse(MouseEventKind::Down(left), 2, 3),
            mouse(MouseEventKind::Drag(left), 5, 5),
            mouse(MouseEventKind::Drag(left), 9, 6),
        ];
        let released = vec![mouse(MouseEventKind::Up(left), 9, 6)];
        for events in [dragged, released] {
            let input = Arrival::Input(Who::Browser(1), Input::Events(events));
            arrive.send(input).expect("mouse");
            step(&mut app, &mut web);
        }

        let mut selections = Vec::new();
        let mut copies = Vec::new();
        for text in sent.try_iter() {
            let message: Value = serde_json::from_str(&text).expect("json");
            match message["t"].as_str() {
                Some("selection") => selections.push(message["text"].clone()),
                Some("copy") => copies.push(message["text"].clone()),
                _ => {}
            }
        }
        assert_eq!(selections.len(), 2, "{selections:?}");
        assert!(selections[1]
            .as_str()
            .is_some_and(|text| text.contains("line")));
        assert_eq!(copies, selections[1..]);
    }

    #[test]
    fn alert_unless_looked() {
        let (mut app, mut web, arrive) = hosted();
        let (outbox, sent) = mpsc::sync_channel(64);
        arrive
            .send(joined(1, outbox, Arc::default()))
            .expect("join");
        let alerts = |sent: &Receiver<String>| {
            let mut alerts = Vec::new();
            for text in sent.try_iter() {
                let message: Value = serde_json::from_str(&text).expect("json");
                if message["t"] == "alert" {
                    alerts.push((message["title"].clone(), message["body"].clone()));
                }
            }
            alerts
        };
        let alert = Alert {
            title: "Café menu".to_owned(),
            body: "Fixed the naïve parser 🐍".to_owned(),
        };

        let looking = Arrival::Input(Who::Browser(1), Input::Focus(true));
        arrive.send(looking).expect("focus");
        app.alert = Some(alert.clone());
        step(&mut app, &mut web);
        assert!(alerts(&sent).is_empty());
        assert_eq!(app.alert, None);

        let away = Arrival::Input(Who::Browser(1), Input::Focus(false));
        arrive.send(away).expect("blur");
        app.alert = Some(alert);
        step(&mut app, &mut web);
        assert_eq!(
            alerts(&sent),
            [("Café menu".into(), "Fixed the naïve parser 🐍".into())]
        );
    }
}
