//! PTY integration tests against the real binary and a vt100 screen.
#[path = "support/mouse.rs"]
mod mouse;
#[path = "support/rendering.rs"]
mod rendering;
#[path = "support/terminal.rs"]
mod terminal;
use std::time::{Duration, Instant};
use terminal::{Sandbox, Tui};
const COLS: u16 = 120;
const ROWS: u16 = 36;

/// SGR button numbers, as the app's mouse capture reports them.
const MOUSE_LEFT: u8 = 0;
const WHEEL_UP: u8 = 64;
const WHEEL_DOWN: u8 = 65;

/// The pid a process-table data row leads with, if the line is one. Card
/// lines never qualify: their first cell is a label or a graph glyph, and
/// the PCIe readout's `MiB/s` sits behind one of those.
fn row_pid(line: &str) -> Option<u32> {
    let l = line.trim_start_matches('│').trim_start();
    if !l.contains("MiB") {
        return None;
    }
    l.split_whitespace().next()?.parse().ok()
}

/// PIDs of the process-table data rows, in the order they are drawn.
fn proc_pids(screen: &str) -> Vec<u32> {
    screen.lines().filter_map(row_pid).collect()
}

/// Text of one screen row. Mouse coordinates are row numbers, so anything
/// feeding them has to come off the cell grid rather than out of
/// `screen_text().lines()`, whose split need not line up with it.
fn row_text(screen: &vt100::Screen, row: u16) -> String {
    (0..COLS)
        .filter_map(|c| screen.cell(row, c).map(|cl| cl.contents()))
        .collect()
}

/// Screen row of the first line containing `needle`.
fn row_y(t: &Tui, needle: &str) -> u16 {
    let screen = t.parser.screen();
    (0..ROWS)
        .find(|&y| row_text(screen, y).contains(needle))
        .unwrap_or_else(|| panic!("no row containing {needle:?}; screen:\n{}", t.screen_text()))
}

/// `(screen row, pid)` for every drawn process data row, top to bottom.
fn proc_rows(t: &Tui) -> Vec<(u16, u32)> {
    let screen = t.parser.screen();
    (0..ROWS)
        .filter_map(|y| Some((y, row_pid(&row_text(screen, y))?)))
        .collect()
}

/// The process pane's window caption — `1-7/12` reads as `(1, 7, 12)`:
/// first and last visible row over the total. `None` while everything fits,
/// which is also when the caption is absent.
fn proc_window(screen: &str) -> Option<(usize, usize, usize)> {
    let line = screen.lines().find(|l| l.contains("┐processes┌"))?;
    // The caption abuts the border glyphs (`1-7/12┌╮`), so trim to digits.
    let num = |s: &str| s.trim_matches(|c: char| !c.is_ascii_digit()).parse().ok();
    let tok = line
        .split_whitespace()
        .find(|w| w.contains('-') && w.contains('/'))?;
    let (range, total) = tok.split_once('/')?;
    let (first, last) = range.split_once('-')?;
    Some((num(first)?, num(last)?, num(total)?))
}

/// Pid of the row carrying the selection background, when it is a process
/// row that carries it.
fn selected_pid(t: &mut Tui) -> Option<u32> {
    row_pid(&highlighted_row(t)?)
}

/// `(screen row, pid)` for a full window of `rows` process rows. The pty
/// hands a frame over in pieces, so a bare read can catch a table with rows
/// still to be painted — and after a popup closes, a whole band of them.
fn wait_for_procs(t: &mut Tui, rows: usize) -> Vec<(u16, u32)> {
    t.wait_for("a full window of process rows", move |s| {
        proc_pids(s).len() == rows
    });
    proc_rows(t)
}

/// Pid under the process cursor, once a row carries the cursor at all.
fn wait_for_selection(t: &mut Tui) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(pid) = selected_pid(t) {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for a selected process row; screen:\n{}",
            t.screen_text()
        );
        t.pump_once(Duration::from_millis(100));
    }
}

