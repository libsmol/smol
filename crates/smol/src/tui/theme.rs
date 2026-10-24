use crate::tui::Spans;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Style};
use std::f32::consts::TAU;
use std::sync::LazyLock;
use std::time::Instant;

pub(crate) const BG: Color = Color::Rgb(0x28, 0x2A, 0x36);
pub(crate) const BAND: Color = Color::Rgb(0x34, 0x37, 0x46);
pub(crate) const LINE: Color = Color::Rgb(0x44, 0x47, 0x5A);
pub(crate) const FG: Color = Color::Rgb(0xF8, 0xF8, 0xF2);
pub(crate) const COMMENT: Color = Color::Rgb(0x62, 0x72, 0xA4);
pub(crate) const PURPLE: Color = Color::Rgb(0xBD, 0x93, 0xF9);
pub(crate) const PINK: Color = Color::Rgb(0xFF, 0x79, 0xC6);
pub(crate) const CYAN: Color = Color::Rgb(0x8B, 0xE9, 0xFD);
pub(crate) const GREEN: Color = Color::Rgb(0x50, 0xFA, 0x7B);
pub(crate) const ORANGE: Color = Color::Rgb(0xFF, 0xB8, 0x6C);
pub(crate) const RED: Color = Color::Rgb(0xFF, 0x55, 0x55);
pub(crate) const YELLOW: Color = Color::Rgb(0xF1, 0xFA, 0x8C);

/// Intensity of each level of the 6×6×6 cube in the 256-color palette
const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

pub(crate) fn wave(column: usize) -> Color {
    let seconds = START.elapsed().as_secs_f32();
    let phase = seconds / 4.0 - column as f32 / 32.0;
    let t = (1.0 - (phase * TAU).cos()) / 2.0;
    mix(PURPLE, PINK, t)
}

pub(crate) fn shimmer(text: &str, base: Style, started: Instant) -> Spans {
    let count = text.chars().count();
    let at = (started.elapsed().as_millis() / 70) as usize % (count + 8);

    let mut spans = Vec::new();
    for (index, c) in text.chars().enumerate() {
        let style = match index.abs_diff(at) {
            0 => base.fg(FG),
            1 => base.fg(PURPLE),
            _ => base,
        };
        spans.push((c.to_string(), style));
    }
    spans
}

fn mix(from: Color, to: Color, t: f32) -> Color {
    let (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) = (from, to) else {
        return from;
    };

    let channel = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
    let (r, g, b) = (channel(r1, r2), channel(g1, g2), channel(b1, b2));
    Color::Rgb(r, g, b)
}

pub(crate) fn downgrade(buffer: &mut Buffer) {
    for cell in &mut buffer.content {
        cell.fg = indexed(cell.fg);
        cell.bg = indexed(cell.bg);
    }
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

#[cfg(test)]
mod tests {
    use crate::tui::theme::*;

    #[test]
    fn colors_downgraded() {
        let pairs = [
            (BG, 236),
            (LINE, 239),
            (FG, 255),
            (COMMENT, 61),
            (PURPLE, 141),
            (PINK, 212),
            // An average ending in 0 is nearer the grey step below it
            (Color::Rgb(0x50, 0x50, 0x50), 239),
        ];
        for (color, index) in pairs {
            assert_eq!(indexed(color), Color::Indexed(index), "{color}");
        }
        assert_eq!(indexed(Color::Reset), Color::Reset);
    }
}
