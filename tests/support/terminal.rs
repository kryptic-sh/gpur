use super::{COLS, MOUSE_LEFT, ROWS, WHEEL_UP};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// A throwaway XDG root for one test. gpur resolves its config/cache/data
/// through hjkl-config, which honours `XDG_*_HOME` on every platform, so
/// redirecting the three vars keeps the suite off the developer's real
/// `~/.cache/gpur/state.json` — which the app both reads at startup and
/// overwrites on a clean quit.
pub(super) struct Sandbox(pub(super) PathBuf);

impl Sandbox {
    pub(super) fn new() -> Self {
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

pub(super) struct Tui {
    pub(super) parser: vt100::Parser,
    rx: mpsc::Receiver<Result<Vec<u8>, String>>,
    output_closed: bool,
    service: Arc<Mutex<vt100::Parser<CursorReplies>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pub(super) child: ChildGuard,
    /// Every byte the app ever wrote, for teardown-sequence assertions.
    pub(super) raw: Vec<u8>,
    /// Removed on drop; keep it alive for the whole run.
    home: Sandbox,
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
}

impl Tui {
    pub(super) fn spawn(extra_args: &[&str]) -> Self {
        Self::spawn_with_env(extra_args, &[])
    }

    pub(super) fn spawn_with_env(extra_args: &[&str], env: &[(&str, Option<&str>)]) -> Self {
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
    pub(super) fn spawn_in(args: &[&str], env: &[(&str, Option<&str>)], home: Sandbox) -> Self {
        let pty = bounded("open", || {
            native_pty_system()
                .openpty(PtySize {
                    rows: ROWS,
                    cols: COLS,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap()
        });
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_gpur"));
        cmd.args(args);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        // A parent shell's NO_COLOR must not leak into the harness: the colour
        // assertions in this file depend on the app painting, and NO_COLOR is
        // the one variable the child would otherwise inherit. An empty value
        // reads as "colour is fine" to `detect_color_mode`, and the explicit
        // `("NO_COLOR", Some("1"))` override in the no_color test replaces it.
        cmd.env("NO_COLOR", "");
        cmd.env("XDG_CONFIG_HOME", &home.0);
        cmd.env("XDG_CACHE_HOME", &home.0);
        cmd.env("XDG_DATA_HOME", &home.0);
        for (k, v) in env {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            }
        }
        let mut reader = pty.master.try_clone_reader().unwrap();
        let writer = Arc::new(Mutex::new(pty.master.take_writer().unwrap()));
        let replies = writer.clone();
        let (tx, rx) = mpsc::channel();
        let service = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            ROWS,
            COLS,
            0,
            CursorReplies(Vec::new(), (ROWS, COLS)),
        )));
        let reader_service = service.clone();
        // Service ConPTY's cursor queries before spawn and throughout shutdown.
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let response = {
                            let mut parser = reader_service.lock().unwrap();
                            parser.process(&buf[..n]);
                            std::mem::take(&mut parser.callbacks_mut().0)
                        };
                        if !response.is_empty() {
                            let mut writer = replies.lock().unwrap();
                            // One write: ConPTY's initial cursor handshake does not
                            // accept write_fmt's separately written fragments.
                            if let Err(e) =
                                writer.write_all(&response).and_then(|()| writer.flush())
                            {
                                let _ = tx.send(Err(format!("cursor response: {e}")));
                            }
                        }
                        // ClosePseudoConsole needs draining even after receiver drop.
                        let _ = tx.send(Ok(buf[..n].to_vec()));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    #[cfg(unix)]
                    Err(e) if e.raw_os_error() == Some(5) => break, // PTY EOF is EIO on Linux.
                    Err(e) => {
                        let _ = tx.send(Err(format!("PTY reader: {e}")));
                        break;
                    }
                }
            }
        });
        let child = bounded("spawn", move || {
            let child = ChildGuard(pty.slave.spawn_command(cmd).unwrap());
            drop(pty.slave);
            child
        });

        Tui {
            parser: vt100::Parser::new(ROWS, COLS, 0),
            rx,
            output_closed: false,
            service,
            writer,
            child,
            raw: Vec::new(),
            home,
            master: Some(pty.master),
        }
    }

    /// Where the app persists its UI state under this test's sandbox.
    pub(super) fn state_file(&self) -> PathBuf {
        self.home.0.join("gpur").join("state.json")
    }

    pub(super) fn pump_once(&mut self, timeout: Duration) -> bool {
        match self.rx.recv_timeout(timeout) {
            Ok(Ok(bytes)) => {
                self.raw.extend_from_slice(&bytes);
                self.parser.process(&bytes);
                true
            }
            Ok(Err(e)) => panic!("{e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.output_closed = true;
                false
            }
        }
    }

    pub(super) fn screen_text(&self) -> String {
        // ConPTY emits wrapped rows; contents() joins those logical lines.
        let screen = self.parser.screen();
        screen
            .rows(0, screen.size().1)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Poll the emulated screen until `pred` holds — never trust a fixed
    /// sleep to mean "rendered".
    pub(super) fn wait_for(&mut self, what: &str, pred: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if pred(&self.screen_text()) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; screen:\n{}; status: {:?}",
                self.screen_text(),
                self.child.try_wait()
            );
            self.pump_once(Duration::from_millis(100));
        }
    }

    pub(super) fn send(&mut self, keys: &str) {
        let writer = self.writer.clone();
        let keys = keys.as_bytes().to_vec();
        bounded("write", move || {
            let mut writer = writer.lock().unwrap();
            writer.write_all(&keys).unwrap();
            writer.flush().unwrap();
        });
    }

    /// One SGR mouse report over a 0-based screen cell. The encoding is
    /// 1-based, `M` presses and `m` releases; the wheel buttons have no
    /// release, exactly as a real terminal reports them.
    pub(super) fn mouse(&mut self, button: u8, col: u16, row: u16) {
        self.send(&format!("\x1b[<{button};{};{}M", col + 1, row + 1));
        if button < WHEEL_UP {
            self.send(&format!("\x1b[<{button};{};{}m", col + 1, row + 1));
        }
    }

    pub(super) fn click(&mut self, col: u16, row: u16) {
        self.mouse(MOUSE_LEFT, col, row);
    }

    /// Like [`wait_for`], but over every byte the app has written — for
    /// states that flash by between frames and would be erased from the
    /// screen before the next poll of the emulator.
    pub(super) fn wait_for_raw(&mut self, what: &str, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if String::from_utf8_lossy(&self.raw).contains(needle) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; screen:\n{}; status: {:?}",
                self.screen_text(),
                self.child.try_wait()
            );
            self.pump_once(Duration::from_millis(100));
        }
    }

    /// Pump for a fixed window, then return the screen. The app repaints on
    /// every tick even when nothing changed, so waiting for the pty to fall
    /// silent would never return.
    pub(super) fn drain(&mut self, window: Duration) -> String {
        let until = Instant::now() + window;
        while Instant::now() < until {
            self.pump_once(Duration::from_millis(50));
        }
        self.screen_text()
    }

    pub(super) fn wait_exit(&mut self) -> portable_pty::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            // Keep draining so the child can't block on a full pty buffer.
            self.pump_once(Duration::from_millis(50));
            if let Some(status) = self.child.try_wait().expect("query child status") {
                let master = self.master.take();
                bounded("close", move || drop(master));
                // Wait for EOF, not a quiet window: teardown can arrive late.
                let deadline = Instant::now() + Duration::from_secs(10);
                while !self.output_closed {
                    assert!(Instant::now() < deadline, "PTY output did not close");
                    self.pump_once(Duration::from_millis(100));
                }
                return status;
            }
            assert!(Instant::now() < deadline, "child did not exit");
        }
    }

    pub(super) fn resize(&mut self, rows: u16, cols: u16) {
        {
            let mut service = self.service.lock().unwrap();
            // vt100 underflows while wrapping a one-row grid. Keep its
            // scratch grid larger, but clamp cursor replies to the real PTY.
            service.screen_mut().set_size(rows.max(2), cols.max(2));
            service.callbacks_mut().1 = (rows, cols);
        }
        let master = self.master.take().expect("open PTY");
        self.master = Some(bounded("resize", move || {
            master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap();
            master
        }));
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        self.child.cleanup();
        let master = self.master.take();
        bounded("close during cleanup", move || drop(master));
    }
}

