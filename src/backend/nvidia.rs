//! NVIDIA backend via NVML (Linux + Windows). Loads libnvidia-ml dynamically;
//! probe fails soft on machines without the driver.

use super::{GpuBackend, GpuProcess, GpuSnapshot, ProcKind};
use anyhow::Result;
use nvml_wrapper::Nvml;
use nvml_wrapper::bitmasks::device::ThrottleReasons;
use nvml_wrapper::enum_wrappers::device::{Clock, PcieUtilCounter, TemperatureSensor};
use nvml_wrapper::enums::device::UsedGpuMemory;
use nvml_wrapper::struct_wrappers::device::ProcessInfo;
use std::collections::HashMap;

pub fn probe() -> Option<Box<dyn GpuBackend>> {
    let nvml = Nvml::init().ok()?;
    match nvml.device_count() {
        Ok(n) if n > 0 => Some(Box::new(NvmlBackend {
            nvml,
            count: n,
            last_util_ts: 0,
        })),
        _ => None,
    }
}

struct NvmlBackend {
    nvml: Nvml,
    count: u32,
    /// Microsecond timestamp of the newest process-utilization sample seen.
    last_util_ts: u64,
}

impl GpuBackend for NvmlBackend {
    fn name(&self) -> &'static str {
        "nvml"
    }

    fn poll(&mut self) -> Result<Vec<GpuSnapshot>> {
        let mut gpus = Vec::with_capacity(self.count as usize);
        for i in 0..self.count {
            // A device can drop off the bus (e.g. driver reset); skip, don't abort.
            let Ok(dev) = self.nvml.device_by_index(i) else {
                continue;
            };
            let memory = dev.memory_info().ok();
            let util = dev.utilization_rates().ok();
            gpus.push(GpuSnapshot {
                name: dev.name().unwrap_or_else(|_| format!("NVIDIA GPU {i}")),
                integrated: false,
                utilization_pct: util.as_ref().map(|u| u.gpu as f64).unwrap_or(0.0),
                mem_util_pct: util.as_ref().map(|u| u.memory as f64),
                video_util_pct: None,
                enc_util_pct: dev.encoder_utilization().ok().map(|u| u.utilization as f64),
                dec_util_pct: dev.decoder_utilization().ok().map(|u| u.utilization as f64),
                throttle: dev.current_throttle_reasons().ok().and_then(throttle_label),
                vram_used_bytes: memory.as_ref().map(|m| m.used).unwrap_or(0),
                vram_total_bytes: memory.as_ref().map(|m| m.total).unwrap_or(0),
                temperature_c: dev
                    .temperature(TemperatureSensor::Gpu)
                    .ok()
                    .map(|t| t as f64),
                // Milliwatts.
                power_w: dev.power_usage().ok().map(|p| p as f64 / 1000.0),
                power_limit_w: dev.enforced_power_limit().ok().map(|p| p as f64 / 1000.0),
                fan_pct: dev.fan_speed(0).ok().map(|f| f as f64),
                clock_mhz: dev.clock_info(Clock::Graphics).ok().map(u64::from),
                mem_clock_mhz: dev.clock_info(Clock::Memory).ok().map(u64::from),
                pcie_gen: dev.current_pcie_link_gen().ok().map(|g| g as u8),
                pcie_width: dev.current_pcie_link_width().ok(),
                pcie_max_gen: dev.max_pcie_link_gen().ok().map(|g| g as u8),
                pcie_max_width: dev.max_pcie_link_width().ok(),
                pcie_rx_kbs: dev
                    .pcie_throughput(PcieUtilCounter::Receive)
                    .ok()
                    .map(u64::from),
                pcie_tx_kbs: dev
                    .pcie_throughput(PcieUtilCounter::Send)
                    .ok()
                    .map(u64::from),
            });
        }
        Ok(gpus)
    }

    fn processes(&mut self) -> Vec<GpuProcess> {
        let mut out = Vec::new();
        for i in 0..self.count {
            let Ok(dev) = self.nvml.device_by_index(i) else {
                continue;
            };
            // pid -> (mem, kind); graphics wins when a pid appears in both.
            let mut procs: HashMap<u32, (u64, ProcKind)> = HashMap::new();
            for p in dev.running_compute_processes().unwrap_or_default() {
                procs.insert(p.pid, (used_bytes(&p), ProcKind::Compute));
            }
            for p in dev.running_graphics_processes().unwrap_or_default() {
                procs.insert(p.pid, (used_bytes(&p), ProcKind::Graphics));
            }
            let mut util: HashMap<u32, u32> = HashMap::new();
            if let Ok(samples) = dev.process_utilization_stats(self.last_util_ts) {
                for s in samples {
                    self.last_util_ts = self.last_util_ts.max(s.timestamp);
                    util.insert(s.pid, s.sm_util.min(100));
                }
            }
            out.extend(procs.into_iter().map(|(pid, (mem, kind))| GpuProcess {
                pid,
                gpu_index: i as usize,
                kind,
                gpu_util_pct: util.get(&pid).map(|u| *u as f64),
                gpu_mem_bytes: mem,
            }));
        }
        out
    }
}

/// Collapse NVML's throttle bitmask into a short human label; idle and
/// applications-clocks states aren't interesting throttles.
fn throttle_label(r: ThrottleReasons) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    if r.intersects(ThrottleReasons::SW_THERMAL_SLOWDOWN | ThrottleReasons::HW_THERMAL_SLOWDOWN) {
        parts.push("thermal");
    }
    if r.intersects(ThrottleReasons::SW_POWER_CAP | ThrottleReasons::HW_POWER_BRAKE_SLOWDOWN) {
        parts.push("power-limit");
    }
    if r.contains(ThrottleReasons::HW_SLOWDOWN) {
        parts.push("hw-slowdown");
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("+"))
    }
}

fn used_bytes(p: &ProcessInfo) -> u64 {
    match p.used_gpu_memory {
        UsedGpuMemory::Used(b) => b,
        UsedGpuMemory::Unavailable => 0,
    }
}
