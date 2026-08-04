//! Test-only backend: a signalable process table so the PTY suite can reach
//! the kill dialog end to end.
//!
//! mock and replay refuse to signal by design, so no backend can open the
//! kill dialog under them. This stub is selected by `GPUR_STUB_BACKEND=1`
//! (the same env-hook pattern as `GPUR_MOCK_FAIL`) and reports one real local
//! process — a spawned `sleep` — with `can_signal()` true, so the dialog,
//! its modal guards, and the confirm path are reachable from the harness.
//! The stub never signals anything itself; the kill path does, and only after
//! the same guards as any live backend.

use super::{GpuBackend, GpuProcess, GpuSnapshot, ProcKind};
use anyhow::Result;

pub struct StubBackend {
    /// The real local process the kill dialog can target. `None` if the spawn
    /// failed (no `sleep` on the host); the harness machines all have it.
    child: Option<std::process::Child>,
}

impl StubBackend {
    pub fn new() -> Self {
        Self {
            child: std::process::Command::new("sleep").arg("60").spawn().ok(),
        }
    }
}

impl Drop for StubBackend {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl GpuBackend for StubBackend {
    fn name(&self) -> &'static str {
        "stub"
    }

    fn poll(&mut self) -> Result<Vec<GpuSnapshot>> {
        Ok(vec![GpuSnapshot {
            name: "Stub GPU 0".into(),
            device_id: Some("stub:0".into()),
            utilization_pct: Some(50.0),
            vram_used_bytes: Some(1 << 30),
            vram_total_bytes: Some(8 << 30),
            temperature_c: Some(50.0),
            ..Default::default()
        }])
    }

    fn processes(&mut self) -> Vec<GpuProcess> {
        // Row 0 is gpur itself (like the mock's own row); row 1 is the child.
        // Two rows let a PTY test move the cursor, which the modal-guard
        // assertions need. Rows are left un-enriched so sysinfo fills the
        // host columns, exactly like the live backends.
        let mut out = vec![GpuProcess {
            pid: std::process::id(),
            gpu_index: 0,
            ..Default::default()
        }];
        if let Some(c) = &self.child {
            out.push(GpuProcess {
                pid: c.id(),
                gpu_index: 0,
                kind: ProcKind::Compute,
                ..Default::default()
            });
        }
        out
    }

    /// The pids are real local ones; the kill path's guards still apply.
    fn can_signal(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stub is what it claims: one fake GPU, two rows, one of which is a
    /// real local process, and a signalable backend.
    #[test]
    fn the_stub_reports_a_real_local_process_and_can_signal() {
        let mut b = StubBackend::new();
        assert!(b.can_signal());
        let gpus = b.poll().unwrap();
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].device_id.as_deref(), Some("stub:0"));
        let procs = b.processes();
        assert_eq!(procs.len(), 2, "own pid plus the child");
        let child_pid = b.child.as_ref().expect("sleep spawned").id();
        assert_eq!(procs[1].pid, child_pid);
        // The child is a live local process: kill(pid, 0) succeeds.
        assert_eq!(
            unsafe { libc::kill(child_pid as i32, 0) },
            0,
            "the stub's child is not a live process"
        );
    }
}
