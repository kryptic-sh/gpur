//! NVIDIA backend via NVML (Linux + Windows). Loads libnvidia-ml dynamically;
//! probe fails soft on machines without the driver, and on Linux falls back to
//! a sysfs read of any nouveau-driven card so an open-driver GPU is still
//! listed rather than dropped off the rig.

use super::{GpuBackend, GpuProcess, GpuSnapshot, ProcKind};
use anyhow::Result;
use nvml_wrapper::bitmasks::device::ThrottleReasons;
use nvml_wrapper::enum_wrappers::device::{
    Clock, PcieUtilCounter, PerformanceState, TemperatureSensor, TemperatureThreshold,
};
use nvml_wrapper::enums::device::{BusType, SampleValue, UsedGpuMemory};
use nvml_wrapper::struct_wrappers::device::{ProcessInfo, ProcessUtilizationSample};
use nvml_wrapper::structs::device::FieldId;
use nvml_wrapper::sys_exports::field_id::NVML_FI_DEV_MEMORY_TEMP;
use nvml_wrapper::{Device, Nvml};
use std::collections::HashMap;

pub fn probe() -> Option<Box<dyn GpuBackend>> {
    #[cfg(target_os = "linux")]
    {
        if let Some(nvml) = nvml_probe() {
            // NVML only sees the cards on the proprietary driver; a rig can
            // mix it with nouveau, so merge the sysfs cards in rather than
            // dropping them from the listing.
            return match nouveau::probe() {
                Some(nv) => Some(Box::new(MergedNvidiaBackend {
                    nvml,
                    nouveau: nv,
                    driver: std::sync::OnceLock::new(),
                }) as Box<dyn GpuBackend>),
                None => Some(Box::new(nvml) as Box<dyn GpuBackend>),
            };
        }
        // NVML is the only source of NVIDIA telemetry, and it exists only with
        // the proprietary driver. Without it the cards are still sitting in
        // /sys/class/drm, and no other backend will claim them — the AMD and
        // Intel scans filter on their own PCI vendor. So claim the
        // proprietary-driver cards from sysfs too: a card NVML cannot read is
        // still listed — name, PCI id, link state, gauges n/a — rather than
        // dropped off the rig.
        nouveau::probe_without_nvml()
    }
    #[cfg(not(target_os = "linux"))]
    {
        nvml_probe().map(|b| Box::new(b) as Box<dyn GpuBackend>)
    }
}

/// Device ids the sysfs fallback claims from a fake `/sys/class/drm`, for the
/// shared test that proves the Linux scans partition that one directory.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn claimed_ids(drm: &str) -> Vec<String> {
    nouveau::claimed_ids(drm)
}

fn nvml_probe() -> Option<NvmlBackend> {
    let nvml = Nvml::init().ok()?;
    match nvml.device_count() {
        Ok(n) if n > 0 => {
            let driver = nvml.sys_driver_version().ok();
            // Read once: name, bus type and max link are fixed for the life of
            // the device and a per-poll query is a driver round-trip per card.
            // `None` marks a query the driver refused, and poll falls back to
            // asking again rather than showing a hole.
            let mut uuids = Vec::with_capacity(n as usize);
            let mut names = Vec::with_capacity(n as usize);
            let mut integrated = Vec::with_capacity(n as usize);
            let mut pcie_max_gen = Vec::with_capacity(n as usize);
            let mut pcie_max_width = Vec::with_capacity(n as usize);
            let mut temp_slowdown = Vec::with_capacity(n as usize);
            let mut fan_count = Vec::with_capacity(n as usize);
            for i in 0..n {
                let Ok(d) = nvml.device_by_index(i) else {
                    uuids.push(None);
                    names.push(None);
                    integrated.push(None);
                    pcie_max_gen.push(None);
                    pcie_max_width.push(None);
                    temp_slowdown.push(None);
                    fan_count.push(None);
                    continue;
                };
                uuids.push(d.uuid().ok());
                names.push(d.name().ok());
                integrated.push(d.bus_type().ok().map(|b| matches!(b, BusType::Fpci)));
                pcie_max_gen.push(d.max_pcie_link_gen().ok().map(|g| g as u8));
                pcie_max_width.push(d.max_pcie_link_width().ok());
                temp_slowdown.push(slowdown_threshold_c(&d));
                // The fan COUNT is a board property, fixed for the device's
                // life — only the speeds are per-poll data.
                fan_count.push(d.num_fans().ok());
            }
            Some(NvmlBackend {
                nvml,
                count: n,
                // The header line is static per boot; format it once at probe
                // rather than re-formatting it on every frame's driver_info().
                driver: driver.map(|d| format!("driver {d}")),
                uuids,
                names,
                integrated,
                pcie_max_gen,
                pcie_max_width,
                temp_slowdown,
                fan_count,
                last_util_ts: vec![0; n as usize],
            })
        }
        _ => None,
    }
}

