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

/// Single source of truth: feeds both the keymap and the `?` overlay.
const BINDS: &[(&str, Action, &str)] = &[
    ("q", Action::Quit, "quit"),
    ("<Esc>", Action::Quit, "quit"),
    ("<C-c>", Action::Quit, "quit"),
    ("<Space>", Action::TogglePause, "pause/resume polling"),
    ("p", Action::FocusProcs, "focus process list"),
    ("j", Action::NextItem, "move down in focused list"),
    ("<Down>", Action::NextItem, "move down in focused list"),
    ("k", Action::PrevItem, "move up in focused list"),
    ("<Up>", Action::PrevItem, "move up in focused list"),
    ("+", Action::TickFaster, "poll faster"),
    // Unshifted alias: = shares the key with + on most layouts.
    ("=", Action::TickFaster, "poll faster"),
    ("-", Action::TickSlower, "poll slower"),
    ("s", Action::SortCycle, "cycle process sort column"),
    ("r", Action::SortReverse, "reverse process sort"),
    ("/", Action::FilterOpen, "filter processes"),
    ("x", Action::KillTerm, "terminate selected process"),
    ("X", Action::KillForce, "kill -9 selected process"),
    ("J", Action::ProcScrollDown, "scroll process list down"),
    (
        "<PageDown>",
        Action::ProcScrollDown,
        "scroll process list down",
    ),
    ("K", Action::ProcScrollUp, "scroll process list up"),
    ("<PageUp>", Action::ProcScrollUp, "scroll process list up"),
    ("?", Action::Help, "show this help"),
];

/// Rows for the `?` overlay: the bind table (aliases folded together by
/// description) plus entries the trie can't express.
pub fn help_rows() -> Vec<(String, &'static str)> {
    let mut rows: Vec<(String, &'static str)> = Vec::new();
    for (chord, _, desc) in BINDS {
        if let Some(row) = rows.iter_mut().find(|(_, d)| d == desc) {
            row.0.push_str(&format!(" {chord}"));
        } else {
            rows.push((chord.to_string(), desc));
        }
    }
    rows.push(("0-9".into(), "focus/select GPU N; same digit folds it"));
    rows.push(("y".into(), "confirm kill dialog (any other key cancels)"));
    rows.push(("wheel/click".into(), "scroll + focus pane; click selects"));
    rows
}

pub fn default_keymap() -> Keymap<Action, Mode> {
    let mut km = Keymap::new(' ');
    for (chord, action, desc) in BINDS {
        km.add(Mode::Normal, chord, action.clone(), desc)
            .expect("static chord parses");
    }
    for d in 0..10usize {
        km.add(
            Mode::Normal,
            &d.to_string(),
            Action::Digit(d),
            "focus/select GPU N, again to fold",
        )
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
