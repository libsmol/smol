use crate::tui::theme::{BG, FG};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{SetCursorStyle, Show};
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::style::Print;
use ratatui::crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use std::cell::Cell;
use std::io::{self, stdout, Stdout};
use std::panic::{self, AssertUnwindSafe, PanicHookInfo};

/// OSC 110 and 111 restore the terminal's own foreground and background
const OWN_COLORS: &str = "\x1b]110\x1b\\\x1b]111\x1b\\";

thread_local! {
    static CAUGHT: Cell<bool> = const { Cell::new(false) };
}

pub(crate) struct Screen {
    pub(crate) terminal: Terminal<CrosstermBackend<Stdout>>,
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

        Ok(Self { terminal })
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        restore();
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
        Print(OWN_COLORS),
        SetCursorStyle::DefaultUserShape,
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
    use crate::tui::screen::{caught, CAUGHT};

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
