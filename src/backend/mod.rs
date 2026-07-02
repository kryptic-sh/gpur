//! GPU telemetry backends. One trait, one impl per vendor/platform.

mod amd;
mod apple;
mod intel;
#[cfg(target_os = "linux")]
mod linux;
mod mock;
mod nvidia;
mod replay;
mod windows;

pub use mock::MockBackend;

use anyhow::Result;

/// One sample of one GPU at one instant.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct GpuSnapshot {
    pub name: String,
    /// Integrated (APU/iGPU) as opposed to a discrete card.
    pub integrated: bool,
    /// Core utilization, 0..=100.
    pub utilization_pct: f64,
    /// Memory-controller busy %, distinct from VRAM fill level.
    pub mem_util_pct: Option<f64>,
    /// Video engine busy % — unified (VCN/media) engines report here.
    pub video_util_pct: Option<f64>,
    /// Split encoder/decoder utilization where the vendor separates them.
    pub enc_util_pct: Option<f64>,
    pub dec_util_pct: Option<f64>,
    /// Active clock-throttle cause ("thermal", "power-limit", ...), when
    /// known or confidently derivable.
    pub throttle: Option<String>,
    pub vram_used_bytes: u64,
    pub vram_total_bytes: u64,
    pub temperature_c: Option<f64>,
    pub power_w: Option<f64>,
    pub power_limit_w: Option<f64>,
    pub fan_pct: Option<f64>,
    pub clock_mhz: Option<u64>,
    pub mem_clock_mhz: Option<u64>,
    /// Current PCIe generation (1..=7).
    pub pcie_gen: Option<u8>,
    /// Current PCIe lane count.
    pub pcie_width: Option<u32>,
    /// Maximum supported PCIe generation/width, for downgrade detection.
    pub pcie_max_gen: Option<u8>,
    pub pcie_max_width: Option<u32>,
    /// PCIe throughput, KiB/s.
    pub pcie_rx_kbs: Option<u64>,
    pub pcie_tx_kbs: Option<u64>,
}

impl GpuSnapshot {
    pub fn vram_pct(&self) -> f64 {
        if self.vram_total_bytes == 0 {
            return 0.0;
        }
        self.vram_used_bytes as f64 / self.vram_total_bytes as f64 * 100.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ProcKind {
    Graphics,
    #[default]
    Compute,
}

impl ProcKind {
    pub fn label(&self) -> &'static str {
        match self {
            ProcKind::Graphics => "Graphic",
            ProcKind::Compute => "Compute",
        }
    }
}

/// One process currently using one GPU.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct GpuProcess {
    pub pid: u32,
    /// Index into the snapshot vec returned by `poll`.
    pub gpu_index: usize,
    pub kind: ProcKind,
    /// GPU utilization attributable to this process, when the backend knows.
    pub gpu_util_pct: Option<f64>,
    pub gpu_mem_bytes: u64,
    /// Pre-enriched host data. Live backends leave these None (sysinfo fills
    /// them in); the replay backend supplies the RECORDED values so playback
    /// doesn't resolve foreign pids against this host.
    pub user: Option<String>,
    pub command: Option<String>,
    pub cpu_pct: Option<f32>,
    pub host_mem_bytes: Option<u64>,
}

/// A source of GPU telemetry. Implementations poll all devices they can see.
pub trait GpuBackend {
    /// Human-readable backend name ("nvml", "amdgpu", "metal", "mock").
    fn name(&self) -> &'static str;
    /// Sample every visible GPU. Index order must be stable across calls.
    fn poll(&mut self) -> Result<Vec<GpuSnapshot>>;
    /// Processes using the GPUs, sampled after `poll`. Backends without
    /// per-process visibility return nothing.
    fn processes(&mut self) -> Vec<GpuProcess> {
        Vec::new()
    }
}

/// Pick the first backend that reports usable devices on this machine.
pub fn detect(
    mock: Option<usize>,
    replay: Option<&std::path::Path>,
) -> Result<Box<dyn GpuBackend>> {
    if let Some(path) = replay {
        return replay::load(path);
    }
    if let Some(n) = mock {
        return Ok(Box::new(MockBackend::new(n.clamp(1, 16))));
    }
    if let Some(b) = nvidia::probe() {
        return Ok(b);
    }
    if let Some(b) = amd::probe() {
        return Ok(b);
    }
    if let Some(b) = intel::probe() {
        return Ok(b);
    }
    if let Some(b) = apple::probe() {
        return Ok(b);
    }
    // Vendor-generic Windows fallback (Task Manager counters).
    if let Some(b) = windows::probe() {
        return Ok(b);
    }
    anyhow::bail!("no supported GPU backend found (run with --mock to demo the UI)")
}
