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

    fn spawn_with_env(extra_args: &[&str], env: &[(&str, &str)]) -> Self {
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
    fn spawn_in(args: &[&str], env: &[(&str, &str)], home: Sandbox) -> Self {
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
            cmd.env(k, v);
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

/// PIDs of the process-table data rows, in the order they are drawn. Card
/// lines never qualify: their first cell is a label or a graph glyph.
fn proc_pids(screen: &str) -> Vec<u32> {
    screen
        .lines()
        .filter_map(|l| {
            let l = l.trim_start_matches('│').trim_start();
            if !l.contains("MiB") {
                return None;
            }
            l.split_whitespace().next()?.parse().ok()
        })
        .collect()
}

/// A drawn meter line — `label`, a run of meter glyphs, then the value.
/// Matching on the glyph run is the point: the process-table header alone
/// carries both "GPU " and "MEM ".
fn meter_line(screen: &str, label: &str) -> Option<String> {
    screen
        .lines()
        .map(|l| l.trim_start_matches('│').trim_end().trim_end_matches('│'))
        .find(|l| l.starts_with(label) && (l.contains('■') || l.contains('·')))
        .map(str::to_string)
}

#[test]
fn renders_dashboard_and_process_table() {
    let mut t = Tui::spawn(&[]);
    t.wait_for("GPU cards", |s| {
        s.contains("Mock GPU 0") && s.contains("Mock GPU 1")
    });
    t.wait_for("process table", |s| s.contains("COMMAND"));
    t.wait_for("meters", |s| meter_line(s, "GPU ").is_some());
    let s = t.screen_text();
    let util = meter_line(&s, "GPU ").expect("GPU meter");
    let glyphs = util.chars().filter(|c| "■·".contains(*c)).count();
    assert!(glyphs >= 40, "GPU meter has no bar: {util:?}");
    assert!(util.trim_end().ends_with('%'), "GPU meter value: {util:?}");
    let vram = meter_line(&s, "MEM ").expect("MEM meter");
    let glyphs = vram.chars().filter(|c| "■·".contains(*c)).count();
    assert!(glyphs >= 40, "MEM meter has no bar: {vram:?}");
    assert!(vram.contains('/'), "MEM meter value: {vram:?}");
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
        meter_line(&s, "GPU ").is_none() && meter_line(&s, "MEM ").is_none(),
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
    let mut t = Tui::spawn_with_env(&[], &[("GPUR_MOCK_FAIL", "2")]);
    t.wait_for("cards", |s| s.contains("Mock GPU 0"));
    // Every second poll fails, so the banner is only up for one frame.
    t.wait_for_raw("poll-failure banner", "poll failed: simulated driver reset");
    let s = t.screen_text();
    assert!(s.contains("Mock GPU 0"), "snapshot dropped on poll failure");
    assert!(meter_line(&s, "GPU ").is_some(), "meters dropped");
    assert!(
        !matches!(t.child.try_wait(), Ok(Some(_))),
        "app exited on a backend failure"
    );
    t.send("q");
    assert!(t.wait_exit().success());
}

/// Every poll fails: the banner stays up, the GPU pane degrades to its
/// empty state, and the 5th consecutive failure triggers a re-detect.
#[test]
fn persistent_poll_failure_redetects_the_backend() {
    let mut t = Tui::spawn_with_env(&[], &[("GPUR_MOCK_FAIL", "1")]);
    t.wait_for("failure banner", |s| {
        s.contains("⚠ poll failed: simulated driver reset")
    });
    t.wait_for("empty GPU pane", |s| {
        s.contains("no GPUs reported by backend")
    });
    t.wait_for("re-detect status", |s| s.contains("backend re-detected"));
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
    assert_eq!(v["folded"], serde_json::json!([0]));
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
        r#"{"folded":[1],"sort_by":"Pid","sort_desc":false,"tick_ms":300}"#,
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
            let text: String = (0..COLS)
                .filter_map(|c| screen.cell(row, c).map(|cl| cl.contents()))
                .collect();
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
