//! Maps an hjkl-theme `Theme` onto the handful of styles gpur draws with.

use anyhow::Context;
use hjkl_theme::{Color, Theme};
use hjkl_theme_tui::ToRatatui;
use ratatui::style::{Color as RColor, Modifier, Style};
use std::path::Path;

pub struct UiTheme {
    pub fg: RColor,
    pub bg: RColor,
    pub border: Style,
    pub border_selected: Style,
    pub title: Style,
    pub dim: Style,
    pub gauge_vram: Style,
    pub spark_util: Style,
    pub spark_power: Style,
    pub temp_ok: Style,
    pub temp_warn: Style,
    pub temp_crit: Style,
    pub accent: RColor,
}

pub fn load(path: Option<&Path>) -> anyhow::Result<UiTheme> {
    let theme = match path {
        Some(p) => hjkl_theme::loader::load_from_path(p)
            .with_context(|| format!("loading theme {}", p.display()))?,
        None => hjkl_theme::loader::default_theme(),
    };
    Ok(UiTheme::from_theme(&theme))
}

impl UiTheme {
    fn from_theme(t: &Theme) -> Self {
        // Palette names follow the built-in (Catppuccin-Mocha-ish) theme; every
        // lookup carries a hard fallback so any palette works.
        let fg =
            t.ui.foreground
                .unwrap_or_else(|| pal(t, &["text"], Color::rgb(0xcd, 0xd6, 0xf4)));
        let bg =
            t.ui.background
                .unwrap_or_else(|| pal(t, &["base"], Color::rgb(0x1e, 0x1e, 0x2e)));
        let accent = pal(t, &["mauve", "blue"], Color::rgb(0xcb, 0xa6, 0xf7));
        let green = pal(t, &["green"], Color::rgb(0xa6, 0xe3, 0xa1));
        let yellow = pal(t, &["yellow", "peach"], Color::rgb(0xf9, 0xe2, 0xaf));
        let red = pal(t, &["red"], Color::rgb(0xf3, 0x8b, 0xa8));
        let blue = pal(t, &["blue", "sky"], Color::rgb(0x89, 0xb4, 0xfa));
        let teal = pal(t, &["teal", "sky"], Color::rgb(0x94, 0xe2, 0xd5));
        let dim = pal(t, &["overlay0", "surface2"], Color::rgb(0x6c, 0x70, 0x86));

        Self {
            fg: fg.to_ratatui(),
            bg: bg.to_ratatui(),
            border: Style::new().fg(dim.to_ratatui()),
            border_selected: Style::new().fg(accent.to_ratatui()),
            title: Style::new()
                .fg(accent.to_ratatui())
                .add_modifier(Modifier::BOLD),
            dim: Style::new().fg(dim.to_ratatui()),
            gauge_vram: Style::new().fg(blue.to_ratatui()),
            spark_util: Style::new().fg(green.to_ratatui()),
            spark_power: Style::new().fg(teal.to_ratatui()),
            temp_ok: Style::new().fg(green.to_ratatui()),
            temp_warn: Style::new().fg(yellow.to_ratatui()),
            temp_crit: Style::new().fg(red.to_ratatui()),
            accent: accent.to_ratatui(),
        }
    }

    /// Gradient stops for utilization-like meters/graphs: cool at the start,
    /// hot at the end.
    pub fn util_stops(&self) -> [(u8, u8, u8); 3] {
        [
            rgb_of(self.spark_util.fg, (0xa6, 0xe3, 0xa1)),
            rgb_of(self.temp_warn.fg, (0xf9, 0xe2, 0xaf)),
            rgb_of(self.temp_crit.fg, (0xf3, 0x8b, 0xa8)),
        ]
    }

    /// Gradient stops for memory meters/graphs.
    pub fn vram_stops(&self) -> [(u8, u8, u8); 2] {
        [
            rgb_of(self.gauge_vram.fg, (0x89, 0xb4, 0xfa)),
            rgb_of(Some(self.accent), (0xcb, 0xa6, 0xf7)),
        ]
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

/// Piecewise-linear interpolation through `stops` at `frac` in 0..=1.
pub fn gradient(stops: &[(u8, u8, u8)], frac: f64) -> RColor {
    let seg = frac.clamp(0.0, 1.0) * (stops.len() - 1) as f64;
    let i = (seg.floor() as usize).min(stops.len().saturating_sub(2));
    let f = seg - i as f64;
    let (a, b) = (stops[i], stops[i + 1]);
    let lerp = |x: u8, y: u8| -> u8 { (x as f64 + (y as f64 - x as f64) * f).round() as u8 };
    RColor::Rgb(lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2))
}

fn pal(t: &Theme, names: &[&str], fallback: Color) -> Color {
    names
        .iter()
        .find_map(|n| t.palette.get(*n).copied())
        .unwrap_or(fallback)
}
