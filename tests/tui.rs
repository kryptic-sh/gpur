//! PTY integration tests: run the real binary against a pseudo-terminal,
//! parse its output with a vt100 emulator, and assert on the rendered
//! screen. Unix-only (ConPTY in CI is flaky); mock backend throughout.
#![cfg(unix)]

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const COLS: u16 = 120;
const ROWS: u16 = 36;

/// A throwaway XDG root for one test. gpur resolves its config/cache/data
/// through hjkl-config, which honours `XDG_*_HOME` on every platform, so
/// redirecting the three vars keeps the suite off the developer's real
/// `~/.cache/gpur/state.json` — which the app both reads at startup and
/// overwrites on a clean quit.
struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "gpur-tui-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Sandbox(dir)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Tui {
    parser: vt100::Parser,
    rx: mpsc::Receiver<Vec<u8>>,
    writer: Box<dyn std::io::Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// Every byte the app ever wrote, for teardown-sequence assertions.
    raw: Vec<u8>,
    /// Removed on drop; keep it alive for the whole run.
    home: Sandbox,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Tui {
    fn spawn(extra_args: &[&str]) -> Self {
        Self::spawn_with_env(extra_args, &[])
    }

    fn spawn_with_env(extra_args: &[&str], env: &[(&str, Option<&str>)]) -> Self {
        // Defaults, omitted when the caller passes its own — clap rejects a
        // repeated flag rather than letting the last one win.
        let mut args = vec!["--no-splash"];
        if !extra_args.contains(&"--mock") {
            args.push("--mock");
        }
        if !extra_args.contains(&"--tick-ms") {
            args.extend(["--tick-ms", "100"]);
        }
        args.extend_from_slice(extra_args);
        Self::spawn_in(&args, env, Sandbox::new())
    }

    /// Full control over the command line and the sandbox — for tests that
    /// plant a cache file, or that need a persisted setting to win because
    /// no CLI flag overrides it.
    ///
    /// `env` overrides the defaults below rather than adding to them:
    /// `CommandBuilder` keys its environment by name, so a caller's entry
    /// replaces the one set here. `None` unsets the variable outright, which
    /// is not the same as the empty string — `detect_color_mode` reads
    /// `NO_COLOR` as set-but-empty meaning "colour is fine".
    fn spawn_in(args: &[&str], env: &[(&str, Option<&str>)], home: Sandbox) -> Self {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_gpur"));
        cmd.args(args);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("XDG_CONFIG_HOME", &home.0);
        cmd.env("XDG_CACHE_HOME", &home.0);
        cmd.env("XDG_DATA_HOME", &home.0);
        for (k, v) in env {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            }
        }
        let child = pty.slave.spawn_command(cmd).unwrap();
        drop(pty.slave);

        let mut reader = pty.master.try_clone_reader().unwrap();
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });

        Tui {
            parser: vt100::Parser::new(ROWS, COLS, 0),
            rx,
            writer: pty.master.take_writer().unwrap(),
            child,
            raw: Vec::new(),
            home,
            _master: pty.master,
        }
    }

    /// Where the app persists its UI state under this test's sandbox.
    fn state_file(&self) -> PathBuf {
        self.home.0.join("gpur").join("state.json")
    }

    fn pump_once(&mut self, timeout: Duration) -> bool {
        match self.rx.recv_timeout(timeout) {
            Ok(bytes) => {
                self.raw.extend_from_slice(&bytes);
                self.parser.process(&bytes);
                true
            }
            Err(_) => false,
        }
    }

    fn screen_text(&self) -> String {
        self.parser.screen().contents()
    }

    /// Poll the emulated screen until `pred` holds — never trust a fixed
    /// sleep to mean "rendered".
    fn wait_for(&mut self, what: &str, pred: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if pred(&self.screen_text()) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; screen:\n{}",
                self.screen_text()
            );
            self.pump_once(Duration::from_millis(100));
        }
    }

    fn send(&mut self, keys: &str) {
        self.writer.write_all(keys.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    /// One SGR mouse report over a 0-based screen cell. The encoding is
    /// 1-based, `M` presses and `m` releases; the wheel buttons have no
    /// release, exactly as a real terminal reports them.
    fn mouse(&mut self, button: u8, col: u16, row: u16) {
        self.send(&format!("\x1b[<{button};{};{}M", col + 1, row + 1));
        if button < WHEEL_UP {
            self.send(&format!("\x1b[<{button};{};{}m", col + 1, row + 1));
        }
    }

    fn click(&mut self, col: u16, row: u16) {
        self.mouse(MOUSE_LEFT, col, row);
    }

    /// Like [`wait_for`], but over every byte the app has written — for
    /// states that flash by between frames and would be erased from the
    /// screen before the next poll of the emulator.
    fn wait_for_raw(&mut self, what: &str, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if String::from_utf8_lossy(&self.raw).contains(needle) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; screen:\n{}",
                self.screen_text()
            );
            self.pump_once(Duration::from_millis(100));
        }
    }

    /// Pump for a fixed window, then return the screen. The app repaints on
    /// every tick even when nothing changed, so waiting for the pty to fall
    /// silent would never return.
    fn drain(&mut self, window: Duration) -> String {
        let until = Instant::now() + window;
        while Instant::now() < until {
            self.pump_once(Duration::from_millis(50));
        }
        self.screen_text()
    }

    fn wait_exit(&mut self) -> portable_pty::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            // Keep draining so the child can't block on a full pty buffer.
            self.pump_once(Duration::from_millis(50));
            if let Ok(Some(status)) = self.child.try_wait() {
                // Drain whatever teardown bytes remain.
                while self.pump_once(Duration::from_millis(100)) {}
                return status;
            }
            assert!(Instant::now() < deadline, "child did not exit");
        }
    }
}

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
        // `╭┐3·Mock GPU 3┌───…` — the index leads the card title.
        let text = row_text(screen, y);
        let (before, _) = text.split_once("·Mock GPU")?;
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

