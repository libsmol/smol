use crate::client::{McpState, McpStatus};
use crate::tui::app::App;
use crate::tui::render::wrap_ranges;
use crate::tui::{
    fitted, in_out, kilo, spinner, theme, ACCENT, BOLD, BORDER, DIM, HEADING, RED, STRONG,
};
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, Padding, Paragraph, Wrap};
use ratatui::Frame;
use ratatui_textarea::{CursorMove, TextArea, WrapMode};
use reaper::tilde;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const SIDEBAR_WIDTH: u16 = 32;
const MIN_WIDE_SCREEN: u16 = 100;
const MAX_INPUT_ROWS: usize = 10;
const MAX_CHOICE_ROWS: usize = 10;
const PINK: Style = Style::new().fg(theme::PINK);
const GREEN: Style = Style::new().fg(theme::GREEN);
const QUEUED: &str = " · queued";
const LOGO: [&str; 2] = ["▄▀▀ ▄▀▄▀▄ ▄▀▄ █ ", "▄▄▀ █ █ █ ▀▄▀ █▄"];

pub(crate) fn input_box(text: &str) -> TextArea<'static> {
    let lines = text.split('\n').map(str::to_owned).collect();
    let mut input = TextArea::new(lines);
    input.move_cursor(CursorMove::Bottom);
    input.move_cursor(CursorMove::End);

    input.set_wrap_mode(WrapMode::Glyph);
    input.set_placeholder_style(DIM);
    input.set_cursor_line_style(Style::new());
    // Input methods place their candidate window at the hardware cursor,
    // so the textarea cursor is not drawn
    input.set_cursor_style(Style::new());

    input
}

