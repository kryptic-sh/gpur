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
        self, ClientSample, FdClient, ProcSnapshot, SweepCursor, SweepDevice, card_name,
        cards_with_driver, first_dir, hwmon_u64, pdev_of, read_u64,
    };
    use crate::backend::{GpuBackend, GpuProcess, GpuSnapshot, clamp_pct};
    use anyhow::Result;
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    const INTEL_VENDOR: &str = "0x8086";

    pub fn probe() -> Option<Box<dyn GpuBackend>> {
        backend().map(|b| Box::new(b) as Box<dyn GpuBackend>)
    }

    /// The probe, but concrete: the hardware tests assert on state this
    /// backend keeps between polls (the per-client counter maps), which a
    /// `Box<dyn GpuBackend>` cannot reach.
    fn backend() -> Option<IntelBackend> {
        let devices = scan("/sys/class/drm");
        if devices.is_empty() {
            return None;
        }
        Some(IntelBackend {
            devices,
            sys_mem_total: linux::sys_mem_total_bytes(),
            i915_state: HashMap::new(),
            xe_state: HashMap::new(),
            energy_state: HashMap::new(),
            cursor: SweepCursor::default(),
            buckets: IntelBuckets::default(),
            last_procs: Vec::new(),
        })
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
        /// Maximum supported PCIe link, fixed per device and resolved once at
        /// scan rather than re-read every poll.
        pcie_max_gen: Option<u8>,
        pcie_max_width: Option<u32>,
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
        /// This backend's place in the shared scanner's stream of /proc walks.
        cursor: SweepCursor,
        /// The last attributed walk's figures, and the process rows served by
        /// `processes()`. Kept rather than recomputed because a poll can find
        /// no new walk to attribute — see `attribute`.
        buckets: IntelBuckets,
        last_procs: Vec<GpuProcess>,
    }

    /// What one fdinfo sweep says about each device: the shared per-device
    /// sums, plus the two things only Intel keeps — memory split by where it
    /// lives, and the evidence that the sweep saw the device at all.
    #[derive(Default)]
    struct IntelBuckets {
        /// gpu index -> summed client utilization %.
        util: HashMap<usize, f64>,
        /// gpu index -> summed client video-engine utilization %.
        video_util: HashMap<usize, f64>,
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
            // It runs against whatever walk the shared scanner has finished; a
            // poll that finds no new one redraws the last figures.
            if let Some(snap) = self.cursor.next() {
                self.attribute(&snap);
            }

            let now = Instant::now();
            let powers: Vec<Option<f64>> = (0..self.devices.len())
                .map(|i| self.power_w(i, now))
                .collect();
            let sys_mem_total = self.sys_mem_total;
            let s = &self.buckets;
            let gpus = self
                .devices
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let h = d.hwmon.as_deref();
                    let power_w = powers[i];
                    let (pcie_gen, pcie_width) = linux::pcie_current_link(&d.dev);
                    // Whether the sweep could see this device's clients at all;
                    // every sum below is meaningless without it.
                    let attributed = s.attributed.contains(&i);
                    GpuSnapshot {
                        name: d.name.clone(),
                        device_id: linux::pci_device_id(d.pdev.as_deref()),
                        integrated: !d.discrete,
                        // Summed over this device's DRM clients — the only
                        // busy figure Intel offers.
                        utilization_pct: attributed_sum(attributed, s.util.get(&i).copied())
                            .map(clamp_pct),
                        mem_util_pct: None,
                        video_util_pct: s.video_util.get(&i).copied().map(clamp_pct),
                        enc_util_pct: None,
                        dec_util_pct: None,
                        throttle: None,
                        // Summed client-resident device-local memory. Only
                        // meaningful where a local pool exists at all: an iGPU
                        // has none, and reporting 0 there would claim an empty
                        // VRAM pool that the device does not have.
                        vram_used_bytes: d
                            .vram_total
                            .and_then(|_| attributed_sum(attributed, s.local_mem.get(&i).copied())),
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
                        pcie_max_gen: d.pcie_max_gen,
                        pcie_max_width: d.pcie_max_width,
                        // System-RAM-backed graphics memory. This is the only
                        // memory an iGPU has, and the UI renders it exactly
                        // like amdgpu's GTT pool.
                        gtt_used_bytes: attributed_sum(attributed, s.system_mem.get(&i).copied()),
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

        /// Names the drivers actually in use: a box can have an i915 iGPU beside
        /// an xe Arc card, and the two publish different telemetry, so a header
        /// naming one of them misattributes what the other lacks.
        fn driver_info(&self) -> Option<String> {
            linux::driver_line_for(self.devices.iter().map(|d| d.driver.as_str()))
        }
    }

    impl IntelBackend {
        /// Attribute one /proc walk: refreshes the per-device figures and the
        /// process rows.
        ///
        /// Only ever called with a walk this backend has not seen before. A
        /// poll that re-derived the same one would divide every i915 counter
        /// delta by a zero interval and report each card idle, so the caller
        /// redraws the previous figures instead.
        fn attribute(&mut self, snap: &ProcSnapshot) {
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
            // The walk's own timestamp, not the clock at attribution: these
            // counters were read when the walk ran, and dividing them by the
            // interval since some later moment is a different measurement.
            let now = snap.at;
            let i915_state = &mut self.i915_state;
            let xe_state = &mut self.xe_state;

            let mut sweep = linux::sweep_clients(snap, &devices, |pid, gpu, client| {
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
                    // which is all an iGPU ever reports. A mainline-i915 dGPU
                    // proves itself discrete only once a sweep shows local
                    // residency, which upgrades `discrete` after this closure —
                    // so a client that showed local regions this sweep is
                    // charged its local bytes regardless of the stale flag.
                    mem_bytes: if discrete[gpu] || mem.saw_local {
                        mem.local
                    } else {
                        mem.system
                    },
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

            self.last_procs = std::mem::take(&mut sweep.procs);
            self.buckets = IntelBuckets {
                util: sweep.util,
                video_util: sweep.video_util,
                local_mem,
                system_mem,
                attributed,
            };
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
            // The energy counter was unreadable this poll (a transient sensor
            // outage). The stored baseline, if any, spans an unknown interval —
            // keeping it would report average power over the whole outage on the next
            // good read. Drop it so the next delta covers exactly one interval; the
            // instantaneous fallback below, where present, is the honest figure.
            self.energy_state.remove(&i);
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
                let (pcie_max_gen, pcie_max_width) = linux::pcie_max_link(&dev);
                Some(IntelDevice {
                    name,
                    hwmon: first_dir(&dev.join("hwmon")),
                    pdev: pdev_of(&dev),
                    card,
                    dev,
                    driver,
                    discrete: vram_total.is_some(),
                    vram_total,
                    pcie_max_gen,
                    pcie_max_width,
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
        use crate::backend::linux::{ProcScanner, SweepCursor, parse_fdinfo, testing};
        use std::fs;
        use std::sync::Arc;
        use std::time::Duration;

        /// One i915 client of the card at 00:02.0, with cumulative engine
        /// counters as fdinfo prints them.
        fn client(render_ns: u64, video_ns: u64) -> FdClient {
            parse_fdinfo(&format!(
                "drm-driver:\ti915\n\
                 drm-client-id:\t7\n\
                 drm-pdev:\t0000:00:02.0\n\
                 drm-engine-render:\t{render_ns} ns\n\
                 drm-engine-video:\t{video_ns} ns\n\
                 drm-resident-system0:\t1048576\n"
            ))
            .unwrap()
        }

        /// The same two rules the AMD backend carries, and they bind harder
        /// here: Intel has no device busy% in sysfs at all, so this sum IS the
        /// card's utilization. A poll that re-derived the same walk would
        /// divide identical counters by a zero interval and blank the gauge,
        /// and one that measured against the clock at attribution rather than
        /// against when the walk happened would peg it.
        #[test]
        fn utilization_spans_the_two_walks_and_survives_a_poll_without_one() {
            let root = testing::tri_vendor("intel-async");
            let scanner = ProcScanner::detached();
            let devices = scan(&testing::drm(&root));
            let mut b = IntelBackend {
                devices,
                sys_mem_total: Some(1 << 34),
                i915_state: HashMap::new(),
                xe_state: HashMap::new(),
                energy_state: HashMap::new(),
                cursor: SweepCursor::on(Arc::clone(&scanner)),
                buckets: IntelBuckets::default(),
                last_procs: Vec::new(),
            };
            let card = 0; // 00:02.0, the i915 card; 06:00.0 is the xe one

            let t0 = Instant::now();
            scanner.publish(vec![(10, client(0, 0))], t0);
            let gpus = b.poll().unwrap();
            assert_eq!(
                gpus[card].utilization_pct,
                Some(0.0),
                "a first reading has no interval to divide by"
            );

            // A second later: 400 ms on render and 100 ms on the video engine,
            // so the card was 50% busy over that second and 10% of it video.
            scanner.publish(
                vec![(10, client(400_000_000, 100_000_000))],
                t0 + Duration::from_secs(1),
            );
            let gpus = b.poll().unwrap();
            assert_eq!(gpus[card].utilization_pct, Some(50.0));
            assert_eq!(gpus[card].video_util_pct, Some(10.0));
            assert_eq!(gpus[card].gtt_used_bytes, Some(1 << 20));
            let procs = b.processes();
            assert_eq!(procs.len(), 1);
            assert_eq!(procs[0].gpu_util_pct, Some(50.0));

            // A poll with no new walk redraws the last figures. The card the
            // sweep never saw stays unreadable rather than turning into a
            // confident zero.
            let gpus = b.poll().unwrap();
            assert_eq!(gpus[card].utilization_pct, Some(50.0));
            assert_eq!(gpus[card].video_util_pct, Some(10.0));
            assert_eq!(gpus[card].gtt_used_bytes, Some(1 << 20));
            assert_eq!(b.processes().len(), 1);
            assert_eq!(gpus[1].utilization_pct, None, "no client on the xe card");
        }

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

        /// The max link is a fixed capability, so the scan resolves it once and
        /// the device carries it rather than re-reading sysfs per poll.
        #[test]
        fn scan_caches_the_max_pcie_link() {
            let root = testing::tri_vendor("intel-pcie-max");
            let pci = root.join("pci/0000:00:02.0");
            fs::write(pci.join("max_link_speed"), "8.0 GT/s PCIe\n").unwrap();
            fs::write(pci.join("max_link_width"), "16\n").unwrap();
            let devices = scan(&testing::drm(&root));
            assert_eq!(devices[0].pcie_max_gen, Some(3));
            assert_eq!(devices[0].pcie_max_width, Some(16));
            // Each card reads its own files: the xe card has none.
            assert_eq!(devices[1].pcie_max_gen, None);
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

        /// A transiently unreadable energy counter must not leave a baseline
        /// that spans the outage: the next good read would report average
        /// power over the whole window instead of one interval. The baseline
        /// is dropped on a failed read, so the next reading restarts fresh.
        #[test]
        fn an_unreadable_energy_counter_drops_the_power_baseline() {
            let h = testing::Sandbox::new("intel-power-outage");
            let dev = IntelDevice {
                name: "test".into(),
                card: h.join("drm/card0"),
                dev: h.join("pci/0000:00:02.0"),
                hwmon: Some(h.to_path_buf()),
                pdev: Some("0000:00:02.0".into()),
                driver: "i915".into(),
                vram_total: None,
                discrete: false,
                pcie_max_gen: None,
                pcie_max_width: None,
            };
            let mut b = IntelBackend {
                devices: vec![dev],
                sys_mem_total: None,
                i915_state: HashMap::new(),
                xe_state: HashMap::new(),
                energy_state: HashMap::new(),
                cursor: SweepCursor::on(Arc::clone(&ProcScanner::detached())),
                buckets: IntelBuckets::default(),
                last_procs: Vec::new(),
            };
            let t0 = Instant::now();

            // First reading seeds the baseline: no delta yet.
            fs::write(h.join("energy1_input"), "1000000\n").unwrap();
            assert_eq!(b.power_w(0, t0), None);

            // One second later, 0.5 J accumulated: 0.5 W.
            fs::write(h.join("energy1_input"), "1500000\n").unwrap();
            let w = b.power_w(0, t0 + Duration::from_secs(1)).unwrap();
            assert!((w - 0.5).abs() < 1e-9, "expected 0.5 W, got {w}");

            // The counter vanishes: the instantaneous fallback is present and
            // wins, and the baseline is dropped.
            fs::remove_file(h.join("energy1_input")).unwrap();
            fs::write(h.join("power1_input"), "3000000\n").unwrap(); // 3 W
            let w = b.power_w(0, t0 + Duration::from_secs(2)).unwrap();
            assert!((w - 3.0).abs() < 1e-9, "expected the 3 W fallback, got {w}");

            // The counter comes back. With the fix the baseline is gone, so
            // this is a fresh first sample (None); without the fix it would
            // report (2e6 - 1.5e6)/1e6/2s = 0.25 W averaged over the outage.
            fs::write(h.join("energy1_input"), "2000000\n").unwrap();
            assert_eq!(b.power_w(0, t0 + Duration::from_secs(3)), None);

            // One more second: the delta now covers exactly one interval —
            // 0.5 W.
            fs::write(h.join("energy1_input"), "2500000\n").unwrap();
            let w = b.power_w(0, t0 + Duration::from_secs(4)).unwrap();
            assert!((w - 0.5).abs() < 1e-9, "expected 0.5 W, got {w}");
        }
    }

    /// Tests that read this machine's own Intel GPU. Everything above runs on
    /// fabricated sysfs trees and canned fdinfo, which cannot catch what only
    /// live hardware shows: a sysfs path that moved between kernel releases, a
    /// counter the driver stopped publishing, or an invariant that holds on a
    /// fixture and breaks against a real `/proc` sweep.
    ///
    /// So these read `/sys/class/drm` and `/proc` as they are, and skip
    /// themselves where `probe` finds no i915/xe card — which is every CI
    /// runner this project has. Set `GPUR_REQUIRE_INTEL=1` on a runner that is
    /// supposed to have an Intel GPU: the skip becomes a failure, so the day
    /// the card or the driver disappears from that machine is not the day the
    /// suite quietly stops testing this backend.
    ///
    /// Read-only throughout: no test here writes sysfs, signals a process, or
    /// depends on the machine being idle or busy.
    #[cfg(test)]
    mod hardware {
        use super::*;
        use crate::backend::ProcKind;
        use std::fs;
        use std::time::Duration;

        /// This machine's Intel backend, or `None` with a note saying why the
        /// caller is about to do nothing.
        fn intel() -> Option<IntelBackend> {
            // These tests open DRM clients and then poll, so they are
            // asserting on a walk that has to happen after the open — not on
            // whichever one the worker thread last finished.
            linux::ProcScanner::shared().set_synchronous(true);
            if let Some(b) = backend() {
                return Some(b);
            }
            assert!(
                std::env::var_os("GPUR_REQUIRE_INTEL").is_none(),
                "GPUR_REQUIRE_INTEL is set, but no i915/xe card was found in \
                 /sys/class/drm — this machine cannot test the Intel backend"
            );
            eprintln!("skipping: no Intel GPU on this machine");
            None
        }

        /// Enough of a gap for the counter deltas to have something to divide.
        const GAP: Duration = Duration::from_millis(100);

        #[test]
        fn the_scan_claims_this_machines_intel_cards() {
            let Some(b) = intel() else { return };
            for d in &b.devices {
                assert!(
                    !d.name.is_empty(),
                    "a nameless card renders as a blank header row"
                );
                assert!(
                    is_intel_driver(&d.driver),
                    "{} claimed on driver {:?}",
                    d.name,
                    d.driver
                );
                // Every Intel GPU is a PCI device, iGPUs included. The BDF is
                // what fdinfo's `drm-pdev` is matched against, so a card
                // without one can never have a single client attributed to it.
                assert!(d.pdev.is_some(), "{} has no PCI address", d.name);
                // The card dir has to stay the DRM minor, not the PCI device:
                // i915 keeps the clock and `lmem_total_bytes` there.
                assert!(
                    d.card
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("card")),
                    "{:?} is not a DRM minor dir",
                    d.card
                );
                assert!(d.dev.join("vendor").exists(), "{:?}", d.dev);
            }
        }

        #[test]
        fn a_live_poll_reports_only_values_this_hardware_can_produce() {
            let Some(mut b) = intel() else { return };
            let gpus = b.poll().unwrap();
            assert_eq!(gpus.len(), b.devices.len(), "one snapshot per card");

            for g in &gpus {
                for (what, pct) in [
                    ("utilization", g.utilization_pct),
                    ("video", g.video_util_pct),
                ] {
                    if let Some(v) = pct {
                        assert!((0.0..=100.0).contains(&v), "{} {what} {v}%", g.name);
                    }
                }
                if let Some(t) = g.temperature_c {
                    assert!((-50.0..=150.0).contains(&t), "{} at {t}°C", g.name);
                }
                if let Some(w) = g.power_w {
                    assert!((0.0..=1000.0).contains(&w), "{} at {w} W", g.name);
                }
                if let Some(w) = g.power_limit_w {
                    assert!((0.0..=1000.0).contains(&w), "{} limit {w} W", g.name);
                }
                if let Some(mhz) = g.clock_mhz {
                    assert!(mhz <= 5000, "{} at {mhz} MHz", g.name);
                }
                assert!(
                    g.device_id
                        .as_deref()
                        .is_some_and(|id| id.starts_with("pci:")),
                    "{} has no PCI-derived identity: {:?}",
                    g.name,
                    g.device_id
                );
                assert_eq!(
                    g.gtt_total_bytes,
                    linux::sys_mem_total_bytes(),
                    "{}: system-backed graphics memory is capped by system RAM",
                    g.name
                );
                if let Some(total) = g.vram_total_bytes {
                    assert!(total > 0, "{}: a published total of 0 is not one", g.name);
                }
                assert!(
                    g.vram_used_bytes.is_none() || g.vram_total_bytes.is_some(),
                    "{}: VRAM usage without a total draws a meter with no scale",
                    g.name
                );
                if g.integrated {
                    // An iGPU has no local memory pool at all, and a 0/0 VRAM
                    // meter over one claims an empty pool that doesn't exist.
                    assert_eq!(g.vram_total_bytes, None, "{} is integrated", g.name);
                    assert_eq!(g.vram_used_bytes, None, "{} is integrated", g.name);
                }
                // Counters neither Intel driver publishes. A value here means
                // something started fabricating one.
                assert_eq!(g.mem_util_pct, None, "{}", g.name);
                assert_eq!(g.enc_util_pct, None, "{}", g.name);
                assert_eq!(g.dec_util_pct, None, "{}", g.name);
                assert_eq!(g.fan_pct, None, "{}", g.name);
                assert_eq!(g.fan_rpm, None, "{}", g.name);
                assert_eq!(g.throttle, None, "{}", g.name);
                assert_eq!(g.mem_clock_mhz, None, "{}", g.name);
                assert_eq!(g.temp_junction_c, None, "{}", g.name);
                assert_eq!(g.temp_mem_c, None, "{}", g.name);
                assert_eq!(g.volt_mv, None, "{}", g.name);
                assert_eq!(g.perf_level, None, "{}", g.name);
                assert_eq!(g.pcie_rx_kbs, None, "{}", g.name);
                assert_eq!(g.pcie_tx_kbs, None, "{}", g.name);
            }
        }

        /// `App` keys graph history, session peaks and folding on the device
        /// id, and holds a card's row at its index. Both have to name the same
        /// card on every poll of the same machine.
        #[test]
        fn device_identity_and_order_survive_repeated_polls() {
            let Some(mut b) = intel() else { return };
            let first = b.poll().unwrap();
            std::thread::sleep(GAP);
            let second = b.poll().unwrap();

            let identity = |gpus: &[GpuSnapshot]| {
                gpus.iter()
                    .map(|g| (g.name.clone(), g.device_id.clone(), g.integrated))
                    .collect::<Vec<_>>()
            };
            assert_eq!(identity(&first), identity(&second));
        }

        /// Intel publishes cumulative energy (µJ), not watts, so the first
        /// poll has nothing to subtract from: it must report `None` rather
        /// than a card sitting at a confident 0 W. The second poll is the
        /// first real reading.
        #[test]
        fn power_is_an_energy_delta_so_the_first_poll_reports_none() {
            let Some(mut b) = intel() else { return };
            let energy: Vec<bool> = b
                .devices
                .iter()
                .map(|d| {
                    d.hwmon
                        .as_deref()
                        .is_some_and(|h| read_u64(&h.join("energy1_input")).is_some())
                })
                .collect();
            if !energy.contains(&true) {
                eprintln!("skipping: no readable hwmon energy counter on this machine");
                return;
            }

            let first = b.poll().unwrap();
            std::thread::sleep(GAP);
            let second = b.poll().unwrap();

            for (i, _) in energy.iter().enumerate().filter(|(_, has)| **has) {
                assert_eq!(
                    first[i].power_w, None,
                    "{}: nothing to take a delta against yet",
                    first[i].name
                );
                let w = second[i].power_w.unwrap_or_else(|| {
                    panic!("{}: no energy delta on the second poll", first[i].name)
                });
                assert!((0.0..=1000.0).contains(&w), "{} at {w} W", first[i].name);
            }
        }

        /// The process table is built by the same fdinfo sweep that feeds the
        /// gauges, so a row in it is proof the sweep read that device's
        /// clients — which is exactly the condition under which its gauges are
        /// measurements rather than `None`.
        #[test]
        fn processes_come_from_the_poll_sweep_and_name_a_device_it_read() {
            let Some(mut b) = intel() else { return };
            assert!(
                b.processes().is_empty(),
                "processes() serves the last poll's sweep, it does not scan"
            );

            let gpus = b.poll().unwrap();
            let procs = b.processes();
            for p in &procs {
                assert!(
                    p.gpu_index < gpus.len(),
                    "pid {} attributed to card {} of {}",
                    p.pid,
                    p.gpu_index,
                    gpus.len()
                );
                let util = p
                    .gpu_util_pct
                    .expect("the sweep computes a figure for every client it reads");
                assert!((0.0..=100.0).contains(&util), "pid {} at {util}%", p.pid);
                assert!(
                    p.gpu_mem_bytes.is_some(),
                    "pid {}: fdinfo regions were read, so zero is a reading",
                    p.pid
                );

                let g = &gpus[p.gpu_index];
                assert!(
                    g.utilization_pct.is_some(),
                    "{} has attributed clients but reports no utilization",
                    g.name
                );
                assert!(
                    g.gtt_used_bytes.is_some(),
                    "{} has attributed clients but reports no system memory",
                    g.name
                );
            }
            if procs.is_empty() {
                eprintln!(
                    "note: no readable DRM clients — run as a user owning one to \
                     exercise attribution"
                );
            }
        }

        /// The counter-delta maps are keyed on (pid, drm-client-id) and pruned
        /// against what each sweep saw. A leak here is unbounded: a gpur left
        /// running on a busy box would keep an entry for every short-lived
        /// client forever.
        #[test]
        fn per_client_delta_state_is_pruned_to_clients_that_still_exist() {
            let Some(mut b) = intel() else { return };
            b.poll().unwrap();
            std::thread::sleep(GAP);
            // Bracket the sweep: a pid it charges must have been alive during
            // it, so it shows up in one of these two readings unless it both
            // appeared and exited inside the sweep — which a process holding a
            // DRM client does not do.
            let before: HashSet<u32> = linux::proc_pids().into_iter().collect();
            b.poll().unwrap();
            let after: HashSet<u32> = linux::proc_pids().into_iter().collect();

            for (pid, client) in b.i915_state.keys().chain(b.xe_state.keys()) {
                assert!(
                    before.contains(pid) || after.contains(pid),
                    "counter state kept for client {client} of dead pid {pid}"
                );
            }
            assert!(
                b.energy_state.len() <= b.devices.len(),
                "energy state is per card, not per poll"
            );
        }

        /// What the MEM meter ends up showing on this machine. An iGPU has no
        /// local pool, so the readout has to fall through to the system-backed
        /// one and mark it shared — reporting `n/a` there described a card
        /// that was demonstrably holding memory as holding none.
        #[test]
        fn a_card_with_no_local_pool_meters_its_share_of_system_ram() {
            let Some(mut b) = intel() else { return };
            let gpus = b.poll().unwrap();
            for g in &gpus {
                let m = g.mem_primary();
                if g.vram_total_bytes.is_some() {
                    assert!(!m.shared, "{} has a local pool of its own", g.name);
                    continue;
                }
                assert_eq!(m.used, g.gtt_used_bytes, "{}", g.name);
                assert_eq!(m.total, g.gtt_total_bytes, "{}", g.name);
                assert!(m.shared, "{}: system RAM must be marked shared", g.name);
                assert_eq!(
                    g.mem_secondary(),
                    None,
                    "{}: one pool, so nothing sits beside it",
                    g.name
                );
                if g.gtt_used_bytes.is_some() {
                    assert!(
                        g.mem_pct().is_some(),
                        "{}: the memory graph would stay blank",
                        g.name
                    );
                }
            }
        }

        /// The header names the drivers actually in use, because an i915 iGPU
        /// beside an xe card publishes different telemetry and a line naming
        /// one of them misattributes what the other lacks.
        #[test]
        fn the_driver_line_names_every_driver_this_machine_runs() {
            let Some(b) = intel() else { return };
            let line = b
                .driver_info()
                .expect("the kernel release is readable on Linux");
            for d in &b.devices {
                assert!(line.contains(&d.driver), "{line:?} omits {}", d.driver);
            }
            assert!(line.contains("kernel"), "{line:?}");
        }

        /// The total is chained across three sysfs locations that no fixture
        /// can prove are the real ones. Looking on the PCI dir instead of the
        /// DRM minor is what made every DKMS-i915 Arc card read as having no
        /// VRAM at all, and that reads as a missing meter, never an error.
        #[test]
        fn the_vram_total_is_read_from_where_this_driver_publishes_it() {
            let Some(b) = intel() else { return };
            for d in &b.devices {
                assert_eq!(
                    d.vram_total,
                    vram_total(&d.card, &d.dev),
                    "{}: total is not a stable read of sysfs",
                    d.name
                );
                if d.driver == "i915" && !d.card.join("lmem_total_bytes").exists() {
                    assert_eq!(
                        d.vram_total, None,
                        "{}: mainline i915 publishes no total, so none may be invented",
                        d.name
                    );
                }
            }
        }

        /// i915 keeps the current clock on the card dir, xe under the tile.
        /// A path that drifts reads as a card with no clock rather than an
        /// error, so the check is against the files that are actually there.
        #[test]
        fn the_current_clock_is_read_from_where_this_driver_keeps_it() {
            let Some(b) = intel() else { return };
            for d in &b.devices {
                let published = d.card.join("gt_cur_freq_mhz").exists()
                    || d.dev.join("tile0/gt0/freq0/cur_freq").exists()
                    || d.card.join("gt/gt0/rps_cur_freq_mhz").exists();
                if !published {
                    eprintln!("note: {} publishes no clock file", d.name);
                    continue;
                }
                assert!(
                    gt_cur_freq_mhz(d).is_some(),
                    "{}: sysfs has a clock file this backend did not read",
                    d.name
                );
                let mhz = gt_cur_freq_mhz(d).unwrap();
                assert!(
                    (1..=5000).contains(&mhz),
                    "{} at {mhz} MHz — a clock read as 0 is a file that stopped parsing",
                    d.name
                );
            }
        }

        /// The three hwmon gauges, checked against the hwmon dir this card
        /// actually has. Both directions matter: a gauge missing where the
        /// file exists is telemetry silently dropped, and a gauge present
        /// where no file does is a number invented from nothing.
        ///
        /// Whole classes of Intel iGPU (Tiger Lake and friends on i915)
        /// register no hwmon at all, which is why the UI has to render
        /// temperature and power as absent rather than as zero.
        #[test]
        fn the_hwmon_gauges_are_read_where_hwmon_exists_and_stay_none_where_it_does_not() {
            let Some(mut b) = intel() else { return };
            let hwmon: Vec<Option<PathBuf>> = b.devices.iter().map(|d| d.hwmon.clone()).collect();
            // Two polls: power is an energy delta, and the first has none.
            b.poll().unwrap();
            std::thread::sleep(GAP);
            let gpus = b.poll().unwrap();

            for (i, g) in gpus.iter().enumerate() {
                let h = hwmon[i].as_deref();
                assert_eq!(
                    g.temperature_c.is_some(),
                    hwmon_u64(h, "temp1_input").is_some(),
                    "{}: temperature disagrees with hwmon temp1_input",
                    g.name
                );
                assert_eq!(
                    g.power_limit_w.is_some(),
                    hwmon_u64(h, "power1_max").is_some_and(|v| v > 0),
                    "{}: power limit disagrees with hwmon power1_max",
                    g.name
                );
                assert_eq!(
                    g.power_w.is_some(),
                    hwmon_u64(h, "energy1_input").is_some()
                        || hwmon_u64(h, "power1_input").is_some(),
                    "{}: power disagrees with the hwmon counters on disk",
                    g.name
                );
                if h.is_none() {
                    assert_eq!(
                        (g.temperature_c, g.power_w, g.power_limit_w),
                        (None, None, None),
                        "{}: no hwmon dir, so all three are unknown",
                        g.name
                    );
                    eprintln!(
                        "note: {} registers no hwmon — temperature and power are \
                         unavailable on this card",
                        g.name
                    );
                }
            }
        }

        /// PCIe comes from the PCI core's own attributes, identical for every
        /// vendor's endpoint. An iGPU is not a PCIe endpoint in any useful
        /// sense and its bridge answers "Unknown", which has to read as absent.
        #[test]
        fn the_pcie_link_is_the_pci_core_attributes_verbatim() {
            let Some(mut b) = intel() else { return };
            let gpus = b.poll().unwrap();
            for (i, g) in gpus.iter().enumerate() {
                let (cur_gen, width, max_gen, max_width) = linux::pcie_link(&b.devices[i].dev);
                // The maximum is a fixed capability. The negotiated link can
                // change between the poll and this read — cards downshift when
                // idle — so only its presence is comparable.
                assert_eq!(g.pcie_max_gen, max_gen, "{}", g.name);
                assert_eq!(g.pcie_max_width, max_width, "{}", g.name);
                assert_eq!(g.pcie_gen.is_some(), cur_gen.is_some(), "{}", g.name);
                assert_eq!(g.pcie_width.is_some(), width.is_some(), "{}", g.name);
                if cur_gen.is_none() {
                    eprintln!("note: {} negotiates no readable PCIe link", g.name);
                }
            }
        }

        /// The header should name the card, not repeat its PCI id. The lookup
        /// is checked against a second, independent read of the pci.ids
        /// database rather than against the same helper's output.
        #[test]
        fn the_card_name_is_this_devices_pci_ids_entry() {
            let Some(b) = intel() else { return };
            let Ok(ids) = fs::read_to_string("/usr/share/hwdata/pci.ids") else {
                eprintln!("skipping: no pci.ids database installed");
                return;
            };
            for d in &b.devices {
                let idx = d
                    .card
                    .file_name()
                    .and_then(|n| linux::card_index(&n.to_string_lossy()))
                    .expect("a card dir is named cardN");
                assert_eq!(d.name, card_name(&d.dev, idx, "8086", "Intel"));

                let device_id = fs::read_to_string(d.dev.join("device")).unwrap();
                let device_id = device_id.trim().trim_start_matches("0x");
                if let Some(marketing) = linux::pci_device_name(&ids, "8086", device_id) {
                    assert_eq!(
                        d.name, marketing,
                        "card{idx} is listed in pci.ids but rendered as a fallback"
                    );
                }
            }
        }

        /// The device gauges and the process rows are two views of one fdinfo
        /// sweep, and the UI shows them side by side: a card metered at 6%
        /// over rows summing to 40% is a bug a user can see. Asserted on the
        /// second poll, because the first has no counter deltas to divide and
        /// is also where a mainline-i915 Arc first learns it is discrete.
        #[test]
        fn the_device_gauges_are_the_sum_of_the_rows_from_the_same_sweep() {
            let Some(mut b) = intel() else { return };
            b.poll().unwrap();
            std::thread::sleep(GAP);
            let gpus = b.poll().unwrap();
            let procs = b.processes();

            for (i, g) in gpus.iter().enumerate() {
                let rows: Vec<&GpuProcess> = procs.iter().filter(|p| p.gpu_index == i).collect();
                if rows.is_empty() {
                    continue;
                }
                let util: f64 = rows.iter().filter_map(|p| p.gpu_util_pct).sum();
                let mem: u64 = rows.iter().filter_map(|p| p.gpu_mem_bytes).sum();

                let metered = g
                    .utilization_pct
                    .expect("rows exist, so this device's clients were read");
                assert!(
                    (metered - clamp_pct(util)).abs() < 1e-6,
                    "{}: meter says {metered}%, rows sum to {util}%",
                    g.name
                );
                // Rows charge the pool the device actually spends: the system
                // pool on an iGPU, VRAM on a card that publishes a total.
                let pool = if g.integrated {
                    g.gtt_used_bytes
                } else if g.vram_total_bytes.is_some() {
                    g.vram_used_bytes
                } else {
                    continue;
                };
                assert_eq!(
                    pool,
                    Some(mem),
                    "{}: memory meter and rows disagree",
                    g.name
                );
            }
        }

        /// Core and video utilization are filled from the same sweep by two
        /// different code paths — one goes through `attributed_sum`, one reads
        /// its map directly. They must still agree on whether this device was
        /// measured at all, or the UI shows a video figure beside a core gauge
        /// reading n/a.
        #[test]
        fn video_utilization_is_reported_wherever_core_utilization_is() {
            let Some(mut b) = intel() else { return };
            let gpus = b.poll().unwrap();
            for g in &gpus {
                assert_eq!(
                    g.utilization_pct.is_some(),
                    g.video_util_pct.is_some(),
                    "{}: core {:?} vs video {:?}",
                    g.name,
                    g.utilization_pct,
                    g.video_util_pct
                );
            }
        }

        /// Attribution end to end: every DRM client this user owns has to
        /// reach a row. Found by walking `/proc` independently of the sweep,
        /// then intersected with a second walk so a client that opened or
        /// closed around the poll cannot fail the test.
        #[test]
        fn every_drm_client_this_user_owns_reaches_a_process_row() {
            let Some(mut b) = intel() else { return };
            let devices: Vec<SweepDevice> = b
                .devices
                .iter()
                .map(|d| SweepDevice {
                    pdev: d.pdev.clone(),
                    driver: d.driver.clone(),
                })
                .collect();
            // (pid, card) -> touched a render engine.
            let walk = || {
                let mut out: HashMap<(u32, usize), bool> = HashMap::new();
                for pid in linux::proc_pids() {
                    for c in linux::drm_clients(pid) {
                        if let Some(gpu) = linux::client_device(&devices, &c) {
                            let graphics = c.engine_ns.keys().any(|k| k == "render" || k == "rcs")
                                || c.cycles.keys().any(|k| k == "rcs");
                            *out.entry((pid, gpu)).or_default() |= graphics;
                        }
                    }
                }
                out
            };

            let before = walk();
            b.poll().unwrap();
            let after = walk();
            let rows: HashMap<(u32, usize), ProcKind> = b
                .processes()
                .iter()
                .map(|p| ((p.pid, p.gpu_index), p.kind))
                .collect();

            if before.is_empty() {
                eprintln!(
                    "note: this user owns no readable DRM client — nothing to \
                     attribute on this machine"
                );
            }
            for (key, graphics) in &before {
                let Some(still_graphics) = after.get(key) else {
                    continue; // client went away around the poll
                };
                let (pid, gpu) = *key;
                let kind = rows.get(key).unwrap_or_else(|| {
                    panic!("pid {pid}'s client of card {gpu} was not attributed to any row")
                });
                if *graphics && *still_graphics {
                    assert_eq!(
                        *kind,
                        ProcKind::Graphics,
                        "pid {pid} runs on a render engine but is filed as compute"
                    );
                }
            }
        }
    }
}