struct NvmlBackend {
    nvml: Nvml,
    count: u32,
    driver: Option<String>,
    /// `GPU-<uuid>` per device index, cached at probe. This is the device
    /// identity `App` keys its per-GPU state on; `None` for a device whose
    /// UUID the driver refused, which degrades to a positional key.
    uuids: Vec<Option<String>>,
    /// Name, "is this a Tegra/on-SoC iGPU", and the maximum PCIe link, all
    /// fixed for the life of the device and resolved once at probe — a
    /// per-poll query is a driver round-trip per card. `None` means the probe
    /// query failed and the poll falls back to asking again.
    names: Vec<Option<String>>,
    integrated: Vec<Option<bool>>,
    pcie_max_gen: Vec<Option<u8>>,
    pcie_max_width: Vec<Option<u32>>,
    /// Hardware-slowdown temperature threshold (°C) per device index, cached
    /// at probe like `names`/`integrated`/`pcie_max_*`: it is fixed for the
    /// life of the device and a per-poll query is a driver round-trip per
    /// card. `None` marks a query the driver declined, and poll falls back to
    /// asking again.
    temp_slowdown: Vec<Option<f64>>,
    /// Fan count per device index, cached at probe like the fields above — a
    /// board property, fixed for the device's life; only the speeds are
    /// per-poll data. `None` marks a query the driver declined, and the poll
    /// falls back to probing fan 0 the way it always has.
    fan_count: Vec<Option<u32>>,
    /// Microsecond timestamp of the newest process-utilization sample seen,
    /// **per device index**. `nvmlDeviceGetProcessUtilization` only returns
    /// samples strictly newer than the timestamp handed to it, and the
    /// timestamp is a host-wide CPU clock — so a single shared watermark
    /// lets device 0 advance it to "now" and starves devices 1..N of every
    /// sample. Each device therefore needs its own watermark.
    ///
    /// Seeded to 0, which makes the first call per device drain NVML's whole
    /// ring buffer (up to a few seconds of history). That is deliberate: it
    /// is the only way to get a populated GPU% column on the very first
    /// frame, and `fold_util` keeps only the newest sample per pid, so the
    /// extra backlog costs one larger allocation and nothing else.
    last_util_ts: Vec<u64>,
}

