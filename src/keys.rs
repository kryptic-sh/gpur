//! Keybindings via hjkl-keymap: vim chord notation, trie dispatch.

use hjkl_keymap::{KeyResolve, Keymap};
use std::sync::OnceLock;
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

impl Action {
    /// Whether the action works on the process pane, and so means nothing
    /// while config `show_processes = false` hides it.
    pub fn needs_procs(&self) -> bool {
        matches!(
            self,
            Action::FocusProcs
                | Action::ProcScrollDown
                | Action::ProcScrollUp
                | Action::SortCycle
                | Action::SortReverse
                | Action::FilterOpen
                | Action::KillTerm
                | Action::KillForce
        )
    }
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

type Bind = (&'static str, Action, &'static str, Option<&'static str>);

/// The binds worth advertising: all of them, or without the process-pane
/// ones when that pane is hidden and they would do nothing.
fn shown_binds(show_procs: bool) -> impl Iterator<Item = &'static Bind> {
    BINDS
        .iter()
        .filter(move |(_, action, _, _)| show_procs || !action.needs_procs())
}

/// Rows for the `?` overlay: the bind table (aliases folded together by
/// description) plus entries the trie can't express.
pub fn help_rows(show_procs: bool) -> Vec<(String, &'static str)> {
    let mut rows: Vec<(String, &'static str)> = Vec::new();
    for (chord, _, desc, _) in shown_binds(show_procs) {
        if let Some(row) = rows.iter_mut().find(|(_, d)| d == desc) {
            row.0.push_str(&format!(" {chord}"));
        } else {
            rows.push((chord.to_string(), desc));
        }
    }
    rows.push((DIGITS.0.into(), DIGITS.1));
    if show_procs {
        rows.push(("y".into(), "confirm kill dialog (any other key cancels)"));
    }
    rows.push(("wheel/click".into(), "scroll + focus pane; click selects"));
    rows
}

/// The persistent one-line footer, derived from [`BINDS`] so a new binding
/// never has to be transcribed by hand. Built once per variant: the binding
/// table is const, so re-formatting the same string every frame is pure waste.
pub fn footer_hints(show_procs: bool) -> &'static str {
    static HINTS: [OnceLock<String>; 2] = [OnceLock::new(), OnceLock::new()];
    HINTS[usize::from(show_procs)].get_or_init(|| {
        let mut out = String::new();
        for short in shown_binds(show_procs).filter_map(|(_, _, _, s)| *s) {
            out.push(' ');
            out.push_str(short);
            out.push(' ');
        }
        out.push(' ');
        out.push_str(DIGITS.2);
        out
    })
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
        let f = footer_hints(true);
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
                assert!(footer_hints(true).contains(s), "footer dropped {s:?}");
            }
        }
        // Nothing in the footer that isn't a binding.
        let footer = footer_hints(true);
        for chunk in footer.split("  ").map(str::trim) {
            assert!(
                chunk == DIGITS.2 || BINDS.iter().any(|(_, _, _, s)| *s == Some(chunk)),
                "footer chunk {chunk:?} has no binding"
            );
        }
    }

    /// The footer is built once and handed back as the same static string — a
    /// per-frame rebuild is the waste the cache exists to remove.
    #[test]
    fn the_footer_is_built_once() {
        for show_procs in [true, false] {
            assert!(
                std::ptr::eq(footer_hints(show_procs), footer_hints(show_procs)),
                "footer_hints({show_procs}) rebuilt its string"
            );
        }
    }

    /// `show_processes = false` turns every process-pane key into a no-op,
    /// so neither the footer nor the overlay may still offer one.
    #[test]
    fn a_hidden_process_pane_advertises_none_of_its_keys() {
        let footer = footer_hints(false);
        assert!(footer.contains("? help"), "{footer}");
        assert!(footer.contains("q quit"), "{footer}");
        let rows = help_rows(false);
        for (chord, action, desc, short) in BINDS {
            if !action.needs_procs() {
                continue;
            }
            if let Some(s) = short {
                assert!(!footer.contains(s), "footer still offers {s:?}");
                assert!(footer_hints(true).contains(s), "{s:?} never shown");
            }
            assert!(
                !rows.iter().any(|(_, d)| d == desc),
                "overlay still offers {chord} ({desc})"
            );
        }
        assert!(!rows.iter().any(|(k, _)| k == "y"), "kill dialog key shown");
        assert!(rows.iter().any(|(k, _)| k == "0-9"));
    }

    #[test]
    fn help_rows_fold_aliases_and_cover_untried_keys() {
        let rows = help_rows(true);
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
