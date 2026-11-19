use crate::client::tool_count;
use crate::conversation::Stats;
use crate::tui::app::App;
use crate::tui::picture;
use crate::tui::render::{wrap_ranges, Target};
use crate::tui::{
    clipped, fitted, kilo, spinner, theme, ACCENT, BOLD, BORDER, DIM, HEADING, RED, STRONG, YELLOW,
};
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, Padding, Paragraph, Wrap};
use ratatui::Frame;
use ratatui_textarea::TextArea;
use reaper::wire::{McpState, McpStatus};
use reaper::{one_line, tilde};
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const SIDEBAR_WIDTH: u16 = 32;
const MIN_WIDE_COLUMNS: u16 = 100;
const MAX_INPUT_ROWS: usize = 10;
const MAX_CHOICE_ROWS: usize = 10;
const SCROLL_EVERY: Duration = Duration::from_millis(60);
const SCROLL_WAIT: Duration = Duration::from_millis(1500);

const PINK: Style = Style::new().fg(theme::PINK);
const GREEN: Style = Style::new().fg(theme::GREEN);

const QUEUED: &str = " · queued";
const LOGO: [&str; 2] = ["▄▀▀ ▄▀▄▀▄ ▄▀▄ █ ", "▄▄▀ █ █ █ ▀▄▀ █▄"];