impl GpuBackend for NvmlBackend {
    fn name(&self) -> &'static str {
        "nvml"
    }

    fn poll(&mut self) -> Result<Vec<GpuSnapshot>> {
        let mut gpus = Vec::with_capacity(self.count as usize);
        for i in 0..self.count {
            let device_id = self.uuids.get(i as usize).cloned().flatten();
            // A device can drop off the bus (e.g. driver reset). Push a degraded
            // placeholder instead of skipping, so the card keeps its place on
            // screen rather than every later card jumping up a row and back
            // again next tick. It carries the cached UUID, so its graphs and
            // peaks continue as the same device — the placeholder is display
            // continuity, not the thing that keeps state attached.
            let Ok(dev) = self.nvml.device_by_index(i) else {
                gpus.push(GpuSnapshot {
                    name: format!("NVIDIA GPU {i} (unavailable)"),
                    device_id,
                    ..Default::default()
                });
                continue;
            };
            let memory = dev.memory_info().ok();
            let util = dev.utilization_rates().ok();
            let fans = self.fan_count.get(i as usize).copied().flatten();
            let (fan_pct, fan_rpm) = fan_speeds(&dev, fans);
            gpus.push(GpuSnapshot {
                name: self
                    .names
                    .get(i as usize)
                    .cloned()
                    .flatten()
                    .unwrap_or_else(|| dev.name().unwrap_or_else(|_| format!("NVIDIA GPU {i}"))),
                device_id,
                // NVML has no "is integrated" query. FPCI is Tegra's on-SoC host
                // interface and the only bus type no discrete card reports, so it
                // is the one signal that can flag a Jetson iGPU; anything else,
                // including an unsupported/missing query, stays discrete.
                integrated: self
                    .integrated
                    .get(i as usize)
                    .copied()
                    .flatten()
                    .unwrap_or_else(|| dev.bus_type().is_ok_and(|b| matches!(b, BusType::Fpci))),
                utilization_pct: util.as_ref().map(|u| super::clamp_pct(u.gpu as f64)),
                mem_util_pct: util.as_ref().map(|u| super::clamp_pct(u.memory as f64)),
                video_util_pct: None,
                enc_util_pct: dev
                    .encoder_utilization()
                    .ok()
                    .map(|u| super::clamp_pct(u.utilization as f64)),
                dec_util_pct: dev
                    .decoder_utilization()
                    .ok()
                    .map(|u| super::clamp_pct(u.utilization as f64)),
                throttle: dev.current_throttle_reasons().ok().and_then(throttle_label),
                vram_used_bytes: memory.as_ref().map(|m| m.used),
                vram_total_bytes: memory.as_ref().map(|m| m.total),
                temperature_c: dev
                    .temperature(TemperatureSensor::Gpu)
                    .ok()
                    .map(|t| t as f64),
                temp_slowdown_c: self
                    .temp_slowdown
                    .get(i as usize)
                    .copied()
                    .flatten()
                    .or_else(|| slowdown_threshold_c(&dev)),
                // NVML exposes no core-hotspot/junction field id, so
                // `temp_junction_c` stays None here (see `memory_temp_c`).
                temp_mem_c: memory_temp_c(&dev),
                // Milliwatts.
                power_w: dev.power_usage().ok().map(|p| p as f64 / 1000.0),
                power_limit_w: dev.enforced_power_limit().ok().map(|p| p as f64 / 1000.0),
                fan_pct,
                fan_rpm,
                clock_mhz: dev.clock_info(Clock::Graphics).ok().map(u64::from),
                mem_clock_mhz: dev.clock_info(Clock::Memory).ok().map(u64::from),
                pcie_gen: dev.current_pcie_link_gen().ok().map(|g| g as u8),
                pcie_width: dev.current_pcie_link_width().ok(),
                pcie_max_gen: self
                    .pcie_max_gen
                    .get(i as usize)
                    .copied()
                    .flatten()
                    .or_else(|| dev.max_pcie_link_gen().ok().map(|g| g as u8)),
                pcie_max_width: self
                    .pcie_max_width
                    .get(i as usize)
                    .copied()
                    .flatten()
                    .or_else(|| dev.max_pcie_link_width().ok()),
                pcie_rx_kbs: dev
                    .pcie_throughput(PcieUtilCounter::Receive)
                    .ok()
                    .map(kb_to_kib),
                pcie_tx_kbs: dev
                    .pcie_throughput(PcieUtilCounter::Send)
                    .ok()
                    .map(kb_to_kib),
                perf_level: dev.performance_state().ok().and_then(pstate_label),
                ..Default::default()
            });
        }
        Ok(gpus)
    }

    fn driver_info(&self) -> Option<String> {
        self.driver.clone()
    }

    fn processes(&mut self) -> Vec<GpuProcess> {
        let mut out = Vec::new();
        for i in 0..self.count {
            let Ok(dev) = self.nvml.device_by_index(i) else {
                continue;
            };
            // pid -> (mem, kind); graphics wins when a pid appears in both.
            let mut procs: HashMap<u32, (Option<u64>, ProcKind)> = HashMap::new();
            for p in dev.running_compute_processes().unwrap_or_default() {
                procs.insert(p.pid, (used_bytes(&p), ProcKind::Compute));
            }
            for p in dev.running_graphics_processes().unwrap_or_default() {
                procs.insert(p.pid, (used_bytes(&p), ProcKind::Graphics));
            }
            // Per-device watermark; a shared one would blind every device but 0.
            let watermark = self.last_util_ts.get(i as usize).copied().unwrap_or(0);
            let mut util: HashMap<u32, u32> = HashMap::new();
            if let Ok(samples) = dev.process_utilization_stats(watermark) {
                let (folded, newest) = fold_util(samples, watermark);
                util = folded;
                // Only advance on a successful query, so a transient error
                // doesn't skip a window of samples.
                if let Some(w) = self.last_util_ts.get_mut(i as usize) {
                    *w = newest;
                }
            }
            out.extend(procs.into_iter().map(|(pid, (mem, kind))| GpuProcess {
                pid,
                gpu_index: i as usize,
                kind,
                gpu_util_pct: util.get(&pid).map(|u| *u as f64),
                gpu_mem_bytes: mem,
                ..Default::default()
            }));
        }
        out
    }
}