/// Index of the GPU card drawn with the selected border (accent #cba6f7,
/// against #6c7086 for every other card) — a direct read of the card
/// selection, which no key or caption otherwise spells out.
fn selected_card(t: &mut Tui) -> Option<usize> {
    t.pump_once(Duration::from_millis(50));
    let screen = t.parser.screen();
    (0..ROWS).find_map(|y| {
        let cell = screen.cell(y, 0)?;
        if cell.contents() != "╭" || cell.fgcolor() != vt100::Color::Rgb(0xcb, 0xa6, 0xf7) {
            return None;
        }
        // `╭┐3·Mock GPU 3┌───…` — the index leads the card title, whatever
        // the card's name (the stub backend's "0·Stub GPU 0" among them).
        let text = row_text(screen, y);
        let (before, _) = text.split_once('·')?;
        before
            .trim_start_matches(|c: char| !c.is_ascii_digit())
            .parse()
            .ok()
    })
}

/// Index of the selected GPU card, once one is drawn.
fn wait_for_a_selected_card(t: &mut Tui) -> usize {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(idx) = selected_card(t) {
            return idx;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for a selected GPU card; screen:\n{}",
            t.screen_text()
        );
        t.pump_once(Duration::from_millis(100));
    }
}

/// Poll until the card selection lands on `idx`.
fn wait_for_selected_card(t: &mut Tui, idx: usize, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while selected_card(t) != Some(idx) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; screen:\n{}",
            t.screen_text()
        );
        t.pump_once(Duration::from_millis(100));
    }
}

/// Poll until the process cursor sits on `pid`.
fn wait_for_selected_pid(t: &mut Tui, pid: u32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while selected_pid(t) != Some(pid) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; screen:\n{}",
            t.screen_text()
        );
        t.pump_once(Duration::from_millis(100));
    }
}

/// A drawn meter line — `label`, a run of meter glyphs, then the value.
/// Matching on the glyph run is the point: the process-table header alone
/// carries both "GPU " and "MEM ". Which glyphs count is a parameter because
/// `draw_meter` swaps `■·` for `=.` under `--graphs ascii`, and a caller that
/// accepted either could not tell the two apart.
fn meter_line(screen: &str, label: &str, glyphs: &str) -> Option<String> {
    screen
        .lines()
        .map(|l| l.trim_start_matches('│').trim_end().trim_end_matches('│'))
        .find(|l| l.starts_with(label) && l.chars().any(|c| glyphs.contains(c)))
        .map(str::to_string)
}

#[test]
fn renders_dashboard_and_process_table() {
    let mut t = Tui::spawn(&[]);
    t.wait_for("GPU cards", |s| {
        s.contains("Mock GPU 0") && s.contains("Mock GPU 1")
    });
    t.wait_for("process table", |s| s.contains("COMMAND"));
    t.wait_for("meters", |s| meter_line(s, "GPU ", "■·").is_some());
    let s = t.screen_text();
    let util = meter_line(&s, "GPU ", "■·").expect("GPU meter");
    let glyphs = util.chars().filter(|c| "■·".contains(*c)).count();
    assert!(glyphs >= 40, "GPU meter has no bar: {util:?}");
    assert!(util.trim_end().ends_with('%'), "GPU meter value: {util:?}");
    let vram = meter_line(&s, "MEM ", "■·").expect("MEM meter");
    let glyphs = vram.chars().filter(|c| "■·".contains(*c)).count();
    assert!(glyphs >= 40, "MEM meter has no bar: {vram:?}");
    assert!(vram.contains('/'), "MEM meter value: {vram:?}");
    t.send("q");
    assert!(t.wait_exit().success());
}