pub(crate) fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let base = Style::new().bg(theme::BG).fg(theme::FG);
    let ground = Block::new().style(base);
    frame.render_widget(ground, area);

    let input_height = input_rows(&app.draft.area, area.width) + 2;
    let heights = [
        Constraint::Fill(1),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ];
    let [body, input, status] = Layout::vertical(heights).areas(area);

    let narrow = area.width < MIN_WIDE_COLUMNS;
    let columns = [Constraint::Fill(1), Constraint::Length(SIDEBAR_WIDTH)];
    let [beside, sidebar] = Layout::horizontal(columns).areas(body);
    let conversation = if narrow { body } else { beside };

    let title = match app.session.title.as_str() {
        "" => "new session",
        title => title,
    };
    let talk = &app.session.conversation;
    let mut block = panel();
    let mut usage_width = 0;

    if narrow {
        let stats = talk.stats();
        let mut usage = format!(" {}", stats.tokens);
        if stats.context.is_some() && stats.window.is_some() {
            let mark = approx(stats);
            let share = match stats.ratio() {
                Some(ratio) => format!("{mark}{:.0}%", ratio * 100.0),
                None => "?%".to_owned(),
            };
            usage += &format!(" · context {share}");
        }

        let programs = app.session.programs.len();
        if programs > 0 {
            usage += &format!(" · programs {programs}");
        }

        // Startup failures are not reported in the conversation,
        // so on a narrow screen they only show here
        let failed = |server: &&McpStatus| matches!(server.state, McpState::Failed { .. });
        let failed = app.session.mcp.iter().filter(failed).count();
        if failed > 0 {
            usage += &format!(" · mcp {failed} failed");
        }
        let unauthorized =
            |server: &&McpStatus| matches!(server.state, McpState::Unauthorized { .. });
        let unauthorized = app.session.mcp.iter().filter(unauthorized).count();
        if unauthorized > 0 {
            usage += &format!(" · mcp {unauthorized} unauthorized");
        }
        usage.push(' ');

        usage_width = usage.width();
        let usage = Line::styled(usage, DIM).right_aligned();
        block = block.title(usage);
    } else {
        let side = panel();
        let inner = side.inner(sidebar);
        let width = usize::from(inner.width);

        let cwd = tilde(&app.session.client.cwd);
        let cwd = tail(&cwd, width);

        // The row count is fixed, so rows hold their place even before data arrives
        let stats = talk.stats();
        let turns = stats.turns.to_string();
        let steps = stats.steps.to_string();
        let turns = vec![Span::styled("turns ", DIM), Span::raw(turns)];
        let steps = vec![Span::styled("steps ", DIM), Span::raw(steps)];
        let counts = spread(turns, steps, width);

        let tokens = stats.tokens.to_string();
        let tokens = labeled("tokens", tokens, width);

        let ratio = stats.ratio();
        let mark = approx(stats);
        let context = match (stats.context, stats.window) {
            (Some(used), Some(window)) => {
                let used = kilo(used);
                let window = kilo(window);
                match ratio {
                    Some(ratio) => {
                        let share = ratio * 100.0;
                        format!("{mark}{used} / {window} · {share:.0}%")
                    }
                    None => format!("? / {window}"),
                }
            }
            (Some(used), None) => {
                let used = kilo(used);
                format!("{mark}{used}")
            }
            (None, _) => "–".to_owned(),
        };
        let context = labeled("context", context, width);
        let bar = gauge(ratio, width);

        let mut lines = vec![
            Line::styled(cwd, DIM),
            Line::default(),
            counts,
            tokens,
            context,
            bar,
        ];

        if !app.session.mcp.is_empty() {
            let section = Span::styled("MCP", HEADING);
            let section = Line::from(section);
            lines.extend([Line::default(), section]);
        }
        for server in &app.session.mcp {
            let (dot, state) = match &server.state {
                McpState::Off => (theme::COMMENT, Span::styled("off", DIM)),
                McpState::Connecting => (theme::ORANGE, Span::styled("connecting", DIM)),
                McpState::Connected { tools } => {
                    let tools = tool_count(*tools);
                    (theme::GREEN, Span::styled(tools, DIM))
                }
                McpState::Failed { .. } => (theme::RED, Span::styled("failed", RED)),
                McpState::Unauthorized { .. } => {
                    (theme::YELLOW, Span::styled("unauthorized", YELLOW))
                }
            };
            let dot = Style::new().fg(dot);
            let dot = Span::styled("● ", dot);
            let name = Span::raw(server.name.clone());
            let name = vec![dot, name];
            let line = spread(name, vec![state], width);
            lines.push(line);
        }

        let programs = &app.session.programs;
        if !programs.is_empty() {
            let section = vec![Span::styled("Programs", HEADING)];
            let count = vec![Span::styled(programs.len().to_string(), DIM)];
            let section = spread(section, count, width);
            lines.extend([Line::default(), section]);
        }

        let height = usize::from(inner.height);
        let free = height.saturating_sub(lines.len());
        let shown = match programs.len() > free {
            true => free.saturating_sub(1),
            false => programs.len(),
        };
        for (program, since) in &programs[..shown] {
            let pid = format!("{} ", program.pid);
            let seconds = since.elapsed().as_secs();
            let elapsed = match seconds {
                0..60 => format!("{seconds}s"),
                60..3600 => format!("{}m", seconds / 60),
                _ => format!("{}h", seconds / 3600),
            };
            // A command ends with what tells programs apart, such as the script it runs
            let room = width.saturating_sub(pid.width() + 1 + elapsed.width());
            let command = one_line(&program.command, &app.session.client.cwd);
            let command = tail(&command, room);
            let left = vec![Span::styled(pid, DIM), Span::raw(command)];
            let right = vec![Span::styled(elapsed, DIM)];
            let line = spread(left, right, width);
            lines.push(line);
        }

        let hidden = programs.len() - shown;
        if hidden > 0 {
            let more = format!("… +{hidden} more");
            lines.push(Line::styled(more, DIM));
        }

        frame.render_widget(side, sidebar);
        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }

    // Two corners, a space on each side of the title and at least one cell of border line
    let taken = usage_width + 5;
    let room = usize::from(conversation.width).saturating_sub(taken);
    let title = clipped(title, room);
    let title = format!(" {} ", title.trim_end());
    let title = Span::styled(title, HEADING);
    block = block.title(title);

    let conversation_inner = block.inner(conversation);
    // The rows on screen depend on how many these take, so they come from the previous frame's
    let pinned = app.transcript.pinned(
        talk,
        conversation_inner.width,
        usize::from(conversation_inner.height),
    );
    let height = u16::try_from(pinned.len()).unwrap_or(u16::MAX);
    let heights = [Constraint::Fill(1), Constraint::Length(height)];
    let [inner, pinned_area] = Layout::vertical(heights).areas(conversation_inner);
    app.messages_area = inner;
    app.pinned_area = pinned_area;

    let mut progress = Vec::new();
    if let Some(run) = &app.session.run {
        let started = run.step_started;
        let elapsed = started.elapsed();
        let tick = spinner(started);
        let spinner = format!("  {tick} ");

        let writing = talk.writing();
        let left = talk.retry_in();
        let seconds = elapsed.as_secs();
        let estimated = match talk.stats().estimated {
            Some(tokens) => {
                let tokens = kilo(tokens);
                format!(" · ≈⇣{tokens}")
            }
            None => String::new(),
        };

        let helping = talk.awaited();
        let label = match (run.interrupting, left, writing) {
            (true, _, _) => "interrupting…".to_owned(),
            (false, Some(left), _) => {
                let wait = left.as_secs_f64().ceil();
                format!("retrying in {wait}s")
            }
            (false, None, _) if talk.shelling() => format!("running · {seconds}s"),
            (false, None, _) if helping > 0 => {
                let agents = match helping {
                    1 => "sub-agent",
                    _ => "sub-agents",
                };
                format!("waiting for {helping} {agents} · {seconds}s")
            }
            (false, None, false) => format!("thinking · {seconds}s{estimated}"),
            (false, None, true) => format!("working · {seconds}s{estimated}"),
        };

        // Lines up with the blank row that starts `Render::step`
        if !talk.stepping() {
            progress.push(Line::default());
        }
        let italic = DIM.add_modifier(Modifier::ITALIC);
        let mut line = vec![Span::styled(spinner, ACCENT)];
        for (text, style) in theme::shimmer(&label, italic, theme::PURPLE, started) {
            line.push(Span::styled(text, style));
        }
        progress.push(Line::from(line));
    }

    let old_top = app.viewport.top();
    // Expanding, collapsing or resizing relays out everything,
    // and a scrolled-back view must stay on the same content
    let anchor = app.transcript.anchor(old_top, inner.width, app.expanded);
    let anchor = anchor.filter(|_| app.viewport.back > 0);
    let rows = app.transcript.rows(talk, inner.width, app.expanded);

    app.pictures.clear();
    // The welcome logo moves, and so do the spinners of pictures loading
    app.animating = rows.is_empty() || picture::loading();
    if rows.is_empty() {
        // Clicks and selections number rows from the bottom,
        // so an empty conversation must not keep the previous one's
        app.viewport.settle(0, 0, None);

        let mut welcome = Vec::new();
        for row in LOGO {
            let mut spans = Vec::new();
            for (column, c) in row.chars().enumerate() {
                let color = theme::wave(column);
                let style = BOLD.fg(color);
                let text = c.to_string();
                spans.push(Span::styled(text, style));
            }
            welcome.push(Line::from(spans));
        }

        let hint = vec![
            Span::raw(":"),
            Span::styled(" commands · ", DIM),
            Span::raw("!"),
            Span::styled(" runs a shell command · ", DIM),
            Span::raw(":resume"),
            Span::styled(" picks up an earlier session", DIM),
        ];
        welcome.extend([
            Line::default(),
            Line::styled("Ask anything to start.", DIM),
            Line::from(hint),
        ]);

        let used = welcome.len() as u16;
        let top = inner.height.saturating_sub(used) / 2;
        let block = block.padding(Padding::top(top));
        let paragraph = Paragraph::new(welcome)
            .centered()
            .wrap(Wrap { trim: true })
            .block(block);
        frame.render_widget(paragraph, conversation);
    } else {
        let mut queued = Vec::new();
        // Lines up with the blank row before a task in `Render`
        if !app.queued.is_empty() {
            queued.push(Line::default());
        }

        let width = usize::from(inner.width);
        let marks = "❯ ".width() + QUEUED.width();
        let room = width.saturating_sub(marks).max(1);
        for message in &app.queued {
            let first = first_row(message, room);
            let text = format!("❯ {first}{QUEUED}");
            queued.push(Line::styled(text, DIM));
        }

        let height = usize::from(inner.height);
        let total = rows.len() + progress.len() + queued.len();
        let anchored = anchor.map(|anchor| rows.row_at(anchor));
        let (top, scrolled) = app.viewport.settle(total, height, anchored);

        let lines = rows
            .iter()
            .map(|row| &row.line)
            .chain(&progress)
            .chain(&queued);
        let visible = lines.skip(top).take(height);
        let visible: Vec<Line> = visible.cloned().collect();

        if app.viewport.back > 0 {
            let more = format!(" ↓ {} more lines · esc to follow ", app.viewport.back);
            let more = Line::styled(more, DIM).right_aligned();
            block = block.title_bottom(more);
        }
        let paragraph = Paragraph::new(visible).block(block);
        frame.render_widget(paragraph, conversation);
        let pinned: Vec<Line> = pinned.into_iter().map(|row| row.line).collect();
        frame.render_widget(Paragraph::new(pinned), pinned_area);

        let inside = conversation.inner(Margin::new(1, 0));
        let fills = rows.iter().map(|row| row.fill).skip(top).take(height);
        for (y, fill) in (inner.y..).zip(fills) {
            if let Some(fill) = fill {
                let band = Rect::new(inside.x, y, inside.width, 1);
                let style = Style::new().bg(fill);
                frame.buffer_mut().set_style(band, style);
            }
        }
        let spins = rows.iter().map(|row| row.spin).skip(top).take(height);
        for (y, spin) in (inner.y..).zip(spins) {
            let Some((column, since)) = spin else {
                continue;
            };
            let x = inner.x.saturating_add(column as u16);
            if x < inner.right() {
                let frame_char = spinner(since).to_string();
                frame.buffer_mut()[(x, y)].set_symbol(&frame_char);
            }
        }
        // A picture's rows are adjacent, so each shows once
        let targets = rows
            .iter()
            .skip(top)
            .take(height)
            .flat_map(|row| &row.targets);
        for (_, target) in targets {
            let Target::Picture(source) = target else {
                continue;
            };
            if app.pictures.last() != Some(source) {
                app.pictures.push(source.clone());
            }
        }

        let track = conversation.inner(Margin::new(0, 1));
        if let Some(thumb) = app.viewport.place_bar(track) {
            let cell = &mut frame.buffer_mut()[thumb];
            cell.set_symbol("█").set_style(ACCENT);
        }

        if let Some(selection) = &app.selection {
            let buffer = frame.buffer_mut();
            selection.highlight(rows, top, inner, buffer);
        }

        // After `rows`, which forgets the view when it lays everything out again
        app.transcript.view(top..top + height, scrolled);
    }

    let draft = &mut app.draft;
    let placeholder = match draft.shell {
        true => "Run a shell command · backspace goes back",
        false => "Ask anything",
    };
    draft.area.set_placeholder_text(placeholder);
    // Same color as the ! before the command in the conversation
    let border = match draft.shell {
        true => PINK,
        false => ACCENT,
    };
    let input_block = panel().border_style(border);
    draft.area.set_block(input_block);
    frame.render_widget(&draft.area, input);

    let inner = panel().inner(input);
    let cursor = draft.area.screen_cursor();
    let row = cursor.row as u16;
    draft.top = match draft.area.is_empty() {
        true => 0,
        false => scroll_top(draft.top, row, inner.height),
    };

    let last_row = inner.height.saturating_sub(1);
    let last_col = inner.width.saturating_sub(1);
    let row = row.saturating_sub(draft.top);
    let row = row.min(last_row);
    let col = (cursor.col as u16).min(last_col);
    let position = Position::new(inner.x + col, inner.y + row);
    frame.set_cursor_position(position);
    app.input_area = input;

    let highlighted = app.highlighted();
    app.choices_area = Rect::default();
    app.scroll_at = None;
    let marquee = app.marquee.take();
    if let Some(highlighted) = highlighted {
        let choices = &app.choices;
        let count = choices.len();
        // Borders, padding, the selection pointer and the gap between columns
        let chrome = 2 + 2 + 2 + 2;
        let labels = choices.iter().map(|choice| choice.label.width());
        let label_width = labels.max().unwrap_or(0);
        let details = choices.iter().map(|choice| choice.detail.width());
        let detail_width = details.max().unwrap_or(0);
        let inside = conversation.inner(Margin::new(1, 1));
        let room = usize::from(inside.width).saturating_sub(chrome);
        let (label_width, detail_width) = widths(label_width, detail_width, room);

        let picked = &choices[highlighted].label;
        let since = match marquee {
            Some((shown, at)) if shown == *picked => at,
            _ => Instant::now(),
        };
        app.marquee = Some((picked.clone(), since));
        let elapsed = since.elapsed();
        let (rolling, changes) = rolled(picked, label_width, elapsed);
        app.scroll_at = changes.map(|changes| since + changes);

        let listed = count.min(MAX_CHOICE_ROWS);
        let height = (listed as u16 + 2).min(inside.height);
        let width = (label_width + detail_width + chrome) as u16;
        let area = Rect {
            y: inside.bottom().saturating_sub(height),
            width: width.min(inside.width),
            height,
            ..inside
        };

        // Clear wipes the background color too
        let ground = Style::new().bg(theme::BG);
        let mut block = panel().border_style(ACCENT).style(ground);
        if count > listed {
            let position = format!(" {}/{count} ", highlighted + 1);
            let position = Line::styled(position, DIM).right_aligned();
            block = block.title_bottom(position);
        }

        let mut items = Vec::new();
        for (index, choice) in choices.iter().enumerate() {
            let label = match index == highlighted {
                true => rolling.clone(),
                false => fitted(&choice.label, label_width),
            };
            let label = Span::raw(label + "  ");
            let detail = fitted(&choice.detail, detail_width);
            let detail = Span::styled(detail, DIM);
            let line = Line::from(vec![label, detail]);
            items.push(ListItem::new(line));
        }
        let pointer = Span::styled("❯ ", PINK);
        let selected = match app.marked() {
            true => BOLD.fg(theme::RED).bg(theme::LINE),
            false => BOLD.bg(theme::LINE),
        };
        let list = List::new(items)
            .block(block)
            .highlight_symbol(pointer)
            .highlight_style(selected);

        frame.render_widget(Clear, area);
        frame.render_stateful_widget(list, area, &mut app.highlight);
        app.choices_area = area;
    }

    let model = app.session.model();
    let name = match app.session.effort() {
        Some(effort) => format!(" · {model} · {effort}"),
        None => format!(" · {model}"),
    };
    let brand = vec![Span::styled(" smol", STRONG), Span::styled(name, DIM)];

    let help = if let Some((flash, _)) = &app.flash {
        let (text, style) = match flash {
            Ok(done) => (format!("✓ {done} "), GREEN),
            Err(reason) => (format!("✗ {reason} "), RED),
        };
        vec![Span::styled(text, style)]
    } else if let Some(highlighted) = highlighted {
        let choice = &app.choices[highlighted];
        let text = match (app.marked(), choice.forgets(), choice.submits()) {
            (true, _, _) => "enter forget · esc keep",
            (false, true, _) => "↑↓ select · del forget · esc close",
            (false, false, true) => "↑↓ select · tab complete · enter run · esc close",
            (false, false, false) => "↑↓ select · tab/enter complete · esc close",
        };
        keys(text)
    } else {
        let expand = match app.expanded {
            true => "ctrl+o collapse",
            false => "ctrl+o expand",
        };
        let enter = match app.session.run {
            Some(_) => "enter queue",
            None => "enter send",
        };
        let queued = match app.queued.is_empty() {
            true => "",
            false => "↑ edit queued · ",
        };

        let text = if app.session.running() {
            format!("{enter} · {queued}esc interrupt · {expand}")
        } else if app.session.run.is_some() {
            format!("{enter} · {queued}{expand}")
        } else if app.draft.whole().is_empty() {
            format!("ctrl+d quit · {expand} · : commands")
        } else {
            // VS Code takes ctrl+j, Windows Terminal takes alt+enter and macOS sends it as enter
            let newline = match cfg!(target_os = "linux") {
                true => "alt+enter",
                false => "ctrl+j",
            };
            format!("{newline} newline · {expand} · : commands")
        };
        keys(&text)
    };

    let width = usize::from(status.width);
    let line = spread(brand, help, width);
    frame.render_widget(line, status);
}

