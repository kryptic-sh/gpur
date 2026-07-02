//! NVIDIA backend via NVML (all platforms). Not yet implemented.

use super::GpuBackend;

pub fn probe() -> Option<Box<dyn GpuBackend>> {
    // TODO: nvml-wrapper — Nvml::init(), device_count(), per-device
    // utilization_rates / memory_info / temperature / power_usage.
    None
}
