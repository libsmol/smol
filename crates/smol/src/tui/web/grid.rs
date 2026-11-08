use crate::tui::picture;
use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::{Buffer, Cell, CellWidth};
use ratatui::layout::{Position, Rect, Size};
use ratatui::style::{Color, Modifier};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::mem;

/// The standard and bright colors as xterm draws them
const ANSI: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00),
    (0xCD, 0x00, 0x00),
    (0x00, 0xCD, 0x00),
    (0xCD, 0xCD, 0x00),
    (0x00, 0x00, 0xEE),
    (0xCD, 0x00, 0xCD),
    (0x00, 0xCD, 0xCD),
    (0xE5, 0xE5, 0xE5),
    (0x7F, 0x7F, 0x7F),
    (0xFF, 0x00, 0x00),
    (0x00, 0xFF, 0x00),
    (0xFF, 0xFF, 0x00),
    (0x5C, 0x5C, 0xFF),
    (0xFF, 0x00, 0xFF),
    (0x00, 0xFF, 0xFF),
    (0xFF, 0xFF, 0xFF),
];

/// The screen every device shows, which keeps its cells for those that join later
/// and the cells drawn since they were last sent
pub(crate) struct Grid {
    size: Size,
    screen: Buffer,
    cursor: Position,
    shown: bool,
    drawn: Vec<Position>,
    cleared: bool,
    cursor_moved: bool,
}

/// What a frame changed
pub(crate) struct Changes {
    pub(crate) cleared: bool,
    /// By row, then column, without the columns a wide symbol covers, as ratatui's diff leaves
    /// them out and a terminal drawing them would shift the rest of the row
    pub(crate) cells: Vec<Position>,
    /// Moved, showed or hid
    pub(crate) cursor_moved: bool,
}

impl Changes {
    pub(crate) fn is_empty(&self) -> bool {
        !self.cleared && self.cells.is_empty() && !self.cursor_moved
    }
}

impl Grid {
    pub(crate) fn new(size: Size) -> Self {
        let area = Rect::from((Position::ORIGIN, size));
        Self {
            size,
            screen: Buffer::empty(area),
            cursor: Position::ORIGIN,
            shown: false,
            drawn: Vec::new(),
            cleared: true,
            cursor_moved: false,
        }
    }

    /// Takes effect at the next draw, which clears the screen and draws it whole
    pub(crate) fn resize(&mut self, size: Size) {
        self.size = size;
    }

    pub(crate) fn screen(&self) -> &Buffer {
        &self.screen
    }

    pub(crate) fn cursor(&self) -> Option<Position> {
        self.shown.then_some(self.cursor)
    }

    pub(crate) fn take(&mut self) -> Changes {
        let mut cells = mem::take(&mut self.drawn);
        cells.sort_by_key(|at| (at.y, at.x));
        cells.dedup();

        let cleared = mem::take(&mut self.cleared);
        let cursor_moved = mem::take(&mut self.cursor_moved);
        Changes {
            cleared,
            cells,
            cursor_moved,
        }
    }

    /// Every cell, as a device that just joined needs them
    pub(crate) fn whole(&self) -> Changes {
        let mut cells = Vec::new();
        let mut covered = 0;
        for at in self.screen.area.positions() {
            if at.x == self.screen.area.x {
                covered = 0;
            }
            if covered > 0 {
                covered -= 1;
                continue;
            }
            covered = self.screen[at].symbol().cell_width().saturating_sub(1);
            cells.push(at);
        }
        Changes {
            cleared: true,
            cells,
            cursor_moved: true,
        }
    }
}

impl Backend for Grid {
    type Error = Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        for (x, y, cell) in content {
            let at = Position::new(x, y);
            if self.screen.area.contains(at) {
                self.screen[at] = cell.clone();
                self.drawn.push(at);
            }
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.cursor_moved |= self.shown;
        self.shown = false;
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.cursor_moved |= !self.shown;
        self.shown = true;
        Ok(())
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        let position = position.into();
        self.cursor_moved |= position != self.cursor;
        self.cursor = position;
        Ok(())
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.clear_region(ClearType::All)
    }