#[test]
fn filter_narrows_process_table() {
    let mut t = Tui::spawn(&[]);
    t.wait_for("all rows", |s| proc_pids(s).len() == 6);
    // The mock's first process row is this test binary itself; the other
    // five are fabricated (ollama, blender, Xorg, ffmpeg, firefox) and must
    // all drop out — the caption alone renders unconditionally.
    t.send("/gpur\r");
    t.wait_for("narrowed table", |s| {
        s.contains("filter:gpur") && proc_pids(s).len() == 1
    });
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
    let mut t = Tui::spawn(&[]);
    t.wait_for("cards", |s| s.contains("Mock GPU 0"));
    t.send("q");
    assert!(t.wait_exit().success());
    let raw = String::from_utf8_lossy(&t.raw);
    assert!(raw.contains("[?1049l"), "alt screen not left");
    assert!(raw.contains("[?1006l"), "mouse capture not disabled");
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
        t._master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
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
            !matches!(t.child.try_wait(), Ok(Some(_))),
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
    let user = std::env::var("USER").expect("USER");
    t.wait_for("own process row", move |s| {
        s.lines().any(|l| {
            let l = l.trim_start_matches('│').trim_start();
            let mut cols = l.split_whitespace();
            cols.next() == Some(pid.as_str())
                && cols.next() == Some(user.as_str()) // sysinfo resolved it
                && l.contains("MiB")
        })
    });
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
        !matches!(t.child.try_wait(), Ok(Some(_))),
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
        !matches!(t.child.try_wait(), Ok(Some(_))),
        "app died after a refused kill"
    );
    t.send("q");
    assert!(t.wait_exit().success());
}

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

// --- graph glyph sets ----------------------------------------------------
//
// `--graphs` picks the glyph set every graph draws with. The negative half of
// each test below is the one that carries weight: a style that silently fell
// back to braille would satisfy any assertion that only looked for its own
// glyphs, because braille cells are what the default already paints.

/// A cell from the braille block, whichever dots are set.
fn is_braille(c: char) -> bool {
    ('\u{2800}'..='\u{28ff}').contains(&c)
}

