//! Playback backend: re-drives the TUI from a `--log` JSONL recording.
//! One record per poll; at EOF (or on any read/parse trouble) the last
//! frame holds forever — a replay must never trip the failure-re-detect
//! path or swap itself for a live backend mid-session.

use super::{GpuBackend, GpuProcess, GpuSnapshot};
use anyhow::{Context, Result};
use std::io::BufRead;
use std::path::Path;

/// Line cap for the replay reader. A recording is untrusted input, and a
/// single oversized line (or a `gpus`/`processes` array with millions of
/// entries) must not be materialized wholesale — that is how a crafted
/// recording OOMs the process. Lines past the cap are dropped the way
/// malformed ones are.
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

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

/// `BufRead::lines` with a per-line cap: reads in bounded chunks instead of
/// allocating a whole line up front, so a multi-GB line costs no more than
/// the cap. A line past the cap is drained and dropped, like a malformed
/// one; a final line without a trailing newline is still returned.
struct BoundedLines<R: BufRead> {
    inner: R,
    cap: usize,
}

impl<R: BufRead> BoundedLines<R> {
    fn new(inner: R, cap: usize) -> Self {
        BoundedLines { inner, cap }
    }

    /// Skip the rest of a line whose start already exceeded the cap, without
    /// materializing any of it.
    fn drain_to_newline(&mut self) {
        loop {
            let chunk = match self.inner.fill_buf() {
                Ok([]) => return,
                Ok(c) => c,
                Err(_) => return,
            };
            match chunk.iter().position(|&b| b == b'\n') {
                Some(p) => {
                    self.inner.consume(p + 1);
                    return;
                }
                None => {
                    let len = chunk.len();
                    self.inner.consume(len);
                }
            }
        }
    }
}

impl<R: BufRead> Iterator for BoundedLines<R> {
    type Item = std::io::Result<String>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let chunk = match self.inner.fill_buf() {
                Ok([]) => {
                    // EOF. A buffered tail without a trailing newline is a
                    // real (final) line; an empty buffer is the end.
                    return if buf.is_empty() {
                        None
                    } else {
                        Some(Ok(String::from_utf8_lossy(&buf).into_owned()))
                    };
                }
                Ok(c) => c,
                Err(e) => return Some(Err(e)),
            };
            if let Some(pos) = chunk.iter().position(|&b| b == b'\n') {
                if buf.len() + pos <= self.cap {
                    buf.extend_from_slice(&chunk[..pos]);
                    self.inner.consume(pos + 1);
                    return Some(Ok(String::from_utf8_lossy(&buf).into_owned()));
                }
                // Oversized: drop it and move on to the next line.
                self.inner.consume(pos + 1);
                buf.clear();
                continue;
            }
            buf.extend_from_slice(chunk);
            let len = chunk.len();
            self.inner.consume(len);
            if buf.len() > self.cap {
                // The line's end is still ahead and it already exceeds the
                // cap — drain the rest of it without allocating it.
                self.drain_to_newline();
                buf.clear();
            }
        }
    }
}

pub struct ReplayBackend {
    lines: BoundedLines<std::io::BufReader<std::fs::File>>,
    last: LogRecord,
    finished: bool,
    /// `load` already consumed the first record into `last` to validate the
    /// file; the first poll must hand that one back without advancing.
    first: bool,
}

pub fn load(path: &Path) -> Result<Box<dyn GpuBackend>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening replay log {}", path.display()))?;
    let mut lines = BoundedLines::new(std::io::BufReader::new(file), MAX_LINE_BYTES);
    // Require at least one valid record up front so a wrong file errors
    // loudly at startup instead of showing an empty dashboard.
    let first = next_record(&mut lines)
        .with_context(|| format!("{}: no valid JSONL records", path.display()))?;
    Ok(Box::new(ReplayBackend {
        lines,
        last: first,
        finished: false,
        first: true,
    }))
}