pub(crate) fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let base = Style::new().bg(theme::BG).fg(theme::FG);
    let ground = Block::new().style(base);
    frame.render_widget(ground, area);

    let input_height = input_rows(&app.input, area.width) + 2;
    let heights = [
        Constraint::Fill(1),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ];
    let [body, input, status] = Layout::vertical(heights).areas(area);

    let narrow = area.width < MIN_WIDE_SCREEN;
    let columns = [Constraint::Fill(1), Constraint::Length(SIDEBAR_WIDTH)];
    let [beside, sidebar] = Layout::horizontal(columns).areas(body);
    let conversation = if narrow { body } else { beside };

    let title = match app.session.title.as_str() {
        "" => "new session",
        title => title,
    };
    let mut block = panel();
    let mut usage_width = 0;

    if narrow {
        let stats = app.transcript.stats();
        let tokens = in_out(stats.input, stats.output);
        let mut usage = format!(" {tokens}");
        if stats.context.is_some() && stats.window.is_some() {
            let approx = stats.approx();
            let share = match stats.ratio() {
                Some(ratio) => format!("{approx}{:.0}%", ratio * 100.0),
                None => "?%".to_owned(),
            };
            usage += &format!(" · context {share}");
        }
        let working = app.transcript.helping();
        let mut agents = Vec::new();
        if working > 0 {
            agents.push(format!("{working} working"));
        }
        if stats.agent_input + stats.agent_output > 0 {
            let tokens = in_out(stats.agent_input, stats.agent_output);
            agents.push(tokens);
        }
        if !agents.is_empty() {
            let agents = agents.join(" · ");
            usage += &format!(" · agents {agents}");
        }

        // Startup failures are not reported in the conversation,
        // so on a narrow screen they only show here
        let failed = |server: &&McpStatus| matches!(server.state, McpState::Failed { .. });
        let failed = app.session.mcp.iter().filter(failed).count();
        if failed > 0 {
            usage += &format!(" · mcp {failed} failed");
        }
        usage.push(' ');

        usage_width = usage.width();
        let usage = Line::styled(usage, DIM).right_aligned();
        block = block.title(usage);
    } else {
        let side = panel();
        let inner = side.inner(sidebar);
        let width = usize::from(inner.width);

        let title = fitted(title, width);

        let cwd = tilde(&app.session.client.cwd);
        let cwd = tail(&cwd, width);

        let model = app.session.model();
        let model = fitted(&model, width);

        // The row count is fixed, so rows hold their place even before data arrives
        let stats = app.transcript.stats();
        let turns = stats.turns.to_string();
        let steps = stats.steps.to_string();
        let turns = vec![Span::styled("turns ", DIM), Span::raw(turns)];
        let steps = vec![Span::styled("steps ", DIM), Span::raw(steps)];
        let counts = spread(turns, steps, width);

        let tokens = in_out(stats.input, stats.output);
        let tokens = labeled("tokens", tokens, width);

        let ratio = stats.ratio();
        let approx = stats.approx();
        let context = match (stats.context, stats.window) {
            (Some(used), Some(window)) => {
                let used = kilo(used);
                let window = kilo(window);
                match ratio {
                    Some(ratio) => {
                        let share = ratio * 100.0;
                        format!("{approx}{used} / {window} · {share:.0}%")
                    }
                    None => format!("? / {window}"),
                }
            }
            (Some(used), None) => {
                let used = kilo(used);
                format!("{approx}{used}")
            }
            (None, _) => "–".to_owned(),
        };
        let context = labeled("context", context, width);
        let bar = gauge(ratio, width);

        let mut lines = vec![
            Line::styled(title, BOLD),
            Line::styled(cwd, DIM),
            Line::styled(model, DIM),
            Line::default(),
            counts,
            tokens,
            context,
            bar,
        ];
        // The progress line already counts the agents at work, and both don't fit here
        let helped = stats.agent_input + stats.agent_output > 0;
        let working = app.transcript.helping();
        let agents = match (helped, working) {
            (true, _) => Some(in_out(stats.agent_input, stats.agent_output)),
            (false, 0) => None,
            (false, working) => Some(format!("{working} working")),
        };
        if let Some(agents) = agents {
            let agents = labeled("agents", agents, width);
            lines.push(agents);
        }

        if !app.session.mcp.is_empty() {
            let section = heading("mcp");
            lines.extend([Line::default(), section]);
        }
        for server in &app.session.mcp {
            let (dot, state) = match &server.state {
                McpState::Off => (theme::COMMENT, Span::styled("off", DIM)),
                McpState::Connecting => (theme::ORANGE, Span::styled("connecting", DIM)),
                McpState::Connected { tools: 1 } => (theme::GREEN, Span::styled("1 tool", DIM)),
                McpState::Connected { tools } => {
                    let tools = format!("{tools} tools");
                    (theme::GREEN, Span::styled(tools, DIM))
                }
                McpState::Failed { .. } => (theme::RED, Span::styled("failed", RED)),
            };
            let dot = Style::new().fg(dot);
            let dot = Span::styled("● ", dot);
            let name = Span::raw(server.name.clone());
            let name = vec![dot, name];
            let line = spread(name, vec![state], width);
            lines.push(line);
        }

        frame.render_widget(side, sidebar);
        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }

    // Two corners, a space on each side of the title and at least one cell of border line
    let taken = usage_width + 5;
    let room = usize::from(conversation.width).saturating_sub(taken);
    let title = fitted(title, room);
    let title = format!(" {} ", title.trim_end());
    let title = Span::styled(title, HEADING);
    block = block.title(title);

    let inner = block.inner(conversation);
    app.messages_area = inner;

    let mut progress = Vec::new();
    if let Some(run) = &app.run {
        let started = run.step_started;
        let elapsed = started.elapsed();
        let tick = spinner(started);
        let spinner = format!("  {tick} ");

        let transcript = &app.transcript;
        let writing = transcript.writing();
        let left = transcript.retry_in();
        let seconds = elapsed.as_secs();
        let estimated = match transcript.stats().estimated {
            Some(tokens) => {
                let tokens = kilo(tokens);
                format!(" · ≈⇣{tokens}")
            }
            None => String::new(),
        };

        // The agent starts the turn only after every MCP server has finished connecting
        let connecting = |server: &&McpStatus| server.state == McpState::Connecting;
        let connecting = app.session.mcp.iter().filter(connecting);
        let connecting: Vec<&str> = connecting.map(|server| server.name.as_str()).collect();
        let waiting = transcript.unstarted() && !connecting.is_empty();
        let helping = transcript.helping();
        let label = match (run.interrupting, left, writing) {
            (true, _, _) => "interrupting…".to_owned(),
            (false, Some(left), _) => {
                let wait = left.as_secs_f64().ceil();
                format!("retrying in {wait}s")
            }
            (false, None, _) if transcript.shelling() => format!("running · {seconds}s"),
            (false, None, _) if waiting => {
                let names = connecting.join(", ");
                format!("waiting for mcp {names} · {seconds}s")
            }
            (false, None, _) if helping > 0 => {
                let agents = match helping {
                    1 => "agent",
                    _ => "agents",
                };
                format!("waiting for {helping} {agents} · {seconds}s")
            }
            (false, None, false) => format!("thinking · {seconds}s{estimated}"),
            (false, None, true) => format!("working · {seconds}s{estimated}"),
        };

        // Lines up with the blank row that starts `Render::step`
        if !transcript.stepping() {
            progress.push(Line::default());
        }
        let italic = DIM.add_modifier(Modifier::ITALIC);
        let mut line = vec![Span::styled(spinner, ACCENT)];
        for (text, style) in theme::shimmer(&label, italic, theme::PURPLE, started) {
            line.push(Span::styled(text, style));
        }
        progress.push(Line::from(line));
    }

    let old_top = app.view_top();
    // Expanding, collapsing or resizing relays out everything,
    // and a scrolled-back view must stay on the same content
    let anchor = app.transcript.anchor(old_top, inner.width, app.expanded);
    let anchor = anchor.filter(|_| app.scrolled_back > 0);
    let rows = app.transcript.rows(inner.width, app.expanded);

    app.scrollbar = Rect::default();
    app.animating = rows.is_empty();
    if rows.is_empty() {
        // Clicks and selections number rows from `bottom`,
        // so an empty conversation must not keep the previous one's
        app.bottom = 0;
        app.scrolled_back = 0;

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
        let bottom = total.saturating_sub(height);
        match anchor {
            Some(anchor) => {
                let top = rows.row_at(anchor);
                app.scrolled_back = bottom.saturating_sub(top);
            }
            // While scrolled back, content growing or shrinking must not shift what is being read
            None if app.scrolled_back > 0 => {
                let moved = app.scrolled_back.saturating_add(bottom);
                app.scrolled_back = moved.saturating_sub(app.bottom);
            }
            None => {}
        }

        app.scrolled_back = app.scrolled_back.min(bottom);
        app.bottom = bottom;
        let top = bottom - app.scrolled_back;

        let lines = rows
            .iter()
            .map(|row| &row.line)
            .chain(&progress)
            .chain(&queued);
        let visible = lines.skip(top).take(height);
        let visible: Vec<Line> = visible.cloned().collect();

        if app.scrolled_back > 0 {
            let more = format!(" ↓ {} more lines · esc to follow ", app.scrolled_back);
            let more = Line::styled(more, DIM).right_aligned();
            block = block.title_bottom(more);
        }
        let paragraph = Paragraph::new(visible).block(block);
        frame.render_widget(paragraph, conversation);

        let inside = conversation.inner(Margin::new(1, 0));
        let fills = rows.iter().map(|row| row.fill).skip(top).take(height);
        for (y, fill) in (inner.y..).zip(fills) {
            if let Some(fill) = fill {
                let band = Rect::new(inside.x, y, inside.width, 1);
                let style = Style::new().bg(fill);
                frame.buffer_mut().set_style(band, style);
            }
        }

        // Without a track the thumb would land on the corner
        let track = conversation.inner(Margin::new(0, 1));
        if bottom > 0 && !track.is_empty() {
            let column = track.right().saturating_sub(1);
            app.scrollbar = Rect::new(column, track.top(), 1, track.height);
            let row = track.top() + thumb(top, bottom, track.height);
            let cell = &mut frame.buffer_mut()[(column, row)];
            cell.set_symbol("█").set_style(ACCENT);
        }

        if let Some(selection) = &app.selection {
            let buffer = frame.buffer_mut();
            selection.highlight(rows, top, inner, buffer);
        }
    }

    let placeholder = match app.shell {
        true => "Run a shell command · backspace goes back",
        false => "Ask anything",
    };
    app.input.set_placeholder_text(placeholder);
    // Same color as the ! before the command in the conversation
    let border = match app.shell {
        true => PINK,
        false => ACCENT,
    };
    let input_block = panel().border_style(border);
    app.input.set_block(input_block);
    frame.render_widget(&app.input, input);

    let inner = panel().inner(input);
    let cursor = app.input.screen_cursor();
    let row = cursor.row as u16;
    app.input_top = match app.input.is_empty() {
        true => 0,
        false => scroll_top(app.input_top, row, inner.height),
    };

    let last_row = inner.height.saturating_sub(1);
    let last_col = inner.width.saturating_sub(1);
    let row = row.saturating_sub(app.input_top);
    let row = row.min(last_row);
    let col = (cursor.col as u16).min(last_col);
    let position = Position::new(inner.x + col, inner.y + row);
    frame.set_cursor_position(position);

    let highlighted = app.highlighted();
    app.choices_area = Rect::default();
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
        for choice in choices {
            let label = fitted(&choice.label, label_width);
            let label = Span::raw(label + "  ");
            let detail = fitted(&choice.detail, detail_width);
            let detail = Span::styled(detail, DIM);
            let line = Line::from(vec![label, detail]);
            items.push(ListItem::new(line));
        }
        let pointer = Span::styled("❯ ", PINK);
        let selected = BOLD.bg(theme::LINE);
        let list = List::new(items)
            .block(block)
            .highlight_symbol(pointer)
            .highlight_style(selected);

        frame.render_widget(Clear, area);
        frame.render_stateful_widget(list, area, &mut app.highlight);
        app.choices_area = area;
    }

    let name = app.session.model();
    let name = format!(" · {name}");
    let brand = vec![Span::styled(" smol", STRONG), Span::styled(name, DIM)];

    let help = if let Some((flash, _)) = &app.flash {
        let (text, style) = match flash {
            Ok(done) => (format!("✓ {done} "), GREEN),
            Err(reason) => (format!("✗ {reason} "), RED),
        };
        vec![Span::styled(text, style)]
    } else if let Some(highlighted) = highlighted {
        let text = match app.choices[highlighted].submits() {
            true => "↑↓ select · tab complete · enter run · esc close",
            false => "↑↓ select · tab/enter complete · esc close",
        };
        keys(text)
    } else {
        let expand = match app.expanded {
            true => "ctrl+o collapse",
            false => "ctrl+o expand",
        };
        let enter = match app.run {
            Some(_) => "enter queue",
            None => "enter send",
        };
        let queued = match app.queued.is_empty() {
            true => "",
            false => "↑ edit queued · ",
        };

        let text = if app.running() {
            format!("{enter} · {queued}esc interrupt · {expand}")
        } else if app.run.is_some() {
            format!("{enter} · {queued}{expand}")
        } else if app.draft().is_empty() {
            format!("ctrl+d quit · {expand} · : commands")
        } else {
            // Windows Terminal keeps alt+enter for full screen
            let newline = if cfg!(windows) { "ctrl+j" } else { "alt+enter" };
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

pub(crate) fn thumb(top: usize, bottom: usize, length: u16) -> u16 {
    let last = usize::from(length.saturating_sub(1));
    let row = (top * last + bottom / 2).checked_div(bottom).unwrap_or(0);
    row.min(last) as u16
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
    let marked = fitted(&marked, width);
    marked.trim_end().to_owned()
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

fn heading(text: &str) -> Line<'static> {
    let text = text.to_uppercase();
    let span = Span::styled(text, HEADING);
    Line::from(span)
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

#[cfg(test)]
mod tests {
    use crate::tui::tests::{app, draw, fill};
    use crate::tui::ui::*;
    use agent_client_protocol_schema::v1::{
        ContentBlock, ContentChunk, SessionUpdate, ToolCall, ToolCallContent, ToolCallStatus,
        ToolCallUpdate, ToolCallUpdateFields, UsageUpdate,
    };
    use ratatui::buffer::Buffer;
    use ratatui::widgets::Widget;
    use serde_json::{json, Value};

    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let terminal = draw(app, width, height);
        let buffer = terminal.backend().buffer();
        decoded(buffer, buffer.area)
    }

    /// The cell after a wide character holds a placeholder space
    fn decoded(buffer: &Buffer, area: Rect) -> Vec<String> {
        let mut rows = Vec::new();
        for y in area.top()..area.bottom() {
            let mut row = String::new();
            let mut x = area.left();
            while x < area.right() {
                let symbol = buffer[(x, y)].symbol();
                row.push_str(symbol);
                x += symbol.width().max(1) as u16;
            }
            rows.push(row);
        }
        rows
    }

    #[test]
    fn gauge_fill() {
        let widths = |line: &Line| line.spans.iter().map(Span::width).collect::<Vec<_>>();

        let half = gauge(Some(0.5), 10);
        assert_eq!(half.to_string(), "━━━━━━━━━━");
        assert_eq!(widths(&half), [5, 5]);
        let full = gauge(Some(1.7), 10);
        assert_eq!(widths(&full), [10, 0]);
        let unknown = gauge(None, 10);
        assert_eq!(widths(&unknown), [0, 10]);
        let red = gauge(Some(0.95), 10);
        assert_eq!(red.spans[0].style.fg, Some(theme::RED));
    }

    #[test]
    fn thumb_rows() {
        assert_eq!(thumb(0, 100, 10), 0);
        assert_eq!(thumb(50, 100, 10), 5);
        assert_eq!(thumb(100, 100, 10), 9);
        assert_eq!(thumb(0, 0, 10), 0);
        assert_eq!(thumb(3, 7, 1), 0);
    }

    #[test]
    fn widths_split() {
        assert_eq!(widths(6, 40, 30), (6, 24));
        assert_eq!(widths(28, 40, 30), (15, 15));
        assert_eq!(widths(10, 8, 30), (10, 8));
        assert_eq!(widths(10, 8, 0), (0, 0));
    }

    #[test]
    fn first_row_cut() {
        assert_eq!(first_row("build the backend", 28), "build the backend");
        assert_eq!(first_row("  ", 28), "");

        let long = "Own the frontend 🎨: components, styles and interactions, pages from the designs 📐, unit tests 🧪";
        let first = first_row(long, 10);
        assert!(first.ends_with('…') && first.width() <= 10, "{first}");
        assert_eq!(first_row("naïve café 🐍 crème brûlée", 10), "naïve caf…");

        let emoji = first_row("⚠️ careful ✔️ done", 13);
        assert!(emoji.width() <= 13, "{emoji}");
    }

    #[test]
    fn tail_width() {
        assert_eq!(tail("~/asd/libsmol", 30), "~/asd/libsmol");
        assert_eq!(tail("/tmp/🍕🍕/🚀🚀", 8), "…🍕/🚀🚀");
        assert_eq!(tail("/tmp/abc", 4), "…abc");
        assert_eq!(tail("/🍕🍕/⚠️🚀🚀", 7), "…⚠️🚀🚀");
        assert_eq!(tail("/🍕🍕/⚠️🚀🚀", 6), "…🚀🚀");
    }

    #[test]
    fn input_wrapping() {
        let texts = [
            "",
            "123456",
            "1234567",
            "🍕🚀🎉🐍",
            "🍕🚀🎉🐍🌮",
            "a\nb\nc",
            "a\tb",
            "abc\td",
            "\t\t",
        ];
        for text in texts {
            let mut input = input_box(text);
            input.set_block(panel());
            let area = Rect::new(0, 0, 10, 12);
            let mut buffer = Buffer::empty(area);
            (&input).render(area, &mut buffer);

            let row = input.screen_cursor().row as u16;
            assert_eq!(input_rows(&input, 10), row + 1, "{text:?}");
        }

        let long = "x\n".repeat(50);
        let long = input_box(&long);
        assert_eq!(input_rows(&long, 10), MAX_INPUT_ROWS as u16);
    }

    #[test]
    fn input_scroll() {
        let lines: Vec<String> = (0..15).map(|n| format!("line {n}")).collect();
        let lines = lines.join("\n");
        let mut input = input_box(&lines);
        input.set_block(panel());
        let area = Rect::new(0, 0, 20, 7);
        let inner = panel().inner(area);
        let mut top = 0;

        let mut shown = |input: &TextArea| {
            let mut buffer = Buffer::empty(area);
            input.render(area, &mut buffer);
            let row = input.screen_cursor().row as u16;
            top = scroll_top(top, row, inner.height);

            let rows = decoded(&buffer, inner);
            let at = usize::from(row - top);
            rows[at].trim_end().to_owned()
        };

        assert_eq!(shown(&input), "line 14");
        for _ in 0..3 {
            input.move_cursor(CursorMove::Up);
        }
        assert_eq!(shown(&input), "line 11");
        for _ in 0..8 {
            input.move_cursor(CursorMove::Up);
        }
        assert_eq!(shown(&input), "line 3");
    }

    #[test]
    fn mcp_wait_shown() {
        let mut app = app();
        for name in ["web", "search"] {
            let server = McpStatus {
                name: name.to_owned(),
                state: McpState::Connecting,
            };
            app.session.mcp.push(server);
        }
        app.submit_text("wait");
        let rows = screen(&mut app, 80, 12);
        let waiting = |row: &String| row.contains("waiting for mcp web, search · ");
        assert!(rows.iter().any(waiting), "{rows:#?}");

        let reply = ContentBlock::from("ok");
        let reply = ContentChunk::new(reply);
        let reply = SessionUpdate::AgentMessageChunk(reply);
        app.transcript.update(&reply);
        let rows = screen(&mut app, 80, 12);
        assert!(!rows.iter().any(|row| row.contains("waiting")), "{rows:#?}");
    }

    #[test]
    fn long_queued_clipped() {
        let mut app = app();
        app.submit_text("wait");
        let long = format!("update: {}", "a long explanation ".repeat(20));
        app.queued.extend([long, "short\nsecond line".to_owned()]);
        let rows = screen(&mut app, 80, 16);

        let queued: Vec<&String> = rows
            .iter()
            .filter(|row| row.contains(" · queued"))
            .collect();
        assert_eq!(queued.len(), 2, "{rows:#?}");
        assert!(queued[0].contains("explanation… · queued"), "{queued:?}");
        assert!(queued[1].contains("❯ short… · queued"), "{queued:?}");
    }

    #[test]
    fn narrow_header_clipped() {
        let mut app = app();
        app.session.title = "Refactor the session store ".repeat(8);
        let error = "connection refused".to_owned();
        let server = McpStatus {
            name: "web".to_owned(),
            state: McpState::Failed { error },
        };
        app.session.mcp.push(server);

        let rows = screen(&mut app, 80, 10);
        let top = &rows[0];
        assert!(top.contains("stor… ─"), "{top}");
        assert!(top.ends_with(" ⇡0 ⇣0 · mcp 1 failed ╮"), "{top}");
    }

    #[test]
    fn welcome_hint_wraps() {
        let mut app = app();
        app.session.set_model("m2-café").expect("switch");
        let rows = screen(&mut app, 60, 20);
        let text = rows.join("\n");
        assert!(text.contains("earlier session"), "{text}");
        let status = " smol · m2-café    ctrl+d quit · ctrl+o expand · : commands ";
        assert_eq!(rows[19], status);

        let heads = LOGO.map(|row| row.chars().take(3).collect::<String>());
        let found = |head: String| rows.iter().find_map(|row| row.find(&head));
        let starts = heads.map(found);
        assert!(starts[0].is_some() && starts[0] == starts[1], "{text}");
    }

    #[test]
    fn scrollbar_follows_view() {
        let mut app = app();
        fill(&mut app, 40);
        let bar = |app: &mut App| {
            let terminal = draw(app, 60, 20);
            let right = app.messages_area.right() + 1;
            let buffer = terminal.backend().buffer();
            let mut column = String::new();
            for row in app.messages_area.top()..app.messages_area.bottom() {
                let symbol = buffer[(right, row)].symbol();
                column.push_str(symbol);
            }
            column
        };

        let following = bar(&mut app);
        assert!(
            following.starts_with('│') && following.ends_with('█'),
            "{following}"
        );
        assert_eq!(following.matches('█').count(), 1, "{following}");

        app.scrolled_back = usize::MAX;
        let top = bar(&mut app);
        assert!(top.starts_with('█') && top.ends_with('│'), "{top}");
    }

    #[test]
    fn scrolled_view_kept() {
        let mut app = app();
        fill(&mut app, 40);
        draw(&mut app, 60, 20);
        app.scrolled_back += 5;
        draw(&mut app, 60, 20);
        let top = app.view_top();

        fill(&mut app, 10);
        draw(&mut app, 60, 20);
        assert_eq!(app.view_top(), top);
        assert!(app.scrolled_back > 5, "{}", app.scrolled_back);
    }

    #[test]
    fn estimate_while_running() {
        let mut app = app();
        app.submit_text("wait");
        let meta = json!({ "estimatedOutput": 1234 });
        let meta = meta.as_object().cloned();
        let usage = UsageUpdate::new(5000, 100_000).meta(meta);
        let update = SessionUpdate::UsageUpdate(usage);
        app.transcript.update(&update);

        let screen = screen(&mut app, 120, 20).join("\n");
        assert!(screen.contains("s · ≈⇣1.23k"), "{screen}");
        assert!(screen.contains("≈5k / 100k · 5%"), "{screen}");
    }

    fn helper_update(app: &mut App, id: &str, status: ToolCallStatus, texts: &[&str], meta: Value) {
        let content: Vec<ToolCallContent> = texts
            .iter()
            .map(|text| ToolCallContent::from(*text))
            .collect();
        let fields = ToolCallUpdateFields::new().status(status).content(content);
        let update = ToolCallUpdate::new(id.to_owned(), fields).meta(meta.as_object().cloned());
        app.transcript
            .update(&SessionUpdate::ToolCallUpdate(update));
    }

    /// Agents sit under the call whose code started them, and their tokens count apart
    #[test]
    fn helpers_grouped() {
        let mut app = app();
        app.submit_text("wait");
        let code = ToolCallContent::from("```python\nagent(tasks)\n```");
        let running = ToolCall::new("call_1", "agent(tasks)")
            .status(ToolCallStatus::InProgress)
            .content(vec![code]);
        app.transcript.update(&SessionUpdate::ToolCall(running));
        let tasks = [
            "Review prices.py",
            "Review names.py",
            "Review dates.py",
            "Review café.py",
            "Review menu.py",
        ];
        for (offset, task) in tasks.iter().enumerate() {
            let id = format!("call_1.{}", offset + 1);
            let meta = json!({ "_smol/agent": true, "parent": "call_1" });
            let opened =
                ToolCall::new(id, format!("agent: {task}")).meta(meta.as_object().cloned());
            app.transcript.update(&SessionUpdate::ToolCall(opened));
        }
        let marked = json!({ "_smol/agent": true });
        let preview = "prices = load('prices.csv')\nprint(prices[:3])\nprint(total(prices))\nprint(sum(prices))\nprint('café', len(prices))";
        helper_update(
            &mut app,
            "call_1.1",
            ToolCallStatus::InProgress,
            &["writing prices = load('prices.csv')", preview],
            marked.clone(),
        );
        let usage = json!({ "_smol/agent": true, "inputTokens": 1200, "outputTokens": 340, "steps": 3, "duration": 12.5 });
        let answer = "**Bug:** `prices[1:]` skips the first price\n\nUse `sum(prices)`.";
        helper_update(
            &mut app,
            "call_1.2",
            ToolCallStatus::Completed,
            &[answer],
            usage,
        );
        helper_update(
            &mut app,
            "call_1.3",
            ToolCallStatus::Failed,
            &["model: http 503: busy"],
            marked.clone(),
        );
        let stopped = json!({ "_smol/agent": true, "interrupted": true });
        helper_update(
            &mut app,
            "call_1.4",
            ToolCallStatus::Failed,
            &["interrupted by the user"],
            stopped,
        );

        let stats = app.transcript.stats();
        assert_eq!((stats.input, stats.output, stats.steps), (0, 0, 0));
        assert_eq!(app.transcript.helping(), 2);
        let rows = screen(&mut app, 120, 30);
        let wide = rows.join("\n");
        let header =
            "╰ agents · 1 running · 1 queued · 1 done · 1 failed · 1 interrupted · ⇡1.2k ⇣340";
        assert!(wide.contains(header), "{wide}");
        assert!(!wide.contains('✓'), "{wide}");
        // Only the conversation panel, without its borders or the sidebar
        let mut panel = Vec::new();
        for row in &rows {
            let inside = row.split("││").next().unwrap_or_default();
            let inside = inside.strip_prefix('│').unwrap_or(inside);
            panel.push(inside.trim_end().to_owned());
        }
        // Ended agents keep a row, or two when they failed,
        // and a running one shows the end of its work above its status
        let group = [
            "Review prices.py",
            "│ print(prices[:3])",
            "│ print(total(prices))",
            "│ print(sum(prices))",
            "│ print('café', len(prices))",
            "writing prices = load('prices.csv') · 0s",
            "◆ Review names.py",
            "✗ Review dates.py",
            "model: http 503: busy",
            "✗ Review café.py",
            "◌ Review menu.py",
            "queued",
        ];
        let header = panel.iter().position(|row| row.contains("╰ agents"));
        let header = header.expect("header");
        let shown = &panel[header + 1..header + 1 + group.len()];
        for (row, expected) in shown.iter().zip(group) {
            assert!(row.ends_with(expected), "{panel:#?}");
        }
        let start = |row: &str, text: &str| row.find(text).map(|at| row[..at].width());
        let menu = header + group.len() - 1;
        let column = start(&panel[menu], "Review");
        assert_eq!(start(&panel[menu + 1], "queued"), column, "{panel:#?}");
        assert!(wide.contains("waiting for 2 agents · "), "{wide}");
        let sidebar = |row: &&str| row.contains("││ agents ") && row.ends_with(" ⇡1.2k ⇣340 │");
        assert!(wide.lines().any(|row| sidebar(&row)), "{wide}");

        app.expanded = true;
        let expanded = screen(&mut app, 120, 40);
        let title = expanded
            .iter()
            .position(|row| row.contains(" Review prices.py"));
        let title = title.expect("title");
        assert!(
            expanded[title + 5].contains("writing prices = load('prices.csv') · 0s"),
            "{expanded:#?}"
        );
        assert!(
            expanded[title + 6].contains("◆ Review names.py"),
            "{expanded:#?}"
        );
        let stats = expanded
            .iter()
            .position(|row| row.contains("3 steps · 12.5s · ⇡1.2k ⇣340"))
            .expect("stats");
        assert!(
            expanded[stats - 1].contains("Use sum(prices)."),
            "{expanded:#?}"
        );
        let gap = expanded[stats + 1].split("││").next().unwrap_or_default();
        assert_eq!(gap.trim_matches([' ', '│']), "", "{expanded:#?}");
        assert!(
            expanded[stats + 2].contains("✗ Review dates.py"),
            "{expanded:#?}"
        );
        let narrow = screen(&mut app, 80, 20);
        assert!(
            narrow[0].contains(" · agents 2 working · ⇡1.2k ⇣340 "),
            "{narrow:?}"
        );
    }
}
