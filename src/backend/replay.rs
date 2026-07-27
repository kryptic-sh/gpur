//! Playback backend: re-drives the TUI from a `--log` JSONL recording.
//! One record per poll; at EOF (or on any read/parse trouble) the last
//! frame holds forever — a replay must never trip the failure-re-detect
//! path or swap itself for a live backend mid-session.

use super::{GpuBackend, GpuProcess, GpuSnapshot};
use anyhow::{Context, Result};
use std::io::BufRead;
use std::path::Path;

#[derive(serde::Deserialize)]
struct LogRecord {
    /// Attribution written since the record schema gained it; older
    /// recordings simply have none.
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    driver: Option<String>,
    #[serde(default)]
    gpus: Vec<GpuSnapshot>,
    #[serde(default)]
    processes: Vec<GpuProcess>,
}

pub struct ReplayBackend {
    lines: std::io::Lines<std::io::BufReader<std::fs::File>>,
    last: LogRecord,
    finished: bool,
}

pub fn load(path: &Path) -> Result<Box<dyn GpuBackend>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening replay log {}", path.display()))?;
    let mut lines = std::io::BufReader::new(file).lines();
    // Require at least one valid record up front so a wrong file errors
    // loudly at startup instead of showing an empty dashboard.
    let first = next_record(&mut lines)
        .with_context(|| format!("{}: no valid JSONL records", path.display()))?;
    Ok(Box::new(ReplayBackend {
        lines,
        last: first,
        finished: false,
    }))
}

/// Next parseable record, skipping malformed lines (truncated tail writes).
fn next_record(lines: &mut std::io::Lines<std::io::BufReader<std::fs::File>>) -> Option<LogRecord> {
    for line in lines.by_ref() {
        let Ok(line) = line else { return None };
        if let Ok(rec) = serde_json::from_str::<LogRecord>(&line) {
            return Some(rec);
        }
    }
    None
}

impl GpuBackend for ReplayBackend {
    fn name(&self) -> &'static str {
        "replay"
    }

    fn poll(&mut self) -> Result<Vec<GpuSnapshot>> {
        if !self.finished {
            match next_record(&mut self.lines) {
                Some(rec) => self.last = rec,
                None => self.finished = true, // hold the final frame
            }
        }
        Ok(self.last.gpus.clone())
    }

    fn processes(&mut self) -> Vec<GpuProcess> {
        self.last.processes.clone()
    }

    /// Show what produced the recording, not what is running here. `name()`
    /// must stay `'static`, so the recorded backend rides along in this line.
    fn driver_info(&self) -> Option<String> {
        match (&self.last.backend, &self.last.driver) {
            (Some(b), Some(d)) => Some(format!("{b} · {d}")),
            (Some(b), None) => Some(b.clone()),
            (None, d) => d.clone(),
        }
    }

    /// Recorded pids belong to the machine that produced the log.
    fn can_signal(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_log(name: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gpur-replay-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rec.jsonl");
        std::fs::write(&path, body).unwrap();
        path
    }

    /// Attribution and container come off the record, not off this host.
    #[test]
    fn recorded_attribution_survives_playback() {
        let path = write_log(
            "attr",
            concat!(
                r#"{"ts_ms":1,"backend":"nvml","driver":"550.1","gpus":[],"#,
                r#""processes":[{"pid":7,"container":"docker:abcdef123456"}]}"#,
                "\n"
            ),
        );
        let mut b = load(&path).unwrap();
        assert_eq!(b.driver_info().as_deref(), Some("nvml · 550.1"));
        assert_eq!(
            b.processes()[0].container.as_deref(),
            Some("docker:abcdef123456")
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// Pre-attribution recordings still load; the header just stays blank.
    #[test]
    fn legacy_records_without_attribution_still_load() {
        let path = write_log("legacy", "{\"gpus\":[],\"processes\":[]}\n");
        let b = load(&path).unwrap();
        assert_eq!(b.driver_info(), None);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
