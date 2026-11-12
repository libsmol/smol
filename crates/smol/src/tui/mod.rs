use crate::chat::Session;
use crate::tui::app::App;
use crate::tui::desktop::Local;
use crate::tui::screen::Screen;
use anyhow::Result;
use ratatui::style::{Modifier, Style};
#[cfg(unix)]
use signal_hook::consts::{SIGINT, SIGTERM};
#[cfg(unix)]
use signal_hook::{flag, low_level};
use std::env;
use std::io::stdout;
#[cfg(unix)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

mod app;
mod desktop;
mod diagram;
mod highlight;
mod markdown;
mod picture;
mod render;
mod screen;
mod select;
mod theme;
mod transcript;
mod ui;
pub(crate) mod web;

const POLL_EVERY: Duration = Duration::from_millis(30);

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
pub(crate) const SPIN_EVERY: Duration = Duration::from_millis(80);

/// Terminals draw DIM differently and some ignore it entirely
pub(crate) const DIM: Style = Style::new().fg(theme::COMMENT);
pub(crate) const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
pub(crate) const ACCENT: Style = Style::new().fg(theme::PURPLE);
pub(crate) const STRONG: Style = BOLD.fg(theme::PINK);
pub(crate) const HEADING: Style = BOLD.fg(theme::PURPLE);
pub(crate) const RED: Style = Style::new().fg(theme::RED);
pub(crate) const YELLOW: Style = Style::new().fg(theme::YELLOW);
pub(crate) const BORDER: Style = Style::new().fg(theme::LINE);

pub(crate) type Spans = Vec<(String, Style)>;

pub(crate) fn spinner(since: Instant) -> char {
    let frames = since.elapsed().as_millis() / SPIN_EVERY.as_millis();
    SPINNER[frames as usize % SPINNER.len()]
}

/// Cuts with … to at most `width` columns
pub(crate) fn clipped(text: &str, width: usize) -> String {
    let long = text.width() > width;
    let room = match long {
        true => width.saturating_sub(1),
        false => width,
    };

    let mut kept = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let w = grapheme.width();
        if used + w > room {
            break;
        }
        kept.push_str(grapheme);
        used += w;
    }

    if long && width > 0 {
        kept.push('…');
    }
    kept
}

/// Cuts with … or pads with spaces to exactly `width` columns
pub(crate) fn fitted(text: &str, width: usize) -> String {
    let kept = clipped(text, width);
    let gap = width.saturating_sub(kept.width());
    let padding = " ".repeat(gap);
    kept + &padding
}

pub(crate) fn joined(spans: &[(String, Style)]) -> String {
    spans.iter().map(|(text, _)| text.as_str()).collect()
}

/// Tabs count as zero width when wrapping and are filtered out when drawing
pub(crate) fn widened(text: &str) -> String {
    text.replace('\t', "    ")
}

pub(crate) fn in_out(input: u64, output: u64) -> String {
    let input = kilo(input);
    let output = kilo(output);
    format!("⇡{input} ⇣{output}")
}

pub(crate) fn kilo(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }

    let mut value = n as f64 / 1000.0;
    let mut unit = "k";
    if value >= 999.5 {
        value /= 1000.0;
        unit = "M";
    }

    // Thresholds apply to the rounded value or a carry would add a digit
    let decimals = match value {
        value if value < 9.995 => 2,
        value if value < 99.95 => 1,
        _ => 0,
    };

    let value = format!("{value:.decimals$}");
    let value = match value.contains('.') {
        true => value.trim_end_matches('0').trim_end_matches('.'),
        false => &value,
    };
    format!("{value}{unit}")
}

/// Inside a multiplexer, which keeps graphics and notifications to itself unless set up to pass
/// them through
pub(crate) fn multiplexed() -> bool {
    let names = ["TMUX", "STY", "ZELLIJ"].map(env::var_os);
    names.iter().any(Option::is_some)
}

/// What shows the TUI and passes on its input
pub(crate) trait Host {
    /// Whether the app asking to quit ends it, where a host may instead let go of only the device that asked
    fn quits(&mut self, _app: &mut App) -> bool {
        true
    }
    /// Shows the app, once on every turn of the loop
    fn show(&mut self, app: &mut App) -> Result<()>;
    /// Hands the app the input that arrives within `wait`
    fn listen(&mut self, app: &mut App, wait: Duration) -> Result<()>;
}

/// Catches the signals that end smol, so the session closes and saves first, and a second one
/// ends smol at once
#[derive(Default)]
pub(crate) struct Ending {
    ended: Arc<AtomicBool>,
    #[cfg(unix)]
    signal: Arc<AtomicUsize>,
}

impl Ending {
    pub(crate) fn catch() -> Self {
        let ending = Self::default();
        // Registered before the flags, the shutdown ends smol only on a second signal
        #[cfg(unix)]
        for signal in [SIGINT, SIGTERM] {
            flag::register_conditional_shutdown(signal, 1, Arc::clone(&ending.ended)).ok();
            flag::register(signal, Arc::clone(&ending.ended)).ok();
            flag::register_usize(signal, Arc::clone(&ending.signal), signal as usize).ok();
        }
        ending
    }

