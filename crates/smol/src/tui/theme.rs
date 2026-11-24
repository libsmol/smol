use crate::tui::Spans;
use ratatui::buffer::Cell;
use ratatui::style::{Color, Style};
use std::f32::consts::TAU;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;

pub(crate) const BG: Color = Color::Rgb(0x28, 0x2A, 0x36);
pub(crate) const BAND: Color = Color::Rgb(0x34, 0x37, 0x46);
pub(crate) const LINE: Color = Color::Rgb(0x44, 0x47, 0x5A);
pub(crate) const FG: Color = Color::Rgb(0xF8, 0xF8, 0xF2);
pub(crate) const COMMENT: Color = Color::Rgb(0x62, 0x72, 0xA4);
const PURPLE_RGB: (u8, u8, u8) = (0xBD, 0x93, 0xF9);
const PINK_RGB: (u8, u8, u8) = (0xFF, 0x79, 0xC6);
pub(crate) const PURPLE: Color = Color::Rgb(PURPLE_RGB.0, PURPLE_RGB.1, PURPLE_RGB.2);
pub(crate) const PINK: Color = Color::Rgb(PINK_RGB.0, PINK_RGB.1, PINK_RGB.2);
pub(crate) const CYAN: Color = Color::Rgb(0x8B, 0xE9, 0xFD);
pub(crate) const GREEN: Color = Color::Rgb(0x50, 0xFA, 0x7B);
pub(crate) const ORANGE: Color = Color::Rgb(0xFF, 0xB8, 0x6C);
pub(crate) const RED: Color = Color::Rgb(0xFF, 0x55, 0x55);
pub(crate) const YELLOW: Color = Color::Rgb(0xF1, 0xFA, 0x8C);

/// Intensity of each level of the 6×6×6 cube in the 256-color palette
const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];
/// How long the logo's colors take to go from purple to pink and back
const WAVE_EVERY: Duration = Duration::from_secs(4);
/// How many columns one cycle of the logo's colors spans
const WAVE_COLUMNS: f32 = 32.0;
/// How often the shimmer's light moves on to the next grapheme
const SHIMMER_EVERY: Duration = Duration::from_millis(70);
/// Graphemes' worth of dark between two passes of the shimmer
const SHIMMER_GAP: usize = 8;

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

pub(crate) fn wave(column: usize) -> Color {
    let seconds = START.elapsed().as_secs_f32();
    let phase = seconds / WAVE_EVERY.as_secs_f32() - column as f32 / WAVE_COLUMNS;
    let t = (1.0 - (phase * TAU).cos()) / 2.0;

    let ((r1, g1, b1), (r2, g2, b2)) = (PURPLE_RGB, PINK_RGB);
    let channel = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
    let (r, g, b) = (channel(r1, r2), channel(g1, g2), channel(b1, b2));
    Color::Rgb(r, g, b)
}

/// One span per grapheme, as ratatui drops a span of a zero-width character such as a joiner
pub(crate) fn shimmer(text: &str, base: Style, started: Instant) -> Spans {
    let graphemes: Vec<&str> = text.graphemes(true).collect();
    let count = graphemes.len();
    let steps = started.elapsed().as_millis() / SHIMMER_EVERY.as_millis();
    let at = steps as usize % (count + SHIMMER_GAP);

    let mut spans = Vec::new();
    for (index, grapheme) in graphemes.into_iter().enumerate() {
        let style = match index.abs_diff(at) {
            0 => base.fg(FG),
            1 => base.fg(PURPLE),
            _ => base,
        };
        spans.push((grapheme.to_owned(), style));
    }
    spans
}

pub(crate) fn downgrade(cell: &mut Cell) {
    cell.fg = indexed(cell.fg);
    cell.bg = indexed(cell.bg);
}

fn indexed(color: Color) -> Color {
    let Color::Rgb(r, g, b) = color else {
        return color;
    };

    let level = |value: u8| {
        let mut best = 0;
        for (index, step) in CUBE.iter().enumerate() {
            if step.abs_diff(value) < CUBE[best].abs_diff(value) {
                best = index;
            }
        }
        best
    };
    let (i, j, k) = (level(r), level(g), level(b));
    let cube = (CUBE[i], CUBE[j], CUBE[k]);

    // The grey ramp has 24 levels from 8 to 238 in steps of 10
    let average = (u32::from(r) + u32::from(g) + u32::from(b)) / 3;
    let grey = (average.saturating_sub(3) / 10).min(23) as usize;
    let value = (8 + grey * 10) as u8;

    let distance = |(x, y, z): (u8, u8, u8)| {
        let square = |a: u8, b: u8| u32::from(a.abs_diff(b)).pow(2);
        square(x, r) + square(y, g) + square(z, b)
    };
    let index = match distance(cube) <= distance((value, value, value)) {
        true => 16 + 36 * i + 6 * j + k,
        false => 232 + grey,
    };
    Color::Indexed(index as u8)
}
