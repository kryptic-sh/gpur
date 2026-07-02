//! NVIDIA backend via NVML (Linux + Windows). Loads libnvidia-ml dynamically;
//! probe fails soft on machines without the driver.

use super::{GpuBackend, GpuSnapshot};
use anyhow::Result;
use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::{Clock, PcieUtilCounter, TemperatureSensor};

pub fn probe() -> Option<Box<dyn GpuBackend>> {
    let nvml = Nvml::init().ok()?;
    match nvml.device_count() {
        Ok(n) if n > 0 => Some(Box::new(NvmlBackend { nvml, count: n })),
        _ => None,
    }
}

struct NvmlBackend {
    nvml: Nvml,
    count: u32,
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
}