/// The first word of each part is the key
fn keys(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, part) in text.split(" · ").enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", BORDER));
        }

        let (key, action) = part.split_once(' ').unwrap_or((part, ""));
        let key = key.to_owned();
        let action = format!(" {action}");
        spans.push(Span::styled(key, ACCENT));
        spans.push(Span::styled(action, DIM));
    }
    spans.push(Span::raw(" "));
    spans
}

/// Keeps only the left side when the right one does not fit
fn spread(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let spans = left.iter().chain(&right);
    let used: usize = spans.map(Span::width).sum();
    let Some(gap) = width.checked_sub(used).filter(|gap| *gap > 0) else {
        return Line::from(left);
    };

    let padding = " ".repeat(gap);
    let mut spans = left;
    spans.push(Span::raw(padding));
    spans.extend(right);
    Line::from(spans)
}

fn approx(stats: &Stats) -> &'static str {
    match stats.estimated {
        Some(_) => "≈",
        None => "",
    }
}

fn labeled(label: &str, value: String, width: usize) -> Line<'static> {
    if label.width() + value.width() >= width {
        return Line::raw(value).right_aligned();
    }

    let label = label.to_owned();
    let label = vec![Span::styled(label, DIM)];
    let value = vec![Span::raw(value)];
    spread(label, value, width)
}