/// NVML plus any nouveau-bound cards beside them (Linux). NVML only sees
/// devices on the proprietary driver, so a mixed-driver rig would otherwise
/// omit every nouveau card.
#[cfg(target_os = "linux")]
struct MergedNvidiaBackend {
    nvml: NvmlBackend,
    nouveau: Box<dyn GpuBackend>,
    /// Header line, computed once: both children's lines are static for the
    /// backend's life, so the per-frame join is pure waste.
    driver: std::sync::OnceLock<Option<String>>,
}

#[cfg(target_os = "linux")]
impl GpuBackend for MergedNvidiaBackend {
    fn name(&self) -> &'static str {
        // Keep the "nvml" namespace so a pure-NVML session and a mixed one
        // give the proprietary cards the same device-id prefix.
        "nvml"
    }

    fn poll(&mut self) -> Result<Vec<GpuSnapshot>> {
        let mut gpus = self.nvml.poll()?;
        gpus.append(&mut self.nouveau.poll()?);
        Ok(gpus)
    }

    fn processes(&mut self) -> Vec<GpuProcess> {
        // nouveau has no per-process visibility; NVML rows index 0..count,
        // which stay valid because the nouveau snapshots come after them.
        self.nvml.processes()
    }

    fn driver_info(&self) -> Option<String> {
        self.driver
            .get_or_init(|| {
                let a = self.nvml.driver_info();
                let b = self.nouveau.driver_info();
                match (a, b) {
                    (Some(a), Some(b)) => Some(format!("{a} · {b}")),
                    (a, b) => a.or(b),
                }
            })
            .clone()
    }
}

/// NVIDIA cards on the open `nouveau` driver, read from sysfs.
///
/// nouveau publishes no busy counter and no VRAM total anywhere in sysfs, and
/// its fdinfo carries no engine accounting, so this reports what hwmon and the
/// PCI core do have — a name, an identity, temperature, power, fans, link state
/// — and leaves the rest `None` rather than fabricating an idle-looking 0%. A
/// card listed with half its gauges empty is still a listed card; the
/// alternative here is that it does not appear at all.
///
/// Runs alongside NVML: a rig can mix the proprietary driver and nouveau, and
/// NVML only sees the cards on the proprietary side, so the two scans must
/// both run. When NVML is absent this is the only NVIDIA listing left.
#[cfg(target_os = "linux")]
mod nouveau {
    use crate::backend::linux::{
        self, card_name, cards_with_driver, driver_line_for, fan_pct, hwmon_u64, pci_device_id,
        pcie_current_link, pcie_max_link, pdev_of,
    };
    use crate::backend::{GpuBackend, GpuSnapshot};
    use anyhow::Result;
    use std::path::PathBuf;

    const NVIDIA_VENDOR: &str = "0x10de";

    struct NouveauDevice {
        name: String,
        dev: PathBuf,
        hwmon: Option<PathBuf>,
        pdev: Option<String>,
        /// The DRM driver bound to the card ("nouveau" or, on the NVML-absent
        /// fallback, "nvidia"), for the header line.
        driver: String,
        /// Maximum supported PCIe link, fixed per device and resolved once at
        /// scan rather than re-read every poll.
        pcie_max_gen: Option<u8>,
        pcie_max_width: Option<u32>,
    }

    struct NouveauBackend {
        devices: Vec<NouveauDevice>,
        /// Header driver line, computed once: the device set is fixed for the
        /// backend's life (a re-detect builds a new one), so re-joining it
        /// every frame is pure waste — see `CompositeBackend::driver`.
        driver: std::sync::OnceLock<Option<String>>,
    }

    pub fn probe() -> Option<Box<dyn GpuBackend>> {
        let devices = scan("/sys/class/drm", false);
        (!devices.is_empty()).then(|| {
            Box::new(NouveauBackend {
                devices,
                driver: std::sync::OnceLock::new(),
            }) as Box<dyn GpuBackend>
        })
    }

    /// The NVML-absent fallback: claim cards bound to the proprietary `nvidia`
    /// driver too, so a card NVML cannot initialise for is still listed — its
    /// name, PCI id and link state, and every gauge `n/a` — rather than
    /// invisible. Never used when NVML is alive: there those cards are NVML's.
    pub fn probe_without_nvml() -> Option<Box<dyn GpuBackend>> {
        let devices = scan("/sys/class/drm", true);
        (!devices.is_empty()).then(|| {
            Box::new(NouveauBackend {
                devices,
                driver: std::sync::OnceLock::new(),
            }) as Box<dyn GpuBackend>
        })
    }

