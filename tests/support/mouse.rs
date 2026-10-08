use super::*;

// --- mouse input ---------------------------------------------------------
//
// `--mock 4` publishes 12 process rows, of which 7 fit at 120x36: enough
// for a click or a wheel to be able to scroll the window, which is what the
// hit test's bounds are about. Every coordinate below is read off the
// rendered screen so a layout change moves the tests with it.

/// Ordering barrier for the negative assertions. `-` halves the poll rate,
/// and the app reads its input in order, so once the new rate is on screen
/// every byte sent before it has been consumed — where waiting a fixed
/// delay would only be a guess. It touches nothing else the tests look at.
fn tick_marker(t: &mut Tui, now_ms: u64) {
    t.send("-");
    let needle = format!("{now_ms}ms");
    t.wait_for("the tick marker", move |s| s.contains(&needle));
}

/// A click selects the row under the cursor and focuses the pane. The last
/// visible row is the interesting one: it borders the frame, which
/// `Rect::contains` counts as part of the pane.
#[test]
fn click_selects_the_process_row_under_the_cursor() {
    let mut t = Tui::spawn(&["--mock", "4"]);
    t.wait_for("a windowed table", |s| proc_window(s) == Some((1, 7, 12)));
    let rows = wait_for_procs(&mut t, 7);
    let (last_y, last_pid) = *rows.last().expect("data rows");
    t.click(10, last_y);
    wait_for_selected_pid(&mut t, last_pid, "the clicked row to be selected");
    assert_eq!(
        wait_for_procs(&mut t, 7),
        rows,
        "clicking a visible row scrolled the table"
    );
    // Focus followed the click, so j walks the process list from there —
    // one past the last visible row, which does scroll.
    t.send("j");
    t.wait_for("the cursor to move on within the process pane", |s| {
        proc_window(s) == Some((2, 8, 12))
    });
    t.send("q");
    assert!(t.wait_exit().success());
}

/// The pane's bottom border is inside `proc_rect` as far as
/// `Rect::contains` is concerned. Clicking it must select nothing: bounded
/// by the row count instead of by the window, it picked a row that was
/// never on screen and scrolled the view to reveal it.
#[test]
fn click_on_the_process_pane_border_selects_nothing() {
    let mut t = Tui::spawn(&["--mock", "4"]);
    t.wait_for("a windowed table", |s| proc_window(s) == Some((1, 7, 12)));
    let rows = wait_for_procs(&mut t, 7);
    let before_sel = wait_for_selection(&mut t);
    let border_y = rows.last().expect("data rows").0 + 1;
    let border = row_text(t.parser.screen(), border_y);
    assert!(
        border.starts_with('╰'),
        "row {border_y} is not the pane's bottom border: {border:?}"
    );
    t.click(10, border_y);
    tick_marker(&mut t, 200);
    assert_eq!(
        proc_window(&t.screen_text()),
        Some((1, 7, 12)),
        "a click on the border scrolled the table"
    );
    assert_eq!(wait_for_procs(&mut t, 7), rows);
    assert_eq!(
        wait_for_selection(&mut t),
        before_sel,
        "a click on the border moved the cursor"
    );
    t.send("q");
    assert!(t.wait_exit().success());
}

/// The wheel over the process pane walks the cursor, and clamps at both
/// ends rather than wrapping or running off the list.
#[test]
fn wheel_over_the_process_pane_scrolls_and_clamps() {
    let mut t = Tui::spawn(&["--mock", "4"]);
    t.wait_for("a windowed table", |s| proc_window(s) == Some((1, 7, 12)));
    let rows = wait_for_procs(&mut t, 7);
    let (first_pid, second_pid) = (rows[0].1, rows[1].1);
    let y = rows[3].0; // mid-pane: never a border, whatever the window shows

    // The cursor starts on the first row, so wheel-up has nowhere to go.
    t.mouse(WHEEL_UP, 10, y);
    tick_marker(&mut t, 200);
    assert_eq!(
        wait_for_selection(&mut t),
        first_pid,
        "wheel-up at the top of the list moved the cursor"
    );
    assert_eq!(proc_window(&t.screen_text()), Some((1, 7, 12)));

    t.mouse(WHEEL_DOWN, 10, y);
    wait_for_selected_pid(&mut t, second_pid, "the cursor to step down a row");

    // Far more notches than there are rows: the window must stop at the end.
    for _ in 0..20 {
        t.mouse(WHEEL_DOWN, 10, y);
    }
    t.wait_for("the last page of rows", |s| {
        proc_window(s) == Some((6, 12, 12))
    });
    tick_marker(&mut t, 400);
    assert_eq!(
        proc_window(&t.screen_text()),
        Some((6, 12, 12)),
        "the wheel ran off the end of the list"
    );
    let last_pid = wait_for_procs(&mut t, 7).last().expect("data rows").1;
    assert_eq!(
        wait_for_selection(&mut t),
        last_pid,
        "the cursor is not on the final row"
    );
    t.send("q");
    assert!(t.wait_exit().success());
}

