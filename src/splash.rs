//! Startup splash: hjkl-splash animation over the gpur figlet art.

use crate::theme::UiTheme;
use hjkl_splash::{CellKind, Layout, Rgb, Splash, default_trail_color};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Paragraph;

pub const ART: &str = include_str!("art.txt");

fn art_dims() -> (u16, u16) {
    let rows = ART.lines().count() as u16;
    let cols = ART.lines().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
    (rows, cols)
}

/// Rows and longest-line columns of [`ART`], counted at compile time.
///
/// This walks the raw bytes instead of calling `art_dims` because `str::lines`
/// and `str::chars` are not const, and the numbers have to be available to the
/// `const` assertions below. Characters are counted the UTF-8 way — a byte
/// that is not a continuation byte starts a new character — because the art is
/// drawn in U+2588 FULL BLOCK, three bytes per column, so a raw byte count
/// would report today's 33-column banner as 99 columns. The one place this
/// still overcounts is a CRLF-saved file, where the CR that `str::lines` strips
/// is counted as a column; that only ever overstates the width, which is the
/// harmless direction for an upper bound.
const fn art_bounds() -> (usize, usize) {
    let bytes = ART.as_bytes();
    let mut rows = 0;
    let mut cols = 0;
    let mut line = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            rows += 1;
            if line > cols {
                cols = line;
            }
            line = 0;
        } else if bytes[i] & 0xC0 != 0x80 {
            line += 1;
        }
        i += 1;
    }
    // Art that does not end in a newline still has that last line, and
    // `str::lines` yields it, so it has to be measured here too.
    if line > 0 {
        rows += 1;
        if line > cols {
            cols = line;
        }
    }
    (rows, cols)
}

const ART_BOUNDS: (usize, usize) = art_bounds();

// `Splash::new` addresses cells with `u8` coordinates, so `build_path` has to
// cast every row and column down to `u8`. Art taller or wider than 255 cells
// would wrap those casts silently — no panic and no error, just a cursor trail
// scattered across cells it never visited — so refuse to compile such art at
// all rather than shipping a splash that is quietly wrong.
const _: () = assert!(
    ART_BOUNDS.0 <= u8::MAX as usize,
    "src/art.txt is taller than 255 rows: hjkl_splash takes the cursor path as \
     &[(u8, u8, char)], so splash::build_path cannot address those rows"
);
const _: () = assert!(
    ART_BOUNDS.1 <= u8::MAX as usize,
    "src/art.txt has a line longer than 255 columns: hjkl_splash takes the cursor \
     path as &[(u8, u8, char)], so splash::build_path cannot address those columns"
);

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
                // The trail colour comes from hjkl_splash as raw RGB, so it has
                // to be quantized like every other colour or a 16/256-colour
                // terminal gets a truecolor escape it can't render.
                let Rgb(r, g, b) = default_trail_color(age);
                Style::new().fg(crate::theme::paint(theme.mode, (r, g, b)))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The compile-time ceiling is only worth anything if it measures the same
    /// art that `build_path` walks; a const scan that drifted from `str::lines`
    /// would guard numbers nobody uses.
    #[test]
    fn const_art_bounds_agree_with_the_runtime_dimensions() {
        let (rows, cols) = art_dims();
        assert_eq!(ART_BOUNDS, (rows as usize, cols as usize));
    }

    /// Every coordinate the path emits has already been squeezed through `u8`.
    /// Casting it back has to land on the very cell it was read from — the
    /// moment art outgrows 255 columns, a wrapped column silently names some
    /// other glyph and this is what notices.
    #[test]
    fn build_path_coordinates_still_name_their_own_cells_after_the_u8_cast() {
        let grid: Vec<Vec<char>> = ART.lines().map(|l| l.chars().collect()).collect();
        let path = build_path();

        for &(r, c, ch) in &path {
            assert_eq!(
                grid.get(r as usize).and_then(|l| l.get(c as usize)),
                Some(&ch)
            );
        }

        let glyphs = grid
            .iter()
            .flatten()
            .filter(|ch| !ch.is_whitespace())
            .count();
        assert_eq!(path.len(), glyphs);
    }
}
