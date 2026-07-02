//! PTY integration tests: run the real binary against a pseudo-terminal,
//! parse its output with a vt100 emulator, and assert on the rendered
//! screen. Unix-only (ConPTY in CI is flaky); mock backend throughout.
#![cfg(unix)]

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::Read;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const COLS: u16 = 120;
const ROWS: u16 = 36;

struct Tui {
    parser: vt100::Parser,
    rx: mpsc::Receiver<Vec<u8>>,
    writer: Box<dyn std::io::Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// Every byte the app ever wrote, for teardown-sequence assertions.
    raw: Vec<u8>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Tui {
    fn spawn(extra_args: &[&str]) -> Self {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_gpur"));
        cmd.args(["--mock", "--no-splash", "--tick-ms", "100"]);
        cmd.args(extra_args);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
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
            _master: pty.master,
        }
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

#[test]
fn renders_dashboard_and_process_table() {
    let mut t = Tui::spawn(&[]);
    t.wait_for("GPU cards", |s| {
        s.contains("Mock GPU 0") && s.contains("Mock GPU 1")
    });
    t.wait_for("process table", |s| s.contains("COMMAND"));
    t.wait_for("meters", |s| s.contains("GPU ") && s.contains("MEM "));
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
    t.wait_for("process rows", |s| s.contains("COMMAND"));
    // The mock's first process row is this test binary itself.
    t.send("/gpur\r");
    t.wait_for("filter caption", |s| s.contains("filter:gpur"));
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
    // The mock's first process row is the app itself: its pid must appear
    // in the table with the binary name, enriched by sysinfo.
    let pid = t.child.process_id().expect("child pid").to_string();
    t.wait_for("own process row", move |s| {
        s.contains(&pid) && s.contains("gpur")
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

#[test]
fn kill_dialog_opens_and_cancels() {
    let mut t = Tui::spawn(&[]);
    t.wait_for("process rows", |s| s.contains("COMMAND"));
    t.send("p");
    t.send("x");
    t.wait_for("confirm popup", |s| s.contains("send SIGTERM to"));
    t.send("n"); // cancel — nothing must die
    t.wait_for("popup gone", |s| !s.contains("send SIGTERM to"));
    assert!(
        !matches!(t.child.try_wait(), Ok(Some(_))),
        "app died after cancelled kill"
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
