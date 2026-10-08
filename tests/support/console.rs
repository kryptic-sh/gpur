use super::ChildGuard;
use std::fs::File;
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Console::{
    CONSOLE_MODE, COORD, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_MOUSE_INPUT,
    ENABLE_PROCESSED_INPUT, GetConsoleMode, ReadConsoleOutputCharacterW, SetConsoleMode,
    WriteConsoleOutputCharacterW,
};

fn console(name: &str) -> File {
    File::options().read(true).write(true).open(name).unwrap()
}

fn contents(output: &File) -> String {
    let mut text = vec![0; usize::from(crate::ROWS) * usize::from(crate::COLS)];
    let mut read = 0;
    // The File owns the live console handle and the slice/count remain valid
    // throughout this synchronous call.
    unsafe {
        ReadConsoleOutputCharacterW(
            HANDLE(output.as_raw_handle()),
            &mut text,
            COORD { X: 0, Y: 0 },
            &mut read,
        )
        .unwrap();
    }
    String::from_utf16(&text[..read as usize]).unwrap()
}

fn mode(input: &File) -> CONSOLE_MODE {
    let mut mode = CONSOLE_MODE::default();
    // The File retains the console handle; mode is a valid writable output.
    unsafe { GetConsoleMode(HANDLE(input.as_raw_handle()), &mut mode).unwrap() };
    mode
}

/// Re-entered in a separate process inside the PTY, so native console queries
/// cannot interfere with other tests sharing the runner's console.
pub(crate) fn observe_console() -> bool {
    if std::env::var_os("GPUR_TEST_CONSOLE_OBSERVER").is_none() {
        return false;
    }
    let home = PathBuf::from(std::env::var_os("GPUR_TEST_CONSOLE_HOME").unwrap());
    let input = console("CONIN$");
    let main = console("CONOUT$");
    let cooked = ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT;
    let baseline = (mode(&input) | cooked) & !ENABLE_MOUSE_INPUT;
    const MARKER: &str = "gpur original console buffer";
    let marker: Vec<u16> = MARKER.encode_utf16().collect();
    let mut written = 0;
    // Both Files own live console handles; marker and written live for the call.
    unsafe {
        SetConsoleMode(HANDLE(input.as_raw_handle()), baseline).unwrap();
        WriteConsoleOutputCharacterW(
            HANDLE(main.as_raw_handle()),
            &marker,
            COORD { X: 0, Y: 0 },
            &mut written,
        )
        .unwrap();
    }
    assert_eq!(written as usize, marker.len());
    let original = contents(&main);
    assert!(original.starts_with(MARKER));
    let mut child = ChildGuard(Box::new(
        std::process::Command::new(env!("CARGO_BIN_EXE_gpur"))
            .args(["--no-splash", "--mock", "--tick-ms", "100"])
            .env_remove("GPUR_TEST_CONSOLE_OBSERVER")
            .spawn()
            .unwrap(),
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    let active = loop {
        // Observe console contents directly, not ConPTY's translated VT stream.
        let active = console("CONOUT$");
        let dashboard = contents(&active);
        if dashboard.contains("Mock GPU 0") {
            assert!(!dashboard.contains(MARKER));
            let live_mode = mode(&input);
            assert_eq!(live_mode & cooked, CONSOLE_MODE(0), "raw mode not enabled");
            assert_ne!(live_mode & ENABLE_MOUSE_INPUT, CONSOLE_MODE(0));
            break active;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "dashboard exited early"
        );
        assert!(
            Instant::now() < deadline,
            "active dashboard was not observed"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    std::fs::write(home.join("console-active"), b"observed").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "dashboard did not quit");
        std::thread::sleep(Duration::from_millis(10));
    }
    let restored = console("CONOUT$");
    assert!(contents(&restored) == original, "main screen not restored");
    assert_eq!(mode(&input), baseline, "input modes not restored");
    drop(active);
    true
}
