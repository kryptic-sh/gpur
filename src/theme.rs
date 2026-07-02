//! Maps an hjkl-theme `Theme` onto the handful of styles gpur draws with.

use anyhow::Context;
use hjkl_theme::{Color, Theme};
use ratatui::style::{Color as RColor, Modifier, Style};
use std::path::Path;

/// How much color the terminal gets. Truecolor is the native path; the
/// others quantize at style-construction time so the rest of the app never
/// thinks about it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorMode {
    Truecolor,
    Ansi256,
    Ansi16,
    /// NO_COLOR / TERM=dumb: glyphs and bold only.
    Mono,
}

/// Honor NO_COLOR (https://no-color.org/), then sniff COLORTERM/TERM.
pub fn detect_color_mode() -> ColorMode {
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return ColorMode::Mono;
    }
    let term = std::env::var("TERM").unwrap_or_default();
    if term == "dumb" {
        return ColorMode::Mono;
    }
    let colorterm = std::env::var("COLORTERM").unwrap_or_default();
    if colorterm == "truecolor" || colorterm == "24bit" {
        return ColorMode::Truecolor;
    }
    if term.contains("256color") {
        return ColorMode::Ansi256;
    }
    ColorMode::Ansi16
}

/// Quantize an RGB triple for the active mode.
pub fn paint(mode: ColorMode, (r, g, b): (u8, u8, u8)) -> RColor {
    match mode {
        ColorMode::Truecolor => RColor::Rgb(r, g, b),
        ColorMode::Ansi256 => RColor::Indexed(rgb_to_256(r, g, b)),
        ColorMode::Ansi16 => RColor::Indexed(rgb_to_16(r, g, b)),
        ColorMode::Mono => RColor::Reset,
    }
}

/// xterm 256-color: 6x6x6 cube (16..231) with the gray ramp (232..255) for
/// near-neutral colors.
fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    let (ri, gi, bi) = (r as i32, g as i32, b as i32);
    let spread = ri.max(gi).max(bi) - ri.min(gi).min(bi);
    if spread < 12 {
        let v = (ri + gi + bi) / 3;
        return if v < 8 {
            16
        } else if v > 238 {
            231
        } else {
            (232 + (v - 8) / 10) as u8
        };
    }
    let c = |v: i32| -> i32 { (v * 5 + 127) / 255 };
    (16 + 36 * c(ri) + 6 * c(gi) + c(bi)) as u8
}

/// Basic 16: dominant-channel bits + bright bit.
fn rgb_to_16(r: u8, g: u8, b: u8) -> u8 {
    let base = u8::from(r > 96) | (u8::from(g > 96) << 1) | (u8::from(b > 96) << 2);
    let bright = r.max(g).max(b) > 192;
    base + if bright { 8 } else { 0 }
}

pub struct UiTheme {
    pub mode: ColorMode,
    pub fg: RColor,
    pub bg: RColor,
    pub border: Style,
    pub border_selected: Style,
    pub title: Style,
    pub dim: Style,
    pub spark_util: Style,
    pub spark_power: Style,
    pub temp_ok: Style,
    pub temp_warn: Style,
    pub temp_crit: Style,
    pub accent: RColor,
    /// Cursor-row highlight for lists.
    pub selection: Style,
    util_stops: [(u8, u8, u8); 3],
    vram_stops: [(u8, u8, u8); 2],
}

pub fn load(path: Option<&Path>, mode: ColorMode) -> anyhow::Result<UiTheme> {
    let theme = match path {
        Some(p) => hjkl_theme::loader::load_from_path(p)
            .with_context(|| format!("loading theme {}", p.display()))?,
        None => hjkl_theme::loader::default_theme(),
    };
    Ok(UiTheme::from_theme(&theme, mode))
}