/// The MEM row names the pool each card actually spends. Mock GPU 0 is the
/// unified-memory part — no local pool, so its meter is the system one and
/// has to say `shared`; the discrete cards meter their own VRAM and carry the
/// spill pool beside it as `gtt`. Rendering host RAM as a card's dedicated
/// VRAM is the failure this pins, and it is invisible in every other test
/// because both readouts are the same shape.
#[test]
fn the_mem_row_marks_shared_memory_and_carries_the_second_pool() {
    let mut t = Tui::spawn(&["--mock", "2"]);
    t.wait_for("both cards", |s| {
        s.contains("Mock GPU 0") && s.contains("Mock GPU 1")
    });
    t.wait_for("the MEM meters", |s| {
        s.lines()
            .filter(|l| l.contains("MEM ") && l.contains('/'))
            .count()
            >= 2
    });
    let s = t.screen_text();
    let mem: Vec<&str> = s
        .lines()
        .filter(|l| l.contains("MEM ") && l.contains('/'))
        .collect();

    assert!(
        mem[0].contains("shared") && !mem[0].contains("gtt"),
        "unified card did not mark its pool shared: {:?}",
        mem[0]
    );
    assert!(
        mem[1].contains("· gtt ") && !mem[1].contains("shared"),
        "discrete card lost its spill pool, or called it shared: {:?}",
        mem[1]
    );
    // The footer used to carry it; printing it in both places was the
    // alternative to moving it.
    assert_eq!(
        s.matches("gtt ").count(),
        1,
        "the system pool is printed twice:\n{s}"
    );
    t.send("q");
    assert!(t.wait_exit().success());
}

/// A card whose backend publishes no utilization and no VRAM figures — a
/// mainline-i915 iGPU, a PDH adapter with no matching counter instance, an
/// Apple accelerator with no PerformanceStatistics. It must read `n/a`, never
/// a full-width meter confidently claiming `GPU 0%` / `MEM 0M/0M`, and the
/// session row must not appear at all rather than peak at `0°C  0W`.
#[test]
fn unreadable_metrics_render_as_na() {
    let home = Sandbox::new();
    let log = home.0.join("rec.jsonl");
    std::fs::write(
        &log,
        concat!(
            r#"{"ts_ms":1,"backend":"intel","driver":"i915","#,
            r#""gpus":[{"name":"Unreadable GPU"}],"processes":[]}"#,
            "\n"
        ),
    )
    .unwrap();
    let log = log.to_string_lossy().into_owned();

    let mut t = Tui::spawn_in(
        &["--no-splash", "--tick-ms", "100", "--replay", &log],
        &[],
        home,
    );
    t.wait_for("the card", |s| s.contains("Unreadable GPU"));
    let s = t.drain(Duration::from_millis(400));
    assert!(
        s.contains("GPU n/a"),
        "utilization not marked unknown:\n{s}"
    );
    assert!(s.contains("MEM n/a"), "vram not marked unknown:\n{s}");
    assert!(
        meter_line(&s, "GPU ", "■·").is_none() && meter_line(&s, "MEM ", "■·").is_none(),
        "an unreadable metric still drew a meter track:\n{s}"
    );
    assert!(
        !s.contains("session"),
        "session row rendered with nothing measured:\n{s}"
    );
    t.send("q");
    assert!(t.wait_exit().success());
}

#[test]
fn fold_toggles_card() {
    let mut t = Tui::spawn(&[]);
    t.wait_for("cards", |s| s.contains("Mock GPU 0"));
    // GPU0 starts selected: first press folds, second unfolds.
    t.send("0");
    t.wait_for("folded summary", |s| s.contains("▸ 0·Mock GPU 0"));
    t.send("0");
    t.wait_for("unfolded card", |s| !s.contains("▸ 0·Mock GPU 0"));
    t.send("q");
    t.wait_exit();
}

fn filtered_process_table_ready(screen: &str, pid: u32) -> bool {
    screen.contains("filter:gpur") && proc_pids(screen) == [pid]
}

#[test]
fn filtered_process_table_waits_for_the_expected_row() {
    let mut parser = vt100::Parser::new(ROWS, COLS, 0);
    parser.process(b"filter:gpur\r\n1000005 user 0 Compute 40 256MiB firefox");
    assert!(!filtered_process_table_ready(
        &parser.screen().contents(),
        8208
    ));
    parser.process(b"\x1b[2;1H\x1b[2K8208 user 0 Compute 40 3064MiB gpur");
    assert!(filtered_process_table_ready(
        &parser.screen().contents(),
        8208
    ));
}

