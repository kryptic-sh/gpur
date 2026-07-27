//! Deterministic fake GPUs for UI development and demos (`--mock`).

use super::{GpuBackend, GpuProcess, GpuSnapshot, ProcKind};
use anyhow::Result;

/// Mock pids start well above anything the kernel hands out (Linux caps
/// pid_max at 2^22, and the classic default is 32768), so a demo can never
/// point a row at a live system process the way plain `1..3n` did.
const PID_BASE: u32 = 1_000_000;

/// Synthetic rows: mock pids resolve to nothing on this host, so supply the
/// host columns here instead of rendering a table full of "?".
const FAKE: [(&str, &str); 6] = [
    ("alice", "python train.py --epochs 30 --amp"),
    ("bob", "ollama runner --model llama3:70b"),
    ("alice", "blender -b scene.blend -f 120"),
    ("root", "/usr/lib/xorg/Xorg vt2 -displayfd 3"),
    ("bob", "ffmpeg -hwaccel cuda -i in.mkv out.mp4"),
    ("carol", "/usr/lib/firefox/firefox -contentproc"),
];

pub struct MockBackend {
    tick: u64,
    count: usize,
}

impl MockBackend {
    pub fn new(count: usize) -> Self {
        Self { tick: 0, count }
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
        // Test hook: GPUR_MOCK_FAIL=N fails every Nth poll to exercise the
        // graceful-degradation path.
        if let Some(n) = std::env::var("GPUR_MOCK_FAIL")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            && n > 0
            && self.tick.is_multiple_of(n)
        {
            anyhow::bail!("simulated driver reset (tick {})", self.tick);
        }
        let total = 16 * 1024 * 1024 * 1024u64;
        let gpus = (0..self.count)
            .map(|i| {
                let util = self.wave(i as f64 * 1.3, 37.0 + 11.0 * i as f64);
                let vram = self.wave(i as f64 * 2.1 + 0.7, 97.0);
                GpuSnapshot {
                    name: format!("Mock GPU {i}"),
                    integrated: i == 0,
                    utilization_pct: util,
                    mem_util_pct: Some(util * 0.6),
                    video_util_pct: (i % 2 == 0).then_some(util * 0.4),
                    enc_util_pct: (i % 2 == 1).then_some(util * 0.3),
                    dec_util_pct: (i % 2 == 1).then_some(util * 0.5),
                    throttle: (util > 95.0).then(|| "power-limit".to_string()),
                    vram_used_bytes: (total as f64 * vram / 100.0) as u64,
                    vram_total_bytes: total,
                    temperature_c: Some(45.0 + util * 0.4),
                    power_w: Some(60.0 + util * 2.4),
                    power_limit_w: Some(320.0),
                    fan_pct: Some((util * 0.9).min(100.0)),
                    clock_mhz: Some(1200 + (util * 12.0) as u64),
                    mem_clock_mhz: Some(9000),
                    // GPU 1 demos the PCIe downgrade warning.
                    pcie_gen: if i == 1 { Some(3) } else { Some(4) },
                    pcie_width: if i == 1 { Some(8) } else { Some(16) },
                    pcie_max_gen: Some(4),
                    pcie_max_width: Some(16),
                    pcie_rx_kbs: Some((util * 8000.0) as u64),
                    pcie_tx_kbs: Some((util * 3000.0) as u64),
                    fan_rpm: Some(800 + (util * 14.0) as u64),
                    temp_junction_c: Some(50.0 + util * 0.45),
                    gtt_used_bytes: Some((util * 2e7) as u64),
                    gtt_total_bytes: Some(8 * 1024 * 1024 * 1024),
                    volt_mv: Some(700 + (util * 4.0) as u64),
                    ..Default::default()
                }
            })
            .collect();
        Ok(gpus)
    }

    fn driver_info(&self) -> Option<String> {
        Some("mock driver 1.0".into())
    }

    fn processes(&mut self) -> Vec<GpuProcess> {
        let util = self.wave(0.4, 53.0);
        // A few rows per GPU so the table can overflow and scroll in demos.
        (0..self.count * 3)
            .map(|i| {
                // Row 0 is gpur itself, left un-enriched so sysinfo fills in
                // genuine host columns for one row; the rest are fabricated.
                let own = i == 0;
                let (user, command) = FAKE[i % FAKE.len()];
                GpuProcess {
                    pid: if own {
                        std::process::id()
                    } else {
                        PID_BASE + i as u32
                    },
                    gpu_index: i % self.count,
                    kind: if i % 3 == 0 {
                        ProcKind::Graphics
                    } else {
                        ProcKind::Compute
                    },
                    gpu_util_pct: Some((util * (0.9 - 0.1 * i as f64)).max(0.0)),
                    gpu_mem_bytes: (3000 >> i.min(8)) as u64 * 1024 * 1024 + 64 * 1024 * 1024,
                    user: (!own).then(|| user.to_string()),
                    command: (!own).then(|| command.to_string()),
                    cpu_pct: (!own).then(|| (util * (0.5 - 0.05 * i as f64)).max(0.0) as f32),
                    host_mem_bytes: (!own).then(|| (400 >> i.min(6)) as u64 * 1024 * 1024),
                    container: None,
                }
            })
            .collect()
    }

    /// Mock pids are fabricated; nothing here may be signalled.
    fn can_signal(&self) -> bool {
        false
    }
}
