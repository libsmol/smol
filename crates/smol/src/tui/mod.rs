use crate::chat::Session;
use crate::tui::app::{pasted, App};
use crate::tui::screen::Screen;
use anyhow::Result;
use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::style::{Modifier, Style};
use std::env;
use std::io::stdout;
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

mod app;
mod browser;
mod diagram;
mod highlight;
mod markdown;
mod render;
mod screen;
mod select;
mod theme;
mod transcript;
mod ui;

const POLL_EVERY: Duration = Duration::from_millis(30);
/// Keys buffered after this long without reading events may have been typed during a stall
const STALL_WAIT: Duration = Duration::from_millis(200);
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const SPIN_EVERY: Duration = Duration::from_millis(80);

/// Terminals draw DIM differently and some ignore it entirely
pub(crate) const DIM: Style = Style::new().fg(theme::COMMENT);
pub(crate) const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
pub(crate) const ACCENT: Style = Style::new().fg(theme::PURPLE);
pub(crate) const STRONG: Style = BOLD.fg(theme::PINK);
pub(crate) const HEADING: Style = BOLD.fg(theme::PURPLE);
pub(crate) const RED: Style = Style::new().fg(theme::RED);
pub(crate) const BORDER: Style = Style::new().fg(theme::LINE);

pub(crate) type Spans = Vec<(String, Style)>;

pub(crate) fn spinner(since: Instant) -> char {
    let frames = since.elapsed().as_millis() / SPIN_EVERY.as_millis();
    SPINNER[frames as usize % SPINNER.len()]
}

/// Cuts with … or pads with spaces to exactly `width` columns
pub(crate) fn fitted(text: &str, width: usize) -> String {
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
        used += 1;
    }

    let gap = width.saturating_sub(used);
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

pub(crate) fn run(
    command: &[String],
    model: Option<String>,
    resume: Option<&str>,
    task: Option<String>,
) -> Result<()> {
    let session = Session::start(command, model, resume)?;
    let mut app = App::new(session);

    if resume.is_some() {
        app.show_session();
    }
    if let Some(task) = task.filter(|task| !task.is_empty()) {
        app.submit_text(&task);
    }

    // SSH drops COLORTERM, so only terminals known to lack truecolor get 256 colors
    let term = env::var("TERM").unwrap_or_default();
    let program = env::var("TERM_PROGRAM").unwrap_or_default();
    let truecolor = term != "linux" && program != "Apple_Terminal";

    let mut screen = Screen::enter()?;
    let mut listened = Instant::now();
    while !app.quit {
        if app.take_dirty() {
            execute!(stdout(), BeginSynchronizedUpdate)?;
            screen.terminal.draw(|frame| {
                ui::draw(frame, &mut app);
                if !truecolor {
                    theme::downgrade(frame.buffer_mut());
                }
            })?;
            execute!(stdout(), EndSynchronizedUpdate)?;
        }

        let stalled = listened.elapsed() > STALL_WAIT;
        let ready = event::poll(POLL_EVERY)?;
        listened = Instant::now();
        if ready {
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
                Some(text) => app.on_event(Event::Paste(text)),
                None => {
                    for event in events {
                        app.on_event(event);
                    }
                }
            }
        }
        app.pump();
    }

    drop(screen);
    if let Some(line) = app.session.parting() {
        println!("{line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::tests::fake;
    use crate::tui::transcript::Entry;
    use crate::tui::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    pub(crate) fn app() -> App {
        let agent = fake();
        let session = Session::start(&agent, None, None).expect("start");
        App::new(session)
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
            app.transcript.push(Entry::Info(text));
        }
    }

    #[test]
    fn kilo_units() {
        let cases = [
            0, 999, 1_000, 1_250, 1_300, 9_999, 45_442, 123_195, 999_500, 1_048_576,
        ];
        let shown = cases.map(kilo);
        let expected = [
            "0", "999", "1k", "1.25k", "1.3k", "10k", "45.4k", "123k", "1M", "1.05M",
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
