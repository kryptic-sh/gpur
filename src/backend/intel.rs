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

/// Device ids this backend claims from a fake `/sys/class/drm`, for the shared
/// test that proves the Linux scans partition that one directory.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn claimed_ids(drm: &str) -> Vec<String> {
    linux_impl::claimed_ids(drm)
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use crate::backend::linux::{
        self, ClientSample, FdClient, SweepDevice, card_name, cards_with_driver, first_dir,
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

    /// One fdinfo sweep, with the per-device buckets only Intel keeps: memory
    /// split by where it lives, and the evidence that the sweep saw the device
    /// at all.
    struct IntelSweep {
        sweep: linux::Sweep,
        /// gpu index -> summed client-resident device-local (VRAM) bytes.
        local_mem: HashMap<usize, u64>,
        /// gpu index -> summed system-backed (GTT-equivalent) bytes.
        system_mem: HashMap<usize, u64>,
        /// gpu indices at least one DRM client was attributed to this pass.
        /// `Sweep::seen` is keyed by (pid, client-id) and so cannot answer
        /// this. Without it an unattributed device is indistinguishable from
        /// an idle one, because both leave every bucket above empty.
        attributed: HashSet<usize>,
    }

    impl GpuBackend for IntelBackend {
        fn name(&self) -> &'static str {
            "intel"
        }

        fn poll(&mut self) -> Result<Vec<GpuSnapshot>> {
            // One fdinfo sweep feeds device utilization AND the process table.
            let mut s = self.sweep_clients();
            self.last_procs = std::mem::take(&mut s.sweep.procs);

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
                    // Whether the sweep could see this device's clients at all;
                    // every sum below is meaningless without it.
                    let attributed = s.attributed.contains(&i);
                    GpuSnapshot {
                        name: d.name.clone(),
                        device_id: linux::pci_device_id(d.pdev.as_deref()),
                        integrated: !d.discrete,
                        // Summed over this device's DRM clients — the only
                        // busy figure Intel offers.
                        utilization_pct: attributed_sum(attributed, s.sweep.util.remove(&i))
                            .map(clamp_pct),
                        mem_util_pct: None,
                        video_util_pct: s.sweep.video_util.remove(&i).map(clamp_pct),
                        enc_util_pct: None,
                        dec_util_pct: None,
                        throttle: None,
                        // Summed client-resident device-local memory. Only
                        // meaningful where a local pool exists at all: an iGPU
                        // has none, and reporting 0 there would claim an empty
                        // VRAM pool that the device does not have.
                        vram_used_bytes: d
                            .vram_total
                            .and_then(|_| attributed_sum(attributed, s.local_mem.remove(&i))),
                        // None, never 0: mainline i915 publishes no total.
                        vram_total_bytes: d.vram_total,
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
                        gtt_used_bytes: attributed_sum(attributed, s.system_mem.remove(&i)),
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
        fn sweep_clients(&mut self) -> IntelSweep {
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
            let mut attributed: HashSet<usize> = HashSet::new();
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
                attributed.insert(gpu);

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

            IntelSweep {
                sweep,
                local_mem,
                system_mem,
                attributed,
            }
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

    /// A per-device sum from the fdinfo sweep, but only where the sweep was in
    /// a position to measure anything: `attributed` says at least one client
    /// of that device was read. Everything Intel reports is a sum over other
    /// processes' fdinfo, and `/proc/<pid>/fd` is unreadable for every process
    /// another user owns — so an unprivileged gpur watching someone else's
    /// saturated iGPU attributes nothing to it and every bucket comes back
    /// empty. Reporting the empty sum there paints a confident 0% meter over a
    /// pegged GPU, so an unattributed device is None. A device whose clients
    /// were read and summed to zero really is idle, and keeps its Some.
    fn attributed_sum<T: Default>(attributed: bool, sum: Option<T>) -> Option<T> {
        attributed.then(|| sum.unwrap_or_default())
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

    /// Intel's two DRM drivers. Anything else on vendor 8086 in `/sys/class/drm`
    /// — a future non-GPU DRM device, an unbound card — is not ours to read.
    fn is_intel_driver(driver: &str) -> bool {
        driver == "i915" || driver == "xe"
    }

    fn scan(drm: &str) -> Vec<IntelDevice> {
        cards_with_driver(drm, INTEL_VENDOR, is_intel_driver)
            .into_iter()
            .filter_map(|(idx, dev, driver)| {
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
    pub fn claimed_ids(drm: &str) -> Vec<String> {
        scan(drm)
            .iter()
            .filter_map(|d| linux::pci_device_id(d.pdev.as_deref()))
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::backend::linux::{parse_fdinfo, testing};
        use std::fs;

        /// Exactly the i915/xe cards, on a tree that also holds AMD and NVIDIA
        /// cards plus the render nodes and connectors that reach the same PCI
        /// devices — including an 8086 render node whose `vendor` reads 0x8086.
        #[test]
        fn scan_claims_intel_driven_cards_only() {
            let root = testing::tri_vendor("intel-scan");
            let devices = scan(&testing::drm(&root));
            assert_eq!(
                devices
                    .iter()
                    .map(|d| (d.pdev.as_deref(), d.driver.as_str()))
                    .collect::<Vec<_>>(),
                [(Some("0000:00:02.0"), "i915"), (Some("0000:06:00.0"), "xe"),],
                "card index order; nothing from another vendor, and not card7, \
                 whose driver symlink does not resolve"
            );
            // `dev.parent()` has to stay the DRM minor dir: i915's clock and
            // lmem files live there, not on the PCI device.
            assert!(devices[0].card.ends_with("card0"));
        }

        /// Fake `cardN` + `cardN/device` pair. The sandbox *is* the card dir —
        /// `vram_total` only reads files sitting directly in each of the two,
        /// never the names above them — so the returned `dev` is cleaned up
        /// with the card when the guard drops.
        fn fake_card(name: &str) -> (testing::Sandbox, PathBuf) {
            let card = testing::Sandbox::new(name);
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

        /// An attributed client that happens to be doing nothing is a real
        /// measurement of an idle device, and has to survive as `Some(0.0)` —
        /// the meter is supposed to be drawn, empty.
        #[test]
        fn idle_attributed_clients_still_measure_zero() {
            assert_eq!(attributed_sum(true, Some(0.0)), Some(0.0));
            assert_eq!(attributed_sum(true, Some(0u64)), Some(0));
            // Clients were seen, so the device's bucket exists even when no
            // client of it contributed to this particular sum.
            assert_eq!(attributed_sum(true, None::<f64>), Some(0.0));
            assert_eq!(attributed_sum(true, Some(37.5)), Some(37.5));
        }

        /// The unprivileged case: every DRM client on the box belongs to
        /// another user, `/proc/<pid>/fd` is unreadable, and the sweep charges
        /// this device nothing. That is "cannot read", not "idle" — reporting
        /// 0 renders another user's fully loaded iGPU as a measured 0%.
        #[test]
        fn a_device_with_no_attributed_clients_is_unknown() {
            assert_eq!(attributed_sum(false, None::<f64>), None);
            assert_eq!(attributed_sum(false, None::<u64>), None);
            // Even a stale bucket cannot promote it: no client, no measurement.
            assert_eq!(attributed_sum(false, Some(0u64)), None);
            assert_eq!(attributed_sum(false, Some(90.0)), None);
        }

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