impl UiTheme {
    fn from_theme(t: &Theme, mode: ColorMode) -> Self {
        // Palette names follow the built-in (Catppuccin-Mocha-ish) theme; every
        // lookup carries a hard fallback so any palette works.
        let fg =
            t.ui.foreground
                .unwrap_or_else(|| pal(t, &["text"], Color::rgb(0xcd, 0xd6, 0xf4)));
        let bg =
            t.ui.background
                .unwrap_or_else(|| pal(t, &["base"], Color::rgb(0x1e, 0x1e, 0x2e)));
        let accent = rgb(pal(t, &["mauve", "blue"], Color::rgb(0xcb, 0xa6, 0xf7)));
        let green = rgb(pal(t, &["green"], Color::rgb(0xa6, 0xe3, 0xa1)));
        let yellow = rgb(pal(t, &["yellow", "peach"], Color::rgb(0xf9, 0xe2, 0xaf)));
        let red = rgb(pal(t, &["red"], Color::rgb(0xf3, 0x8b, 0xa8)));
        let blue = rgb(pal(t, &["blue", "sky"], Color::rgb(0x89, 0xb4, 0xfa)));
        let teal = rgb(pal(t, &["teal", "sky"], Color::rgb(0x94, 0xe2, 0xd5)));
        let dim = rgb(pal(
            t,
            &["overlay0", "surface2"],
            Color::rgb(0x6c, 0x70, 0x86),
        ));
        let surface = rgb(pal(
            t,
            &["surface1", "surface0"],
            Color::rgb(0x45, 0x47, 0x5a),
        ));
        let p = |c: (u8, u8, u8)| paint(mode, c);

        Self {
            mode,
            fg: p(rgb(fg)),
            bg: if mode == ColorMode::Mono {
                RColor::Reset
            } else {
                p(rgb(bg))
            },
            border: Style::new().fg(p(dim)),
            border_selected: Style::new().fg(p(accent)).add_modifier(Modifier::BOLD),
            title: Style::new().fg(p(accent)).add_modifier(Modifier::BOLD),
            dim: if mode == ColorMode::Mono {
                Style::new().add_modifier(Modifier::DIM)
            } else {
                Style::new().fg(p(dim))
            },
            spark_util: Style::new().fg(p(green)),
            spark_power: Style::new().fg(p(teal)),
            temp_ok: Style::new().fg(p(green)),
            temp_warn: Style::new().fg(p(yellow)).add_modifier(Modifier::BOLD),
            temp_crit: Style::new().fg(p(red)).add_modifier(Modifier::BOLD),
            accent: p(accent),
            // A background highlight is invisible in mono; reverse instead.
            selection: if mode == ColorMode::Mono {
                Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::new().bg(p(surface)).add_modifier(Modifier::BOLD)
            },
            util_stops: [green, yellow, red],
            vram_stops: [blue, accent],
        }
    }

    /// Gradient stops for utilization-like meters/graphs: cool at the start,
    /// hot at the end. Raw RGB — quantization happens in `gradient`.
    pub fn util_stops(&self) -> [(u8, u8, u8); 3] {
        self.util_stops
    }

    /// Gradient stops for memory meters/graphs.
    pub fn vram_stops(&self) -> [(u8, u8, u8); 2] {
        self.vram_stops
    }

    pub fn temp_style(&self, c: f64) -> Style {
        if c >= 90.0 {
            self.temp_crit
        } else if c >= 75.0 {
            self.temp_warn
        } else {
            self.temp_ok
        }
    }
}

pub fn rgb_of(c: Option<RColor>, fallback: (u8, u8, u8)) -> (u8, u8, u8) {
    match c {
        Some(RColor::Rgb(r, g, b)) => (r, g, b),
        _ => fallback,
    }
}

fn rgb(c: Color) -> (u8, u8, u8) {
    (c.r, c.g, c.b)
}

/// Piecewise-linear interpolation through `stops` at `frac` in 0..=1,
/// quantized for the active color mode.
pub fn gradient(stops: &[(u8, u8, u8)], frac: f64, mode: ColorMode) -> RColor {
    let seg = frac.clamp(0.0, 1.0) * (stops.len() - 1) as f64;
    let i = (seg.floor() as usize).min(stops.len().saturating_sub(2));
    let f = seg - i as f64;
    let (a, b) = (stops[i], stops[i + 1]);
    let lerp = |x: u8, y: u8| -> u8 { (x as f64 + (y as f64 - x as f64) * f).round() as u8 };
    paint(mode, (lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2)))
}

fn pal(t: &Theme, names: &[&str], fallback: Color) -> Color {
    names
        .iter()
        .find_map(|n| t.palette.get(*n).copied())
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantizers_hit_expected_ranges() {
        assert_eq!(rgb_to_256(0, 0, 0), 16);
        assert_eq!(rgb_to_256(255, 255, 255), 231);
        assert_eq!(rgb_to_256(128, 128, 128), 232 + (128 - 8) / 10); // gray ramp
        assert_eq!(rgb_to_256(255, 0, 0), 16 + 36 * 5); // pure red corner
        assert_eq!(rgb_to_16(255, 60, 60), 9); // bright red
        assert_eq!(rgb_to_16(60, 120, 60), 2); // dim green
    }

    #[test]
    fn mono_paints_reset() {
        assert_eq!(paint(ColorMode::Mono, (255, 0, 0)), RColor::Reset);
        assert_eq!(
            gradient(&[(0, 0, 0), (255, 255, 255)], 0.5, ColorMode::Mono),
            RColor::Reset
        );
    }
}
