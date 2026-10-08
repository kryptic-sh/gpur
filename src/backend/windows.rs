//! Windows vendor-generic backend: PDH "GPU Engine" / "GPU Adapter Memory"
//! performance counters (what Task Manager uses) + DXGI for adapter names and
//! VRAM totals. Covers AMD and Intel on Windows, where NVML doesn't apply.
//! It runs alongside the vendor backends rather than instead of them —
//! `detect()` passes the PCI vendors those already cover and the adapters from
//! those vendors are dropped here, so an NVIDIA dGPU comes from NVML while the
//! AMD or Intel iGPU beside it still shows up.
//! Temperature/fan/clocks are not exposed by PDH — those need ADLX (TODO).

use super::GpuBackend;

/// `claimed` lists PCI vendor ids another backend already reports devices for.
/// `None` when nothing is left to report, so an NVIDIA-only rig keeps NVML as
/// its sole backend instead of gaining an empty peer.
pub fn probe(claimed: &[u16]) -> Option<Box<dyn GpuBackend>> {
    #[cfg(windows)]
    if let Some(b) = win::probe(claimed) {
        return Some(b);
    }
    #[cfg(not(windows))]
    let _ = claimed;
    None
}

#[cfg_attr(not(windows), allow(dead_code))]
mod aggregate;
#[cfg_attr(not(windows), allow(dead_code))]
mod parse;
#[cfg(windows)]
#[path = "windows/native.rs"]
mod win;

#[cfg(windows)]
mod pdh;