/// `ui::EIGHTHS` without its blank level, and the `ui::ASCII_RAMP` levels a
/// waveform can actually index — it skips empty cells, so the `_` baseline
/// belongs to `mini_spark` alone and never appears in a graph.
const EIGHTHS: &str = "▁▂▃▄▅▆▇█";
const ASCII_RAMP: &str = ".-+#";

/// Screen rows of the first card's waveform, top and bottom inclusive.
/// `waveform_halves` writes its own `gpu%` at the graph's top-left and
/// `mem%` at its bottom-left, so the labels bracket exactly the rows the
/// graph owns — and nothing else on screen is mistakable for them, the
/// process table's column being upper-case `GPU%`.
fn waveform_rows(t: &mut Tui) -> (u16, u16) {
    // The graph is drawn only once a card has history and the card has rows
    // to spare, so wait for the labels rather than assume the first frame.
    t.wait_for("a drawn waveform", |s| {
        s.contains("gpu%") && s.contains("mem%")
    });
    let screen = t.parser.screen();
    let top = (0..ROWS)
        .find(|&y| row_text(screen, y).contains("gpu%"))
        .expect("a gpu% label");
    let bot = (top..ROWS)
        .find(|&y| row_text(screen, y).contains("mem%"))
        .expect("a mem% label below the gpu% one");
    (top, bot)
}

/// Every glyph the first card's waveform drew. Whole rows go in as they are:
/// the two labels and the card border carry no character from any of the
/// three glyph sets, while the process commands and card captions that do
/// (`.`, `-`, `+`, `·`) all sit outside these rows.
fn waveform_glyphs(t: &Tui, (top, bot): (u16, u16)) -> String {
    let screen = t.parser.screen();
    (top..=bot).map(|y| row_text(screen, y)).collect()
}

/// The default style, and the contrast the other two are read against.
#[test]
fn the_default_graph_style_draws_braille_dots() {
    let mut t = Tui::spawn(&[]);
    let rows = waveform_rows(&mut t);
    let glyphs = waveform_glyphs(&t, rows);
    assert!(
        glyphs.chars().any(is_braille),
        "no braille in the waveform:\n{glyphs}"
    );
    assert!(
        !glyphs
            .chars()
            .any(|c| EIGHTHS.contains(c) || ASCII_RAMP.contains(c)),
        "the braille waveform drew a cell glyph:\n{glyphs}"
    );
    assert!(
        meter_line(&t.screen_text(), "GPU ", "■·").is_some(),
        "meters are not on the unicode glyphs:\n{}",
        t.screen_text()
    );
    t.send("q");
    assert!(t.wait_exit().success());
}

/// `--graphs block` must reach `draw_waveform_cells`' block arm — including
/// the fg/bg swap it draws the down-growing half with, which is where the
/// colour-quantization bug lived.
#[test]
fn block_graphs_draw_eighth_blocks_and_swap_colours_below_the_midline() {
    let mut t = Tui::spawn(&["--graphs", "block"]);
    let rows = waveform_rows(&mut t);
    let glyphs = waveform_glyphs(&t, rows);
    assert!(
        glyphs.chars().any(|c| EIGHTHS.contains(c)),
        "no eighth blocks in the waveform:\n{glyphs}"
    );
    assert!(
        !glyphs
            .chars()
            .any(|c| is_braille(c) || ASCII_RAMP.contains(c)),
        "the block waveform drew another style's glyph:\n{glyphs}"
    );
    let screen_text = t.screen_text();
    // `mini_spark` branches on the same setting, and it is the only other
    // braille in the UI — so one sweep of the whole screen catches it.
    assert!(
        !screen_text.chars().any(is_braille),
        "braille outside the waveform: a graph ignored --graphs block:\n{screen_text}"
    );
    assert!(
        meter_line(&screen_text, "GPU ", "■·").is_some(),
        "block mode changed the meter glyphs, which only ascii does:\n{screen_text}"
    );

    // Unicode has no upper-partial block, so the down-growing half paints the
    // *empty* part of a cell in the theme background over a bar-coloured one.
    // Prove that reaches the terminal: a cell below the midline whose
    // foreground is the page background, over a background that is not.
    let (top, bot) = rows;
    let screen = t.parser.screen();
    let page_bg = screen.cell(0, 0).expect("the top-left cell").bgcolor();
    // `waveform_halves` gives the up half `height / 2` rows and the rest to
    // the down half, so the down half starts here.
    let height = bot - top + 1;
    let midline = top + height / 2;
    let swapped = (midline..=bot).any(|y| {
        (0..COLS).any(|x| {
            screen
                .cell(y, x)
                .is_some_and(|c| c.fgcolor() == page_bg && c.bgcolor() != page_bg)
        })
    });
    assert!(
        swapped,
        "no fg/bg-swapped cell below the midline; the down half is not \
         drawing its partial cells:\n{glyphs}"
    );
    t.send("q");
    assert!(t.wait_exit().success());
}

