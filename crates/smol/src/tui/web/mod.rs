use crate::chat::Session;
use crate::conversation::Entry;
use crate::tui::app::App;
use crate::tui::desktop::{Desktop, Local};
use crate::tui::render::Target;
use crate::tui::screen::{self, Screen};
use crate::tui::web::grid::{encode, Changes, Grid};
use crate::tui::web::input::Input;
use crate::tui::web::serve::{Served, LEFT};
use crate::tui::{diagram, drive, leave, picture, theme, ui, Ending, Host, POLL_EVERY};
use anyhow::{bail, Context, Result};
use ratatui::backend::{Backend, ClearType};
use ratatui::buffer::{self, CellWidth};
use ratatui::crossterm::event::{Event, MouseEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{self, BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::layout::{Position, Rect, Size};
use ratatui::Terminal;
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
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
const MAX_IMAGES: usize = 64;
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
    /// With the browser's outbox, and the closing code its socket sends once the outbox goes
    Joined(u64, SyncSender<String>, Arc<AtomicU16>),
    Left(u64),
    Input(Who, Input),
}

struct Browser {
    size: Option<Size>,
    /// Whether the user looks at its page, as the page last said
    focused: bool,
    outbox: SyncSender<String>,
    parting: Arc<AtomicU16>,
    /// Missed frames, so the next one it gets is the whole screen
    behind: bool,
}

/// The terminal `smol web` runs in, which shows the shared screen cut to its own size
struct Mirror {
    screen: Screen,
    size: Size,
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
    served: Arc<Mutex<Served>>,
    arrivals: Receiver<Arrival>,
    /// The whole screen as a frame for browsers that fell behind, until the next draw
    whole: Option<(String, Vec<u32>)>,
}

impl Desktop for Relay {
    fn copy(&mut self, text: &str) {
        let Who::Browser(id) = self.asker.get() else {
            return Local.copy(text);
        };
        let message = json!({ "t": "copy", "text": text });
        self.told.borrow_mut().push((id, message.to_string()));
    }

    fn open(&mut self, target: &Target) -> Result<(), String> {
        let Who::Browser(id) = self.asker.get() else {
            return Local.open(target);
        };

        let url = match target {
            Target::Url(url) => url.clone(),
            Target::Diagram(code) => {
                let html = diagram::page(code)?;
                let mut served = self.served.lock().expect("served");
                let number = served.open(html.into_bytes(), "text/html; charset=utf-8");
                format!("open/{number}")
            }
            Target::Picture(source) => {
                let (bytes, format) = picture::original(source)?;
                let mut served = self.served.lock().expect("served");
                let number = served.open(bytes, format.to_mime_type());
                format!("open/{number}")
            }
        };
        let message = json!({ "t": "open", "url": url });
        self.told.borrow_mut().push((id, message.to_string()));
        Ok(())
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
        let browsed = self.browsers.values().any(|browser| browser.focused);
        let looked = browsed || mirror.is_some_and(|mirror| mirror.screen.focused);
        let alert = app.alert.take();
        if let Some(text) = alert.filter(|_| !looked) {
            if let Some(mirror) = mirror {
                mirror.screen.alert(&text)?;
            }
            let message = json!({ "t": "alert", "text": text }).to_string();
            for browser in self.browsers.values() {
                browser.outbox.try_send(message.clone()).ok();
            }
        }

        if mirror.is_some_and(|mirror| mirror.kitty) {
            picture::send(&mut stdout(), &app.shown)?;
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

        let mut served = self.served.lock().expect("served");
        for (_, ids) in diff.iter().chain(&self.whole) {
            for &id in ids {
                if served.images.contains_key(&id) {
                    continue;
                }
                if let Some(picture) = picture::image(id) {
                    served.images.insert(id, picture);
                }
            }
        }
        // Browsers keep the images they fetched, and may still fetch any on the screen
        if served.images.len() > MAX_IMAGES {
            let cells = grid.screen().content.iter();
            let shown: Vec<u32> = cells
                .filter_map(picture::tile)
                .map(|tile| tile.image)
                .collect();
            served.images.retain(|id, _| shown.contains(id));
        }
        drop(served);

        for browser in self.browsers.values_mut() {
            let frame = match (browser.behind, &self.whole, &diff) {
                (true, Some((frame, _)), _) => frame,
                (false, _, Some((frame, _))) => frame,
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
                Arrival::Joined(id, outbox, parting) => {
                    let browser = Browser {
                        size: None,
                        focused: false,
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
                Arrival::Input(who, Input::Size(width, height)) => {
                    let size = Size::new(width, height);
                    match who {
                        Who::Here => {
                            if let Some(mirror) = &mut self.mirror {
                                mirror.size = size;
                                mirror.stale = true;
                            }
                        }
                        Who::Browser(id) => {
                            if let Some(browser) = self.browsers.get_mut(&id) {
                                browser.size = Some(size);
                            }
                        }
                    }
                    self.latest = Some(who);
                    self.fit(app);
                }
                Arrival::Input(who, Input::Focus(focused)) => match who {
                    Who::Here => {
                        if let Some(mirror) = &mut self.mirror {
                            mirror.screen.focused = focused;
                        }
                    }
                    Who::Browser(id) => {
                        if let Some(browser) = self.browsers.get_mut(&id) {
                            browser.focused = focused;
                        }
                    }
                },
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
    /// Closes a browser that quit, which then hears it left
    fn let_go(&mut self, app: &mut App, id: u64) {
        if let Some(browser) = self.browsers.remove(&id) {
            browser.parting.store(LEFT, Ordering::Relaxed);
        }
        self.left(app, id);
    }

    /// Makes the screen the size of the device that acted last
    fn fit(&mut self, app: &mut App) {
        let size = match self.latest {
            Some(Who::Here) => self.mirror.as_ref().map(|mirror| mirror.size),
            Some(Who::Browser(id)) => self.browsers.get(&id).and_then(|browser| browser.size),
            None => None,
        };
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

        let area = Rect::from((Position::ORIGIN, self.size));
        let mut cells = Vec::new();
        for &at in changes.cells.iter().filter(|at| area.contains(**at)) {
            let mut cell = grid.screen()[at].clone();
            // The terminal would wrap a wide symbol cut by its edge
            let end = at.x.saturating_add(cell.symbol().cell_width());
            let cut = end > area.right();
            if cut || (!self.kitty && picture::tile(&cell).is_some()) {
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
    command: &[String],
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
    let bound = serve::listen(address, token.clone(), Arc::clone(&served), arrive.clone());
    let bound = bound.with_context(|| format!("web: {address}"))?;

    let kitty = picture::kitty_here();
    // Browsers draw every picture from its Kitty placeholders, whatever this terminal does
    picture::set_kitty(true);
    let session = Session::start(command, model, resume, true)?;

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

    let asker = Rc::new(Cell::new(Who::Here));
    let told = Rc::default();
    let relay = Relay {
        asker: Rc::clone(&asker),
        told: Rc::clone(&told),
        served: Arc::clone(&served),
    };
    let mut app = App::new(session, Box::new(relay));
    if resume.is_some() {
        app.show_session();
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
                                sized = Some(Input::Size(width, height))
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

            Some(Mirror {
                screen,
                size: Size::new(columns, rows),
                kitty,
                stale: true,
                stop,
            })
        }
        false => None,
    };
    drop(arrive);

    let size = mirror.as_ref().map_or(FIRST_SIZE, |mirror| mirror.size);
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
        served,
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

    // A Kitty terminal keeps the images it was sent after smol exits unless they are deleted
    picture::clear();
    if web.mirror.as_ref().is_some_and(|mirror| mirror.kitty) {
        picture::send(&mut stdout(), &[]).ok();
    }
    drop(web);
    leave(app, &ending);
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::chat::Session;
    use crate::tests::fake;
    use crate::tui::app::App;
    use crate::tui::web::grid::Grid;
    use crate::tui::web::input::Input;
    use crate::tui::web::serve::LEFT;
    use crate::tui::web::{fixed_token, Arrival, Relay, Web, Who, FIRST_SIZE};
    use crate::tui::Host;
    use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use serde_json::Value;
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
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
            served: Arc::clone(&served),
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
            served,
            arrivals,
            whole: None,
        };
        (app, web, arrive)
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

        let joined = Arrival::Joined(1, first, Arc::clone(&parting));
        arrive.send(joined).expect("join");
        arrive
            .send(Arrival::Input(one, Input::Size(60, 20)))
            .expect("size");
        step(&mut app, &mut web);
        let (size, screen) = seen(&to_first);
        assert_eq!(size, (60, 20));
        assert!(screen.contains("Ask anything"), "{screen}");

        // The one who joins later gets the whole screen, now at its size, and so does the first
        let joined = Arrival::Joined(2, second, Arc::default());
        arrive.send(joined).expect("join");
        arrive
            .send(Arrival::Input(two, Input::Size(90, 30)))
            .expect("size");
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
        let sized = Input::Size(90, 30);
        arrive.send(Arrival::Input(two, sized)).expect("size");
        step(&mut app, &mut web);
        assert!(!app.quit);
        assert_eq!(parting.load(Ordering::Relaxed), LEFT);
        while to_first.try_recv().is_ok() {}
        assert_eq!(to_first.try_recv(), Err(TryRecvError::Disconnected));
        assert!(!web.browsers.contains_key(&1) && web.browsers.contains_key(&2));
    }

    #[test]
    fn alert_unless_looked() {
        let (mut app, mut web, arrive) = hosted();
        let (outbox, sent) = mpsc::sync_channel(64);
        let joined = Arrival::Joined(1, outbox, Arc::default());
        arrive.send(joined).expect("join");
        let alerts = |sent: &Receiver<String>| {
            let mut alerts = Vec::new();
            for text in sent.try_iter() {
                let message: Value = serde_json::from_str(&text).expect("json");
                if message["t"] == "alert" {
                    alerts.push(message["text"].clone());
                }
            }
            alerts
        };

        let looking = Arrival::Input(Who::Browser(1), Input::Focus(true));
        arrive.send(looking).expect("focus");
        app.alert = Some("Café menu · done".to_owned());
        step(&mut app, &mut web);
        assert!(alerts(&sent).is_empty());
        assert_eq!(app.alert, None);

        let away = Arrival::Input(Who::Browser(1), Input::Focus(false));
        arrive.send(away).expect("blur");
        app.alert = Some("Café menu · done".to_owned());
        step(&mut app, &mut web);
        assert_eq!(alerts(&sent), ["Café menu · done"]);
    }
}