/// Clicking a card selects that GPU: `card_rects` hit-testing.
#[test]
fn click_selects_the_gpu_card_under_the_cursor() {
    let mut t = Tui::spawn(&[]);
    t.wait_for("both cards", |s| {
        s.contains("0·Mock GPU 0") && s.contains("1·Mock GPU 1")
    });
    wait_for_selected_card(&mut t, 0, "GPU 0 to be selected at startup");
    // Two rows below the title: inside the second card, clear of its border.
    let y = row_y(&t, "1·Mock GPU 1") + 2;
    t.click(10, y);
    wait_for_selected_card(&mut t, 1, "the clicked card to be selected");
    t.send("q");
    assert!(t.wait_exit().success());
}

/// The wheel over the GPU pane moves the card selection, dragging the card
/// window with it, and clamps at both ends. Eight cards do not fit at
/// 120x36, so the selection is visible in what the pane shows.
#[test]
fn wheel_over_the_gpu_pane_moves_the_card_selection() {
    let mut t = Tui::spawn(&["--mock", "8"]);
    t.wait_for("windowed cards", |s| {
        s.contains("0·Mock GPU 0") && !s.contains("Mock GPU 7")
    });
    // Fixed for the whole test: the GPU pane keeps its rows while the cards
    // inside it scroll.
    let y = row_y(&t, "0·Mock GPU 0") + 2;

    // GPU 0 is selected at startup, so wheel-up must not wrap to the last.
    t.mouse(WHEEL_UP, 10, y);
    tick_marker(&mut t, 200);
    assert_eq!(
        wait_for_a_selected_card(&mut t),
        0,
        "wheel-up wrapped past the first card"
    );

    for _ in 0..7 {
        t.mouse(WHEEL_DOWN, 10, y);
    }
    wait_for_selected_card(&mut t, 7, "the wheel to reach the last card");
    t.wait_for("the card window to follow the selection", |s| {
        s.contains("7·Mock GPU 7") && !s.contains("Mock GPU 0")
    });

    for _ in 0..5 {
        t.mouse(WHEEL_DOWN, 10, y);
    }
    tick_marker(&mut t, 400);
    assert_eq!(
        wait_for_a_selected_card(&mut t),
        7,
        "wheel-down past the last card did not clamp"
    );
    t.send("q");
    assert!(t.wait_exit().success());
}

/// Modals own the screen: with the help overlay up, a click or a wheel must
/// not move the process cursor, the card selection or the focus underneath
/// it.
#[test]
fn mouse_is_ignored_while_the_help_overlay_is_open() {
    let mut t = Tui::spawn(&["--mock", "4"]);
    t.wait_for("a windowed table", |s| proc_window(s) == Some((1, 7, 12)));
    let rows = wait_for_procs(&mut t, 7);
    let before_sel = wait_for_selection(&mut t);
    let proc_y = rows.last().expect("data rows").0;
    let gpu_y = row_y(&t, "0·Mock GPU 0") + 2;

    t.send("?");
    t.wait_for("help overlay", |s| s.contains("any key closes"));
    t.click(10, proc_y);
    for _ in 0..3 {
        t.mouse(WHEEL_DOWN, 10, proc_y);
    }
    t.mouse(WHEEL_DOWN, 10, gpu_y);
    // Closing the overlay takes a key, and input is read in order: once the
    // overlay is gone, every mouse event above has been read.
    t.send("x");
    t.wait_for("overlay gone", |s| !s.contains("any key closes"));

    assert_eq!(
        wait_for_procs(&mut t, 7),
        rows,
        "the table scrolled under the overlay"
    );
    assert_eq!(
        wait_for_selection(&mut t),
        before_sel,
        "the process cursor moved under the overlay"
    );
    assert_eq!(
        wait_for_a_selected_card(&mut t),
        0,
        "the card selection moved under the overlay"
    );
    // Focus is still on the GPU pane, so the digit folds the card it
    // already selects. Had the click stolen the focus, the same key would
    // merely take it back.
    t.send("0");
    t.wait_for("GPU 0 folded by the digit", |s| {
        s.contains("▸ 0·Mock GPU 0")
    });
    t.send("q");
    assert!(t.wait_exit().success());
}

