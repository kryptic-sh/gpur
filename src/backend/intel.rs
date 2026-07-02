//! Intel backend (Linux, i915 + xe drivers).
//!
//! Intel exposes no device-level busy% in sysfs, so utilization is derived
//! the way nvtop does it: aggregate per-client fdinfo engine counters across
//! all processes each poll (i915: busy-ns deltas; xe: cycles ratios). That
//! means the same scan feeds both the device gauges and the process table.
//! Power comes from the hwmon cumulative energy counter delta.

use super::GpuBackend;

pub fn probe() -> Option<Box<dyn GpuBackend>> {
    #[cfg(target_os = "linux")]
    if let Some(b) = linux_impl::probe() {
        return Some(b);
    }
    None
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use crate::backend::linux::{
        self, FdClient, card_name, cards_with_vendor, first_dir, pdev_of, read_u64,
    };
    use crate::backend::{GpuBackend, GpuProcess, GpuSnapshot, ProcKind};
    use anyhow::Result;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::time::Instant;

    const INTEL_VENDOR: &str = "0x8086";

    pub fn probe() -> Option<Box<dyn GpuBackend>> {
        let devices = scan("/sys/class/drm");
        if devices.is_empty() {
            return None;
        }
        Some(Box::new(IntelBackend {
            devices,
            i915_state: HashMap::new(),
            xe_state: HashMap::new(),
            energy_state: HashMap::new(),
            last_procs: Vec::new(),
        }))
    }

    struct IntelDevice {
        name: String,
        /// /sys/class/drm/cardN
        card: PathBuf,
        /// /sys/class/drm/cardN/device
        dev: PathBuf,
        hwmon: Option<PathBuf>,
        pdev: Option<String>,
        /// "i915" or "xe", from the device's driver symlink.
        driver: String,
        integrated: bool,
    }

    struct IntelBackend {
        devices: Vec<IntelDevice>,
        /// (pid, client-id) -> cumulative engine ns (i915 accounting).
        i915_state: HashMap<(u32, u64), (u64, Instant)>,
        /// (pid, client-id) -> last cycles snapshot (xe accounting).
        xe_state: HashMap<(u32, u64), FdClient>,
        /// gpu index -> (energy µJ, at) for power-from-energy deltas.
        energy_state: HashMap<usize, (u64, Instant)>,
        /// Built during poll (same fdinfo sweep), served by processes().
        last_procs: Vec<GpuProcess>,
    }

    impl GpuBackend for IntelBackend {
        fn name(&self) -> &'static str {
            "intel"
        }

        fn poll(&mut self) -> Result<Vec<GpuSnapshot>> {
            // One fdinfo sweep feeds device utilization AND the process table.
            let (mut device_util, mut device_mem, procs) = self.sweep_clients();
            self.last_procs = procs;

            let now = Instant::now();
            let powers: Vec<Option<f64>> = (0..self.devices.len())
                .map(|i| self.power_w(i, now))
                .collect();
            let gpus = self
                .devices
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let h = d.hwmon.as_deref();
                    let power_w = powers[i];
                    GpuSnapshot {
                        name: d.name.clone(),
                        integrated: d.integrated,
                        utilization_pct: device_util.remove(&i).unwrap_or(0.0).clamp(0.0, 100.0),
                        mem_util_pct: None,
                        // No total VRAM in sysfs for iGPUs; report the summed
                        // client-resident local memory as "used".
                        vram_used_bytes: device_mem.remove(&i).unwrap_or(0),
                        vram_total_bytes: read_u64(&d.dev.join("lmem_total_bytes")).unwrap_or(0),
                        temperature_c: h
                            .and_then(|h| read_u64(&h.join("temp1_input")))
                            .map(|v| v as f64 / 1000.0),
                        power_w,
                        power_limit_w: h
                            .and_then(|h| read_u64(&h.join("power1_max")))
                            .filter(|v| *v > 0)
                            .map(|v| v as f64 / 1e6),
                        fan_pct: None,
                        clock_mhz: gt_cur_freq_mhz(d),
                        mem_clock_mhz: None,
                        pcie_gen: None,
                        pcie_width: None,
                        pcie_rx_kbs: None,
                        pcie_tx_kbs: None,
                    }
                })
                .collect();
            Ok(gpus)
        }

        fn processes(&mut self) -> Vec<GpuProcess> {
            self.last_procs.clone()
        }
    }

    impl IntelBackend {
        /// Scan all processes' Intel DRM clients once. Returns per-device
        /// utilization (sum of client utils), per-device local-memory bytes,
        /// and the process rows.
        #[allow(clippy::type_complexity)]
        fn sweep_clients(&mut self) -> (HashMap<usize, f64>, HashMap<usize, u64>, Vec<GpuProcess>) {
            let pdev_to_gpu: HashMap<String, usize> = self
                .devices
                .iter()
                .enumerate()
                .filter_map(|(i, d)| d.pdev.clone().map(|p| (p, i)))
                .collect();
            let driver_of: HashMap<usize, &str> = self
                .devices
                .iter()
                .enumerate()
                .map(|(i, d)| (i, d.driver.as_str()))
                .collect();

            let mut device_util: HashMap<usize, f64> = HashMap::new();
            let mut device_mem: HashMap<usize, u64> = HashMap::new();
            let mut agg: HashMap<(u32, usize), (f64, u64, bool)> = HashMap::new();
            let mut seen: HashSet<(u32, u64)> = HashSet::new();
            let now = Instant::now();

            for pid in linux::proc_pids() {
                for driver in ["i915", "xe"] {
                    for client in linux::drm_clients(pid, driver) {
                        let Some(&gpu) = client.pdev.as_ref().and_then(|p| pdev_to_gpu.get(p))
                        else {
                            continue;
                        };
                        if driver_of.get(&gpu).copied() != Some(driver) {
                            continue;
                        }
                        if !seen.insert((pid, client.id)) {
                            continue;
                        }

                        let util = if driver == "xe" {
                            let prev = self.xe_state.get(&(pid, client.id));
                            let u =
                                prev.and_then(|p| client.xe_util_since(p)).unwrap_or(0.0) * 100.0;
                            self.xe_state
                                .insert((pid, client.id), client_snapshot(&client));
                            u
                        } else {
                            let engine_ns = client.total_engine_ns();
                            let u = match self.i915_state.get(&(pid, client.id)) {
                                Some((prev_ns, prev_at)) => {
                                    let wall = now.duration_since(*prev_at).as_nanos() as f64;
                                    if wall > 0.0 {
                                        engine_ns.saturating_sub(*prev_ns) as f64 / wall * 100.0
                                    } else {
                                        0.0
                                    }
                                }
                                None => 0.0,
                            };
                            self.i915_state.insert((pid, client.id), (engine_ns, now));
                            u
                        }
                        .clamp(0.0, 100.0);

                        // "local*"/"vram*" = device memory (dGPU); ignore
                        // system/gtt so iGPU numbers don't count plain RAM.
                        let mem: u64 = client
                            .memory
                            .iter()
                            .filter(|(k, _)| k.starts_with("local") || k.starts_with("vram"))
                            .map(|(_, v)| *v)
                            .sum();
                        let graphics = client.engine_ns.keys().any(|k| k == "render" || k == "rcs")
                            || client.cycles.keys().any(|k| k == "rcs");

                        *device_util.entry(gpu).or_default() += util;
                        *device_mem.entry(gpu).or_default() += mem;
                        let e = agg.entry((pid, gpu)).or_insert((0.0, 0, false));
                        e.0 += util;
                        e.1 += mem;
                        e.2 |= graphics;
                    }
                }
            }

            self.i915_state.retain(|k, _| seen.contains(k));
            self.xe_state.retain(|k, _| seen.contains(k));

            let procs = agg
                .into_iter()
                .map(|((pid, gpu_index), (util, mem, graphics))| GpuProcess {
                    pid,
                    gpu_index,
                    kind: if graphics {
                        ProcKind::Graphics
                    } else {
                        ProcKind::Compute
                    },
                    gpu_util_pct: Some(util.min(100.0)),
                    gpu_mem_bytes: mem,
                })
                .collect();
            (device_util, device_mem, procs)
        }

        /// Watts from the hwmon cumulative energy counter (µJ) delta, with a
        /// fall-back to the instantaneous power file where present.
        fn power_w(&mut self, i: usize, now: Instant) -> Option<f64> {
            let h = self.devices[i].hwmon.as_deref()?;
            if let Some(uj) = read_u64(&h.join("energy1_input")) {
                let prev = self.energy_state.insert(i, (uj, now));
                if let Some((prev_uj, prev_at)) = prev {
                    let secs = now.duration_since(prev_at).as_secs_f64();
                    if secs > 0.0 && uj >= prev_uj {
                        return Some((uj - prev_uj) as f64 / 1e6 / secs);
                    }
                }
                return None; // first sample: no delta yet
            }
            read_u64(&h.join("power1_input")).map(|v| v as f64 / 1e6)
        }
    }

    /// Keep only what xe_util_since needs from a client.
    fn client_snapshot(c: &FdClient) -> FdClient {
        FdClient {
            cycles: c.cycles.clone(),
            ..FdClient::default()
        }
    }

    fn scan(drm: &str) -> Vec<IntelDevice> {
        cards_with_vendor(drm, INTEL_VENDOR)
            .into_iter()
            .filter_map(|(idx, dev)| {
                // Only real GPU drivers; skips e.g. future non-GPU 8086 DRM devs.
                let driver = std::fs::read_link(dev.join("driver"))
                    .ok()?
                    .file_name()?
                    .to_string_lossy()
                    .into_owned();
                if driver != "i915" && driver != "xe" {
                    return None;
                }
                let card = dev.parent()?.to_path_buf();
                let name = card_name(&dev, idx, "8086", "Intel");
                // dGPUs (Arc) have dedicated local memory; iGPUs don't.
                let integrated = read_u64(&dev.join("lmem_total_bytes")).is_none();
                Some(IntelDevice {
                    name,
                    hwmon: first_dir(&dev.join("hwmon")),
                    pdev: pdev_of(&dev),
                    card,
                    dev,
                    driver,
                    integrated,
                })
            })
            .collect()
    }

    /// Current graphics clock: i915 keeps it on the card dir, xe under gt0.
    fn gt_cur_freq_mhz(d: &IntelDevice) -> Option<u64> {
        read_u64(&d.card.join("gt_cur_freq_mhz"))
            .or_else(|| read_u64(&d.dev.join("tile0/gt0/freq0/cur_freq")))
            .or_else(|| read_u64(&d.card.join("gt/gt0/rps_cur_freq_mhz")))
    }
}