pub(super) struct ChildGuard(Box<dyn portable_pty::Child + Send + Sync>);
impl std::ops::Deref for ChildGuard {
    type Target = dyn portable_pty::Child + Send + Sync;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}
impl std::ops::DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.0
    }
}
impl ChildGuard {
    fn cleanup(&mut self) {
        match self.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(e) => eprintln!("query child during cleanup: {e}"),
        }
        let mut killer = self.clone_killer();
        // portable-pty's WinChildKiller reports Err even when TerminateProcess
        // succeeds. The observed exit below is authoritative on every platform.
        if let Err(e) = bounded("kill child", move || killer.kill()) {
            eprintln!("kill child during cleanup: {e}");
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                status => panic!("child cleanup failed: {status:?}"),
            }
        }
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn bounded<T: Send + 'static>(
    operation: &'static str,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(15))
        .unwrap_or_else(|e| {
            // Unwind through ChildGuard so a failed native operation still
            // kills/reaps the child. Nextest bounds the whole test process too.
            panic!("PTY {operation} failed: {e}");
        })
}

struct CursorReplies(Vec<u8>, (u16, u16));
impl vt100::Callbacks for CursorReplies {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        if i1.is_none() && i2.is_none() && params == [&[6_u16][..]] && c == 'n' {
            let (row, col) = screen.cursor_position();
            self.0.extend_from_slice(
                format!(
                    "\x1b[{};{}R",
                    (row + 1).min(self.1.0),
                    (col + 1).min(self.1.1)
                )
                .as_bytes(),
            );
        }
    }
}

#[test]
fn cursor_queries_are_fragmented_repeated_and_position_aware() {
    let mut p =
        vt100::Parser::new_with_callbacks(ROWS, COLS, 0, CursorReplies(Vec::new(), (ROWS, COLS)));
    for chunk in [
        b"\x1b[4;9H\x1b[".as_slice(),
        b"6",
        b"nabc\x1b[6n",
        b"\x1b[5n\x1b[?6n",
    ] {
        p.process(chunk);
    }
    assert_eq!(p.callbacks().0, b"\x1b[4;9R\x1b[4;12R");
    p.callbacks_mut().0.clear();
    p.callbacks_mut().1 = (1, 1);
    p.screen_mut().set_size(2, 2);
    p.process(b"abcdef\x1b[6n");
    assert_eq!(p.callbacks().0, b"\x1b[1;1R");
}

#[test]
fn unwind_reaps_child_after_output_receiver_disappears() {
    let mut t = Tui::spawn(&[]);
    t.wait_for("live dashboard", |s| s.contains("Mock GPU 0"));
    let pid = sysinfo::Pid::from_u32(t.child.process_id().expect("child pid"));
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    assert!(
        system.process(pid).is_some(),
        "child was never observed alive"
    );
    let (_, rx) = mpsc::channel();
    drop(std::mem::replace(&mut t.rx, rx));
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _t = t;
        panic!("exercise assertion-failure cleanup");
    }));
    assert!(panic.is_err());
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    assert!(system.process(pid).is_none(), "child survived unwinding");
}