/// The same guard covers every non-Normal input mode, not just the help
/// overlay: the filter prompt must swallow mouse input too. (The kill
/// dialog is the third such mode, but no mock or replay backend will open
/// one — it is refused before the dialog exists.)
#[test]
fn mouse_is_ignored_while_the_filter_prompt_is_open() {
    let mut t = Tui::spawn(&["--mock", "4"]);
    t.wait_for("a windowed table", |s| proc_window(s) == Some((1, 7, 12)));
    let rows = wait_for_procs(&mut t, 7);
    let before_sel = wait_for_selection(&mut t);
    let proc_y = rows.last().expect("data rows").0;
    let gpu_y = row_y(&t, "0·Mock GPU 0") + 2;

    t.send("/");
    t.wait_for("filter prompt", |s| s.contains("filter>"));
    t.click(10, proc_y);
    for _ in 0..3 {
        t.mouse(WHEEL_DOWN, 10, proc_y);
    }
    t.mouse(WHEEL_DOWN, 10, gpu_y);
    // A typed character reaches the prompt only after the mouse bytes ahead
    // of it have been read.
    t.send("z");
    t.wait_for("the typed character", |s| s.contains("filter> z"));

    assert_eq!(
        wait_for_procs(&mut t, 7),
        rows,
        "the table scrolled under the filter prompt"
    );
    assert_eq!(
        wait_for_selection(&mut t),
        before_sel,
        "the process cursor moved under the filter prompt"
    );
    assert_eq!(
        wait_for_a_selected_card(&mut t),
        0,
        "the card selection moved under the filter prompt"
    );
    // Back out with an empty filter — the table must come back untouched.
    t.send("\x7f\r");
    t.wait_for("prompt closed", |s| !s.contains("filter>"));
    assert_eq!(wait_for_procs(&mut t, 7), rows);
    t.send("q");
    assert!(t.wait_exit().success());
}

// --- kill dialog ---------------------------------------------------------
//
// mock and replay refuse to signal by design, so no backend could open the
// kill dialog under the PTY harness. `GPUR_STUB_BACKEND=1` injects a stub
// backend that reports one real local process, reaching the dialog, its
// modal guards, and the confirm path end to end.

/// Spawn with the stub backend: no `--mock`, one real local `sleep` process.
#[cfg(unix)]
fn spawn_stub() -> Tui {
    Tui::spawn_in(
        &["--no-splash", "--tick-ms", "100"],
        &[("GPUR_STUB_BACKEND", Some("1"))],
        Sandbox::new(),
    )
}

#[test]
#[cfg(unix)]
fn kill_dialog_opens_for_a_real_process_and_cancels() {
    let mut t = spawn_stub();
    t.wait_for("the stub's process row", |s| s.contains("sleep 60"));
    t.send("p");
    t.send("x");
    t.wait_for("the kill dialog", |s| s.contains("send SIGTERM to"));
    assert!(
        t.screen_text().contains("sleep 60"),
        "the dialog does not name the command:\n{}",
        t.screen_text()
    );
    // Any key other than y cancels — nothing may be signalled.
    t.send("n");
    t.wait_for("dialog closed", |s| !s.contains("send SIGTERM to"));
    t.send("q");
    assert!(t.wait_exit().success());
}

#[test]
#[cfg(unix)]
fn mouse_is_ignored_while_the_kill_dialog_is_open() {
    let mut t = spawn_stub();
    t.wait_for("the stub's rows", |s| proc_pids(s).len() == 2);
    let rows = wait_for_procs(&mut t, 2);
    // The child (higher pid) is the second row; select it by click, which
    // also focuses the process pane.
    let child = *rows.last().expect("two rows");
    t.click(10, child.0);
    wait_for_selected_pid(&mut t, child.1, "the child row to be selected");
    t.send("x");
    t.wait_for("the kill dialog", |s| s.contains("send SIGTERM to"));

    // Mouse events under the dialog: a click on the other row, wheel on the
    // process pane, wheel on the GPU pane.
    let own = rows[0];
    t.click(10, own.0);
    for _ in 0..3 {
        t.mouse(WHEEL_DOWN, 10, child.0);
    }
    let gpu_y = row_y(&t, "0·Stub GPU 0") + 2;
    t.mouse(WHEEL_DOWN, 10, gpu_y);
    // Input is read in order, so once the dialog closes every event above
    // has been consumed.
    t.send("n");
    t.wait_for("dialog closed", |s| !s.contains("send SIGTERM to"));

    assert_eq!(
        wait_for_procs(&mut t, 2),
        rows,
        "the table changed under the kill dialog"
    );
    assert_eq!(
        wait_for_selection(&mut t),
        child.1,
        "the process cursor moved under the kill dialog"
    );
    assert_eq!(
        wait_for_a_selected_card(&mut t),
        0,
        "the card selection moved under the kill dialog"
    );
    t.send("q");
    assert!(t.wait_exit().success());
}

#[test]
#[cfg(unix)]
fn kill_confirm_sends_the_signal_and_reports_it() {
    let mut t = spawn_stub();
    t.wait_for("the stub's rows", |s| proc_pids(s).len() == 2);
    let rows = wait_for_procs(&mut t, 2);
    let child = *rows.last().expect("two rows");
    t.click(10, child.0);
    wait_for_selected_pid(&mut t, child.1, "the child row to be selected");
    t.send("x");
    t.wait_for("the kill dialog", |s| s.contains("send SIGTERM to"));
    t.send("y");
    t.wait_for("the status line", |s| s.contains("sent SIGTERM to"));
    t.send("q");
    assert!(t.wait_exit().success());
}
