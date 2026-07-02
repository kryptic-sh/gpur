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