    /// `include_nvidia` claims cards bound to the proprietary `nvidia` driver
    /// as well as `nouveau` — only the NVML-absent fallback passes true; when
    /// NVML is alive those cards are NVML's, and claiming them here too would
    /// list them twice.
    fn scan(drm: &str, include_nvidia: bool) -> Vec<NouveauDevice> {
        cards_with_driver(drm, NVIDIA_VENDOR, |d| {
            d == "nouveau" || (include_nvidia && d == "nvidia")
        })
        .into_iter()
        .map(|(idx, dev, driver)| {
            let (pcie_max_gen, pcie_max_width) = pcie_max_link(&dev);
            NouveauDevice {
                name: card_name(&dev, idx, "10de", "NVIDIA"),
                hwmon: linux::hwmon_dir(&dev, &driver),
                pdev: pdev_of(&dev),
                dev,
                driver,
                pcie_max_gen,
                pcie_max_width,
            }
        })
        .collect()
    }

    impl GpuBackend for NouveauBackend {
        fn name(&self) -> &'static str {
            "nouveau"
        }

        fn poll(&mut self) -> Result<Vec<GpuSnapshot>> {
            Ok(self.devices.iter().map(sample).collect())
        }

        fn driver_info(&self) -> Option<String> {
            self.driver
                .get_or_init(|| driver_line_for(self.devices.iter().map(|d| d.driver.as_str())))
                .clone()
        }
    }

    fn sample(d: &NouveauDevice) -> GpuSnapshot {
        let h = d.hwmon.as_deref();
        let (pcie_gen, pcie_width) = pcie_current_link(&d.dev);
        GpuSnapshot {
            name: d.name.clone(),
            device_id: pci_device_id(d.pdev.as_deref()),
            // Every nouveau-supported part on a PCI bus is discrete; the Tegra
            // ones are not enumerated here at all.
            integrated: false,
            // nouveau_hwmon: millidegrees, microwatts, pwm 0..pwm1_max.
            temperature_c: hwmon_u64(h, "temp1_input").map(|v| v as f64 / 1000.0),
            power_w: hwmon_u64(h, "power1_input").map(|v| v as f64 / 1e6),
            power_limit_w: hwmon_u64(h, "power1_max")
                .filter(|v| *v > 0)
                .map(|v| v as f64 / 1e6),
            fan_pct: fan_pct(h),
            fan_rpm: hwmon_u64(h, "fan1_input"),
            volt_mv: hwmon_u64(h, "in0_input"),
            pcie_gen,
            pcie_width,
            pcie_max_gen: d.pcie_max_gen,
            pcie_max_width: d.pcie_max_width,
            // Everything else — utilization, VRAM, clocks — has no sysfs source
            // under nouveau. None keeps "unknown" distinct from "idle"/"empty".
            ..Default::default()
        }
    }

    /// Cards this backend claims from a fake `/sys/class/drm`, for the shared
    /// test that proves the Linux scans partition that directory. The
    /// NVML-present view: only `nouveau` cards, never the proprietary ones.
    #[cfg(test)]
    pub fn claimed_ids(drm: &str) -> Vec<String> {
        scan(drm, false)
            .iter()
            .filter_map(|d| pci_device_id(d.pdev.as_deref()))
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::backend::linux::testing;

        /// The gap this backend exists to close: with nouveau bound, NVML never
        /// initialises and nothing else claims vendor 0x10de. The NVML-present
        /// view: the proprietary-driver card is NVML's and stays unclaimed.
        #[test]
        fn scan_claims_nouveau_cards_only() {
            let root = testing::tri_vendor("nouveau-scan");
            let devices = scan(&testing::drm(&root), false);
            assert_eq!(
                devices
                    .iter()
                    .map(|d| d.pdev.as_deref())
                    .collect::<Vec<_>>(),
                [Some("0000:04:00.0")],
                "card2 is on the proprietary driver — NVML's, not ours"
            );
        }

        /// The NVML-absent fallback claims the proprietary-driver card too: a
        /// card NVML cannot initialise for is listed (name, PCI id, link
        /// state, gauges n/a) rather than invisible.
        #[test]
        fn the_nvml_absent_scan_claims_proprietary_cards() {
            let root = testing::tri_vendor("nvidia-fallback");
            let devices = scan(&testing::drm(&root), true);
            // card2 (nvidia driver) and card3 (nouveau), in card-index order.
            assert_eq!(
                devices
                    .iter()
                    .map(|d| d.pdev.as_deref())
                    .collect::<Vec<_>>(),
                [Some("0000:01:00.0"), Some("0000:04:00.0")]
            );
            assert_eq!(devices[0].driver, "nvidia");
            assert_eq!(devices[1].driver, "nouveau");
            // The proprietary card's own attributes are read; its gauges are n/a.
            let s = sample(&devices[0]);
            assert_eq!(s.device_id.as_deref(), Some("pci:0000:01:00.0"));
            assert!(s.utilization_pct.is_none());
            assert!(s.vram_total_bytes.is_none());
            assert!(!s.integrated);
        }

        /// The max link is a fixed capability, so the scan resolves it once and
        /// the device carries it rather than re-reading sysfs per poll.
        #[test]
        fn scan_caches_the_max_pcie_link() {
            let root = testing::tri_vendor("nouveau-pcie-max");
            let pci = root.join("pci/0000:04:00.0");
            std::fs::write(pci.join("max_link_speed"), "16.0 GT/s PCIe\n").unwrap();
            std::fs::write(pci.join("max_link_width"), "16\n").unwrap();
            let devices = scan(&testing::drm(&root), false);
            assert_eq!(devices[0].pcie_max_gen, Some(4));
            assert_eq!(devices[0].pcie_max_width, Some(16));
        }

        /// Absent hwmon files must read as unknown, never as a cold, idle card.
        #[test]
        fn missing_sysfs_reads_as_unknown_not_zero() {
            let root = testing::tri_vendor("nouveau-sample");
            let d = &scan(&testing::drm(&root), false)[0];
            let s = sample(d);
            assert_eq!(s.device_id.as_deref(), Some("pci:0000:04:00.0"));
            assert!(!s.integrated);
            for v in [s.utilization_pct, s.temperature_c, s.power_w, s.fan_pct] {
                assert!(v.is_none());
            }
            assert!(s.vram_total_bytes.is_none() && s.clock_mhz.is_none());
        }
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
    super::join_throttle(&parts)
}