/// `--graphs ascii` is the setting for a terminal with no block or braille
/// coverage at all, so nothing it draws may reach outside 7-bit ascii — the
/// meters included, which are `draw_meter`'s own branch on the style.
#[test]
fn ascii_graphs_draw_the_ascii_ramp_and_plain_meters() {
    let mut t = Tui::spawn(&["--graphs", "ascii"]);
    let rows = waveform_rows(&mut t);
    let glyphs = waveform_glyphs(&t, rows);
    assert!(
        glyphs.chars().any(|c| ASCII_RAMP.contains(c)),
        "no ascii ramp in the waveform:\n{glyphs}"
    );
    assert!(
        !glyphs.chars().any(|c| is_braille(c) || EIGHTHS.contains(c)),
        "the ascii waveform drew a unicode glyph:\n{glyphs}"
    );
    let s = t.screen_text();
    assert!(
        !s.chars().any(is_braille),
        "braille outside the waveform: a graph ignored --graphs ascii:\n{s}"
    );
    let util = meter_line(&s, "GPU ", "=.").expect("an ascii GPU meter");
    let glyph_count = util.chars().filter(|c| "=.".contains(*c)).count();
    assert!(glyph_count >= 40, "GPU meter has no ascii bar: {util:?}");
    assert!(
        !s.contains('■'),
        "the meters kept their unicode fill glyph under --graphs ascii:\n{s}"
    );
    t.send("q");
    assert!(t.wait_exit().success());
}

// --- colour modes --------------------------------------------------------
//
// `theme::detect_color_mode` reads the environment once at startup and every
// style is quantized as it is built, so the only end-to-end proof is the byte
// stream: what the app writes for a terminal that cannot take 24-bit colour.
// These read `Tui::raw` rather than the screen because vt100 normalises SGR
// into cell attributes and would hide the encoding entirely.

/// The parameter list of every SGR (`ESC [ … m`) sequence in a byte stream.
fn sgr_sequences(raw: &str) -> Vec<Vec<u16>> {
    let mut out = Vec::new();
    let mut rest = raw;
    while let Some(i) = rest.find("\x1b[") {
        let tail = &rest[i + 2..];
        let end = tail
            .find(|c: char| !c.is_ascii_digit() && c != ';')
            .unwrap_or(tail.len());
        if tail[end..].starts_with('m') {
            out.push(
                tail[..end]
                    .split(';')
                    .filter_map(|p| p.parse().ok())
                    .collect(),
            );
        }
        rest = &tail[end..];
    }
    out
}

/// The colours those sequences set, tagged the way SGR spells them: `38`/`48`
/// introduce an extended colour whose next parameter picks the encoding — `5`
/// for one palette index, `2` for three rgb components — and 30-37 / 40-47
/// plus their bright ranges are direct palette codes. `39`/`49` are "back to
/// the terminal's own default", which is what mono paints with and is not a
/// colour; every other parameter is an attribute (bold, dim, reverse).
fn colour_sgr(raw: &str) -> Vec<String> {
    let mut found = Vec::new();
    for params in sgr_sequences(raw) {
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                p @ (38 | 48) => {
                    let encoding = params.get(i + 1).copied().unwrap_or_default();
                    found.push(format!("{p};{encoding}"));
                    i += if encoding == 2 { 5 } else { 3 };
                }
                p @ (30..=37 | 40..=47 | 90..=97 | 100..=107) => {
                    found.push(p.to_string());
                    i += 1;
                }
                _ => i += 1,
            }
        }
    }
    found
}

