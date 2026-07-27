//! Keybindings via hjkl-keymap: vim chord notation, trie dispatch.

use hjkl_keymap::{KeyResolve, Keymap};
use std::time::Instant;

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum Mode {
    Normal,
}

#[derive(Clone, Debug)]
pub enum Action {
    Quit,
    TogglePause,
    /// Move down/up within the focused pane (GPU selection or process rows).
    NextItem,
    PrevItem,
    /// Explicit GPU selection regardless of focus (mouse wheel routing).
    NextGpu,
    PrevGpu,
    TickFaster,
    TickSlower,
    /// Focus the GPU pane and select GPU N; pressed again on the already
    /// selected GPU it folds/unfolds the card.
    Digit(usize),
    FocusProcs,
    ProcScrollDown,
    ProcScrollUp,
    /// Cycle the process-table sort column / flip its direction.
    SortCycle,
    SortReverse,
    /// Open the process filter input.
    FilterOpen,
    /// SIGTERM / SIGKILL the process under the cursor (with confirmation).
    KillTerm,
    KillForce,
    /// Open the help overlay (any key closes it).
    Help,
}

/// Single source of truth: feeds the keymap, the `?` overlay and the footer.
///
/// The 4th field is the footer chunk. It is set on the *first* chord of a
/// group only — `j/k move` covers both `j` and `k` — and `None` for chords
/// the one-line footer has no room for. Keeping it here means the footer
/// cannot drift out of sync with the bindings.
const BINDS: &[(&str, Action, &str, Option<&str>)] = &[
    ("q", Action::Quit, "quit", Some("q quit")),
    ("<Esc>", Action::Quit, "quit", None),
    ("<C-c>", Action::Quit, "quit", None),
    (
        "<Space>",
        Action::TogglePause,
        "pause/resume polling",
        Some("␣ pause"),
    ),
    (
        "p",
        Action::FocusProcs,
        "focus process list",
        Some("p procs"),
    ),
    ("?", Action::Help, "show this help", Some("? help")),
    (
        "j",
        Action::NextItem,
        "move down in focused list",
        Some("j/k move"),
    ),
    (
        "<Down>",
        Action::NextItem,
        "move down in focused list",
        None,
    ),
    ("k", Action::PrevItem, "move up in focused list", None),
    ("<Up>", Action::PrevItem, "move up in focused list", None),
    ("+", Action::TickFaster, "poll faster", Some("+/- rate")),
    // Unshifted alias: = shares the key with + on most layouts.
    ("=", Action::TickFaster, "poll faster", None),
    ("-", Action::TickSlower, "poll slower", None),
    (
        "s",
        Action::SortCycle,
        "cycle process sort column",
        Some("s sort"),
    ),
    (
        "r",
        Action::SortReverse,
        "reverse process sort",
        Some("r rev"),
    ),
    (
        "/",
        Action::FilterOpen,
        "filter processes",
        Some("/ filter"),
    ),
    (
        "x",
        Action::KillTerm,
        "terminate selected process",
        Some("x/X kill"),
    ),
    ("X", Action::KillForce, "kill -9 selected process", None),
    (
        "J",
        Action::ProcScrollDown,
        "scroll process list down",
        None,
    ),
    (
        "<PageDown>",
        Action::ProcScrollDown,
        "scroll process list down",
        None,
    ),
    ("K", Action::ProcScrollUp, "scroll process list up", None),
    (
        "<PageUp>",
        Action::ProcScrollUp,
        "scroll process list up",
        None,
    ),
];

/// The 0-9 GPU selection binds. Not in [`BINDS`] because the trie needs one
/// entry per digit, but the overlay and footer describe them as one row.
const DIGITS: (&str, &str, &str) = ("0-9", "focus/select GPU N; same digit folds it", "0-9 gpu");

/// Rows for the `?` overlay: the bind table (aliases folded together by
/// description) plus entries the trie can't express.
pub fn help_rows() -> Vec<(String, &'static str)> {
    let mut rows: Vec<(String, &'static str)> = Vec::new();
    for (chord, _, desc, _) in BINDS {
        if let Some(row) = rows.iter_mut().find(|(_, d)| d == desc) {
            row.0.push_str(&format!(" {chord}"));
        } else {
            rows.push((chord.to_string(), desc));
        }
    }
    rows.push((DIGITS.0.into(), DIGITS.1));
    rows.push(("y".into(), "confirm kill dialog (any other key cancels)"));
    rows.push(("wheel/click".into(), "scroll + focus pane; click selects"));
    rows
}

/// The persistent one-line footer, derived from [`BINDS`] so a new binding
/// never has to be transcribed by hand.
pub fn footer_hints() -> String {
    let mut out = String::new();
    for short in BINDS.iter().filter_map(|(_, _, _, s)| *s) {
        out.push(' ');
        out.push_str(short);
        out.push(' ');
    }
    out.push(' ');
    out.push_str(DIGITS.2);
    out
}

pub fn default_keymap() -> Keymap<Action, Mode> {
    let mut km = Keymap::new(' ');
    for (chord, action, desc, _) in BINDS {
        km.add(Mode::Normal, chord, action.clone(), desc)
            .expect("static chord parses");
    }
    for d in 0..10usize {
        km.add(Mode::Normal, &d.to_string(), Action::Digit(d), DIGITS.1)
            .expect("static chord parses");
    }
    km
}

/// Bridge a crossterm key through kitty normalization into the keymap.
pub fn resolve(km: &mut Keymap<Action, Mode>, ev: crossterm::event::KeyEvent) -> Option<Action> {
    let ev = hjkl_kitty::normalize_legacy(ev);
    let key = hjkl_keymap_tui::from_crossterm(&ev)?;
    match km.feed(Mode::Normal, key, Instant::now()) {
        KeyResolve::Match(binding) => Some(binding.action),
        KeyResolve::Pending | KeyResolve::Ambiguous | KeyResolve::Unbound(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footer_advertises_help_and_stays_one_line() {
        let f = footer_hints();
        assert!(f.contains("? help"), "help overlay must be discoverable");
        assert!(!f.contains('\n'));
        // Fits an 80-column terminal only when truncated; keep it sane for
        // the common 100+ column case.
        assert!(f.chars().count() <= 100, "footer too long: {}", f.len());
    }

    #[test]
    fn footer_chunks_come_from_binds() {
        // Literal chords lead their own footer chunk; named ones (<Space>)
        // get a glyph instead, so only check that they carry a hint at all.
        for (chord, _, _, short) in BINDS {
            if let Some(s) = short {
                assert!(
                    chord.starts_with('<') || s.starts_with(*chord),
                    "footer chunk {s:?} does not lead with its chord {chord:?}"
                );
                assert!(footer_hints().contains(s), "footer dropped {s:?}");
            }
        }
        // Nothing in the footer that isn't a binding.
        let footer = footer_hints();
        for chunk in footer.split("  ").map(str::trim) {
            assert!(
                chunk == DIGITS.2 || BINDS.iter().any(|(_, _, _, s)| *s == Some(chunk)),
                "footer chunk {chunk:?} has no binding"
            );
        }
    }

    #[test]
    fn help_rows_fold_aliases_and_cover_untried_keys() {
        let rows = help_rows();
        let quit = rows.iter().find(|(_, d)| *d == "quit").unwrap();
        assert_eq!(quit.0, "q <Esc> <C-c>");
        for chord in ["0-9", "y", "wheel/click", "?"] {
            assert!(
                rows.iter().any(|(k, _)| k.split(' ').any(|c| c == chord)),
                "help overlay missing {chord}"
            );
        }
    }
}