/// A long detail still leaves at least half the room to the label
fn widths(label: usize, detail: usize, room: usize) -> (usize, usize) {
    let floor = room.saturating_sub(detail).max(room / 2);
    let label = label.min(floor);
    let detail = detail.min(room - label);
    (label, detail)
}

fn first_row(text: &str, width: usize) -> String {
    let text = text.trim();
    let wrapped = wrap_ranges(text, width);
    let Some((range, _)) = wrapped.first() else {
        return String::new();
    };

    let first = &text[range.clone()];
    if wrapped.len() == 1 {
        return first.to_owned();
    }
    let marked = format!("{first}…");
    clipped(&marked, width)
}

/// The `width` columns of `text` shown `elapsed` after it was highlighted, and when they next change
fn rolled(text: &str, width: usize, elapsed: Duration) -> (String, Option<Duration>) {
    let overflow = text.width().saturating_sub(width);
    if overflow == 0 {
        return (fitted(text, width), None);
    }

    let every = SCROLL_EVERY.as_millis() as usize;
    let pause = SCROLL_WAIT.as_millis() as usize / every;
    let cycle = overflow + 2 * pause;
    let steps = elapsed.as_millis() as usize / every;
    let position = steps % cycle;
    let offset = position.saturating_sub(pause).min(overflow);
    let next = match position < pause + overflow {
        true => position.max(pause) + 1,
        false => cycle,
    };
    let millis = (steps - position + next) * every;
    let changes = Some(Duration::from_millis(millis as u64));
    if offset == 0 {
        return (fitted(text, width), changes);
    }

    let mut shown = String::new();
    let mut column = 0;
    for grapheme in text.graphemes(true) {
        let start = column;
        column += grapheme.width();
        // A wide character cut by either edge is left out
        if start < offset {
            continue;
        }
        if column > offset + width {
            break;
        }
        shown.push_str(grapheme);
    }
    (fitted(&shown, width), changes)
}

