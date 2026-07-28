//! NVIDIA backend via NVML (Linux + Windows). Loads libnvidia-ml dynamically;
//! probe fails soft on machines without the driver, and on Linux falls back to
//! a sysfs read of any nouveau-driven card so an open-driver GPU is still
//! listed rather than dropped off the rig.

use super::{GpuBackend, GpuProcess, GpuSnapshot, ProcKind};
use anyhow::Result;
use nvml_wrapper::bitmasks::device::ThrottleReasons;
use nvml_wrapper::enum_wrappers::device::{
    Clock, PcieUtilCounter, PerformanceState, TemperatureSensor,
};
use nvml_wrapper::enums::device::{BusType, SampleValue, UsedGpuMemory};
use nvml_wrapper::struct_wrappers::device::{ProcessInfo, ProcessUtilizationSample};
use nvml_wrapper::structs::device::FieldId;
use nvml_wrapper::sys_exports::field_id::NVML_FI_DEV_MEMORY_TEMP;
use nvml_wrapper::{Device, Nvml};
use std::collections::HashMap;

pub fn probe() -> Option<Box<dyn GpuBackend>> {
    if let Some(b) = nvml_probe() {
        return Some(b);
    }
    // NVML is the only source of NVIDIA telemetry, and it exists only with the
    // proprietary driver. Without it the card is still sitting in
    // /sys/class/drm, and no other backend will claim it — the AMD and Intel
    // scans filter on their own PCI vendor — so leaving it there drops a card
    // off a mixed rig entirely rather than showing it with fewer gauges.
    #[cfg(target_os = "linux")]
    if let Some(b) = nouveau::probe() {
        return Some(b);
    }
    None
}

/// Device ids the sysfs fallback claims from a fake `/sys/class/drm`, for the
/// shared test that proves the Linux scans partition that one directory.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn claimed_ids(drm: &str) -> Vec<String> {
    nouveau::claimed_ids(drm)
}

