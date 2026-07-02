//! AMD backend. Linux: sysfs/amdgpu + libdrm. Windows: ADLX. Not yet implemented.

use super::GpuBackend;

pub fn probe() -> Option<Box<dyn GpuBackend>> {
    // TODO Linux: /sys/class/drm/card*/device/{gpu_busy_percent,mem_info_vram_*,hwmon}.
    // TODO Windows: ADLX bindings.
    None
}