    pub(crate) fn ended(&self) -> bool {
        self.ended.load(Ordering::Relaxed)
    }

    /// Ends smol by the signal that was caught, as that signal would have
    fn resend(&self) {
        #[cfg(unix)]
        if self.ended() {
            let signal = self.signal.load(Ordering::Relaxed);
            low_level::emulate_default_handler(signal as i32).ok();
        }
    }
}

pub(crate) fn drive(app: &mut App, host: &mut impl Host, ending: &Ending) -> Result<()> {
    while !ending.ended() {
        if app.quit && host.quits(app) {
            break;
        }
        host.show(app)?;

        let now = Instant::now();
        let wait = match app.scroll_at {
            Some(at) => at.saturating_duration_since(now).min(POLL_EVERY),
            None => POLL_EVERY,
        };
        host.listen(app, wait)?;

        app.pump();
    }
    Ok(())
}

pub(crate) fn run(
    command: &[String],
    model: Option<String>,
    resume: Option<&str>,
    task: Option<String>,
) -> Result<()> {
    let kitty = picture::kitty_here();
    picture::set_kitty(kitty);
    let session = Session::start(command, model, resume, kitty)?;
    let mut app = App::new(session, Box::new(Local));

    if resume.is_some() {
        app.show_session();
    }
    if let Some(task) = task.filter(|task| !task.is_empty()) {
        app.submit_text(&task);
    }

    let ending = Ending::catch();
    let mut screen = Screen::enter()?;
    drive(&mut app, &mut screen, &ending)?;

    // The terminal keeps the images it was sent after smol exits unless they are deleted,
    // as placeholders don't count as placements on screen
    picture::clear();
    picture::send(&mut stdout(), &[]).ok();
    drop(screen);
    leave(app, &ending);
    Ok(())
}

/// Says goodbye once the host gave the terminal back, then lets libsmol save before a caught
/// signal ends smol
pub(crate) fn leave(app: App, ending: &Ending) {
    if let Some(line) = app.session.parting() {
        println!("{line}");
    }
    drop(app);
    ending.resend();
}

#[cfg(test)]
mod tests {
    use crate::conversation::{Entry, Helper};
    use crate::tests::fake;
    use crate::tui::picture::{state, Load, Source};
    use crate::tui::*;
    use agent_client_protocol_schema::v1::ToolCallStatus;
    use image::{ImageBuffer, Rgba, RgbaImage};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use std::{env, fs, process, thread};

    pub(crate) fn app() -> App {
        let agent = fake();
        let session = Session::start(&agent, None, None, false).expect("start");
        App::new(session, Box::new(Local))
    }

    pub(crate) fn draw(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| ui::draw(frame, app)).expect("draw");
        terminal
    }

    pub(crate) fn fill(app: &mut App, lines: usize) {
        for line in 0..lines {
            let text = format!("line {line}");
            app.session.conversation.push(Entry::Info(text));
        }
    }

    pub(crate) fn helper(task: &str, status: ToolCallStatus, text: &str) -> Helper {
        Helper {
            id: "call_1.1".to_owned(),
            task: task.to_owned(),
            role: None,
            name: None,
            status,
            text: text.to_owned(),
            preview: String::new(),
            interrupted: false,
            started: None,
            hue: 0,
            tokens: None,
            totals: None,
        }
    }

    /// Pictures load on loader threads
    pub(crate) fn settled(source: &Source) -> Load {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let state = state(source);
            if !matches!(state, Load::Loading(_)) || Instant::now() > deadline {
                return state;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// A PNG `width` by `height` pixels, red above and blue below, in a folder of its own
    pub(crate) fn written(name: &str, width: u32, height: u32) -> PathBuf {
        let painted = |_: u32, y: u32| match y < height / 2 {
            true => Rgba([255, 0, 0, 255]),
            false => Rgba([0, 0, 255, 255]),
        };
        let pixels: RgbaImage = ImageBuffer::from_fn(width, height, painted);
        let dir = env::temp_dir().join(format!("smol-picture-{name}-{}", process::id()));
        fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("café.png");
        pixels.save(&path).expect("save");
        path
    }

    #[test]
    fn kilo_units() {
        let cases = [
            999, 1_000, 1_250, 1_300, 9_999, 45_442, 123_195, 999_500, 1_048_576,
        ];
        let shown = cases.map(kilo);
        let expected = [
            "999", "1k", "1.25k", "1.3k", "10k", "45.4k", "123k", "1M", "1.05M",
        ];
        assert_eq!(shown, expected, "{cases:?}");
    }

    #[test]
    fn fitted_width() {
        assert_eq!(fitted("ab", 4), "ab  ");
        assert_eq!(fitted("abcdef", 4), "abc…");
        assert_eq!(fitted("🍕🚀🎉", 5), "🍕🚀…");
        assert_eq!(fitted("🍕🚀🎉", 4), "🍕… ");
        assert_eq!(fitted("abc", 0), "");
        assert_eq!(fitted("⚠️🍕🚀🎉🐍", 5), "⚠️🍕…");
        assert_eq!(fitted("⚠️🍕", 5), "⚠️🍕 ");
    }
}