fn nvml_probe() -> Option<Box<dyn GpuBackend>> {
    let nvml = Nvml::init().ok()?;
    match nvml.device_count() {
        Ok(n) if n > 0 => {
            let driver = nvml.sys_driver_version().ok();
            // Read once: a UUID is fixed for the life of the device and a
            // per-poll query is a driver round-trip per card.
            let uuids = (0..n)
                .map(|i| nvml.device_by_index(i).ok().and_then(|d| d.uuid().ok()))
                .collect();
            Some(Box::new(NvmlBackend {
                nvml,
                count: n,
                driver,
                uuids,
                last_util_ts: vec![0; n as usize],
            }))
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
            let (fan_pct, fan_rpm) = fan_speeds(&dev);
            gpus.push(GpuSnapshot {
                name: dev.name().unwrap_or_else(|_| format!("NVIDIA GPU {i}")),
                device_id,
                // NVML has no "is integrated" query. FPCI is Tegra's on-SoC host
                // interface and the only bus type no discrete card reports, so it
                // is the one signal that can flag a Jetson iGPU; anything else,
                // including an unsupported/missing query, stays discrete.
                integrated: dev.bus_type().is_ok_and(|b| matches!(b, BusType::Fpci)),
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
                perf_level: dev.performance_state().ok().and_then(pstate_label),
                ..Default::default()
            });
        }
        Ok(gpus)
    }

    fn driver_info(&self) -> Option<String> {
        self.driver.as_ref().map(|d| format!("driver {d}"))
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

/// NVIDIA cards on the open `nouveau` driver, read from sysfs.
///
/// nouveau publishes no busy counter and no VRAM total anywhere in sysfs, and
/// its fdinfo carries no engine accounting, so this reports what hwmon and the
/// PCI core do have — a name, an identity, temperature, power, fans, link state
/// — and leaves the rest `None` rather than fabricating an idle-looking 0%. A
/// card listed with half its gauges empty is still a listed card; the
/// alternative here is that it does not appear at all.
///
/// Reached only when NVML did not initialise, so it can never list a card NVML
/// already reported.
#[cfg(target_os = "linux")]
mod nouveau {
    use crate::backend::linux::{
        card_name, cards_with_driver, driver_line, first_dir, hwmon_u64, pci_device_id, pcie_link,
        pdev_of,
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
    }

    struct NouveauBackend {
        devices: Vec<NouveauDevice>,
    }

    pub fn probe() -> Option<Box<dyn GpuBackend>> {
        let devices = scan("/sys/class/drm");
        (!devices.is_empty()).then(|| Box::new(NouveauBackend { devices }) as Box<dyn GpuBackend>)
    }

    fn scan(drm: &str) -> Vec<NouveauDevice> {
        cards_with_driver(drm, NVIDIA_VENDOR, |d| d == "nouveau")
            .into_iter()
            .map(|(idx, dev, _)| NouveauDevice {
                name: card_name(&dev, idx, "10de", "NVIDIA"),
                hwmon: first_dir(&dev.join("hwmon")),
                pdev: pdev_of(&dev),
                dev,
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
            driver_line("nouveau")
        }
    }

    fn sample(d: &NouveauDevice) -> GpuSnapshot {
        let h = d.hwmon.as_deref();
        let (pcie_gen, pcie_width, pcie_max_gen, pcie_max_width) = pcie_link(&d.dev);
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
            fan_pct: fan_pct(d),
            fan_rpm: hwmon_u64(h, "fan1_input"),
            volt_mv: hwmon_u64(h, "in0_input"),
            pcie_gen,
            pcie_width,
            pcie_max_gen,
            pcie_max_width,
            // Everything else — utilization, VRAM, clocks — has no sysfs source
            // under nouveau. None keeps "unknown" distinct from "idle"/"empty".
            ..Default::default()
        }
    }

    /// pwm duty as a percentage of this card's own scale. `pwm1_max` defaults
    /// to hwmon's 255 when absent, as it does for amdgpu.
    fn fan_pct(d: &NouveauDevice) -> Option<f64> {
        let h = d.hwmon.as_deref();
        let pwm = hwmon_u64(h, "pwm1")?;
        let max = hwmon_u64(h, "pwm1_max").filter(|v| *v > 0).unwrap_or(255);
        Some(pwm as f64 / max as f64 * 100.0)
    }

    /// Cards this backend claims from a fake `/sys/class/drm`, for the shared
    /// test that proves the Linux scans partition that directory.
    #[cfg(test)]
    pub fn claimed_ids(drm: &str) -> Vec<String> {
        scan(drm)
            .iter()
            .filter_map(|d| pci_device_id(d.pdev.as_deref()))
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::backend::linux::testing;

        /// The gap this backend exists to close: with nouveau bound, NVML never
        /// initialises and nothing else claims vendor 0x10de.
        #[test]
        fn scan_claims_nouveau_cards_only() {
            let root = testing::tri_vendor("nouveau-scan");
            let devices = scan(&testing::drm(&root));
            assert_eq!(
                devices
                    .iter()
                    .map(|d| d.pdev.as_deref())
                    .collect::<Vec<_>>(),
                [Some("0000:04:00.0")],
                "card2 is on the proprietary driver — NVML's, not ours"
            );
        }

        /// Absent hwmon files must read as unknown, never as a cold, idle card.
        #[test]
        fn missing_sysfs_reads_as_unknown_not_zero() {
            let root = testing::tri_vendor("nouveau-sample");
            let d = &scan(&testing::drm(&root))[0];
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

fn used_bytes(p: &ProcessInfo) -> u64 {
    match p.used_gpu_memory {
        UsedGpuMemory::Used(b) => b,
        UsedGpuMemory::Unavailable => 0,
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

/// Highest fan duty cycle and RPM across all fans on the board.
///
/// `fan_speed(0)` alone under-reports 2- and 3-fan cards, whose fans run
/// different curves. Report the max, not the mean: the loudest, hardest-working
/// fan is what explains audible noise and remaining thermal headroom, and a
/// mean would dilute one pegged fan into a comfortable-looking number. On
/// single-fan cards this is identical to the old behaviour. `num_fans` is
/// `NotSupported` on fanless parts, in which case index 0 is probed once and
/// also fails, leaving both values None rather than 0.
fn fan_speeds(dev: &Device<'_>) -> (Option<f64>, Option<u64>) {
    let fans = dev.num_fans().unwrap_or(1).max(1);
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
}
