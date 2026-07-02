//! Apple Silicon backend via IOReport/IOKit (macOS only). Not yet implemented.

use super::GpuBackend;

pub fn probe() -> Option<Box<dyn GpuBackend>> {
    // TODO: IOReport energy/utilization channels + Metal device info.
    #[cfg(not(target_os = "macos"))]
    return None;
    #[cfg(target_os = "macos")]
    None
}