fn used_bytes(p: &ProcessInfo) -> Option<u64> {
    mem_bytes(&p.used_gpu_memory)
}

/// NVML's per-process memory, or `None` when it declines to say.
///
/// `Unavailable` is `NVML_VALUE_NOT_AVAILABLE`, and nvml-wrapper is blunt
/// about when that shows up: "Under WDDM, `NVML_VALUE_NOT_AVAILABLE` is always
/// reported because Windows KMD manages all the memory, not the NVIDIA
/// driver." WDDM is the ordinary consumer Windows configuration, so folding it
/// to `0` did not mean a rare unknown rendered as zero — it meant every NVIDIA
/// process row on Windows read a confident `0MiB`, always.
///
/// Split out from [`used_bytes`] because `ProcessInfo` is awkward to build in a
/// test and this mapping is the whole unit worth testing.
fn mem_bytes(m: &UsedGpuMemory) -> Option<u64> {
    match m {
        UsedGpuMemory::Used(b) => Some(*b),
        UsedGpuMemory::Unavailable => None,
    }
}

/// Collapse one `nvmlDeviceGetProcessUtilization` batch to one SM% per pid,
/// plus the new watermark. NVML hands back *many* samples per pid per call in
/// an order it does not document, so taking whichever arrived last showed an
/// arbitrary sample. Keep the **newest** rather than averaging: the mean would
/// be taken over a window whose length we don't control (the first call drains
/// the whole ring buffer), so it would smear a card that just went idle, while
/// the newest sample is the live figure the rest of the UI shows.
fn fold_util(
    samples: impl IntoIterator<Item = ProcessUtilizationSample>,
    watermark: u64,
) -> (HashMap<u32, u32>, u64) {
    let mut newest_ts = watermark;
    let mut per_pid: HashMap<u32, (u64, u32)> = HashMap::new();
    for s in samples {
        newest_ts = newest_ts.max(s.timestamp);
        let slot = per_pid.entry(s.pid).or_insert((s.timestamp, s.sm_util));
        if s.timestamp >= slot.0 {
            *slot = (s.timestamp, s.sm_util);
        }
    }
    let util = per_pid
        .into_iter()
        .map(|(pid, (_, sm))| (pid, sm.min(100)))
        .collect();
    (util, newest_ts)
}

