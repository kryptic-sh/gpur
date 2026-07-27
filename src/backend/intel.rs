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
        self, ClientSample, FdClient, SweepDevice, card_name, cards_with_vendor, first_dir,
        hwmon_u64, pdev_of, read_u64,
    };
    use crate::backend::{GpuBackend, GpuProcess, GpuSnapshot, clamp_pct};
    use anyhow::Result;
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    const INTEL_VENDOR: &str = "0x8086";

    pub fn probe() -> Option<Box<dyn GpuBackend>> {
        let devices = scan("/sys/class/drm");
        if devices.is_empty() {
            return None;
        }
        Some(Box::new(IntelBackend {
            devices,
            sys_mem_total: linux::sys_mem_total_bytes(),
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
        /// Device-local memory total, when the driver publishes one at all.
        /// None means unknown, never "zero" — see `vram_total`.
        vram_total: Option<u64>,
        /// Discrete card. Sticky once proven: mainline i915 publishes nothing
        /// that identifies an Arc, so the first client resident in a local
        /// memory region is the evidence, and an idle dGPU must not fall back
        /// to looking integrated afterwards.
        discrete: bool,
    }

    struct IntelBackend {
        devices: Vec<IntelDevice>,
        /// Ceiling for system-backed (GTT) graphics memory; static per boot.
        sys_mem_total: Option<u64>,
        /// (pid, client-id) -> (total ns, video ns) (i915 accounting).
        i915_state: HashMap<(u32, u64), (u64, u64, Instant)>,
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
            let (mut sweep, mut local_mem, mut system_mem) = self.sweep_clients();
            self.last_procs = std::mem::take(&mut sweep.procs);

            let now = Instant::now();
            let powers: Vec<Option<f64>> = (0..self.devices.len())
                .map(|i| self.power_w(i, now))
                .collect();
            let sys_mem_total = self.sys_mem_total;
            let gpus = self
                .devices
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let h = d.hwmon.as_deref();
                    let power_w = powers[i];
                    let (pcie_gen, pcie_width, pcie_max_gen, pcie_max_width) =
                        linux::pcie_link(&d.dev);
                    GpuSnapshot {
                        name: d.name.clone(),
                        integrated: !d.discrete,
                        utilization_pct: clamp_pct(sweep.util.remove(&i).unwrap_or(0.0)),
                        mem_util_pct: None,
                        video_util_pct: sweep.video_util.remove(&i).map(clamp_pct),
                        enc_util_pct: None,
                        dec_util_pct: None,
                        throttle: None,
                        // Summed client-resident device-local memory; 0 on an
                        // iGPU, which has no local region at all.
                        vram_used_bytes: local_mem.remove(&i).unwrap_or(0),
                        // 0 here still means "unknown" rather than "empty" —
                        // finding 7 turns these into Options. Nothing is
                        // fabricated: `vram_total` returns None when no driver
                        // publishes a figure.
                        vram_total_bytes: d.vram_total.unwrap_or(0),
                        temperature_c: hwmon_u64(h, "temp1_input").map(|v| v as f64 / 1000.0),
                        power_w,
                        power_limit_w: hwmon_u64(h, "power1_max")
                            .filter(|v| *v > 0)
                            .map(|v| v as f64 / 1e6),
                        fan_pct: None,
                        clock_mhz: gt_cur_freq_mhz(d),
                        mem_clock_mhz: None,
                        pcie_gen,
                        pcie_width,
                        pcie_max_gen,
                        pcie_max_width,
                        // System-RAM-backed graphics memory. This is the only
                        // memory an iGPU has, and the UI renders it exactly
                        // like amdgpu's GTT pool.
                        gtt_used_bytes: Some(system_mem.remove(&i).unwrap_or(0)),
                        gtt_total_bytes: sys_mem_total,
                        ..Default::default()
                    }
                })
                .collect();
            Ok(gpus)
        }

        fn processes(&mut self) -> Vec<GpuProcess> {
            self.last_procs.clone()
        }

        fn driver_info(&self) -> Option<String> {
            let drivers: std::collections::BTreeSet<&str> =
                self.devices.iter().map(|d| d.driver.as_str()).collect();
            let names = drivers.into_iter().collect::<Vec<_>>().join("+");
            linux::driver_line(&names)
        }
    }

    impl IntelBackend {
        /// Scan all processes' Intel DRM clients once, via the shared sweep.
        /// Returns the sweep plus the two per-device memory buckets Intel
        /// needs: device-local (VRAM) and system-backed (GTT) bytes.
        fn sweep_clients(&mut self) -> (linux::Sweep, HashMap<usize, u64>, HashMap<usize, u64>) {
            let devices: Vec<SweepDevice> = self
                .devices
                .iter()
                .map(|d| SweepDevice {
                    pdev: d.pdev.clone(),
                    driver: d.driver.clone(),
                })
                .collect();
            let discrete: Vec<bool> = self.devices.iter().map(|d| d.discrete).collect();

            let mut local_mem: HashMap<usize, u64> = HashMap::new();
            let mut system_mem: HashMap<usize, u64> = HashMap::new();
            let mut has_local: HashSet<usize> = HashSet::new();
            let now = Instant::now();
            let i915_state = &mut self.i915_state;
            let xe_state = &mut self.xe_state;

            let sweep = linux::sweep_clients(&devices, |pid, gpu, client| {
                let key = (pid, client.id);
                let (util, vutil) = if client.driver == "xe" {
                    let r = match xe_state.get(&key) {
                        Some(prev) => (
                            client.xe_ratio(prev, |_| true) * 100.0,
                            client.xe_ratio(prev, is_video) * 100.0,
                        ),
                        None => (0.0, 0.0),
                    };
                    xe_state.insert(key, client_snapshot(client));
                    r
                } else {
                    let engine_ns = client.total_engine_ns();
                    let video_ns = client.engine_ns_where(is_video);
                    let r = linux::ns_delta_util(i915_state.get(&key), engine_ns, video_ns, now);
                    i915_state.insert(key, (engine_ns, video_ns, now));
                    r
                };

                let mem = split_memory(client, discrete[gpu]);
                *local_mem.entry(gpu).or_default() += mem.local;
                *system_mem.entry(gpu).or_default() += mem.system;
                if mem.saw_local {
                    has_local.insert(gpu);
                }

                ClientSample {
                    // xe_ratio can exceed 1.0 on odd counters; clamp both
                    // paths (ns_delta_util already clamps the i915 branch).
                    util_pct: clamp_pct(util),
                    video_pct: clamp_pct(vutil),
                    // A process row wants the memory the device actually
                    // spends: VRAM on a dGPU, the system pool on an iGPU,
                    // which is all an iGPU ever reports.
                    mem_bytes: if discrete[gpu] { mem.local } else { mem.system },
                    graphics: client.engine_ns.keys().any(|k| k == "render" || k == "rcs")
                        || client.cycles.keys().any(|k| k == "rcs"),
                }
            });

            self.i915_state.retain(|k, _| sweep.seen.contains(k));
            self.xe_state.retain(|k, _| sweep.seen.contains(k));
            // Any local-memory region proves a discrete card, and it is the
            // only proof mainline i915 offers.
            for gpu in has_local {
                self.devices[gpu].discrete = true;
            }

            (sweep, local_mem, system_mem)
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

    /// i915 names media engines "video"/"video-enhance"; xe "vcs"/"vecs".
    fn is_video(k: &str) -> bool {
        k.starts_with("video") || k.starts_with("vcs") || k.starts_with("vecs")
    }

    /// Keep only what the cycle-ratio math needs from a client.
    fn client_snapshot(c: &FdClient) -> FdClient {
        FdClient {
            cycles: c.cycles.clone(),
            ..FdClient::default()
        }
    }

    /// One client's fdinfo memory regions, split by where the bytes live.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
    struct MemSplit {
        /// Device-local (VRAM) bytes.
        local: u64,
        /// System-RAM-backed bytes — the GTT-equivalent pool.
        system: u64,
        /// A local region was named at all, even at zero bytes. Proof the
        /// device is discrete; see `IntelDevice::discrete`.
        saw_local: bool,
    }

    /// Split a client's memory regions. i915 names them
    /// `intel_memory_type_str(type) + instance`: "system0" / "stolen-system0"
    /// on an iGPU, "local0" / "stolen-local0" on a dGPU. xe uses its
    /// `xe_mem_type_to_name[]`: "system", "gtt", "vram0", "stolen". Bare
    /// "stolen" is xe's stolen pool, carved out of VRAM on a dGPU and out of
    /// system RAM on an iGPU, so it follows the device.
    ///
    /// Filtering to "local*"/"vram*" alone is what made every iGPU read 0.
    fn split_memory(c: &FdClient, discrete: bool) -> MemSplit {
        let mut s = MemSplit::default();
        for (region, bytes) in &c.memory {
            let local = region.starts_with("local")
                || region.starts_with("vram")
                || region.contains("-local")
                || (region == "stolen" && discrete);
            if local {
                s.local += bytes;
                s.saw_local = true;
            } else {
                s.system += bytes;
            }
        }
        s
    }

    /// Total device-local memory, or None when nothing publishes it. Chained
    /// by driver:
    ///
    /// - xe: `device/tileN/physical_vram_size_bytes` (igt's `xe_sysfs_tile`
    ///   reads exactly this). It is *physical* VRAM including reserved and
    ///   stolen pages — igt asserts the usable `vram_size` is strictly
    ///   smaller — so it slightly overstates capacity. It is still the only
    ///   figure xe publishes, and overstating by the reserved carve-out beats
    ///   reporting no card at all; the alternative is a DRM query ioctl.
    /// - i915: `lmem_total_bytes` is **not** in mainline. Intel's out-of-tree
    ///   DKMS/backport i915 registers it on the DRM minor, i.e. the `cardN`
    ///   dir — not `cardN/device`, which is where this used to look.
    /// - mainline i915: no sysfs total exists. None, never 0.
    fn vram_total(card: &Path, dev: &Path) -> Option<u64> {
        // Multi-tile parts (Ponte Vecchio) split VRAM across tiles; consumer
        // Arc has tile0 only. Stop at the first absent tile.
        let tiles: u64 = (0..)
            .map_while(|t| read_u64(&dev.join(format!("tile{t}/physical_vram_size_bytes"))))
            .sum();
        (tiles > 0)
            .then_some(tiles)
            .or_else(|| read_u64(&card.join("lmem_total_bytes")))
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
                // A published local-memory total means discrete. Mainline i915
                // publishes none even for an Arc, so the sweep upgrades this
                // the first time a client shows local-memory residency.
                let vram_total = vram_total(&card, &dev);
                Some(IntelDevice {
                    name,
                    hwmon: first_dir(&dev.join("hwmon")),
                    pdev: pdev_of(&dev),
                    card,
                    dev,
                    driver,
                    discrete: vram_total.is_some(),
                    vram_total,
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

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::backend::linux::parse_fdinfo;
        use std::fs;

        /// Fake `/sys/class/drm/cardN` + `cardN/device` pair in a scratch dir.
        fn fake_card(name: &str) -> (PathBuf, PathBuf) {
            let root = std::env::temp_dir().join(format!("gpur-intel-test-{name}"));
            let _ = fs::remove_dir_all(&root);
            let card = root.join("card0");
            let dev = card.join("device");
            fs::create_dir_all(&dev).unwrap();
            (card, dev)
        }

        /// Lunar Lake / Alder Lake class iGPU on i915: system-backed regions
        /// only, in the mixed units the DRM size printer actually emits.
        const I915_IGPU_FDINFO: &str = "\
drm-driver:\ti915
drm-client-id:\t3
drm-pdev:\t0000:00:02.0
drm-engine-render:\t123456789 ns
drm-resident-system0:\t81920 KiB
drm-resident-stolen-system0:\t1048576
";

        /// Arc A770 on i915: local memory, plus a system-backed staging BO.
        const I915_DGPU_FDINFO: &str = "\
drm-driver:\ti915
drm-client-id:\t11
drm-pdev:\t0000:03:00.0
drm-engine-render:\t500 ns
drm-resident-local0:\t512 MiB
drm-resident-stolen-local0:\t8 MiB
drm-resident-system0:\t2048 KiB
";

        /// Battlemage-era iGPU on xe: "system"/"gtt"/"stolen", no vram.
        const XE_IGPU_FDINFO: &str = "\
drm-driver:\txe
drm-client-id:\t9
drm-pdev:\t0000:00:02.0
drm-cycles-rcs:\t10
drm-total-cycles-rcs:\t1000
drm-resident-system:\t4096 KiB
drm-resident-gtt:\t2 MiB
drm-resident-stolen:\t65536
";

        /// Arc B580 on xe.
        const XE_DGPU_FDINFO: &str = "\
drm-driver:\txe
drm-client-id:\t4
drm-pdev:\t0000:03:00.0
drm-resident-vram0:\t1024 MiB
drm-resident-stolen:\t8 MiB
drm-resident-gtt:\t1024 KiB
";

        #[test]
        fn xe_reports_physical_vram_per_tile() {
            let (card, dev) = fake_card("xe-vram");
            fs::create_dir_all(dev.join("tile0")).unwrap();
            fs::write(
                dev.join("tile0/physical_vram_size_bytes"),
                "12884901888\n", // 12 GiB, a B580
            )
            .unwrap();
            assert_eq!(vram_total(&card, &dev), Some(12 << 30));

            // Multi-tile parts sum; the scan stops at the first absent tile.
            fs::create_dir_all(dev.join("tile1")).unwrap();
            fs::write(dev.join("tile1/physical_vram_size_bytes"), "12884901888").unwrap();
            assert_eq!(vram_total(&card, &dev), Some(24 << 30));
        }

        #[test]
        fn dkms_i915_lmem_total_lives_on_the_card_dir() {
            let (card, dev) = fake_card("dkms-lmem");
            // The old code looked here (the PCI dir) and always missed.
            fs::write(dev.join("lmem_total_bytes"), "17179869184\n").unwrap();
            assert_eq!(vram_total(&card, &dev), None);

            fs::write(card.join("lmem_total_bytes"), "17179869184\n").unwrap();
            assert_eq!(vram_total(&card, &dev), Some(16 << 30));
        }

        #[test]
        fn mainline_i915_has_no_vram_total() {
            // Neither file exists on a stock kernel, even for an Arc card:
            // unknown must stay None so nothing fabricates a 0-byte total.
            let (card, dev) = fake_card("mainline");
            assert_eq!(vram_total(&card, &dev), None);
        }

        #[test]
        fn igpu_memory_lands_in_the_system_bucket() {
            let c = parse_fdinfo(I915_IGPU_FDINFO).unwrap();
            let m = split_memory(&c, false);
            assert_eq!(m.local, 0);
            assert_eq!(m.system, (80 << 20) + (1 << 20));
            assert!(!m.saw_local, "an iGPU never names a local region");

            let c = parse_fdinfo(XE_IGPU_FDINFO).unwrap();
            let m = split_memory(&c, false);
            assert_eq!(m.local, 0);
            // stolen on an iGPU is carved out of system RAM.
            assert_eq!(m.system, (4 << 20) + (2 << 20) + 65_536);
            assert!(!m.saw_local);
        }

        #[test]
        fn dgpu_memory_splits_local_from_system() {
            let c = parse_fdinfo(I915_DGPU_FDINFO).unwrap();
            let m = split_memory(&c, true);
            assert_eq!(m.local, (512 << 20) + (8 << 20));
            assert_eq!(m.system, 2 << 20);
            assert!(m.saw_local, "local residency is what proves a dGPU");

            let c = parse_fdinfo(XE_DGPU_FDINFO).unwrap();
            let m = split_memory(&c, true);
            // stolen on a dGPU is carved out of VRAM.
            assert_eq!(m.local, (1024 << 20) + (8 << 20));
            assert_eq!(m.system, 1 << 20);
            assert!(m.saw_local);
        }
    }
}
