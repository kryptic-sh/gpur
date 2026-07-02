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
}