#[test]
fn filter_narrows_process_table() {
    let mut t = Tui::spawn(&[]);
    t.wait_for("all rows", |s| proc_pids(s).len() == 6);
    // The mock's first process row is this test binary itself; the other
    // five are fabricated (ollama, blender, Xorg, ffmpeg, firefox) and must
    // all drop out — the caption alone renders unconditionally.
    t.send("/gpur\r");
    let pid = t.child.process_id().expect("child pid");
    t.wait_for("narrowed table", |s| filtered_process_table_ready(s, pid));
    let s = t.screen_text();
    assert!(!s.contains("ollama runner"), "filtered-out row still drawn");
    assert!(!s.contains("blender -b"), "filtered-out row still drawn");
    assert_eq!(
        proc_pids(&s),
        vec![t.child.process_id().expect("child pid")],
        "the surviving row is not gpur's own"
    );
    // Re-opening seeds the edit buffer with the committed filter, so
    // backspace it away: an empty filter clears and every row comes back.
    t.send("/\x7f\x7f\x7f\x7f\r");
    t.wait_for("filter cleared", |s| {
        !s.contains("filter:") && proc_pids(s).len() == 6
    });
    t.send("q");
    t.wait_exit();
}

#[test]
fn quit_restores_terminal_modes() {
    #[cfg(windows)]
    if terminal::observe_console() {
        return;
    }
    #[cfg(windows)]
    let mut t = Tui::spawn_with_env(&[], &[("GPUR_TEST_CONSOLE_OBSERVER", Some("1"))]);
    #[cfg(unix)]
    let mut t = Tui::spawn(&[]);
    t.wait_for("cards", |s| s.contains("Mock GPU 0"));
    #[cfg(windows)]
    t.wait_console_observation();
    #[cfg(unix)]
    assert!(
        t.parser.screen().alternate_screen(),
        "alternate screen never entered"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !t.parser.screen().hide_cursor() {
        assert!(Instant::now() < deadline, "dashboard cursor was not hidden");
        t.pump_once(Duration::from_millis(50));
    }
    t.send("q");
    assert!(
        t.wait_exit().success(),
        "teardown failed:\n{}",
        t.screen_text()
    );
    #[cfg(unix)]
    {
        assert!(
            !t.parser.screen().alternate_screen(),
            "alternate screen not restored"
        );
        let raw = String::from_utf8_lossy(&t.raw);
        assert!(raw.contains("[?1049l"), "alt screen not left");
        assert!(raw.contains("[?1006l"), "mouse capture not disabled");
    }
    assert!(!t.parser.screen().hide_cursor(), "cursor not restored");
}

#[test]
fn resize_changes_layout_and_restores_it() {
    let mut t = Tui::spawn(&["--mock", "4"]);
    t.wait_for("full-size process window", |s| {
        proc_window(s) == Some((1, 7, 12))
    });
    t.parser.screen_mut().set_size(24, 80);
    t.resize(24, 80);
    t.wait_for("smaller process window", |s| {
        proc_window(s) == Some((1, 3, 12))
            && proc_pids(s).len() == 3
            && s.lines().nth(22).is_some_and(|line| line.ends_with('╯'))
    });
    assert_eq!(proc_pids(&t.screen_text()).len(), 3);
    assert_eq!(t.parser.screen().cell(22, 79).unwrap().contents(), "╯");
    t.parser.screen_mut().set_size(ROWS, COLS);
    t.resize(ROWS, COLS);
    t.wait_for("restored process window", |s| {
        proc_window(s) == Some((1, 7, 12))
            && proc_pids(s).len() == 7
            && s.lines()
                .nth(usize::from(ROWS - 2))
                .is_some_and(|line| line.ends_with('╯'))
    });
    assert_eq!(proc_pids(&t.screen_text()).len(), 7);
    assert_eq!(
        t.parser
            .screen()
            .cell(ROWS - 2, COLS - 1)
            .unwrap()
            .contents(),
        "╯"
    );
    t.send("q");
    assert!(t.wait_exit().success());
}

#[test]
fn survives_resize_storm() {
    let mut t = Tui::spawn(&[]);
    t.wait_for("cards", |s| s.contains("Mock GPU 0"));
    // Storm through degenerate sizes; the app must neither crash nor wedge.
    // (ratatui only emits diffs, so don't expect a spontaneous full redraw
    // afterwards — macOS coalesces the resize events. Instead prove the app
    // is alive by demanding a NEW screen element.)
    for (rows, cols) in [
        (5, 5),
        (2, 40),
        (50, 3),
        (1, 1),
        (200, 250),
        (10, 30),
        (ROWS, COLS),
    ] {
        t.resize(rows, cols);
        // NB: the vt100 test parser stays at full size — it panics on 1x1
        // grids, and out-of-range coords from small-screen frames clamp.
        t.pump_once(Duration::from_millis(50));
    }
    // Let the app drain the resize backlog before probing.
    for _ in 0..10 {
        t.pump_once(Duration::from_millis(100));
    }
    // Prove liveness: the overlay is a NEW element. Retry the key — a
    // keypress racing the tail of the resize burst can be coalesced away
    // on slow CI runners; a genuinely wedged input loop still fails.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(
            t.child.try_wait().expect("query child status").is_none(),
            "app exited during/after resize storm"
        );
        t.send("?");
        let round = Instant::now() + Duration::from_secs(2);
        while Instant::now() < round {
            t.pump_once(Duration::from_millis(100));
            if t.screen_text().contains("any key closes") {
                break;
            }
        }
        if t.screen_text().contains("any key closes") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no response to '?' after resize storm; screen:\n{}",
            t.screen_text()
        );
    }
    t.send(" "); // close overlay
    t.send("q");
    assert!(t.wait_exit().success());
}