fn tail(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }

    let mut kept = Vec::new();
    let mut used = 1;
    for grapheme in text.graphemes(true).rev() {
        let w = grapheme.width();
        if used + w > width {
            break;
        }
        kept.push(grapheme);
        used += w;
    }

    let kept: String = kept.into_iter().rev().collect();
    format!("…{kept}")
}

fn panel() -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(BORDER)
        .padding(Padding::horizontal(1))
}

fn gauge(ratio: Option<f64>, width: usize) -> Line<'static> {
    let ratio = ratio.unwrap_or_default();
    let filled = (ratio.clamp(0.0, 1.0) * width as f64).round() as usize;
    let color = match ratio {
        r if r >= 0.9 => theme::RED,
        r if r >= 0.7 => theme::ORANGE,
        _ => theme::PURPLE,
    };

    let full = "━".repeat(filled);
    let empty = "━".repeat(width - filled);
    let fill = Style::new().fg(color);
    let full = Span::styled(full, fill);
    let empty = Span::styled(empty, BORDER);
    Line::from(vec![full, empty])
}

/// Must agree with the textarea's `WrapMode::Glyph`
fn input_rows(input: &TextArea, width: u16) -> u16 {
    let area = Rect::new(0, 0, width, 3);
    let width = panel().inner(area).width.max(1);
    let width = usize::from(width);
    let tab = input.tab_length();
    let tab = usize::from(tab);
    let cells = |c: char, col: usize| match c {
        '\t' => tab - col % tab,
        c => c.width().unwrap_or(0),
    };

    let mut rows = 0;
    for line in input.lines() {
        rows += 1;
        let mut col = 0;
        for c in line.chars() {
            let mut w = cells(c, col);
            if col > 0 && col + w > width {
                rows += 1;
                col = 0;
                w = cells(c, col);
            }
            col += w;
        }
    }

    rows.clamp(1, MAX_INPUT_ROWS) as u16
}

/// Copies the rule the textarea follows to move its scroll position when rendering
fn scroll_top(top: u16, row: u16, height: u16) -> u16 {
    if row < top {
        row
    } else if top + height <= row {
        row + 1 - height
    } else {
        top
    }
}