/// GDDR6/6X memory-junction temperature, °C.
///
/// `NVML_FI_DEV_MEMORY_TEMP` (82) is the only temperature field id this NVML
/// header defines — the other `NVML_FI_DEV_TEMPERATURE_*` ids (193-196) are
/// *TLIMIT margins*, not a core hotspot reading — so NVIDIA can fill
/// `temp_mem_c` but never `temp_junction_c`. Cards without a memory sensor
/// (pre-Turing, most laptop parts) answer `NotSupported` or report 0; both
/// degrade to None so the UI omits the reading instead of claiming 0 °C.
fn memory_temp_c(dev: &Device<'_>) -> Option<f64> {
    let samples = dev
        .field_values_for(&[FieldId(NVML_FI_DEV_MEMORY_TEMP)])
        .ok()?;
    // Outer Result: the batch call itself. Inner: this one field's value.
    field_temp_c(samples.into_iter().next()?.ok()?.value.ok()?)
}

/// Widen an NVML field value to °C. Split out from `memory_temp_c` so the
/// unit-type handling is testable without a GPU.
fn field_temp_c(v: SampleValue) -> Option<f64> {
    let c = match v {
        SampleValue::F64(v) => v,
        SampleValue::U32(v) => f64::from(v),
        SampleValue::U64(v) => v as f64,
        SampleValue::I64(v) => v as f64,
    };
    // 0 is NVML's "answered but no reading"; a powered GPU is never at 0 °C.
    (c > 0.0).then_some(c)
}

/// NVML's hardware-slowdown temperature threshold, or None when the driver
/// declines to publish one. Unsupported queries return the sentinels 0 and -1
/// (the latter wrapped into u32 as 4294967295) rather than erroring on every
/// driver, so the value is filtered to a plausible range instead of trusted.
fn slowdown_threshold_c(dev: &Device) -> Option<f64> {
    dev.temperature_threshold(TemperatureThreshold::Slowdown)
        .ok()
        .filter(|t| (1..=200).contains(t))
        .map(|t| t as f64)
}

/// Highest fan duty cycle and RPM across all fans on the board.
///
/// `fan_speed(0)` alone under-reports 2- and 3-fan cards, whose fans run
/// different curves. Report the max, not the mean: the loudest, hardest-working
/// fan is what explains audible noise and remaining thermal headroom, and a
/// mean would dilute one pegged fan into a comfortable-looking number. On
/// single-fan cards this is identical to the old behaviour. `fans` is the
/// count cached at probe; `None` means the driver declined the query (fanless
/// parts), in which case index 0 is probed once and also fails, leaving both
/// values None rather than 0.
fn fan_speeds(dev: &Device<'_>, fans: Option<u32>) -> (Option<f64>, Option<u64>) {
    let fans = fans.unwrap_or(1).max(1);
    let mut pct: Option<u32> = None;
    let mut rpm: Option<u32> = None;
    for f in 0..fans {
        if let Ok(v) = dev.fan_speed(f) {
            pct = Some(pct.map_or(v, |cur| cur.max(v)));
        }
        if let Ok(v) = dev.fan_speed_rpm(f) {
            rpm = Some(rpm.map_or(v, |cur| cur.max(v)));
        }
    }
    (pct.map(f64::from), rpm.map(u64::from))
}

/// Render a P-state as `P0` (max clocks) .. `P15` (deepest idle). NVML numbers
/// the variants 0..=15 with `NVML_PSTATE_UNKNOWN` at 32; `Unknown` carries no
/// information, so it renders as nothing rather than a bogus label.
fn pstate_label(p: PerformanceState) -> Option<String> {
    let n = p.as_c();
    (n <= 15).then(|| format!("P{n}"))
}