#[test]
fn help_overlay_opens_and_closes() {
    let mut t = Tui::spawn(&[]);
    t.wait_for("cards", |s| s.contains("Mock GPU 0"));
    t.send("?");
    t.wait_for("help overlay", |s| s.contains("any key closes"));
    t.send("x"); // closes overlay, must not open the kill dialog
    t.wait_for("overlay gone", |s| {
        !s.contains("any key closes") && !s.contains("SIGTERM")
    });
    t.send("q");
    t.wait_exit();
}

#[test]
#[cfg(unix)]
fn sigterm_restores_terminal_modes() {
    let mut t = Tui::spawn(&[]);
    t.wait_for("cards", |s| s.contains("Mock GPU 0"));
    // External kill — the signal handler must run the same teardown.
    let pid = t.child.process_id().expect("child pid") as i32;
    unsafe { libc_kill(pid) };
    let status = t.wait_exit();
    assert!(!status.success()); // 128+15
    let raw = String::from_utf8_lossy(&t.raw);
    assert!(raw.contains("[?1049l"), "alt screen not left on SIGTERM");
    assert!(
        raw.contains("[?1006l"),
        "mouse capture not disabled on SIGTERM"
    );
}

fn own_process_row(screen: &str, pid: &str, user: &str) -> bool {
    use ratatui::{style::Style, text::Line};

    // Match draw_processes' USER column in display columns, not bytes or chars.
    let line = Line::from(user);
    let mut remaining: usize = 10;
    let user: String = line
        .styled_graphemes(Style::default())
        .map_while(|g| {
            remaining = remaining.checked_sub(Line::from(g.symbol).width())?;
            Some(g.symbol)
        })
        .collect();
    screen.lines().any(|l| {
        let l = l.trim_start_matches('│').trim_start();
        let mut cols = l.split_whitespace();
        cols.next() == Some(pid) && cols.next() == Some(user.as_str()) && l.contains("MiB")
    })
}

