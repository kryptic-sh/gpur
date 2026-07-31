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
        let mut gpus = self.last.gpus.clone();
        // Recordings made before device ids existed carry none. Position
        // within one recording IS a stable identity — the log is a fixed
        // sequence of frames, not live hardware — so filling the gap here is
        // honest, and a recorded id always wins over it.
        for (i, g) in gpus.iter_mut().enumerate() {
            if g.device_id.is_none() {
                g.device_id = Some(format!("replay:{i}"));
            }
        }
        Ok(gpus)
    }

    fn processes(&mut self) -> Vec<GpuProcess> {
        // Nothing vets a recording: a truncated write, a hand-edited log or a
        // third-party writer can carry a gpu_index past the end of the frame
        // it belongs to, which renders as a "DEV 7" row against a two-card
        // machine. `CompositeBackend::processes` drops exactly this case for
        // its live children; a replay is never composed, so it has to apply
        // the rule itself. Dropping the row loses one row; keeping it
        // misattributes it, or invents a device that was never recorded.
        let frame = self.last.gpus.len();
        self.last
            .processes
            .iter()
            .filter(|p| p.gpu_index < frame)
            .cloned()
            .collect()
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
                r#"{"ts_ms":1,"backend":"nvml","driver":"550.1","gpus":[{"name":"card0"}],"#,
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

    /// A recorded row can name a card the frame doesn't contain — a truncated
    /// or hand-edited log. It has no card to be drawn against, so it goes.
    #[test]
    fn rows_naming_a_card_outside_the_recorded_frame_are_dropped() {
        let path = write_log(
            "stray",
            concat!(
                r#"{"gpus":[{"name":"card0"},{"name":"card1"}],"processes":["#,
                r#"{"pid":1,"gpu_index":0},{"pid":2,"gpu_index":1},"#,
                r#"{"pid":3,"gpu_index":7}]}"#,
                "\n"
            ),
        );
        let mut b = load(&path).unwrap();
        assert_eq!(b.poll().unwrap().len(), 2);
        let pids: Vec<u32> = b.processes().iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![1, 2]);
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
