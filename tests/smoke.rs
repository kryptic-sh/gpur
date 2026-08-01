use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// A throwaway XDG root for one test. gpur resolves its config/cache/data
/// through hjkl-config, which honours `XDG_*_HOME` on every platform
/// (Windows and macOS included — it deliberately does not use `%APPDATA%`
/// or `~/Library`), so redirecting the three vars is enough to keep the
/// suite off the developer's real `~/.cache/gpur/state.json`.
struct Sandbox(PathBuf);

impl Sandbox {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "gpur-test-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Sandbox(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_gpur"));
        cmd.env("XDG_CONFIG_HOME", &self.0);
        cmd.env("XDG_CACHE_HOME", &self.0);
        cmd.env("XDG_DATA_HOME", &self.0);
        cmd
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn version_prints_name_and_semver() {
    let sb = Sandbox::new("version");
    let out = sb.cmd().arg("--version").output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.starts_with("gpur "), "unexpected --version output: {s}");
}

#[test]
fn help_shows_usage_and_art() {
    let sb = Sandbox::new("help");
    let out = sb.cmd().arg("--help").output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("Usage:"), "no usage section: {s}");
    assert!(s.contains("██"), "figlet art missing from help: {s}");
}

#[test]
fn replay_round_trips_a_log_recording() {
    let sb = Sandbox::new("replay");
    let log = sb.path().join("rec.jsonl");

    let rec = sb
        .cmd()
        .args(["--mock", "--once", "--tick-ms", "100", "--log"])
        .arg(&log)
        .output()
        .unwrap();
    assert!(rec.status.success());

    let out = sb
        .cmd()
        .args(["--json", "--tick-ms", "100", "--replay"])
        .arg(&log)
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(v["backend"], "replay");
    assert_eq!(v["gpus"].as_array().unwrap().len(), 2);
    // Recorded enrichment must survive playback (no re-resolution of pids).
    let procs = v["processes"].as_array().unwrap();
    assert!(
        procs
            .iter()
            .any(|p| p["command"].as_str().unwrap().contains("gpur")),
        "recorded command lost in replay"
    );
}

/// A metric the backend could not read is `null` in the record and `n/a` in
/// the plain line — never a 0 that a consumer reads as a real idle sample.
#[test]
fn absent_metrics_are_null_not_zero() {
    let sb = Sandbox::new("nulls");
    let log = sb.path().join("rec.jsonl");
    std::fs::write(
        &log,
        "{\"gpus\":[{\"name\":\"Unreadable GPU\"}],\"processes\":[]}\n",
    )
    .unwrap();

    let out = sb
        .cmd()
        .args(["--json", "--tick-ms", "100", "--replay"])
        .arg(&log)
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    let g = &v["gpus"][0];
    assert_eq!(g["name"], "Unreadable GPU");
    for key in ["utilization_pct", "vram_used_bytes", "vram_total_bytes"] {
        assert!(g[key].is_null(), "{key} should be null, got {}", g[key]);
    }

    let out = sb
        .cmd()
        .args(["--once", "--tick-ms", "100", "--replay"])
        .arg(&log)
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("util n/a"),
        "plain output fabricated a util: {s}"
    );
    assert!(
        s.contains("vram n/a/n/a"),
        "plain output fabricated a vram pool: {s}"
    );
}

#[test]
fn unknown_flag_fails() {
    let sb = Sandbox::new("badflag");
    let out = sb.cmd().arg("--definitely-not-a-flag").output().unwrap();
    assert!(!out.status.success());
}