#[test]
fn own_process_row_matches_display_width_and_exact_pid() {
    for (user, displayed) in [
        ("runneradmin", "runneradmi"),
        ("界界界界界界", "界界界界界"),
        ("abcdefghi界", "abcdefghi"),
        ("e\u{301}123456789x", "e\u{301}123456789"),
    ] {
        let row = format!("│12345    {displayed} 0 Compute 40 3064MiB");
        assert!(own_process_row(&row, "12345", user), "{user:?}");
        assert!(!own_process_row(&row, "1234", user));
        assert!(!own_process_row(&row, "123456", user));
        assert!(!own_process_row(&row, "12345", "different"));
        assert!(!own_process_row(&row.replace("MiB", ""), "12345", user));
    }
}

#[test]
fn process_rows_show_real_content() {
    let mut t = Tui::spawn(&[]);
    // The mock's first process row is the app itself, left un-enriched so
    // sysinfo fills the host columns. Anchor on that one row: "gpur" alone
    // also matches the header on every frame.
    let pid = t.child.process_id().expect("child pid").to_string();
    // Anchor on USER, not COMMAND: the command column holds the binary's full
    // path and is truncated to the terminal width, so whether the basename
    // survives depends on where the repo happens to live.
    #[cfg(unix)]
    let user = std::env::var("USER").expect("USER");
    #[cfg(windows)]
    let user = std::env::var("USERNAME").expect("USERNAME");
    t.wait_for("own process row", move |s| own_process_row(s, &pid, &user));
    t.send("q");
    t.wait_exit();
}

#[test]
fn sort_cycle_and_reverse_update_caption() {
    let mut t = Tui::spawn(&[]);
    t.wait_for("default sort", |s| s.contains("gpu-mem↓"));
    t.send("s");
    t.wait_for("cycled to gpu%", |s| s.contains("gpu%↓"));
    t.send("r");
    t.wait_for("reversed arrow", |s| s.contains("gpu%↑"));
    t.send("q");
    t.wait_exit();
}

/// The caption is not the behaviour: reversing must reorder the rows.
#[test]
fn sort_reverse_reorders_rows() {
    let mut t = Tui::spawn(&[]);
    t.wait_for("all rows", |s| {
        s.contains("gpu-mem↓") && proc_pids(s).len() == 6
    });
    let desc = proc_pids(&t.screen_text());
    t.send("r");
    let asc: Vec<u32> = desc.iter().rev().copied().collect();
    let want = asc.clone();
    t.wait_for("rows in ascending gpu-mem order", move |s| {
        s.contains("gpu-mem↑") && proc_pids(s) == want
    });
    assert_ne!(desc, asc, "mock rows are not distinguishable by order");
    t.send("q");
    t.wait_exit();
}

/// Space pauses polling: the badge appears and the screen stops changing.
#[test]
fn pause_freezes_the_dashboard() {
    let mut t = Tui::spawn(&[]);
    t.wait_for("cards", |s| s.contains("Mock GPU 0"));
    t.send(" ");
    t.wait_for("paused badge", |s| s.contains("PAUSED"));
    let frozen = t.drain(Duration::from_millis(400));
    // Six ticks' worth of wall clock: a live poll would move the meters.
    let later = t.drain(Duration::from_millis(600));
    assert_eq!(frozen, later, "screen changed while paused");
    t.send(" ");
    t.wait_for("resumed", |s| !s.contains("PAUSED"));
    t.send("q");
    assert!(t.wait_exit().success());
}

/// `-` halves the poll rate, `+` restores it, and the 100 ms floor holds.
#[test]
fn tick_keys_change_the_poll_interval() {
    let mut t = Tui::spawn(&["--tick-ms", "400"]);
    t.wait_for("initial rate", |s| s.contains("400ms"));
    t.send("-");
    t.wait_for("slower", |s| s.contains("800ms"));
    t.send("+");
    t.wait_for("faster", |s| s.contains("400ms"));
    t.send("+");
    t.wait_for("faster again", |s| s.contains("200ms"));
    t.send("+");
    t.wait_for("faster still", |s| s.contains("100ms"));
    t.send("+");
    t.wait_for("at the floor", |s| s.contains("50ms"));
    // The floor must hold rather than bounce back up to a slower rate, which
    // is what the old key-only floor of 100 did to anyone starting below it.
    t.send("+");
    let s = t.drain(Duration::from_millis(400));
    let header = s.lines().next().unwrap_or_default();
    assert!(s.contains("50ms"), "tick left the 50ms floor: {header:?}");
    t.send("q");
    t.wait_exit();
}

