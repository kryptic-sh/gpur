use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_gpur"))
}

#[test]
fn version_prints_name_and_semver() {
    let out = bin().arg("--version").output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.starts_with("gpur "), "unexpected --version output: {s}");
}

#[test]
fn help_shows_usage_and_art() {
    let out = bin().arg("--help").output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("Usage:"), "no usage section: {s}");
    assert!(s.contains("██"), "figlet art missing from help: {s}");
}

#[test]
fn unknown_flag_fails() {
    let out = bin().arg("--definitely-not-a-flag").output().unwrap();
    assert!(!out.status.success());
}

#[test]
fn json_snapshot_emits_valid_shape() {
    let out = bin()
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