#[test]
fn json_snapshot_emits_valid_shape() {
    let sb = Sandbox::new("json");
    let out = sb
        .cmd()
        .args(["--mock", "--json", "--tick-ms", "100"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
    assert_eq!(v["backend"], "mock");
    assert_eq!(v["gpus"].as_array().unwrap().len(), 2);
    assert!(v["gpus"][0]["utilization_pct"].is_number());

    // Process rows must be populated and sorted (gpu-mem desc) — this runs
    // on the Windows runner too, where the PTY suite can't.
    let procs = v["processes"].as_array().unwrap();
    assert!(procs.len() >= 4, "expected mock process rows");
    for p in procs {
        assert!(p["pid"].as_u64().unwrap() > 0);
        assert!(p["gpu_index"].is_number());
        assert!(!p["command"].as_str().unwrap().is_empty());
        assert!(!p["user"].as_str().unwrap().is_empty());
    }
    let first = procs.first().unwrap()["gpu_mem_bytes"].as_u64().unwrap();
    let last = procs.last().unwrap()["gpu_mem_bytes"].as_u64().unwrap();
    assert!(first >= last, "rows not sorted by gpu-mem desc");
    // The snapshot process (this test's child) must attribute itself.
    assert!(
        procs
            .iter()
            .any(|p| p["command"].as_str().unwrap().contains("gpur")),
        "own process missing from attribution"
    );
}

#[test]
fn completions_flag_covers_all_shells() {
    // A shell-specific dispatch line, not just the binary name: every
    // generator prints "gpur" in its preamble, so a generator emitting only
    // a comment header would pass a bare-name assertion.
    for (shell, marker) in [
        ("bash", "complete -F _gpur"),
        ("zsh", "#compdef gpur"),
        ("fish", "complete -c gpur"),
        (
            "powershell",
            "Register-ArgumentCompleter -Native -CommandName 'gpur'",
        ),
        ("elvish", "set edit:completion:arg-completer[gpur]"),
        ("nushell", "export extern gpur"),
    ] {
        let sb = Sandbox::new(shell);
        let out = sb
            .cmd()
            .args(["--completions", shell])
            .output()
            .expect("spawn gpur");
        assert!(out.status.success(), "--completions {shell} failed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(marker),
            "{shell} completions missing {marker:?}:\n{stdout}"
        );
    }
}

/// `--man` is hidden, so nothing in normal use would notice it rotting — but
/// the packaging CI ships its output as a release asset, which means an empty
/// or truncated render reaches every distro's `man gpur`. The option markers
/// are the assertion with teeth: `.TH` alone would still pass if clap_mangen
/// emitted a header and nothing else, and troff escapes the dashes, so these
/// also catch a renderer that stopped escaping and started emitting raw text.
#[test]
fn man_page_documents_the_current_cli_flags() {
    let sb = Sandbox::new("man");
    let out = sb.cmd().arg("--man").output().expect("spawn gpur");
    assert!(out.status.success(), "--man failed");
    let s = String::from_utf8_lossy(&out.stdout);

    assert!(s.contains(".TH gpur 1"), "no troff man header:\n{s}");
    assert!(s.contains(".SH OPTIONS"), "no OPTIONS section:\n{s}");
    // Distinctive flags: a man page that no longer tracks `Cli` drops these
    // long before it drops the boilerplate around them.
    for marker in [
        r"\fB\-\-replay\fR",
        r"\fB\-\-graphs\fR",
        r"\fB\-\-tick\-ms\fR",
    ] {
        assert!(s.contains(marker), "man page missing {marker:?}:\n{s}");
    }
}

/// The populated counterpart to `absent_metrics_are_null_not_zero`, which only
/// pins the `n/a` path. `--mock` is deterministic, so the whole plain-text
/// layout can be checked: two cards, three process rows each, every metric
/// filled in. The `16384MiB` total is the load-bearing part — `--once` keeps
/// raw MiB on purpose (a deliberate divergence from the TUI's `human_bytes`,
/// recorded in the backlog's decisions), so a well-meaning switch to `16.0GiB`
/// would silently break every script parsing this output.
#[test]
fn once_prints_populated_plain_rows_for_the_mock_gpus() {
    let sb = Sandbox::new("once");
    let out = sb
        .cmd()
        .args(["--mock", "--once", "--tick-ms", "100"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    let (gpu_lines, proc_lines): (Vec<&str>, Vec<&str>) =
        s.lines().partition(|l| !l.starts_with("  pid "));

    assert_eq!(gpu_lines.len(), 2, "expected one line per mock GPU:\n{s}");
    for (i, line) in gpu_lines.iter().enumerate() {
        assert!(
            line.starts_with(&format!("{i}  Mock GPU {i}  util ")),
            "GPU line {i} malformed: {line:?}"
        );
        assert!(
            line.contains("MiB/16384MiB"),
            "vram pool is not raw used/total MiB: {line:?}"
        );
        // The mock fills in every optional sensor, so the trailing temp and
        // power segments must both be present and neither may be `n/a`.
        assert!(line.contains("°C"), "temperature dropped: {line:?}");
        assert!(line.ends_with('W'), "power dropped: {line:?}");
        assert!(
            !line.contains("n/a"),
            "mock metric came back unread: {line:?}"
        );
    }

    // Three rows per card, all attributed and all carrying a utilization —
    // `-` here would mean the per-process delta never got its priming poll.
    assert_eq!(
        proc_lines.len(),
        6,
        "expected 3 process rows per card:\n{s}"
    );
    for line in &proc_lines {
        let cols: Vec<&str> = line.split_whitespace().collect();
        assert!(
            cols[1].parse::<u32>().is_ok_and(|pid| pid > 0),
            "process row has no pid: {line:?}"
        );
        assert!(cols[3].parse::<usize>().is_ok(), "no gpu index: {line:?}");
        assert!(
            cols[4].ends_with('%'),
            "per-process utilization missing: {line:?}"
        );
        assert!(cols[5].ends_with("MiB"), "no raw MiB column: {line:?}");
    }
    for command in [
        "ollama runner --model llama3:70b",
        "blender -b scene.blend -f 120",
        "ffmpeg -hwaccel cuda -i in.mkv out.mp4",
    ] {
        assert!(
            s.contains(command),
            "mock command {command:?} missing:\n{s}"
        );
    }
    // Row 0 is gpur itself, deliberately left un-enriched so the host columns
    // come from sysinfo rather than the mock's fabricated table.
    assert!(
        s.contains("gpur"),
        "own process missing from the rows:\n{s}"
    );
}

/// `snapshot` polls twice — a priming poll so delta-based metrics are real,
/// then the one it reports. Only the second may be logged; if the priming poll
/// ever reaches the sink, every `--once --log` invocation silently doubles its
/// record count and poisons the averages of whatever consumes the file.
#[test]
fn once_with_log_writes_exactly_one_record() {
    let sb = Sandbox::new("oncelog");
    let log = sb.path().join("rec.jsonl");

    let out = sb
        .cmd()
        .args(["--mock", "--once", "--tick-ms", "100", "--log"])
        .arg(&log)
        .output()
        .unwrap();
    assert!(out.status.success());

    let body = std::fs::read_to_string(&log).unwrap();
    let records: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        records.len(),
        1,
        "--once --log wrote {} records",
        records.len()
    );
    let v: serde_json::Value = serde_json::from_str(records[0]).expect("valid JSON record");
    assert_eq!(v["gpus"].as_array().unwrap().len(), 2);
}

/// Proves the sandbox is the directory the binary actually reads from: a
/// config planted in it must reach the binary. Without this, a broken
/// redirect would silently fall back to the developer's real dotfiles and
/// every other test in this file would go back to being non-hermetic.
#[test]
fn config_is_read_from_the_sandboxed_xdg_dir() {
    let sb = Sandbox::new("config");
    let dir = sb.path().join("gpur");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), "graphs = \"nope\"\n").unwrap();

    let out = sb.cmd().args(["--mock", "--json"]).output().unwrap();
    assert!(!out.status.success(), "invalid sandbox config was ignored");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("unknown graphs value"),
        "config not loaded from XDG_CONFIG_HOME: {err}"
    );
}