/// Eight cards cannot fit at 120x36, so the card list must window and
/// scroll to keep the selection visible.
#[test]
fn gpu_cards_scroll_when_they_overflow() {
    let mut t = Tui::spawn(&["--mock", "8"]);
    t.wait_for("windowed cards", |s| {
        s.contains("Mock GPU 0") && !s.contains("Mock GPU 7")
    });
    t.wait_for("card scrollbar", |s| s.contains('║'));
    // Selecting a card below the window scrolls it into view.
    t.send("5");
    t.wait_for("scrolled to GPU 5", |s| {
        s.contains("Mock GPU 5") && !s.contains("Mock GPU 0")
    });
    t.send("q");
    assert!(t.wait_exit().success());
}

/// A failing backend must not kill the monitor: the banner shows, the last
/// good snapshot stays on screen, and quitting is still clean.
#[test]
fn poll_failure_degrades_gracefully() {
    let mut t = Tui::spawn_with_env(&[], &[("GPUR_MOCK_FAIL", Some("2"))]);
    t.wait_for("cards", |s| s.contains("Mock GPU 0"));
    // Every second poll fails, so the banner is only up for one frame.
    t.wait_for_raw("poll-failure banner", "poll failed: simulated driver reset");
    let s = t.screen_text();
    assert!(s.contains("Mock GPU 0"), "snapshot dropped on poll failure");
    assert!(meter_line(&s, "GPU ", "■·").is_some(), "meters dropped");
    assert!(
        t.child.try_wait().expect("query child status").is_none(),
        "app exited on a backend failure"
    );
    t.send("q");
    assert!(t.wait_exit().success());
}

/// Every poll fails: the banner stays up, the GPU pane degrades to its
/// empty state, and the 5th consecutive failure triggers a re-detect.
///
/// The re-detect repeats the detection this session started from, so under
/// `--mock` it hands back another mock and the status names it: what a
/// re-detect can never do is turn a fabricated or recorded session into a
/// live one. That half is pinned by
/// `app::tests::a_failing_replay_re_detects_to_a_replay_not_to_live_hardware`.
#[test]
fn persistent_poll_failure_redetects_the_backend() {
    let mut t = Tui::spawn_with_env(&[], &[("GPUR_MOCK_FAIL", Some("1"))]);
    t.wait_for("failure banner", |s| {
        s.contains("⚠ poll failed: simulated driver reset")
    });
    t.wait_for("empty GPU pane", |s| {
        s.contains("no GPUs reported by backend")
    });
    t.wait_for("re-detect status", |s| s.contains("backend re-detected"));
    assert!(
        t.screen_text().contains("backend re-detected (mock)"),
        "re-detect produced something other than a mock:\n{}",
        t.screen_text()
    );
    t.send("q");
    assert!(t.wait_exit().success());
}

/// Sort and fold state round-trips through the cache dir — and lands in
/// this test's sandbox, never the developer's real `~/.cache/gpur`.
#[test]
fn ui_state_is_saved_into_the_sandboxed_cache() {
    let mut t = Tui::spawn(&[]);
    t.wait_for("default sort", |s| s.contains("gpu-mem↓"));
    t.send("s");
    t.wait_for("cycled sort", |s| s.contains("gpu%↓"));
    t.send("0");
    t.wait_for("folded card", |s| s.contains("▸ 0·Mock GPU 0"));
    t.send("q");
    assert!(t.wait_exit().success());

    let path = t.state_file();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("no state at {}: {e}", path.display()));
    let v: serde_json::Value = serde_json::from_str(&text).expect("valid state JSON");
    assert_eq!(v["sort_by"], "GpuUtil");
    assert_eq!(v["tick_ms"], 100);
    // Folds persist by device id, never by position.
    assert_eq!(v["folded_devices"], serde_json::json!(["mock:0"]));
}

