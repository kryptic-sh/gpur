//! AMD backend. Linux: sysfs, amdgpu and (name and hwmon only) radeon.
//! Windows: ADLX (not yet implemented).

use super::GpuBackend;

pub fn probe() -> Option<Box<dyn GpuBackend>> {
    #[cfg(target_os = "linux")]
    if let Some(b) = linux_impl::probe() {
        return Some(b);
    }
    // TODO Windows: ADLX bindings.
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
        cards_with_driver, fan_pct, first_dir, hwmon_u64, pdev_of, read_trim, read_u64,
    };
    use crate::backend::{GpuBackend, GpuProcess, GpuSnapshot, clamp_pct};
    use anyhow::Result;
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    const AMD_VENDOR: &str = "0x1002";

    /// AMD's two in-tree DRM drivers. `radeon` drives pre-GCN parts and shares
    /// almost none of amdgpu's sysfs — no `gpu_busy_percent`, no
    /// `mem_info_vram_*`, no fdinfo engine accounting — so it arrives with most
    /// gauges empty. It is still listed: an old card in a box is a card, and
    /// hwmon still gives it a temperature, a fan and a name.
    fn is_amd_driver(driver: &str) -> bool {
        driver == "amdgpu" || driver == "radeon"
    }

    pub fn probe() -> Option<Box<dyn GpuBackend>> {
        backend().map(|b| Box::new(b) as Box<dyn GpuBackend>)
    }

    /// The probe, but concrete: the hardware tests assert on state this
    /// backend keeps between polls (the per-client engine counters, the
    /// `pcie_bw` samples), which a `Box<dyn GpuBackend>` cannot reach.
    fn backend() -> Option<AmdBackend> {
        let devices = scan("/sys/class/drm");
        if devices.is_empty() {
            return None;
        }
        Some(AmdBackend {
            pcie_state: vec![None; devices.len()],
            devices,
            engine_state: HashMap::new(),
            cursor: SweepCursor::default(),
            media: HashMap::new(),
            last_procs: Vec::new(),
        })
    }

    struct AmdDevice {
        name: String,
        dev: PathBuf,
        hwmon: Option<PathBuf>,
        /// PCI address ("0000:75:00.0"), matched against fdinfo drm-pdev.
        pdev: Option<String>,
        /// Bound DRM driver, "amdgpu" or "radeon". A client's fdinfo has to
        /// name the same one before it counts against this device.
        driver: String,
        /// APU rather than discrete card. Fixed per device, so resolved once:
        /// the per-process memory rule depends on it every sweep.
        integrated: bool,
        /// Critical edge temperature (°C), for the throttle heuristic.
        temp_crit_c: Option<f64>,
        /// hwmon channel numbers for the junction / memory temp sensors.
        temp_junction_ch: Option<u8>,
        temp_mem_ch: Option<u8>,
        /// Maximum supported PCIe link, fixed per device and resolved once at
        /// scan rather than re-read every poll.
        pcie_max_gen: Option<u8>,
        pcie_max_width: Option<u32>,
    }

    struct AmdBackend {
        devices: Vec<AmdDevice>,
        /// (pid, drm-client-id) -> that client's engine counters at last scan.
        engine_state: HashMap<(u32, u64), EngineSample>,
        /// Per device: (rx count, tx count, sampled at) from `pcie_bw`.
        pcie_state: Vec<Option<(u64, u64, Instant)>>,
        /// This backend's place in the shared scanner's stream of /proc walks.
        cursor: SweepCursor,
        /// Last attribution's per-device media utilization, and the process
        /// rows served by `processes()`. Both are kept rather than recomputed
        /// because a poll can arrive with no new walk to attribute — see
        /// `attribute`.
        media: HashMap<usize, MediaUtil>,
        last_procs: Vec<GpuProcess>,
    }

    /// Cumulative fdinfo engine counters of one DRM client at the last sweep.
    #[derive(Clone, Copy)]
    struct EngineSample {
        total_ns: u64,
        video_ns: u64,
        enc_ns: u64,
        dec_ns: u64,
        at: Instant,
    }

    /// Media-engine utilization accumulated over one device's clients. `enc` /
    /// `dec` stay None unless some client actually reported that engine class —
    /// amdgpu only prints the engines a client used, so absence means "this
    /// device has no separate encoder/decoder activity", not "0%".
    #[derive(Clone, Copy, Default)]
    struct MediaUtil {
        video: f64,
        enc: Option<f64>,
        dec: Option<f64>,
    }

    /// VCN/media engines in amdgpu fdinfo naming (`amdgpu_ip_name[]`).
    fn is_video_engine(name: &str) -> bool {
        is_dec_engine(name)
            || is_enc_engine(name)
            || name.starts_with("vcn")
            || name.starts_with("vpe")
    }

    /// Encoder rings: VCE, UVD-ENC and VCN-ENC all report as "enc".
    fn is_enc_engine(name: &str) -> bool {
        name.starts_with("enc")
    }

    /// Decoder rings: UVD/VCN-DEC report as "dec", the JPEG block as "jpeg".
    fn is_dec_engine(name: &str) -> bool {
        name.starts_with("dec") || name.starts_with("jpeg")
    }

    impl GpuBackend for AmdBackend {
        fn name(&self) -> &'static str {
            "amdgpu"
        }

        fn poll(&mut self) -> Result<Vec<GpuSnapshot>> {
            // The fdinfo sweep gives the per-process rows and the device video
            // util (sysfs has gpu_busy_percent but nothing for the VCN
            // engines). It runs against whatever walk the shared scanner has
            // finished; a poll that finds no new one keeps the last reading.
            if let Some(snap) = self.cursor.next() {
                self.attribute(&snap);
            }
            let now = Instant::now();
            let pcie_state = &mut self.pcie_state;
            let media = &self.media;
            Ok(self
                .devices
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let bw = pcie_state
                        .get_mut(i)
                        .and_then(|prev| pcie_bw_kbs(&d.dev, prev, now));
                    sample(d, media.get(&i).copied(), bw)
                })
                .collect())
        }

        fn processes(&mut self) -> Vec<GpuProcess> {
            self.last_procs.clone()
        }

        /// Names the drivers actually in use, not the backend: a box with a
        /// pre-GCN card beside a modern one is running both, and a header
        /// reading "amdgpu" would misattribute the gauges the old card lacks.
        fn driver_info(&self) -> Option<String> {
            linux::driver_line_for(self.devices.iter().map(|d| d.driver.as_str()))
        }
    }

    impl AmdBackend {
        /// Attribute one /proc walk: refreshes the media-utilization map and
        /// the process rows.
        ///
        /// Only ever called with a walk this backend has not seen before. A
        /// poll that re-derived the same one would compute every counter delta
        /// over a zero interval and report the whole machine idle, so the
        /// caller redraws the previous result instead.
        fn attribute(&mut self, snap: &ProcSnapshot) {
            let devices: Vec<SweepDevice> = self
                .devices
                .iter()
                .map(|d| SweepDevice {
                    pdev: d.pdev.clone(),
                    driver: d.driver.clone(),
                })
                .collect();
            let integrated: Vec<bool> = self.devices.iter().map(|d| d.integrated).collect();

            // The enc/dec split is the one thing the shared sweep can't carry:
            // amdgpu is the only driver naming the two ring classes apart, and
            // "never reported" has to stay distinguishable from 0%.
            let mut media: HashMap<usize, MediaUtil> = HashMap::new();
            // The walk's own timestamp, not the clock at attribution: these
            // counters were read when the walk ran, and dividing them by the
            // interval since some later moment is a different measurement.
            let now = snap.at;
            let engine_state = &mut self.engine_state;

            let mut sweep = linux::sweep_clients(snap, &devices, |pid, gpu, client| {
                let engine_ns = client.total_engine_ns();
                let video_ns = client.engine_ns_where(is_video_engine);
                let enc_ns = client.engine_ns_where(is_enc_engine);
                let dec_ns = client.engine_ns_where(is_dec_engine);
                // ns_delta_util turns one (counter, counter) pair into two
                // percentages; run it twice over the same interval to also get
                // the enc/dec split amdgpu names separately.
                let prev = engine_state.get(&(pid, client.id)).copied();
                let (util, vutil) = linux::ns_delta_util(
                    prev.map(|p| (p.total_ns, p.video_ns, p.at)).as_ref(),
                    engine_ns,
                    video_ns,
                    now,
                );
                let (enc_util, dec_util) = linux::ns_delta_util(
                    prev.map(|p| (p.enc_ns, p.dec_ns, p.at)).as_ref(),
                    enc_ns,
                    dec_ns,
                    now,
                );
                engine_state.insert(
                    (pid, client.id),
                    EngineSample {
                        total_ns: engine_ns,
                        video_ns,
                        enc_ns,
                        dec_ns,
                        at: now,
                    },
                );

                let m = media.entry(gpu).or_default();
                if client.engine_ns.keys().any(|k| is_enc_engine(k)) {
                    *m.enc.get_or_insert(0.0) += enc_util;
                }
                if client.engine_ns.keys().any(|k| is_dec_engine(k)) {
                    *m.dec.get_or_insert(0.0) += dec_util;
                }

                ClientSample {
                    util_pct: util,
                    video_pct: vutil,
                    mem_bytes: client_mem_bytes(client, integrated[gpu]),
                    graphics: client.engine_ns.get("gfx").copied().unwrap_or(0) > 0,
                }
            });

            // Forget clients that vanished so the map doesn't grow forever.
            self.engine_state.retain(|k, _| sweep.seen.contains(k));
            self.last_procs = std::mem::take(&mut sweep.procs);
            // The sweep already totals video util per device; a device with no
            // clients at all stays absent from both maps, which is what keeps
            // video_util_pct None rather than a fabricated 0%.
            for (gpu, video) in sweep.video_util {
                media.entry(gpu).or_default().video = video;
            }
            self.media = media;
        }
    }

    /// Graphics memory to attribute to one client. Discrete cards keep client
    /// allocations in `vram`; an APU has only a small stolen VRAM carve-out and
    /// puts the rest in `gtt`, so both must be summed there or the process rows
    /// contradict the gtt gauge on the device card above them.
    fn client_mem_bytes(c: &FdClient, integrated: bool) -> u64 {
        let vram = c.memory.get("vram").copied().unwrap_or(0);
        if integrated {
            vram.saturating_add(c.memory.get("gtt").copied().unwrap_or(0))
        } else {
            vram
        }
    }

    fn scan(drm: &str) -> Vec<AmdDevice> {
        cards_with_driver(drm, AMD_VENDOR, is_amd_driver)
            .into_iter()
            .map(|(idx, dev, driver)| {
                let name = card_name(&dev, idx, "1002", "AMD");
                let hwmon = first_dir(&dev.join("hwmon"));
                let pdev = pdev_of(&dev);
                let temp_crit_c = hwmon
                    .as_deref()
                    .and_then(|h| read_u64(&h.join("temp1_crit")))
                    .map(|v| v as f64 / 1000.0);
                // Map labelled temp channels (edge is temp1 by convention).
                let mut temp_junction_ch = None;
                let mut temp_mem_ch = None;
                if let Some(h) = hwmon.as_deref() {
                    for ch in 2u8..=4 {
                        match read_trim(&h.join(format!("temp{ch}_label"))).as_deref() {
                            Some("junction") => temp_junction_ch = Some(ch),
                            Some("mem") => temp_mem_ch = Some(ch),
                            _ => {}
                        }
                    }
                }
                let (pcie_max_gen, pcie_max_width) = linux::pcie_max_link(&dev);
                AmdDevice {
                    integrated: is_apu(&dev),
                    name,
                    dev,
                    hwmon,
                    pdev,
                    driver,
                    temp_crit_c,
                    temp_junction_ch,
                    temp_mem_ch,
                    pcie_max_gen,
                    pcie_max_width,
                }
            })
            .collect()
    }

    fn sample(d: &AmdDevice, media: Option<MediaUtil>, pcie_bw: Option<(u64, u64)>) -> GpuSnapshot {
        let h = d.hwmon.as_deref();
        let temperature_c = hwmon_u64(h, "temp1_input").map(|v| v as f64 / 1000.0);
        let power_w = hwmon_u64(h, "power1_average")
            .or_else(|| hwmon_u64(h, "power1_input"))
            .map(|v| v as f64 / 1e6);
        let power_limit_w = hwmon_u64(h, "power1_cap")
            .filter(|v| *v > 0)
            .or_else(|| hwmon_u64(h, "power1_cap_default").filter(|v| *v > 0))
            .map(|v| v as f64 / 1e6);

        // Heuristic (amdgpu's real throttle bits live in the versioned
        // gpu_metrics blob): flag when pinned at the power cap or within a
        // few degrees of the critical temperature.
        let mut throttle_parts: Vec<&str> = Vec::new();
        if let (Some(t), Some(crit)) = (temperature_c, d.temp_crit_c)
            && t >= crit - 3.0
        {
            throttle_parts.push("thermal");
        }
        if let (Some(w), Some(cap)) = (power_w, power_limit_w)
            && w >= cap * 0.99
        {
            throttle_parts.push("power-limit");
        }
        let throttle = crate::backend::join_throttle(&throttle_parts);
        let (pcie_gen, pcie_width) = linux::pcie_current_link(&d.dev);

        GpuSnapshot {
            name: d.name.clone(),
            device_id: linux::pci_device_id(d.pdev.as_deref()),
            integrated: d.integrated,
            // Absent on older ASICs and some APU configs; report the absence
            // rather than a 0% that reads as an idle GPU.
            utilization_pct: read_u64(&d.dev.join("gpu_busy_percent")).map(|v| clamp_pct(v as f64)),
            mem_util_pct: read_u64(&d.dev.join("mem_busy_percent")).map(|v| clamp_pct(v as f64)),
            // amdgpu names enc/dec rings separately, so report the split as
            // well as the total (which also carries the genuinely unified
            // engines: vcn and vpe).
            video_util_pct: media.map(|m| clamp_pct(m.video)),
            enc_util_pct: media.and_then(|m| m.enc).map(clamp_pct),
            dec_util_pct: media.and_then(|m| m.dec).map(clamp_pct),
            throttle,
            // Absent on some APUs, which have no VRAM carve-out to report.
            vram_used_bytes: read_u64(&d.dev.join("mem_info_vram_used")),
            vram_total_bytes: read_u64(&d.dev.join("mem_info_vram_total")),
            // temp1 = edge sensor (millidegrees); power in microwatts with
            // the APU fallback and cap-of-0 handling done above.
            temperature_c,
            temp_junction_c: d
                .temp_junction_ch
                .and_then(|ch| hwmon_u64(h, &format!("temp{ch}_input")))
                .map(|v| v as f64 / 1000.0),
            temp_mem_c: d
                .temp_mem_ch
                .and_then(|ch| hwmon_u64(h, &format!("temp{ch}_input")))
                .map(|v| v as f64 / 1000.0),
            power_w,
            power_limit_w,
            fan_pct: fan_pct(h),
            fan_rpm: hwmon_u64(h, "fan1_input"),
            clock_mhz: clock_mhz(h, "freq1_input", &d.dev.join("pp_dpm_sclk")),
            // APUs have no freq2_input; the active DPM level has it.
            mem_clock_mhz: clock_mhz(h, "freq2_input", &d.dev.join("pp_dpm_mclk")),
            pcie_gen,
            pcie_width,
            pcie_max_gen: d.pcie_max_gen,
            pcie_max_width: d.pcie_max_width,
            // `pcie_bw` only exists where the ASIC implements
            // asic_funcs->get_pcie_usage (Vega10/20, Navi 1x/2x); the kernel
            // marks it unsupported on APUs and it is unimplemented on RDNA3.
            // Absent file -> None, and the first sample has no delta yet.
            pcie_rx_kbs: pcie_bw.map(|(rx, _)| rx),
            pcie_tx_kbs: pcie_bw.map(|(_, tx)| tx),
            gtt_used_bytes: read_u64(&d.dev.join("mem_info_gtt_used")),
            gtt_total_bytes: read_u64(&d.dev.join("mem_info_gtt_total")),
            volt_mv: hwmon_u64(h, "in0_input"),
            perf_level: read_trim(&d.dev.join("power_dpm_force_performance_level"))
                .filter(|l| l != "auto"),
        }
    }

    /// gpu_metrics header byte 2 is the format revision: v1_x = discrete,
    /// v2_x/v3_x = APU. Missing file -> assume discrete.
    fn is_apu(dev: &Path) -> bool {
        fs::read(dev.join("gpu_metrics"))
            .ok()
            .and_then(|b| b.get(2).copied())
            .is_some_and(|rev| rev >= 2)
    }

    /// hwmon `freqN_input` (Hz) as MHz, falling back to the DPM table's active
    /// level. The file reads 0 while the clock domain is power-gated, and
    /// `Some(0).map(..)` would shadow the fallback, so the zero is filtered
    /// first. Note the fallback is not guaranteed to be better: on RDNA3 an
    /// idle `pp_dpm_sclk` carries a sleep row ("S: 0Mhz *") and also yields 0 —
    /// gated really is 0 MHz there. It wins on parts that report a stale or
    /// missing hwmon frequency while the DPM table has a live level.
    fn clock_mhz(hwmon: Option<&Path>, file: &str, dpm: &Path) -> Option<u64> {
        hwmon_u64(hwmon, file)
            .filter(|hz| *hz > 0)
            .map(|hz| hz / 1_000_000)
            .or_else(|| dpm_active_mhz(dpm))
    }

    /// `pcie_bw` is "<received count> <transmitted count> <max packet size>"
    /// (`amdgpu_get_pcie_bw`); the counts are packets, not bytes.
    fn parse_pcie_bw(s: &str) -> Option<(u64, u64, u64)> {
        let mut it = s.split_whitespace();
        let rx = it.next()?.parse().ok()?;
        let tx = it.next()?.parse().ok()?;
        let mps = it.next()?.parse().ok()?;
        Some((rx, tx, mps))
    }

    /// KiB/s from two `pcie_bw` samples: packet-count delta × max packet size ÷
    /// elapsed. None for a non-positive interval; a counter reset saturates to
    /// a 0 delta rather than wrapping.
    fn pcie_kbs(prev: (u64, u64), cur: (u64, u64), mps: u64, secs: f64) -> Option<(u64, u64)> {
        if secs <= 0.0 {
            return None;
        }
        let rate = |delta: u64| (delta as f64 * mps as f64 / secs / 1024.0) as u64;
        Some((
            rate(cur.0.saturating_sub(prev.0)),
            rate(cur.1.saturating_sub(prev.1)),
        ))
    }

    /// Sample `pcie_bw` and fold it into (rx, tx) KiB/s against `prev`, which is
    /// updated in place. None when the file is absent (see the note in
    /// `sample`) or on the first sample of a device.
    fn pcie_bw_kbs(
        dev: &Path,
        prev: &mut Option<(u64, u64, Instant)>,
        now: Instant,
    ) -> Option<(u64, u64)> {
        let (rx, tx, mps) = parse_pcie_bw(&read_trim(&dev.join("pcie_bw"))?)?;
        let kbs = prev.and_then(|(prx, ptx, at)| {
            pcie_kbs(
                (prx, ptx),
                (rx, tx),
                mps,
                now.duration_since(at).as_secs_f64(),
            )
        });
        *prev = Some((rx, tx, now));
        kbs
    }

    /// Parse the '*'-marked active level of a pp_dpm_{s,m}clk table:
    /// "1: 3000Mhz *" -> Some(3000).
    fn dpm_active_mhz(path: &Path) -> Option<u64> {
        let table = read_trim(path)?;
        let active = table.lines().find(|l| l.trim_end().ends_with('*'))?;
        let digits: String = active
            .split(':')
            .nth(1)?
            .trim()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
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
        use crate::backend::ProcKind;
        use crate::backend::linux::testing;
        use std::sync::Arc;
        use std::time::Duration;

        /// Exactly the AMD-driven cards, on a tree that also holds Intel and
        /// NVIDIA cards, an unbound 0x1002 device, and the render nodes and
        /// connectors whose `device/vendor` reads 0x1002 as well.
        #[test]
        fn scan_claims_amd_driven_cards_only() {
            let root = testing::tri_vendor("amd-scan");
            let devices = scan(&testing::drm(&root));
            assert_eq!(
                devices
                    .iter()
                    .map(|d| (d.pdev.as_deref(), d.driver.as_str()))
                    .collect::<Vec<_>>(),
                [
                    (Some("0000:03:00.0"), "amdgpu"),
                    // Pre-GCN, listed rather than dropped: hwmon still names a
                    // temperature and pci.ids still names the card.
                    (Some("0000:05:00.0"), "radeon"),
                ],
                "card6 is unbound, and the 0x10de/0x8086 cards are not ours"
            );
            // Ordered by card index (1 then 4), so the digit keys address the
            // same card on every poll and every restart. Names stay distinct
            // whether or not this host has a pci.ids to resolve them against.
            assert_ne!(devices[0].name, devices[1].name);
        }

        /// The max link is a fixed capability, so the scan resolves it once and
        /// the device carries it rather than re-reading sysfs per poll.
        #[test]
        fn scan_caches_the_max_pcie_link() {
            let root = testing::tri_vendor("amd-pcie-max");
            let pci = root.join("pci/0000:03:00.0");
            std::fs::write(pci.join("max_link_speed"), "16.0 GT/s PCIe\n").unwrap();
            std::fs::write(pci.join("max_link_width"), "16\n").unwrap();
            let devices = scan(&testing::drm(&root));
            assert_eq!(devices[0].pcie_max_gen, Some(4));
            assert_eq!(devices[0].pcie_max_width, Some(16));
            // Each card reads its own files: the radeon card has none.
            assert_eq!(devices[1].pcie_max_gen, None);
        }

        /// A radeon card must not be handed to the amdgpu fdinfo sweep: the
        /// driver a client names has to be the one the device is bound to.
        #[test]
        fn sweep_devices_carry_each_card_own_driver() {
            let root = testing::tri_vendor("amd-sweep");
            let devices = scan(&testing::drm(&root));
            let sweep: Vec<linux::SweepDevice> = devices
                .iter()
                .map(|d| linux::SweepDevice {
                    pdev: d.pdev.clone(),
                    driver: d.driver.clone(),
                })
                .collect();
            let client = linux::parse_fdinfo(
                "drm-driver:\tamdgpu\ndrm-client-id:\t1\ndrm-pdev:\t0000:05:00.0\n",
            )
            .unwrap();
            assert_eq!(
                linux::client_device(&sweep, &client),
                None,
                "the card at 05:00.0 runs radeon; an amdgpu client is not its"
            );
        }

        /// Fake sysfs root for one test: unique per run, removed on drop.
        fn fake_sysfs(name: &str) -> testing::Sandbox {
            testing::Sandbox::new(name)
        }

        /// One amdgpu client of the card at 03:00.0, with cumulative engine
        /// counters as fdinfo prints them.
        fn client(gfx_ns: u64, dec_ns: u64) -> linux::FdClient {
            linux::parse_fdinfo(&format!(
                "drm-driver:\tamdgpu\n\
                 drm-client-id:\t7\n\
                 drm-pdev:\t0000:03:00.0\n\
                 drm-engine-gfx:\t{gfx_ns} ns\n\
                 drm-engine-dec:\t{dec_ns} ns\n"
            ))
            .unwrap()
        }

        /// A backend over a fake `/sys/class/drm`, reading walks from a
        /// scanner the test publishes to by hand.
        fn backend_on(root: &Path, scanner: &Arc<linux::ProcScanner>) -> AmdBackend {
            let devices = scan(&testing::drm(root));
            AmdBackend {
                pcie_state: vec![None; devices.len()],
                devices,
                engine_state: HashMap::new(),
                cursor: linux::SweepCursor::on(Arc::clone(scanner)),
                media: HashMap::new(),
                last_procs: Vec::new(),
            }
        }

        /// The two rules that the walk moving off the poll thread turns on.
        ///
        /// A utilization is a cumulative counter's delta divided by the time
        /// between the two readings, and those readings now happen on another
        /// thread at times the poll does not choose. So the interval has to be
        /// the one between the two WALKS: measuring against the clock at
        /// attribution divides this walk's counters by an interval nothing was
        /// sampled over, and reports a card that idled through a slow tick as
        /// pegged.
        ///
        /// And a poll that finds no new walk has to redraw the last reading.
        /// Re-attributing the same walk would divide identical counters by a
        /// zero interval, so every card would drop to 0% whenever a walk ran
        /// late — a flicker exactly when the machine is busiest.
        #[test]
        fn utilization_spans_the_two_walks_and_survives_a_poll_without_one() {
            let root = testing::tri_vendor("amd-async");
            let scanner = linux::ProcScanner::detached();
            let mut b = backend_on(&root, &scanner);
            let card = 0; // 03:00.0, the amdgpu card; 05:00.0 is the radeon one

            // First walk: counters with nothing to subtract from yet.
            let t0 = Instant::now();
            scanner.publish(vec![(10, client(0, 0))], t0);
            let gpus = b.poll().unwrap();
            assert_eq!(
                b.processes()[0].gpu_util_pct,
                Some(0.0),
                "a first reading has no interval to divide by"
            );
            assert_eq!(gpus[card].video_util_pct, Some(0.0));

            // Second walk, a second later: 500 ms on gfx and 250 ms on the
            // decoder, so 75% busy over that second and 25% of it video.
            scanner.publish(
                vec![(10, client(500_000_000, 250_000_000))],
                t0 + Duration::from_secs(1),
            );
            let gpus = b.poll().unwrap();
            // (pid, card, util, kind) — the whole row, since a cached row that
            // kept its utilization while losing its pid would still be wrong.
            let row = |b: &mut AmdBackend| {
                let p = b.processes();
                assert_eq!(p.len(), 1, "one client, one row");
                (p[0].pid, p[0].gpu_index, p[0].gpu_util_pct, p[0].kind)
            };
            assert_eq!(row(&mut b), (10, card, Some(75.0), ProcKind::Graphics));
            assert_eq!(gpus[card].video_util_pct, Some(25.0));
            assert_eq!(gpus[card].dec_util_pct, Some(25.0));

            // A poll with no new walk: the same figures, not a fresh zero.
            let gpus = b.poll().unwrap();
            assert_eq!(row(&mut b), (10, card, Some(75.0), ProcKind::Graphics));
            assert_eq!(gpus[card].video_util_pct, Some(25.0));
            assert_eq!(gpus[card].dec_util_pct, Some(25.0));
        }

        #[test]
        fn power_gated_clock_falls_through_to_the_dpm_table() {
            let dir = fake_sysfs("clock");
            let dpm = dir.join("pp_dpm_sclk");
            std::fs::write(&dpm, "0: 500Mhz\n1: 1500Mhz *\n2: 2371Mhz\n").unwrap();

            // Live clock: hwmon wins, Hz -> MHz.
            std::fs::write(dir.join("freq1_input"), "3022000000\n").unwrap();
            assert_eq!(clock_mhz(Some(&dir), "freq1_input", &dpm), Some(3022));
            // Power-gated: hwmon reads 0, so the DPM level must be reachable.
            std::fs::write(dir.join("freq1_input"), "0\n").unwrap();
            assert_eq!(clock_mhz(Some(&dir), "freq1_input", &dpm), Some(1500));
            // Absent hwmon file (APU freq2_input): same fallback.
            assert_eq!(clock_mhz(Some(&dir), "freq2_input", &dpm), Some(1500));
            // RDNA3 at idle: the DPM table's active row is a 0 MHz sleep level,
            // so 0 is the honest answer, not a missing reading.
            std::fs::write(&dpm, "S: 0Mhz *\n").unwrap();
            assert_eq!(clock_mhz(Some(&dir), "freq1_input", &dpm), Some(0));
            // Neither source present.
            assert_eq!(clock_mhz(None, "freq1_input", &dir.join("absent")), None);
        }

        #[test]
        fn pcie_bw_line_parses() {
            assert_eq!(parse_pcie_bw("1234 5678 512\n"), Some((1234, 5678, 512)));
            assert_eq!(parse_pcie_bw("0 0 256"), Some((0, 0, 256)));
            assert_eq!(parse_pcie_bw("1234 5678"), None); // truncated
            assert_eq!(parse_pcie_bw(""), None);
            assert_eq!(parse_pcie_bw("a b c"), None);
        }

        #[test]
        fn pcie_bw_delta_is_packets_times_packet_size() {
            // 1024 packets × 512 B over 1 s = 512 KiB/s.
            assert_eq!(pcie_kbs((0, 0), (1024, 2048), 512, 1.0), Some((512, 1024)));
            // Half the interval, double the rate.
            assert_eq!(pcie_kbs((0, 0), (1024, 0), 512, 0.5), Some((1024, 0)));
            // Counter reset must saturate to 0, not wrap.
            assert_eq!(pcie_kbs((5000, 5000), (10, 10), 512, 1.0), Some((0, 0)));
            // Degenerate interval yields nothing rather than a divide-by-zero.
            assert_eq!(pcie_kbs((0, 0), (10, 10), 512, 0.0), None);
        }

        #[test]
        fn pcie_bw_absent_file_stays_none() {
            let dir = fake_sysfs("pciebw");
            let t0 = Instant::now();
            let t1 = t0 + std::time::Duration::from_secs(1);
            let mut prev = None;

            // Absent on APUs and RDNA3: no reading, and no state to carry.
            assert_eq!(pcie_bw_kbs(&dir, &mut prev, t0), None);
            assert!(prev.is_none());

            std::fs::write(dir.join("pcie_bw"), "100 200 512\n").unwrap();
            assert_eq!(pcie_bw_kbs(&dir, &mut prev, t0), None); // first sample
            assert!(prev.is_some());
            std::fs::write(dir.join("pcie_bw"), "1124 2200 512\n").unwrap();
            assert_eq!(pcie_bw_kbs(&dir, &mut prev, t1), Some((512, 1000)));
        }

        const APU_FDINFO: &str = "\
drm-driver:\tamdgpu
drm-client-id:\t56692
drm-pdev:\t0000:75:00.0
drm-memory-vram:\t60060 KiB
drm-memory-gtt: \t21664 KiB
drm-engine-compute:\t1701383665 ns
drm-engine-enc:\t9770559248 ns
";

        #[test]
        fn apu_client_memory_includes_gtt() {
            let c = linux::parse_fdinfo(APU_FDINFO).unwrap();
            // Discrete: the vram carve-out is the whole story.
            assert_eq!(client_mem_bytes(&c, false), 60060 << 10);
            // APU: most of the allocation lives in gtt and must be counted.
            assert_eq!(client_mem_bytes(&c, true), (60060 + 21664) << 10);
        }

        #[test]
        fn media_engines_split_into_enc_and_dec() {
            assert!(is_enc_engine("enc") && !is_dec_engine("enc"));
            assert!(is_dec_engine("dec") && !is_enc_engine("dec"));
            assert!(is_dec_engine("jpeg")); // JPEG block is a decoder
            // Genuinely unified engines belong to neither half but still count
            // towards the video total.
            for unified in ["vcn", "vpe"] {
                assert!(is_video_engine(unified));
                assert!(!is_enc_engine(unified) && !is_dec_engine(unified));
            }
            for name in ["enc", "dec", "jpeg"] {
                assert!(is_video_engine(name));
            }
            for other in ["gfx", "compute", "dma"] {
                assert!(!is_video_engine(other));
            }
        }

        #[test]
        fn dpm_table_active_level_parses() {
            let dir = fake_sysfs("dpm");
            let f = dir.join("pp_dpm_mclk");
            std::fs::write(&f, "0: 96Mhz\n1: 3000Mhz *\n2: 1249Mhz\n").unwrap();
            assert_eq!(dpm_active_mhz(&f), Some(3000));
            std::fs::write(&f, "S: 0Mhz *\n").unwrap();
            assert_eq!(dpm_active_mhz(&f), Some(0));
        }
    }
    /// Tests that read this machine's own AMD hardware. Everything above runs
    /// on fabricated sysfs trees and canned fdinfo, which cannot catch what
    /// only live hardware shows: a sysfs attribute amdgpu stopped publishing,
    /// a hwmon channel that moved, or an invariant that holds on a fixture and
    /// breaks against a real `/proc` sweep.
    ///
    /// So these read `/sys/class/drm` and `/proc` as they are, and skip
    /// themselves where `backend` finds no amdgpu/radeon card — which is every
    /// CI runner this project has. The two hardware classes skip independently
    /// of each other, because a box with an APU and no discrete card can still
    /// prove every APU rule, and a box with only a discrete card every
    /// discrete one.
    ///
    /// Three env gates turn a skip into a failure on a runner that is supposed
    /// to have the hardware, so the day the card or the driver disappears from
    /// it is not the day the suite quietly stops testing this backend:
    /// `GPUR_REQUIRE_AMD` for the backend at all, `GPUR_REQUIRE_AMD_APU` and
    /// `GPUR_REQUIRE_AMD_DGPU` for the class-specific halves.
    ///
    /// Nothing here writes sysfs, signals a process, or depends on the machine
    /// being idle or busy. The attribution tests do open each card's render
    /// node read-only, which is what gives the sweep a DRM client to find —
    /// see `open_render_node`.
    #[cfg(test)]
    mod hardware {
        use super::*;
        use crate::backend::ProcKind;
        use std::collections::HashSet;
        use std::sync::{Mutex, MutexGuard};
        use std::time::Duration;

        /// The backend as a whole.
        const REQUIRE: &str = "GPUR_REQUIRE_AMD";
        /// The integrated half: an APU's carve-out-beside-GTT memory rules.
        const REQUIRE_APU: &str = "GPUR_REQUIRE_AMD_APU";
        /// The discrete half: real VRAM, a negotiated PCIe link.
        const REQUIRE_DGPU: &str = "GPUR_REQUIRE_AMD_DGPU";

        /// Whichever gate is set, if any. Asking for a class implies asking
        /// for the backend: `GPUR_REQUIRE_AMD_DGPU` on a machine whose amdgpu
        /// vanished entirely must fail rather than skip its way past the
        /// backend check first.
        fn forced() -> Option<&'static str> {
            [REQUIRE, REQUIRE_APU, REQUIRE_DGPU]
                .into_iter()
                .find(|v| std::env::var_os(v).is_some())
        }

        /// This machine's AMD backend, or `None` with a note saying why the
        /// caller is about to do nothing.
        fn amd() -> Option<AmdBackend> {
            // These tests open DRM clients and then poll, so they are
            // asserting on a walk that has to happen after the open — not on
            // whichever one the worker thread last finished.
            linux::ProcScanner::shared().set_synchronous(true);
            if let Some(b) = backend() {
                return Some(b);
            }
            if let Some(var) = forced() {
                panic!(
                    "{var} is set, but no amdgpu/radeon card was found in \
                     /sys/class/drm — this machine cannot test the AMD backend"
                );
            }
            eprintln!("skipping: no AMD GPU on this machine");
            None
        }

        /// Indices of this machine's cards of one class. Empty with a note
        /// where it has none, unless that class's gate is set.
        fn class(b: &AmdBackend, integrated: bool) -> Vec<usize> {
            let idx: Vec<usize> = b
                .devices
                .iter()
                .enumerate()
                .filter(|(_, d)| d.integrated == integrated)
                .map(|(i, _)| i)
                .collect();
            let (var, what) = if integrated {
                (REQUIRE_APU, "AMD APU")
            } else {
                (REQUIRE_DGPU, "discrete AMD card")
            };
            if idx.is_empty() {
                assert!(
                    std::env::var_os(var).is_none(),
                    "{var} is set, but this machine has no {what}"
                );
                eprintln!("skipping: no {what} on this machine");
            }
            idx
        }

        /// Enough of a gap for the counter deltas to have something to divide.
        const GAP: Duration = Duration::from_millis(100);

        /// The sweep's view of this machine's cards, for tests that walk
        /// `/proc` themselves rather than through `poll`.
        fn sweep_devices(b: &AmdBackend) -> Vec<SweepDevice> {
            b.devices
                .iter()
                .map(|d| SweepDevice {
                    pdev: d.pdev.clone(),
                    driver: d.driver.clone(),
                })
                .collect()
        }

        /// Open one card's render node read-only, so the sweep has a client of
        /// that card to attribute. Without it these tests only run where a
        /// compositor happens to be up: a headless box owns no DRM client at
        /// all, and a rule about how a client's memory is charged that is only
        /// ever exercised by someone's running desktop is one nothing checks.
        ///
        /// A bare open allocates a few KiB of VRAM and a couple of MiB of GTT,
        /// and fdinfo reports the two separately — which is exactly the
        /// asymmetry the per-class charging rule turns on. `None` where the
        /// card has no render node (a `radeon` card without one) or where this
        /// user cannot open it, both of which are notes rather than failures:
        /// they are facts about the machine, not about this backend.
        fn open_render_node(d: &AmdDevice) -> Option<fs::File> {
            let target = fs::canonicalize(&d.dev).ok()?;
            let node = fs::read_dir("/sys/class/drm")
                .ok()?
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("renderD"))
                .find(|n| {
                    fs::canonicalize(format!("/sys/class/drm/{n}/device"))
                        .ok()
                        .as_ref()
                        == Some(&target)
                })?;
            match fs::File::open(format!("/dev/dri/{node}")) {
                Ok(f) => Some(f),
                Err(e) => {
                    eprintln!("note: cannot open /dev/dri/{node} for {}: {e}", d.name);
                    None
                }
            }
        }

        /// The threaded path, against a real card.
        ///
        /// Every other hardware test pins the shared scanner to synchronous so
        /// that a client it opened is in the very next walk — which means none
        /// of them exercises the worker, and the worker is what ships. This one
        /// gives the backend a scanner wired the production way and asserts the
        /// rows arrive anyway: not on the first poll necessarily, since the
        /// walk is off this thread, but within a bounded number of them.
        #[test]
        fn the_worker_thread_feeds_the_backend_on_real_hardware() {
            let Some(mut b) = amd() else { return };
            b.cursor = linux::SweepCursor::on(linux::ProcScanner::detached_with_worker());
            let all: Vec<usize> = (0..b.devices.len()).collect();
            let held = hold_clients(&b, &all);
            if held.opened.is_empty() {
                eprintln!("note: no render node could be opened on this machine");
                return;
            }

            let me = std::process::id();
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut polls = 0;
            let mut mine: Vec<usize> = Vec::new();
            while Instant::now() < deadline {
                b.poll().unwrap();
                polls += 1;
                mine = b
                    .processes()
                    .iter()
                    .filter(|p| p.pid == me)
                    .map(|p| p.gpu_index)
                    .collect();
                mine.sort_unstable();
                if mine == held.opened {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }

            assert_eq!(
                mine, held.opened,
                "after {polls} polls the worker had not delivered a walk \
                 containing this test's own clients"
            );
        }

        /// Serializes the tests that open render nodes against each other.
        /// They run as threads of one test process, so a client another test
        /// opened is a client of *this* test's pid: it lands in the same
        /// process row and moves the very figures being checked.
        static RENDER_NODES: Mutex<()> = Mutex::new(());

        /// A render-node client of every card in `which`, held for as long as
        /// this value lives, with the other render-node tests locked out.
        struct Held {
            _guard: MutexGuard<'static, ()>,
            _files: Vec<fs::File>,
            /// Device indices a client was actually opened for — what the
            /// callers are entitled to assert on.
            opened: Vec<usize>,
        }

        fn hold_clients(b: &AmdBackend, which: &[usize]) -> Held {
            // A test that panicked while holding the lock poisoned it; its
            // fds are closed all the same, so there is nothing to recover.
            let guard = RENDER_NODES.lock().unwrap_or_else(|e| e.into_inner());
            let mut files = Vec::new();
            let mut opened = Vec::new();
            for &i in which {
                if let Some(f) = open_render_node(&b.devices[i]) {
                    files.push(f);
                    opened.push(i);
                }
            }
            Held {
                _guard: guard,
                _files: files,
                opened,
            }
        }

        /// (pid, card) -> that process's (vram, gtt) fdinfo bytes on that
        /// card, from a `/proc` walk of this machine. Deduplicated on (pid,
        /// client id) the way the sweep is, so a client held on several fds
        /// counts once.
        ///
        /// The two regions are kept apart rather than summed by
        /// `client_mem_bytes`, because which of them a row is charged is the
        /// thing under test: a reference computed by the function being
        /// checked agrees with it however wrong the rule is.
        fn walk_client_mem(devices: &[SweepDevice]) -> HashMap<(u32, usize), (u64, u64)> {
            let mut out: HashMap<(u32, usize), (u64, u64)> = HashMap::new();
            let mut seen: HashSet<(u32, u64)> = HashSet::new();
            for pid in linux::proc_pids() {
                for c in linux::drm_clients(pid) {
                    let Some(gpu) = linux::client_device(devices, &c) else {
                        continue;
                    };
                    if !seen.insert((pid, c.id)) {
                        continue;
                    }
                    let e = out.entry((pid, gpu)).or_default();
                    e.0 += c.memory.get("vram").copied().unwrap_or(0);
                    e.1 += c.memory.get("gtt").copied().unwrap_or(0);
                }
            }
            out
        }

        #[test]
        fn the_scan_claims_this_machines_amd_cards() {
            let Some(b) = amd() else { return };
            for d in &b.devices {
                assert!(
                    !d.name.is_empty(),
                    "a nameless card renders as a blank header row"
                );
                assert!(
                    is_amd_driver(&d.driver),
                    "{} claimed on driver {:?}",
                    d.name,
                    d.driver
                );
                // The BDF is what fdinfo's `drm-pdev` is matched against, so a
                // card without one can never have a client attributed to it.
                assert!(d.pdev.is_some(), "{} has no PCI address", d.name);
                assert_eq!(
                    read_trim(&d.dev.join("vendor")).as_deref(),
                    Some(AMD_VENDOR),
                    "{} is not an AMD device",
                    d.name
                );
                // Every gauge is read from the PCI device dir rather than the
                // DRM minor, which is where amdgpu publishes them.
                assert!(
                    d.dev.join("uevent").exists(),
                    "{:?} is not a PCI device dir",
                    d.dev
                );
                assert_eq!(
                    d.integrated,
                    is_apu(&d.dev),
                    "{}: the class the memory rules key on is not a stable read",
                    d.name
                );
            }
        }

        #[test]
        fn a_live_poll_reports_only_values_this_hardware_can_produce() {
            let Some(mut b) = amd() else { return };
            let gpus = b.poll().unwrap();
            assert_eq!(gpus.len(), b.devices.len(), "one snapshot per card");

            for g in &gpus {
                for (what, pct) in [
                    ("utilization", g.utilization_pct),
                    ("memory", g.mem_util_pct),
                    ("video", g.video_util_pct),
                    ("encode", g.enc_util_pct),
                    ("decode", g.dec_util_pct),
                    ("fan", g.fan_pct),
                ] {
                    if let Some(v) = pct {
                        assert!((0.0..=100.0).contains(&v), "{} {what} {v}%", g.name);
                    }
                }
                for (what, t) in [
                    ("edge", g.temperature_c),
                    ("junction", g.temp_junction_c),
                    ("memory", g.temp_mem_c),
                ] {
                    if let Some(t) = t {
                        assert!((-50.0..=150.0).contains(&t), "{} {what} at {t}°C", g.name);
                    }
                }
                for (what, w) in [("power", g.power_w), ("power limit", g.power_limit_w)] {
                    if let Some(w) = w {
                        assert!((0.0..=1000.0).contains(&w), "{} {what} {w} W", g.name);
                    }
                }
                for (what, mhz) in [("core", g.clock_mhz), ("memory", g.mem_clock_mhz)] {
                    if let Some(mhz) = mhz {
                        assert!(mhz <= 5000, "{} {what} clock at {mhz} MHz", g.name);
                    }
                }
                if let Some(rpm) = g.fan_rpm {
                    assert!(rpm <= 100_000, "{} fan at {rpm} rpm", g.name);
                }
                if let Some(mv) = g.volt_mv {
                    assert!(mv <= 10_000, "{} at {mv} mV", g.name);
                }
                assert!(
                    g.device_id
                        .as_deref()
                        .is_some_and(|id| id.starts_with("pci:")),
                    "{} has no PCI-derived identity: {:?}",
                    g.name,
                    g.device_id
                );
                if let (Some(used), Some(total)) = (g.vram_used_bytes, g.vram_total_bytes) {
                    assert!(total > 0, "{}: a published total of 0 is not one", g.name);
                    assert!(
                        used <= total,
                        "{}: {used} B resident in a {total} B pool",
                        g.name
                    );
                }
                if let (Some(used), Some(total)) = (g.gtt_used_bytes, g.gtt_total_bytes) {
                    assert!(used <= total, "{}: {used} B of {total} B of GTT", g.name);
                }
                // GTT is host RAM the card maps, so system RAM is its ceiling.
                if let (Some(gtt), Some(ram)) = (g.gtt_total_bytes, linux::sys_mem_total_bytes()) {
                    assert!(gtt <= ram, "{}: {gtt} B of GTT over {ram} B of RAM", g.name);
                }
            }
        }

        /// `App` keys graph history, session peaks and folding on the device
        /// id, and holds a card's row at its index. Both have to name the same
        /// card on every poll of the same machine.
        #[test]
        fn device_identity_and_order_survive_repeated_polls() {
            let Some(mut b) = amd() else { return };
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

        /// The sysfs gauges, checked against the files this driver actually
        /// publishes. Both directions matter: a gauge missing where the file
        /// exists is telemetry silently dropped, and a gauge present where no
        /// file is is a number invented from nothing — `mem_busy_percent` is
        /// absent on APUs and on older ASICs, and a fabricated 0% there reads
        /// as an idle memory controller rather than an unmeasured one.
        #[test]
        fn the_sysfs_gauges_are_read_where_this_driver_publishes_them() {
            let Some(mut b) = amd() else { return };
            let gpus = b.poll().unwrap();
            for (i, g) in gpus.iter().enumerate() {
                let dev = &b.devices[i].dev;
                for (what, got, file) in [
                    (
                        "utilization",
                        g.utilization_pct.is_some(),
                        "gpu_busy_percent",
                    ),
                    (
                        "memory utilization",
                        g.mem_util_pct.is_some(),
                        "mem_busy_percent",
                    ),
                    (
                        "vram usage",
                        g.vram_used_bytes.is_some(),
                        "mem_info_vram_used",
                    ),
                    ("gtt usage", g.gtt_used_bytes.is_some(), "mem_info_gtt_used"),
                ] {
                    assert_eq!(
                        got,
                        read_u64(&dev.join(file)).is_some(),
                        "{}: {what} disagrees with {file}",
                        g.name
                    );
                }
                // The pool sizes are fixed for the life of the boot, so these
                // compare by value rather than by presence.
                for (what, got, file) in [
                    ("vram total", g.vram_total_bytes, "mem_info_vram_total"),
                    ("gtt total", g.gtt_total_bytes, "mem_info_gtt_total"),
                ] {
                    assert_eq!(
                        got,
                        read_u64(&dev.join(file)),
                        "{}: {what} is not a stable read of {file}",
                        g.name
                    );
                }
                // "auto" is the default and says nothing, so it is reported as
                // no forced level rather than as one.
                let forced = read_trim(&dev.join("power_dpm_force_performance_level"))
                    .filter(|l| l != "auto");
                assert_eq!(g.perf_level, forced, "{}", g.name);
            }
        }

        /// The hwmon gauges, checked against the hwmon dir this card actually
        /// has. An APU registers a much thinner hwmon than a discrete card —
        /// no fan, no power cap, no critical temperature — which is why the UI
        /// has to render those as absent rather than as zero.
        #[test]
        fn the_hwmon_gauges_are_read_where_hwmon_exists_and_stay_none_where_it_does_not() {
            let Some(mut b) = amd() else { return };
            let hwmon: Vec<Option<PathBuf>> = b.devices.iter().map(|d| d.hwmon.clone()).collect();
            let gpus = b.poll().unwrap();

            for (i, g) in gpus.iter().enumerate() {
                let h = hwmon[i].as_deref();
                assert_eq!(
                    g.temperature_c.is_some(),
                    hwmon_u64(h, "temp1_input").is_some(),
                    "{}: temperature disagrees with hwmon temp1_input",
                    g.name
                );
                // amdgpu publishes an average on discrete parts and an
                // instantaneous input on APUs; either one is a reading.
                assert_eq!(
                    g.power_w.is_some(),
                    hwmon_u64(h, "power1_average").is_some()
                        || hwmon_u64(h, "power1_input").is_some(),
                    "{}: power disagrees with the hwmon counters on disk",
                    g.name
                );
                // A cap of 0 is the card saying it has none, not a card
                // limited to zero watts.
                assert_eq!(
                    g.power_limit_w.is_some(),
                    hwmon_u64(h, "power1_cap").is_some_and(|v| v > 0)
                        || hwmon_u64(h, "power1_cap_default").is_some_and(|v| v > 0),
                    "{}: power limit disagrees with the hwmon caps on disk",
                    g.name
                );
                assert_eq!(
                    g.fan_rpm.is_some(),
                    hwmon_u64(h, "fan1_input").is_some(),
                    "{}: fan rpm disagrees with hwmon fan1_input",
                    g.name
                );
                assert_eq!(
                    g.fan_pct.is_some(),
                    hwmon_u64(h, "pwm1").is_some(),
                    "{}: fan duty disagrees with hwmon pwm1",
                    g.name
                );
                // Presence, not value: the core voltage moves between the poll
                // and this read on a card that is doing anything at all.
                assert_eq!(
                    g.volt_mv.is_some(),
                    hwmon_u64(h, "in0_input").is_some(),
                    "{}: voltage disagrees with hwmon in0_input",
                    g.name
                );
                // The junction and memory sensors are found by label, not by
                // channel number: the channels differ across ASICs, and
                // reading temp2 blind reports one sensor's value as another's.
                for (label, got, ch) in [
                    ("junction", g.temp_junction_c, b.devices[i].temp_junction_ch),
                    ("mem", g.temp_mem_c, b.devices[i].temp_mem_ch),
                ] {
                    let labelled = (2u8..=4).find(|c| {
                        h.and_then(|h| read_trim(&h.join(format!("temp{c}_label"))))
                            .as_deref()
                            == Some(label)
                    });
                    assert_eq!(ch, labelled, "{}: {label} channel", g.name);
                    assert_eq!(
                        got.is_some(),
                        labelled.is_some_and(|c| hwmon_u64(h, &format!("temp{c}_input")).is_some()),
                        "{}: {label} temperature disagrees with its labelled channel",
                        g.name
                    );
                }
                if h.is_none() {
                    assert_eq!(
                        (g.temperature_c, g.power_w, g.power_limit_w, g.fan_rpm),
                        (None, None, None, None),
                        "{}: no hwmon dir, so none of these are known",
                        g.name
                    );
                    eprintln!("note: {} registers no hwmon", g.name);
                }
            }
        }

        /// The clock chain: hwmon first, the DPM table where hwmon reads 0
        /// because the domain is power-gated. A card idling with a gated clock
        /// is the common case, so the fallback is exercised on nearly every
        /// run — and a `Some(0)` from hwmon shadowing it would report every
        /// idle card as stuck at 0 MHz.
        ///
        /// Both sources move under a card that is doing work, so the poll is
        /// bracketed by two reads of each and the value is only compared where
        /// the two agree. Which source was used is still checked every run.
        #[test]
        fn the_clocks_fall_back_to_the_dpm_table_where_hwmon_reads_gated() {
            let Some(mut b) = amd() else { return };
            let sources = |b: &AmdBackend| -> Vec<[(Option<u64>, Option<u64>); 2]> {
                b.devices
                    .iter()
                    .map(|d| {
                        let h = d.hwmon.as_deref();
                        [
                            (
                                hwmon_u64(h, "freq1_input"),
                                dpm_active_mhz(&d.dev.join("pp_dpm_sclk")),
                            ),
                            (
                                hwmon_u64(h, "freq2_input"),
                                dpm_active_mhz(&d.dev.join("pp_dpm_mclk")),
                            ),
                        ]
                    })
                    .collect()
            };
            let before = sources(&b);
            let gpus = b.poll().unwrap();
            let after = sources(&b);

            for (i, g) in gpus.iter().enumerate() {
                for (k, (what, got, file, table)) in [
                    ("core", g.clock_mhz, "freq1_input", "pp_dpm_sclk"),
                    ("memory", g.mem_clock_mhz, "freq2_input", "pp_dpm_mclk"),
                ]
                .into_iter()
                .enumerate()
                {
                    let (hz, dpm) = before[i][k];
                    assert_eq!(
                        got.is_some(),
                        hz.is_some_and(|hz| hz > 0) || dpm.is_some(),
                        "{}: {what} clock disagrees with {file} and {table}",
                        g.name
                    );
                    if hz == Some(0) && dpm.is_some() && after[i][k] == before[i][k] {
                        assert_eq!(
                            got, dpm,
                            "{}: {file} is power-gated, so the {table} level is the reading",
                            g.name
                        );
                    }
                }
            }
        }

        /// PCIe comes from the PCI core's own attributes, identical for every
        /// vendor's endpoint.
        #[test]
        fn the_pcie_link_is_the_pci_core_attributes_verbatim() {
            let Some(mut b) = amd() else { return };
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

        /// `pcie_bw` exists only where the ASIC implements `get_pcie_usage` —
        /// not on APUs, not on RDNA3 — and it is a packet counter, so the
        /// first poll of a card that has one still reports nothing. Both
        /// halves have to hold, or the UI draws a bandwidth row for a card
        /// that measures none.
        #[test]
        fn pcie_bandwidth_is_reported_only_where_this_asic_counts_it() {
            let Some(mut b) = amd() else { return };
            let has_counter: Vec<bool> = b
                .devices
                .iter()
                .map(|d| d.dev.join("pcie_bw").exists())
                .collect();

            let first = b.poll().unwrap();
            std::thread::sleep(GAP);
            let second = b.poll().unwrap();

            for (i, g) in first.iter().enumerate() {
                assert_eq!(
                    (g.pcie_rx_kbs, g.pcie_tx_kbs),
                    (None, None),
                    "{}: nothing to take a packet-count delta against yet",
                    g.name
                );
                assert_eq!(
                    (
                        second[i].pcie_rx_kbs.is_some(),
                        second[i].pcie_tx_kbs.is_some()
                    ),
                    (has_counter[i], has_counter[i]),
                    "{}: bandwidth disagrees with the pcie_bw file on disk",
                    g.name
                );
                if !has_counter[i] {
                    eprintln!("note: {} implements no pcie_bw counter", g.name);
                }
            }
        }

        /// The header should name the card, not repeat its PCI id. The lookup
        /// is checked against a second, independent read of the pci.ids
        /// database rather than against the same helper's output.
        #[test]
        fn the_card_name_is_this_devices_pci_ids_entry() {
            let Some(b) = amd() else { return };
            let Ok(ids) = fs::read_to_string("/usr/share/hwdata/pci.ids") else {
                eprintln!("skipping: no pci.ids database installed");
                return;
            };
            for d in &b.devices {
                let device_id = fs::read_to_string(d.dev.join("device")).unwrap();
                let device_id = device_id.trim().trim_start_matches("0x");
                if let Some(marketing) = linux::pci_device_name(&ids, "1002", device_id) {
                    assert_eq!(
                        d.name, marketing,
                        "{device_id} is listed in pci.ids but rendered as a fallback"
                    );
                }
            }
        }

        /// The header names the drivers actually in use, because a pre-GCN
        /// card on `radeon` beside a modern one publishes almost none of the
        /// same telemetry, and a line naming one of them attributes to that
        /// driver the gauges the other card's driver is the reason for lacking.
        #[test]
        fn the_driver_line_names_every_driver_this_machine_runs() {
            let Some(b) = amd() else { return };
            let line = b
                .driver_info()
                .expect("the kernel release is readable on Linux");
            for d in &b.devices {
                assert!(line.contains(&d.driver), "{line:?} omits {}", d.driver);
            }
            assert!(line.contains("kernel"), "{line:?}");
        }

        /// The process table is built by the same fdinfo sweep that fills the
        /// media gauges, so a row in it is proof the sweep read that device's
        /// clients — which is exactly the condition under which its video
        /// figure is a measurement rather than `None`. The core gauge is
        /// different here, and deliberately: amdgpu publishes
        /// `gpu_busy_percent` itself, so it is readable with no clients at all.
        #[test]
        fn processes_come_from_the_poll_sweep_and_name_a_device_it_read() {
            let Some(mut b) = amd() else { return };
            assert!(
                b.processes().is_empty(),
                "processes() serves the last poll's sweep, it does not scan"
            );
            let all: Vec<usize> = (0..b.devices.len()).collect();
            let held = hold_clients(&b, &all);

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
            }
            for (i, g) in gpus.iter().enumerate() {
                assert_eq!(
                    g.video_util_pct.is_some(),
                    procs.iter().any(|p| p.gpu_index == i),
                    "{}: video utilization disagrees with what the sweep attributed",
                    g.name
                );
                // This test holds a client of its own on every card whose
                // render node it could open, so those cards have to have been
                // swept whatever else is running on the machine.
                if held.opened.contains(&i) {
                    assert!(
                        procs.iter().any(|p| p.gpu_index == i),
                        "{}: this test holds a DRM client of it that no row names",
                        g.name
                    );
                }
            }
            if held.opened.is_empty() {
                eprintln!("note: no render node could be opened on this machine");
            }
        }

        /// The enc/dec split is accumulated by a second pass over the same
        /// interval as the video total, so the two can drift apart into a card
        /// encoding harder than it is doing video at all. A ring class stays
        /// `None` until some client reports it: amdgpu prints only the engines
        /// a client used, so absence means this device has no separate
        /// encode/decode activity, not 0%.
        #[test]
        fn the_media_split_never_exceeds_the_video_total_it_came_from() {
            let Some(mut b) = amd() else { return };
            b.poll().unwrap();
            std::thread::sleep(GAP);
            let gpus = b.poll().unwrap();
            for g in &gpus {
                for (what, part) in [("encode", g.enc_util_pct), ("decode", g.dec_util_pct)] {
                    let Some(part) = part else { continue };
                    let video = g.video_util_pct.unwrap_or_else(|| {
                        panic!(
                            "{}: {what} reported for a device with no video total",
                            g.name
                        )
                    });
                    assert!(
                        part <= video + 1e-6,
                        "{}: {what} {part}% of {video}% of video",
                        g.name
                    );
                }
            }
        }

        /// The counter-delta map is keyed on (pid, drm-client-id) and pruned
        /// against what each sweep saw. A leak here is unbounded: a gpur left
        /// running on a busy box would keep an entry for every short-lived
        /// client forever.
        #[test]
        fn per_client_delta_state_is_pruned_to_clients_that_still_exist() {
            let Some(mut b) = amd() else { return };
            b.poll().unwrap();
            std::thread::sleep(GAP);
            // Bracket the sweep: a pid it charges must have been alive during
            // it, so it shows up in one of these two readings unless it both
            // appeared and exited inside the sweep — which a process holding a
            // DRM client does not do.
            let before: HashSet<u32> = linux::proc_pids().into_iter().collect();
            b.poll().unwrap();
            let after: HashSet<u32> = linux::proc_pids().into_iter().collect();

            for (pid, client) in b.engine_state.keys() {
                assert!(
                    before.contains(pid) || after.contains(pid),
                    "counter state kept for client {client} of dead pid {pid}"
                );
            }
            assert_eq!(
                b.pcie_state.len(),
                b.devices.len(),
                "pcie state is per card, not per poll"
            );
        }

        /// Attribution end to end: every DRM client this user owns has to
        /// reach a row, filed under the right card and the right kind. Found
        /// by walking `/proc` independently of the sweep, then intersected
        /// with a second walk so a client that opened or closed around the
        /// poll cannot fail the test.
        #[test]
        fn every_drm_client_this_user_owns_reaches_a_process_row() {
            let Some(mut b) = amd() else { return };
            let all: Vec<usize> = (0..b.devices.len()).collect();
            let held = hold_clients(&b, &all);
            let devices = sweep_devices(&b);
            // (pid, card) -> touched the gfx ring.
            let walk = || {
                let mut out: HashMap<(u32, usize), bool> = HashMap::new();
                for pid in linux::proc_pids() {
                    for c in linux::drm_clients(pid) {
                        if let Some(gpu) = linux::client_device(&devices, &c) {
                            let graphics = c.engine_ns.get("gfx").copied().unwrap_or(0) > 0;
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

            // The clients this test opened are its own, so they are readable,
            // they did not come or go around the poll, and they submitted no
            // work: every one of them must reach a row, filed as compute.
            for i in &held.opened {
                let key = (std::process::id(), *i);
                assert_eq!(
                    rows.get(&key),
                    Some(&ProcKind::Compute),
                    "this test's own client of card {i} reached no compute row"
                );
                assert!(
                    before.contains_key(&key),
                    "the independent walk missed it too"
                );
            }
            if held.opened.is_empty() {
                eprintln!("note: no render node could be opened on this machine");
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
                        "pid {pid} runs on the gfx ring but is filed as compute"
                    );
                }
            }
        }

        /// What the MEM meter ends up showing on this machine. Both AMD
        /// classes publish two pools, so the meter takes the card's own and
        /// hangs the system one beside it — and only the APU's second pool is
        /// `shared`, because on a discrete card those bytes are host RAM the
        /// working set spilled into across PCIe, a different signal wearing
        /// the same units.
        #[test]
        fn the_memory_meters_name_the_pools_this_card_publishes() {
            let Some(mut b) = amd() else { return };
            let gpus = b.poll().unwrap();
            for g in &gpus {
                let m = g.mem_primary();
                assert_eq!(m.used, g.vram_used_bytes, "{}", g.name);
                assert_eq!(m.total, g.vram_total_bytes, "{}", g.name);
                assert!(
                    !m.shared,
                    "{}: the carve-out is accounted apart from the system pool",
                    g.name
                );
                let second = g
                    .mem_secondary()
                    .unwrap_or_else(|| panic!("{}: GTT is a pool of its own", g.name));
                assert_eq!(second.used, g.gtt_used_bytes, "{}", g.name);
                assert_eq!(second.total, g.gtt_total_bytes, "{}", g.name);
                assert_eq!(
                    second.shared, g.integrated,
                    "{}: an APU shares its GTT, a discrete card spills into it",
                    g.name
                );
                assert!(
                    g.mem_pct().is_some(),
                    "{}: the memory graph would stay blank",
                    g.name
                );
            }
        }

        /// The throttle label is a heuristic over two hwmon readings, so it
        /// can only ever name a reason this card publishes the inputs for. A
        /// card with no critical temperature reading "thermal" would be
        /// reporting a limit nothing measured.
        #[test]
        fn the_throttle_label_only_names_reasons_this_card_measures() {
            let Some(mut b) = amd() else { return };
            let gpus = b.poll().unwrap();
            for (i, g) in gpus.iter().enumerate() {
                let Some(label) = g.throttle.as_deref() else {
                    continue;
                };
                for part in label.split('+') {
                    match part {
                        "thermal" => assert!(
                            b.devices[i].temp_crit_c.is_some() && g.temperature_c.is_some(),
                            "{}: thermal throttle without a critical temperature",
                            g.name
                        ),
                        "power-limit" => assert!(
                            g.power_limit_w.is_some() && g.power_w.is_some(),
                            "{}: power throttle without a cap to hit",
                            g.name
                        ),
                        other => panic!("{}: unknown throttle reason {other:?}", g.name),
                    }
                }
            }
        }

        /// An APU is identified by the `gpu_metrics` format revision, and
        /// nothing else in sysfs says so. Getting it wrong is not cosmetic:
        /// the client memory rule and the `shared` marker both key on it.
        #[test]
        fn an_apu_is_the_gpu_metrics_revision_saying_so() {
            let Some(b) = amd() else { return };
            for i in class(&b, true) {
                let d = &b.devices[i];
                let rev = fs::read(d.dev.join("gpu_metrics"))
                    .ok()
                    .and_then(|b| b.get(2).copied())
                    .unwrap_or_else(|| panic!("{}: an APU publishes gpu_metrics", d.name));
                assert!(
                    rev >= 2,
                    "{}: format revision {rev} is a discrete card's",
                    d.name
                );
            }
        }

        /// An APU's local pool is a BIOS carve-out of system RAM rather than a
        /// device-local one, so both pools it publishes are host RAM and the
        /// second has to say so.
        #[test]
        fn an_apu_meters_a_carve_out_beside_a_pool_it_marks_shared() {
            let Some(mut b) = amd() else { return };
            let apus = class(&b, true);
            let gpus = b.poll().unwrap();
            for i in apus {
                let g = &gpus[i];
                assert!(g.integrated, "{}", g.name);
                let second = g
                    .mem_secondary()
                    .unwrap_or_else(|| panic!("{}: an APU publishes a GTT pool", g.name));
                assert!(
                    second.shared,
                    "{}: an APU's second pool is system RAM, not a spill",
                    g.name
                );
                // The carve-out is small next to what the APU reaches through
                // GTT; a larger one would mean both were read from one file.
                if let (Some(vram), Some(gtt)) = (g.vram_total_bytes, second.total) {
                    assert!(
                        vram <= gtt,
                        "{}: a {vram} B carve-out beside {gtt} B of GTT",
                        g.name
                    );
                }
            }
        }

        /// An APU keeps most of a client's allocation in GTT, so charging it
        /// only `vram` gives rows that contradict the memory meter above them.
        /// Checked against an independent `/proc` walk, restricted to clients
        /// whose figures did not move across the poll — plus this test's own
        /// render-node client, whose GTT allocation is two orders of magnitude
        /// the VRAM one, so a row built from `vram` alone cannot pass.
        #[test]
        fn an_apu_charges_its_clients_gtt_as_well_as_the_carve_out() {
            let Some(mut b) = amd() else { return };
            let apus = class(&b, true);
            if apus.is_empty() {
                return;
            }
            check_client_rows(&mut b, &apus, true);
        }

        /// A discrete card's VRAM is a real pool with a real size, and the
        /// meter's scale comes from it. `None` or 0 here is a card whose MEM
        /// meter cannot be drawn at all.
        #[test]
        fn a_discrete_card_publishes_a_vram_total_it_can_meter() {
            let Some(mut b) = amd() else { return };
            let discrete = class(&b, false);
            let gpus = b.poll().unwrap();
            for i in discrete {
                let g = &gpus[i];
                assert!(!g.integrated, "{}", g.name);
                let total = g
                    .vram_total_bytes
                    .unwrap_or_else(|| panic!("{}: a discrete card has VRAM", g.name));
                assert!(total > 0, "{}: a published total of 0 is not one", g.name);
                let used = g
                    .vram_used_bytes
                    .unwrap_or_else(|| panic!("{}: VRAM usage is readable", g.name));
                assert!(used <= total, "{}: {used} B in a {total} B pool", g.name);
                assert!(
                    !g.mem_primary().shared,
                    "{}: VRAM is the card's own",
                    g.name
                );
                assert_eq!(
                    g.mem_secondary().map(|m| m.shared),
                    Some(false),
                    "{}: GTT here is host RAM spilled into, not shared memory",
                    g.name
                );
                // A discrete card is a PCIe endpoint, and its link is what the
                // UI's PCIe row is drawn from.
                assert!(g.pcie_max_gen.is_some(), "{}: no link capability", g.name);
                assert!(g.pcie_max_width.is_some(), "{}: no link width", g.name);
            }
        }

        /// A discrete card's client rows charge `vram` only: its `gtt` bytes
        /// are host RAM, metered separately beside the card's own pool, and
        /// folding them in would count the spill twice. The check is the same
        /// one the APU runs, against the other half of the rule.
        #[test]
        fn a_discrete_card_charges_its_clients_only_what_lives_in_vram() {
            let Some(mut b) = amd() else { return };
            let discrete = class(&b, false);
            if discrete.is_empty() {
                return;
            }
            check_client_rows(&mut b, &discrete, false);
        }

        /// Every process row of the cards in `which` is the sum this
        /// backend's rule for their class charges, recomputed from a `/proc`
        /// walk of this machine's fdinfo that goes nowhere near the sweep.
        ///
        /// Clients whose figures moved across the poll are skipped — an
        /// allocation is free to change under a test that is not allowed to
        /// stop the machine. What keeps that from emptying the check out is
        /// the render-node client opened here: it is this process's own, it
        /// allocates and then does nothing, and both its pools are non-zero
        /// and wildly different sizes, so whichever half of the rule is wrong
        /// produces a figure that is off by orders of magnitude.
        fn check_client_rows(b: &mut AmdBackend, which: &[usize], integrated: bool) {
            let held = hold_clients(b, which);
            let devices = sweep_devices(b);
            // The rule, stated here rather than borrowed from the code it is
            // checking: an APU's allocation lives mostly in GTT and both pools
            // are RAM, a discrete card's GTT is a separate spill pool.
            let charge = |(vram, gtt): (u64, u64)| if integrated { vram + gtt } else { vram };
            let rule = if integrated { "vram+gtt" } else { "vram" };

            let before = walk_client_mem(&devices);
            b.poll().unwrap();
            let after = walk_client_mem(&devices);
            let rows: HashMap<(u32, usize), Option<u64>> = b
                .processes()
                .iter()
                .map(|p| ((p.pid, p.gpu_index), p.gpu_mem_bytes))
                .collect();

            let mut checked = 0;
            for ((pid, gpu), mem) in &before {
                if !which.contains(gpu) || after.get(&(*pid, *gpu)) != Some(mem) {
                    continue; // absent, or the allocation moved around the poll
                }
                assert_eq!(
                    rows.get(&(*pid, *gpu)),
                    Some(&Some(charge(*mem))),
                    "pid {pid} on card {gpu}: the row is not the {rule} sum of {mem:?}"
                );
                checked += 1;
            }
            // The client opened above is this process's own, so it cannot have
            // gone anywhere: it has to be one of the rows just checked. Both
            // its regions are non-empty and differ by two orders of magnitude,
            // which is what makes the two rules tell each other apart.
            for i in &held.opened {
                let key = (std::process::id(), *i);
                let (vram, gtt) = *before
                    .get(&key)
                    .unwrap_or_else(|| panic!("this test's own client of card {i} was not swept"));
                assert!(
                    vram > 0 && gtt > 0,
                    "card {i}: a bare render-node open allocates in both pools \
                     ({vram} B vram, {gtt} B gtt) — with one of them empty this \
                     test cannot tell the two rules apart"
                );
                assert_eq!(
                    rows.get(&key),
                    Some(&Some(charge((vram, gtt)))),
                    "card {i}: this test's own client is not charged the {rule} sum"
                );
            }
            if held.opened.is_empty() {
                eprintln!("note: no render node could be opened for these cards");
            }
            eprintln!("note: {checked} rows checked against their fdinfo ({rule})");
        }
    }
}
