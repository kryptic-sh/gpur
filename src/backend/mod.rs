//! GPU telemetry backends. One trait, one impl per vendor/platform.

mod amd;
mod apple;
mod mock;
mod nvidia;
mod windows;

pub use mock::MockBackend;

use anyhow::Result;

/// One sample of one GPU at one instant.
#[derive(Debug, Clone, Default)]
pub struct GpuSnapshot {
    pub name: String,
    /// Integrated (APU/iGPU) as opposed to a discrete card.
    pub integrated: bool,
    /// Core utilization, 0..=100.
    pub utilization_pct: f64,
    /// Memory-controller busy %, distinct from VRAM fill level.
    pub mem_util_pct: Option<f64>,
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

/// A source of GPU telemetry. Implementations poll all devices they can see.
pub trait GpuBackend {
    /// Human-readable backend name ("nvml", "amdgpu", "metal", "mock").
    fn name(&self) -> &'static str;
    /// Sample every visible GPU. Index order must be stable across calls.
    fn poll(&mut self) -> Result<Vec<GpuSnapshot>>;
}

/// Pick the first backend that reports usable devices on this machine.
pub fn detect(force_mock: bool) -> Result<Box<dyn GpuBackend>> {
    if force_mock {
        return Ok(Box::new(MockBackend::new()));
    }
    if let Some(b) = nvidia::probe() {
        return Ok(b);
    }
    if let Some(b) = amd::probe() {
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