/// `NO_COLOR=1` has to reach the terminal as no colour at all — not as a
/// muted palette. The app still has to be usable, so this asserts its content
/// is drawn rather than that it drew nothing.
#[test]
fn no_color_renders_the_dashboard_without_a_single_colour_escape() {
    let mut t = Tui::spawn_with_env(&[], &[("NO_COLOR", Some("1"))]);
    t.wait_for("cards", |s| {
        s.contains("Mock GPU 0") && s.contains("Mock GPU 1")
    });
    t.wait_for("process table", |s| s.contains("COMMAND"));
    t.wait_for("meters", |s| meter_line(s, "GPU ", "■·").is_some());
    assert!(
        t.screen_text().contains("gpur v"),
        "banner missing under NO_COLOR:\n{}",
        t.screen_text()
    );
    t.send("q");
    assert!(t.wait_exit().success());

    let raw = String::from_utf8_lossy(&t.raw).into_owned();
    let colours = colour_sgr(&raw);
    assert!(
        colours.is_empty(),
        "NO_COLOR still emitted colour: {colours:?}"
    );
    // Mono is not "no styling": with no background to highlight with, the
    // theme substitutes reverse video for the cursor row. Losing that would
    // leave the selection invisible on exactly the terminals this mode is for.
    assert!(
        raw.contains("\x1b[7m"),
        "mono drew no reverse video, so the selection is unmarked"
    );
}

/// The other route into mono, and the one that has to beat a `COLORTERM` the
/// environment still advertises — `detect_color_mode` reads `TERM=dumb`
/// first.
#[test]
fn term_dumb_renders_the_dashboard_in_mono() {
    let mut t = Tui::spawn_with_env(&[], &[("TERM", Some("dumb"))]);
    t.wait_for("cards", |s| s.contains("Mock GPU 0"));
    t.wait_for("process table", |s| s.contains("COMMAND"));
    t.send("q");
    assert!(t.wait_exit().success());

    let colours = colour_sgr(&String::from_utf8_lossy(&t.raw));
    assert!(
        colours.is_empty(),
        "TERM=dumb still emitted colour (COLORTERM won): {colours:?}"
    );
}

/// A terminal that never claimed truecolor gets the 256-colour palette. This
/// is the regression that matters: a 24-bit escape sent to a 256-colour
/// terminal is exactly what `theme::paint`'s quantization exists to prevent,
/// and it is invisible in every other test because the suite pins
/// `COLORTERM=truecolor`.
#[test]
fn without_colorterm_the_app_quantizes_to_the_256_colour_palette() {
    let mut t = Tui::spawn_with_env(&[], &[("COLORTERM", None)]);
    t.wait_for("cards", |s| s.contains("Mock GPU 0"));
    t.wait_for("meters", |s| meter_line(s, "GPU ", "■·").is_some());
    t.send("q");
    assert!(t.wait_exit().success());

    let colours = colour_sgr(&String::from_utf8_lossy(&t.raw));
    let truecolor: Vec<&String> = colours
        .iter()
        .filter(|c| *c == "38;2" || *c == "48;2")
        .collect();
    assert!(
        truecolor.is_empty(),
        "24-bit colour on a terminal that only advertised 256: {} sequences",
        truecolor.len()
    );
    // Both halves of the frame are painted, so demand both: an app that only
    // ever set a foreground would pass a foreground-only assertion while its
    // backgrounds went out unquantized.
    assert!(
        colours.iter().any(|c| c == "38;5"),
        "no 256-palette foreground was ever set: {colours:?}"
    );
    assert!(
        colours.iter().any(|c| c == "48;5"),
        "no 256-palette background was ever set: {colours:?}"
    );
}

/// Minimal libc-free SIGTERM via /bin/kill would need a shell; declare the
/// one libc fn we need instead of pulling the libc crate into dev-deps.
unsafe fn libc_kill(pid: i32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        kill(pid, 15);
    }
}
