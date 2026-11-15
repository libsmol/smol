use crate::tui::app::{pasted, Alert, App};
use crate::tui::theme::{self, BG, FG};
use crate::tui::{multiplexed, picture, ui, Host};
use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{SetCursorStyle, Show};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::style::Print;
use ratatui::crossterm::terminal::{
    self, BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use ratatui::Terminal;
use std::cell::Cell;
use std::env;
use std::io::{self, stdout, Stdout};
use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::time::{Duration, Instant};

/// OSC 110 and 111 restore the terminal's own foreground and background
const OWN_COLORS: &str = "\x1b]110\x1b\\\x1b]111\x1b\\";
/// Keys buffered after this long without reading events may have been typed during a stall
const STALL_WAIT: Duration = Duration::from_millis(200);

thread_local! {
    static CAUGHT: Cell<bool> = const { Cell::new(false) };
}

/// How a terminal tells the user of an alert
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Notifier {
    /// OSC 777, a desktop notification with a title and a body
    Titled,
    /// OSC 9, a desktop notification of one text
    Plain,
    Bell,
}

pub(crate) struct Screen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    truecolor: bool,
    notifier: Notifier,
    /// Taken to be true until the terminal reports losing focus
    pub(crate) focused: bool,
    /// When events were last read
    listened: Instant,
}

impl Screen {
    pub(crate) fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let default_hook = panic::take_hook();
        let hook = move |info: &PanicHookInfo| {
            if CAUGHT.get() {
                return;
            }
            restore();
            default_hook(info);
        };
        panic::set_hook(Box::new(hook));

        let setup = || -> io::Result<_> {
            // The margins around the window and cells not yet redrawn need this background too
            let colors = format!("\x1b]10;{FG}\x1b\\\x1b]11;{BG}\x1b\\");
            execute!(
                stdout(),
                EnterAlternateScreen,
                EnableMouseCapture,
                EnableBracketedPaste,
                EnableFocusChange,
                Print(colors),
                SetCursorStyle::SteadyBlock
            )?;
            if terminal::supports_keyboard_enhancement().unwrap_or(false) {
                let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES;
                execute!(stdout(), PushKeyboardEnhancementFlags(flags))?;
            }

            let out = stdout();
            let backend = CrosstermBackend::new(out);
            Terminal::new(backend)
        };
        let terminal = setup().inspect_err(|_| restore())?;

        // SSH drops COLORTERM, so only terminals known to lack truecolor get 256 colors
        let term = env::var("TERM").unwrap_or_default();
        let program = env::var("TERM_PROGRAM").unwrap_or_default();
        let truecolor = term != "linux" && program != "Apple_Terminal";
        let notifier = match multiplexed() {
            true => Notifier::Bell,
            false => notifier(&term, &program),
        };

        Ok(Self {
            terminal,
            truecolor,
            notifier,
            focused: true,
            listened: Instant::now(),
        })
    }

    pub(crate) fn alert(&self, alert: &Alert) -> io::Result<()> {
        let sent = alerted(alert, self.notifier);
        execute!(stdout(), Print(sent))
    }

    /// For a host that draws cells it laid out elsewhere
    pub(crate) fn backend(&mut self) -> &mut CrosstermBackend<Stdout> {
        self.terminal.backend_mut()
    }

    pub(crate) fn truecolor(&self) -> bool {
        self.truecolor
    }
}

impl Host for Screen {
    fn show(&mut self, app: &mut App) -> Result<()> {
        let alert = app.alert.take();
        if let Some(alert) = alert.filter(|_| !self.focused) {
            self.alert(&alert)?;
        }

        // Images drawn last frame go out now, and their placeholders show them once they arrive
        picture::send(&mut stdout(), &app.shown)?;
        if !app.take_dirty() {
            return Ok(());
        }

        execute!(stdout(), BeginSynchronizedUpdate)?;
        self.terminal.draw(|frame| {
            ui::draw(frame, app);
            if !self.truecolor {
                for cell in &mut frame.buffer_mut().content {
                    theme::downgrade(cell);
                }
            }
        })?;
        execute!(stdout(), EndSynchronizedUpdate)?;
        Ok(())
    }

    fn listen(&mut self, app: &mut App, wait: Duration) -> Result<()> {
        let stalled = self.listened.elapsed() > STALL_WAIT;
        let events = read(wait, stalled)?;
        self.listened = Instant::now();

        for event in events {
            match event {
                Event::FocusGained => self.focused = true,
                Event::FocusLost => self.focused = false,
                _ => {}
            }
            app.on_event(event);
        }
        Ok(())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        restore();
    }
}