/// Next parseable record, skipping malformed lines (truncated tail writes)
/// and oversized ones (the reader's line cap).
fn next_record<R: BufRead>(lines: &mut BoundedLines<R>) -> Option<LogRecord> {
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
        if self.first {
            // `load` preloaded the first record; hand it back as-is so the
            // first poll plays the same frame `load` validated.
            self.first = false;
        } else if !self.finished {
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

    /// `gpu_mem_bytes` widened to `Option<u64>`; a recording written before
    /// that carries a plain number and must still mean what it said. `0` in
    /// particular is a recorded measurement of zero, not the new "unknown".
    #[test]
    fn a_pre_option_recording_keeps_its_recorded_memory() {
        let path = write_log(
            "mem",
            concat!(
                r#"{"gpus":[{"name":"card0"}],"processes":["#,
                r#"{"pid":1,"gpu_mem_bytes":0},"#,
                r#"{"pid":2,"gpu_mem_bytes":2147483648}]}"#,
                "\n"
            ),
        );
        let mut b = load(&path).unwrap();
        let procs = b.processes();
        assert_eq!(procs[0].gpu_mem_bytes, Some(0));
        assert_eq!(procs[1].gpu_mem_bytes, Some(2_147_483_648));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// A record that names no memory at all — the `#[serde(default)]` path —
    /// is the unknown, and must not default to a fabricated zero.
    #[test]
    fn a_record_omitting_memory_replays_as_unknown() {
        let path = write_log(
            "nomem",
            concat!(
                r#"{"gpus":[{"name":"card0"}],"processes":[{"pid":1}]}"#,
                "\n"
            ),
        );
        let mut b = load(&path).unwrap();
        assert_eq!(b.processes()[0].gpu_mem_bytes, None);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// `load` validates the file by preloading the first record into `last`;
    /// the first poll must replay that record rather than skipping straight
    /// to the second, and EOF must keep holding the final frame.
    #[test]
    fn the_first_poll_returns_the_preloaded_record() {
        let path = write_log(
            "first",
            concat!(
                r#"{"gpus":[{"name":"first"}],"processes":[]}"#,
                "\n",
                r#"{"gpus":[{"name":"second"}],"processes":[]}"#,
                "\n"
            ),
        );
        let mut b = load(&path).unwrap();
        assert_eq!(b.poll().unwrap()[0].name, "first");
        assert_eq!(b.poll().unwrap()[0].name, "second");
        // At EOF the last frame holds forever.
        assert_eq!(b.poll().unwrap()[0].name, "second");
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

    /// The line reader drops a line past its cap instead of materializing
    /// it, keeps lines at or under the cap, and still returns a final line
    /// without a trailing newline. A 4-byte `BufReader` forces every line to
    /// span several chunks, so the accumulate and drain paths both run.
    #[test]
    fn bounded_lines_drops_oversized_lines_and_keeps_the_rest() {
        let data = "123456789\n12345678\nabc\n123456789012345\nno-eol";
        //       ^ 9 > 8            ^ 8 = 8   ^ 3 ok  ^ 15 > 8         ^ 6 ok
        let lines = BoundedLines::new(
            std::io::BufReader::with_capacity(4, std::io::Cursor::new(data)),
            8,
        );
        let got: Vec<String> = lines.map(|l| l.unwrap()).collect();
        assert_eq!(got, vec!["12345678", "abc", "no-eol"]);
    }

    /// End to end: a recording whose line exceeds `MAX_LINE_BYTES` plays
    /// past the drop — `load` skips the giant record the way it skips a
    /// malformed one, and the record after it replays. On the unbounded
    /// reader the giant record was parsed and returned first.
    #[test]
    fn a_record_longer_than_the_line_cap_is_dropped() {
        let giant = format!(
            r#"{{"gpus":[{{"name":"{}"}}],"processes":[]}}"#,
            "x".repeat(MAX_LINE_BYTES + 1)
        );
        let path = write_log(
            "giant",
            &format!("{giant}\n{{\"gpus\":[{{\"name\":\"ok\"}}],\"processes\":[]}}\n"),
        );
        let mut b = load(&path).unwrap();
        assert_eq!(b.poll().unwrap()[0].name, "ok");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
