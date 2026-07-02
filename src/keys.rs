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
    NextGpu,
    PrevGpu,
    TickFaster,
    TickSlower,
    ToggleFold(usize),
    ProcScrollDown,
    ProcScrollUp,
}

pub fn default_keymap() -> Keymap<Action, Mode> {
    let mut km = Keymap::new(' ');
    let binds: &[(&str, Action, &str)] = &[
        ("q", Action::Quit, "quit"),
        ("<Esc>", Action::Quit, "quit"),
        ("<C-c>", Action::Quit, "quit"),
        ("p", Action::TogglePause, "pause/resume polling"),
        ("j", Action::NextGpu, "select next GPU"),
        ("<Down>", Action::NextGpu, "select next GPU"),
        ("k", Action::PrevGpu, "select previous GPU"),
        ("<Up>", Action::PrevGpu, "select previous GPU"),
        ("+", Action::TickFaster, "poll faster"),
        ("-", Action::TickSlower, "poll slower"),
        ("J", Action::ProcScrollDown, "scroll process list down"),
        (
            "<PageDown>",
            Action::ProcScrollDown,
            "scroll process list down",
        ),
        ("K", Action::ProcScrollUp, "scroll process list up"),
        ("<PageUp>", Action::ProcScrollUp, "scroll process list up"),
    ];
    for (chord, action, desc) in binds {
        km.add(Mode::Normal, chord, action.clone(), desc)
            .expect("static chord parses");
    }
    for d in 0..10usize {
        km.add(
            Mode::Normal,
            &d.to_string(),
            Action::ToggleFold(d),
            "fold/unfold GPU card",
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
