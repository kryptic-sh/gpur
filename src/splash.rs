//! Startup splash: hjkl-splash animation over the gpur figlet art.

use crate::theme::UiTheme;
use hjkl_splash::{CellKind, Layout, Rgb, Splash, default_trail_color};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Paragraph;

pub const ART: &str = include_str!("art.txt");

fn art_dims() -> (u16, u16) {
    let rows = ART.lines().count() as u16;
    let cols = ART.lines().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
    (rows, cols)
}

/// Cursor path: sweep the art's glyph cells left-to-right, top-to-bottom.
pub fn build_path() -> Vec<(u8, u8, char)> {
    let mut path = Vec::new();
    for (r, line) in ART.lines().enumerate() {
        for (c, ch) in line.chars().enumerate() {
            if !ch.is_whitespace() {
                path.push((r as u8, c as u8, ch));
            }
        }
    }
    path
}

pub fn render(
    frame: &mut Frame,
    area: Rect,
    anchor: std::time::Instant,
    path: &[(u8, u8, char)],
    theme: &UiTheme,
) {
    let splash = Splash::new(ART, path).with_anchor(anchor);
    let (rows, cols) = art_dims();
    let layout = Layout::centered(area.width, area.height, rows, cols);

    let buf = frame.buffer_mut();
    for cell in splash.cells(layout) {
        if cell.x >= area.width || cell.y >= area.height {
            continue;
        }
        let style = match cell.kind {
            CellKind::Art => Style::new().fg(theme.accent),
            CellKind::Trail { age } => {
                let Rgb(r, g, b) = default_trail_color(age);
                Style::new().fg(Color::Rgb(r, g, b))
            }
            CellKind::Cursor => Style::new().fg(theme.fg).add_modifier(Modifier::BOLD),
        };
        if let Some(c) = buf.cell_mut((area.x + cell.x, area.y + cell.y)) {
            c.set_char(cell.ch);
            c.set_style(style);
        }
    }

    let ver_y = layout.origin_y.saturating_add(rows).saturating_add(1);
    if ver_y < area.height {
        let line = Rect::new(area.x, area.y + ver_y, area.width, 1);
        frame.render_widget(
            Paragraph::new(format!("gpur v{}", env!("CARGO_PKG_VERSION")))
                .alignment(Alignment::Center)
                .style(theme.dim),
            line,
        );
    }
}