/// A cached state file must be honoured on startup — the same read path
/// that used to pick up the developer's real preferences.
#[test]
fn saved_state_is_restored_on_startup() {
    let home = Sandbox::new();
    let dir = home.0.join("gpur");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("state.json"),
        r#"{"folded_devices":["mock:1"],"sort_by":"Pid","sort_desc":false,"tick_ms":300}"#,
    )
    .unwrap();

    // No --tick-ms here: the persisted 300 must be what the header shows.
    let mut t = Tui::spawn_in(&["--no-splash", "--mock"], &[], home);
    t.wait_for("restored sort", |s| s.contains("pid↑"));
    t.wait_for("restored fold", |s| s.contains("▸ 1·Mock GPU 1"));
    t.wait_for("restored tick", |s| s.contains("300ms"));
    t.send("q");
    assert!(t.wait_exit().success());
}

/// A state file written before devices had ids carries `folded` as bare
/// positions. Those cannot be mapped onto this run's devices, so they are
/// dropped — but the file must still load, and the rest of it must survive.
#[test]
fn legacy_positional_folds_load_without_folding_a_card() {
    let home = Sandbox::new();
    let dir = home.0.join("gpur");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("state.json"),
        r#"{"folded":[1],"sort_by":"Pid","sort_desc":false,"tick_ms":300}"#,
    )
    .unwrap();

    let mut t = Tui::spawn_in(&["--no-splash", "--mock"], &[], home);
    t.wait_for("restored sort", |s| s.contains("pid↑"));
    t.wait_for("restored tick", |s| s.contains("300ms"));
    t.wait_for("cards", |s| s.contains("1·Mock GPU 1"));
    let s = t.screen_text();
    assert!(
        !s.contains("▸ 0·Mock GPU 0") && !s.contains("▸ 1·Mock GPU 1"),
        "a stale positional fold was applied:\n{s}"
    );
    t.send("q");
    assert!(t.wait_exit().success());
}

#[test]
fn process_cursor_highlight_moves() {
    let mut t = Tui::spawn(&[]);
    t.wait_for("process rows", |s| s.contains("COMMAND"));
    t.send("p"); // focus process list
    let first = highlighted_row(&mut t).expect("a highlighted row");
    t.send("j");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        t.pump_once(Duration::from_millis(100));
        if let Some(now) = highlighted_row(&mut t)
            && now != first
        {
            break;
        }
        assert!(Instant::now() < deadline, "cursor highlight never moved");
    }
    t.send("q");
    t.wait_exit();
}

/// Text of the row whose cells carry the selection background
/// (surface1 #45475a under the pinned truecolor mode).
fn highlighted_row(t: &mut Tui) -> Option<String> {
    t.pump_once(Duration::from_millis(50));
    let screen = t.parser.screen();
    for row in 0..ROWS {
        if matches!(
            screen.cell(row, 2).map(|c| c.bgcolor()),
            Some(vt100::Color::Rgb(0x45, 0x47, 0x5a))
        ) {
            let text = row_text(screen, row);
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
    }
    None
}

/// The mock backend's pids are fabricated, so the dialog must never open —
/// row 0 is gpur's own pid and the rest name nothing on this host.
#[test]
fn kill_is_refused_under_the_mock_backend() {
    let mut t = Tui::spawn(&[]);
    t.wait_for("process rows", |s| s.contains("COMMAND"));
    t.send("p");
    t.send("x");
    t.wait_for("refusal status", |s| s.contains("kill disabled"));
    assert!(
        !t.screen_text().contains("send SIGTERM to"),
        "mock backend opened a kill dialog"
    );
    assert!(
        t.child.try_wait().expect("query child status").is_none(),
        "app died after a refused kill"
    );
    t.send("q");
    assert!(t.wait_exit().success());
}

/// Minimal libc-free SIGTERM via /bin/kill would need a shell; declare the
/// one libc fn we need instead of pulling the libc crate into dev-deps.
#[cfg(unix)]
unsafe fn libc_kill(pid: i32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        kill(pid, 15);
    }
}