/// The terminal's events that arrive within `wait`, with a paste on Windows made one event
pub(crate) fn read(wait: Duration, stalled: bool) -> io::Result<Vec<Event>> {
    if !event::poll(wait)? {
        return Ok(Vec::new());
    }
    let first = event::read()?;
    let mut events = vec![first];
    while event::poll(Duration::ZERO)? {
        let event = event::read()?;
        events.push(event);
    }

    // Other platforms report pastes as their own event,
    // and keys buffered during a stall are not a paste
    let burst = match cfg!(windows) {
        true => pasted(&events, stalled),
        false => None,
    };
    match burst {
        Some(text) => Ok(vec![Event::Paste(text)]),
        None => Ok(events),
    }
}

/// Terminals known to show desktop notifications, which others ignore
fn notifier(term: &str, program: &str) -> Notifier {
    let own = matches!(term, "xterm-kitty" | "xterm-ghostty" | "wezterm");
    let named = matches!(program, "ghostty" | "WezTerm");
    if own || named {
        return Notifier::Titled;
    }
    match program {
        "iTerm.app" => Notifier::Plain,
        _ => Notifier::Bell,
    }
}

/// The notification for `alert`, whose text may hold anything a title or an answer can
fn alerted(alert: &Alert, notifier: Notifier) -> String {
    // Control characters could end the sequence early, and OSC 777 splits its fields at semicolons
    let clean = |text: &str| {
        let mut cleaned = String::new();
        for c in text.chars() {
            match c {
                ';' => cleaned.push(','),
                c if c.is_control() => {}
                c => cleaned.push(c),
            }
        }
        cleaned
    };
    let title = match alert.title.is_empty() {
        true => "smol".to_owned(),
        false => clean(&alert.title),
    };
    let body = clean(&alert.body);

    match notifier {
        Notifier::Titled => format!("\x1b]777;notify;{title};{body}\x1b\\"),
        Notifier::Plain => format!("\x1b]9;{title} · {body}\x1b\\"),
        Notifier::Bell => "\x07".to_owned(),
    }
}

/// The hook normally restores the terminal first, which a caught panic must skip
pub(crate) fn caught<T>(run: impl FnOnce() -> T) -> Option<T> {
    CAUGHT.set(true);
    let result = panic::catch_unwind(AssertUnwindSafe(run));
    CAUGHT.set(false);
    result.ok()
}

fn restore() {
    // This always fails on Windows and would block the commands chained after it
    // Keyboard modes are kept per screen and must be popped before leaving the alternate screen
    execute!(stdout(), PopKeyboardEnhancementFlags).ok();
    execute!(
        stdout(),
        // A frame that failed halfway leaves the terminal holding back what follows
        EndSynchronizedUpdate,
        Print(OWN_COLORS),
        SetCursorStyle::DefaultUserShape,
        DisableFocusChange,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        Show
    )
    .ok();
    terminal::disable_raw_mode().ok();
}

#[cfg(test)]
mod tests {
    use crate::tui::app::Alert;
    use crate::tui::screen::{alerted, caught, notifier, Notifier, CAUGHT};

    #[test]
    fn alerts_escaped() {
        let alert = Alert {
            title: "Café; 🐍\x1b]0;owned\x07 menu\u{9c}".to_owned(),
            body: "Fixed the naïve parser; 3 tests pass".to_owned(),
        };
        let titled =
            "\x1b]777;notify;Café, 🐍]0,owned menu;Fixed the naïve parser, 3 tests pass\x1b\\";
        assert_eq!(alerted(&alert, Notifier::Titled), titled);
        let plain = "\x1b]9;Café, 🐍]0,owned menu · Fixed the naïve parser, 3 tests pass\x1b\\";
        assert_eq!(alerted(&alert, Notifier::Plain), plain);
        assert_eq!(alerted(&alert, Notifier::Bell), "\x07");

        let untitled = Alert {
            title: String::new(),
            body: "Done".to_owned(),
        };
        let titled = "\x1b]777;notify;smol;Done\x1b\\";
        assert_eq!(alerted(&untitled, Notifier::Titled), titled);
    }

    #[test]
    fn notifier_detected() {
        let cases = [
            ("xterm-ghostty", "ghostty", Notifier::Titled),
            ("xterm-256color", "ghostty", Notifier::Titled),
            ("xterm-kitty", "", Notifier::Titled),
            ("xterm-256color", "iTerm.app", Notifier::Plain),
            ("wezterm", "WezTerm", Notifier::Titled),
            ("tmux-256color", "tmux", Notifier::Bell),
            ("xterm-256color", "Apple_Terminal", Notifier::Bell),
            ("alacritty", "", Notifier::Bell),
        ];
        for (term, program, expected) in cases {
            let found = notifier(term, program);
            assert_eq!(found, expected, "{term} {program}");
        }
    }

    #[test]
    fn panic_caught() {
        assert_eq!(caught(|| "drawn"), Some("drawn"));
        assert_eq!(caught(|| CAUGHT.get()), Some(true));
        assert!(!CAUGHT.get());
        let failed: Option<()> = caught(|| panic!("layout failed"));
        assert_eq!(failed, None);
        assert!(!CAUGHT.get());
        assert_eq!(caught(|| 1 + 1), Some(2));
    }
}