/// NVML's PCIe throughput counter counts decimal KB/s (`nvmlDeviceGetPcieThroughput`
/// documents KB; nvidia-smi prints the same figure), while the snapshot field
/// and the UI label say KiB/s. Convert so the number matches the label.
fn kb_to_kib(kb: u32) -> u64 {
    u64::from(kb) * 1000 / 1024
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(pid: u32, timestamp: u64, sm_util: u32) -> ProcessUtilizationSample {
        ProcessUtilizationSample {
            pid,
            timestamp,
            sm_util,
            mem_util: 0,
            enc_util: 0,
            dec_util: 0,
        }
    }

    #[test]
    fn fold_util_keeps_the_newest_sample_per_pid() {
        // Deliberately out of chronological order: NVML's order is undocumented.
        let samples = vec![
            sample(7, 300, 11),
            sample(7, 500, 42),
            sample(7, 100, 99),
            sample(9, 400, 5),
        ];
        let (util, newest) = fold_util(samples, 0);
        assert_eq!(util.get(&7), Some(&42));
        assert_eq!(util.get(&9), Some(&5));
        assert_eq!(newest, 500);
    }

    #[test]
    fn fold_util_clamps_out_of_range_sm_util() {
        let (util, _) = fold_util(vec![sample(1, 10, 250)], 0);
        assert_eq!(util.get(&1), Some(&100));
    }

    #[test]
    fn fold_util_never_lowers_the_watermark() {
        // Empty batch: watermark must survive so the next call still asks for
        // "newer than the last thing we saw".
        let (util, newest) = fold_util(Vec::new(), 900);
        assert!(util.is_empty());
        assert_eq!(newest, 900);
        // Stale samples (shouldn't happen, but the driver is not ours) also
        // must not rewind it.
        let (_, newest) = fold_util(vec![sample(1, 10, 1)], 900);
        assert_eq!(newest, 900);
    }

    #[test]
    fn per_device_watermarks_are_independent() {
        // Regression for the shared-watermark bug: advancing device 0 must not
        // touch device 1's watermark, or device 1 sees no samples at all.
        let mut watermarks = [0_u64; 2];
        let (_, newest) = fold_util(vec![sample(1, 5_000, 30)], watermarks[0]);
        watermarks[0] = newest;
        assert_eq!(watermarks[0], 5_000);
        assert_eq!(watermarks[1], 0);
        let (util, _) = fold_util(vec![sample(2, 4_000, 60)], watermarks[1]);
        assert_eq!(util.get(&2), Some(&60));
    }

    #[test]
    fn field_temp_widens_every_nvml_value_type() {
        assert_eq!(field_temp_c(SampleValue::U32(84)), Some(84.0));
        assert_eq!(field_temp_c(SampleValue::U64(84)), Some(84.0));
        assert_eq!(field_temp_c(SampleValue::I64(84)), Some(84.0));
        assert_eq!(field_temp_c(SampleValue::F64(84.5)), Some(84.5));
    }

    #[test]
    fn field_temp_rejects_the_no_reading_sentinel() {
        // Must degrade to None, never to a fabricated 0 °C.
        assert_eq!(field_temp_c(SampleValue::U32(0)), None);
        assert_eq!(field_temp_c(SampleValue::I64(-40)), None);
    }

    #[test]
    fn pstate_labels_cover_the_whole_range() {
        use PerformanceState::*;
        let states = [
            Zero, One, Two, Three, Four, Five, Six, Seven, Eight, Nine, Ten, Eleven, Twelve,
            Thirteen, Fourteen, Fifteen,
        ];
        for (n, state) in states.into_iter().enumerate() {
            assert_eq!(pstate_label(state), Some(format!("P{n}")));
        }
        assert_eq!(pstate_label(Unknown), None);
    }

    /// The WDDM case. NVML reports `Unavailable` for every process on ordinary
    /// consumer Windows, and folding that to 0 made every NVIDIA process row
    /// there read a confident `0MiB`. It must be absent instead.
    #[test]
    fn unavailable_process_memory_is_unknown_not_zero() {
        assert_eq!(mem_bytes(&UsedGpuMemory::Unavailable), None);
        assert_ne!(mem_bytes(&UsedGpuMemory::Unavailable), Some(0));
    }

    /// A real reading passes through untouched, including a real zero — a
    /// process holding a context but no memory is a measurement.
    #[test]
    fn reported_process_memory_passes_through() {
        assert_eq!(mem_bytes(&UsedGpuMemory::Used(0)), Some(0));
        assert_eq!(
            mem_bytes(&UsedGpuMemory::Used(2 << 30)),
            Some(2_147_483_648)
        );
    }

    /// NVML counts PCIe throughput in decimal KB/s; the field and label say
    /// KiB/s, so the value is converted — 1024 decimal KB is exactly 1000 KiB.
    #[test]
    fn pcie_throughput_converts_decimal_kb_to_kib() {
        assert_eq!(kb_to_kib(0), 0);
        assert_eq!(kb_to_kib(1024), 1000);
        assert_eq!(kb_to_kib(1000), 976); // 1000 * 1000 / 1024
        assert_eq!(kb_to_kib(u32::MAX), u64::from(u32::MAX) * 1000 / 1024);
    }
}