    /// A full-screen terminal only clears it all, after its size changed
    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        if clear_type == ClearType::All {
            let area = Rect::from((Position::ORIGIN, self.size));
            self.screen = Buffer::empty(area);
            self.drawn.clear();
            self.cleared = true;
        }
        Ok(())
    }

    fn size(&self) -> Result<Size, Self::Error> {
        Ok(self.size)
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        Ok(WindowSize {
            columns_rows: self.size,
            pixels: Size::default(),
        })
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// A frame for browsers and the Kitty images its tiles show, where `null` among the symbols of
/// a run is a column the wide symbol before it covers
pub(crate) fn encode(
    screen: &Buffer,
    changes: &Changes,
    cursor: Option<Position>,
    input: Rect,
) -> (String, Vec<u32>) {
    let mut styles = Vec::new();
    let mut numbered: HashMap<(Color, Color, Modifier), usize> = HashMap::new();
    let mut runs: Vec<(u16, u16, usize, Vec<Value>)> = Vec::new();
    let mut tiles = Vec::new();
    let mut images = Vec::new();

    for &at in &changes.cells {
        let cell = &screen[at];
        let tile = picture::tile(cell);
        let (symbol, key) = match tile {
            Some(tile) => {
                tiles.push(json!([
                    at.x,
                    at.y,
                    tile.image,
                    tile.columns,
                    tile.rows,
                    tile.column,
                    tile.row
                ]));
                if !images.contains(&tile.image) {
                    images.push(tile.image);
                }
                (" ", (Color::Reset, cell.bg, Modifier::empty()))
            }
            None => (cell.symbol(), (cell.fg, cell.bg, cell.modifier)),
        };

        let next = styles.len();
        let style = *numbered.entry(key).or_insert(next);
        if style == next {
            let (fg, bg, modifier) = key;
            styles.push(json!([hex(fg), hex(bg), modifier.bits()]));
        }

        let joins = match runs.last() {
            Some((x, y, last, symbols)) => {
                let end = usize::from(*x) + symbols.len();
                *y == at.y && *last == style && end == usize::from(at.x)
            }
            None => false,
        };
        if !joins {
            runs.push((at.x, at.y, style, Vec::new()));
        }

        let width = symbol.cell_width().max(1);
        let (_, _, _, symbols) = runs.last_mut().expect("run");
        symbols.push(Value::from(symbol));
        for _ in 1..width {
            symbols.push(Value::Null);
        }
    }

    let runs: Vec<Value> = runs
        .into_iter()
        .map(|(x, y, style, symbols)| json!([x, y, style, symbols]))
        .collect();
    let cursor = cursor.map(|at| [at.x, at.y]);
    let input = [input.x, input.y, input.width, input.height];
    let frame = json!({
        "t": "frame",
        "w": screen.area.width,
        "h": screen.area.height,
        "clear": changes.cleared,
        "styles": styles,
        "runs": runs,
        "tiles": tiles,
        "cursor": cursor,
        "input": input,
    });
    (frame.to_string(), images)
}

/// `null` leaves the page's own foreground or background
fn hex(color: Color) -> Value {
    let (red, green, blue) = match color {
        Color::Reset => return Value::Null,
        Color::Rgb(red, green, blue) => (red, green, blue),
        Color::Indexed(index) => indexed(index),
        Color::Black => ANSI[0],
        Color::Red => ANSI[1],
        Color::Green => ANSI[2],
        Color::Yellow => ANSI[3],
        Color::Blue => ANSI[4],
        Color::Magenta => ANSI[5],
        Color::Cyan => ANSI[6],
        Color::Gray => ANSI[7],
        Color::DarkGray => ANSI[8],
        Color::LightRed => ANSI[9],
        Color::LightGreen => ANSI[10],
        Color::LightYellow => ANSI[11],
        Color::LightBlue => ANSI[12],
        Color::LightMagenta => ANSI[13],
        Color::LightCyan => ANSI[14],
        Color::White => ANSI[15],
    };
    Value::from(format!("#{red:02x}{green:02x}{blue:02x}"))
}

/// The 256-color palette: the 16 colors, a 6×6×6 cube and a grey ramp
fn indexed(index: u8) -> (u8, u8, u8) {
    let level = |step: u8| match step {
        0 => 0,
        step => 55 + step * 40,
    };
    match index {
        0..=15 => ANSI[usize::from(index)],
        16..=231 => {
            let cube = index - 16;
            (level(cube / 36), level(cube / 6 % 6), level(cube % 6))
        }
        232..=255 => {
            let grey = 8 + (index - 232) * 10;
            (grey, grey, grey)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::tui::web::grid::{encode, hex, Grid};
    use ratatui::backend::Backend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::{Position, Rect, Size};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::Terminal;
    use serde_json::{json, Value};

    /// A wide symbol takes the column after it, and runs break where the style does
    #[test]
    fn frames_encoded() {
        let mut screen = Buffer::empty(Rect::new(0, 0, 6, 2));
        screen.set_string(0, 0, "a🐍é", Style::new());
        let bold = Style::new()
            .fg(Color::Rgb(0xBD, 0x93, 0xF9))
            .add_modifier(Modifier::BOLD);
        screen.set_string(4, 0, "x", bold);
        screen.set_string(1, 1, "naïve", Style::new());
        let mut grid = Grid::new(Size::new(6, 2));
        let mut cells = Vec::new();
        for (index, cell) in screen.content.iter().enumerate() {
            let (x, y) = screen.pos_of(index);
            cells.push((x, y, cell));
        }
        grid.draw(cells.into_iter()).expect("draw");

        let cursor = Some(Position::new(3, 1));
        let input = Rect::new(0, 1, 6, 1);
        let changes = grid.whole();
        let (frame, images) = encode(grid.screen(), &changes, cursor, input);
        let frame: Value = serde_json::from_str(&frame).expect("json");

        assert!(images.is_empty());
        assert_eq!(
            frame["styles"],
            json!([[null, null, 0], ["#bd93f9", null, 1]])
        );
        let runs = json!([
            [0, 0, 0, ["a", "🐍", null, "é"]],
            [4, 0, 1, ["x"]],
            [5, 0, 0, [" "]],
            [0, 1, 0, [" ", "n", "a", "ï", "v", "e"]],
        ]);
        assert_eq!(frame["runs"], runs);
        assert_eq!(frame["cursor"], json!([3, 1]));
        assert_eq!(frame["input"], json!([0, 1, 6, 1]));
        assert_eq!(
            (frame["w"].clone(), frame["clear"].clone()),
            (json!(6), json!(true))
        );
    }

    /// A redraw to the same screen changes nothing, and moving the cursor alone does
    #[test]
    fn redraws_compared() {
        let mut terminal = Terminal::new(Grid::new(Size::new(8, 2))).expect("terminal");
        let draw = |terminal: &mut Terminal<Grid>, column: u16| {
            let drawn = terminal.draw(|frame| {
                frame.render_widget("café", frame.area());
                frame.set_cursor_position(Position::new(column, 1));
            });
            drawn.expect("draw");
            terminal.backend_mut().take()
        };

        assert!(!draw(&mut terminal, 4).is_empty());
        assert!(draw(&mut terminal, 4).is_empty());
        let moved = draw(&mut terminal, 5);
        assert!(!moved.is_empty() && moved.cells.is_empty());
    }

    #[test]
    fn colors_named() {
        assert_eq!(hex(Color::Reset), Value::Null);
        assert_eq!(hex(Color::Indexed(196)), json!("#ff0000"));
        assert_eq!(hex(Color::Indexed(244)), json!("#808080"));
        assert_eq!(hex(Color::Indexed(9)), hex(Color::LightRed));
    }
}
