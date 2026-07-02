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
        t.pump_once(Duration::from_millis(50));
    }
    t.parser = vt100::Parser::new(ROWS, COLS, 0);
    t.wait_for("re-render after storm", |s| s.contains("Mock GPU 0"));
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
