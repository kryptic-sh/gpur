//! Deterministic fake GPUs for UI development and demos (`--mock`).

use super::{GpuBackend, GpuProcess, GpuSnapshot, ProcKind};
use anyhow::Result;

pub struct MockBackend {
    tick: u64,
}

impl MockBackend {
    pub fn new() -> Self {
        Self { tick: 0 }
    }

    fn wave(&self, phase: f64, period: f64) -> f64 {
        let t = self.tick as f64;
        50.0 + 50.0 * (t / period * std::f64::consts::TAU + phase).sin()
    }
}

impl GpuBackend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn poll(&mut self) -> Result<Vec<GpuSnapshot>> {
        self.tick += 1;
        let total = 16 * 1024 * 1024 * 1024u64;
        let gpus = (0..2)
            .map(|i| {
                let util = self.wave(i as f64 * 1.3, 37.0 + 11.0 * i as f64);
                let vram = self.wave(i as f64 * 2.1 + 0.7, 97.0);
                GpuSnapshot {
                    name: format!("Mock GPU {i}"),
                    integrated: i == 0,
                    utilization_pct: util,
                    mem_util_pct: Some(util * 0.6),
                    vram_used_bytes: (total as f64 * vram / 100.0) as u64,
                    vram_total_bytes: total,
                    temperature_c: Some(45.0 + util * 0.4),
                    power_w: Some(60.0 + util * 2.4),
                    power_limit_w: Some(320.0),
                    fan_pct: Some((util * 0.9).min(100.0)),
                    clock_mhz: Some(1200 + (util * 12.0) as u64),
                    mem_clock_mhz: Some(9000),
                    pcie_gen: Some(4),
                    pcie_width: Some(16),
                    pcie_rx_kbs: Some((util * 8000.0) as u64),
                    pcie_tx_kbs: Some((util * 3000.0) as u64),
                }
            })
            .collect();
        Ok(gpus)
    }

    fn processes(&mut self) -> Vec<GpuProcess> {
        let util = self.wave(0.4, 53.0);
        vec![
            GpuProcess {
                pid: std::process::id(),
                gpu_index: 0,
                kind: ProcKind::Graphics,
                gpu_util_pct: Some(util * 0.7),
                gpu_mem_bytes: 512 * 1024 * 1024 + (util * 1e7) as u64,
            },
            GpuProcess {
                pid: 1,
                gpu_index: 1,
                kind: ProcKind::Compute,
                gpu_util_pct: Some(util * 0.3),
                gpu_mem_bytes: 2048 * 1024 * 1024,
            },
        ]
    }
}
