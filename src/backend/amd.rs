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
        self, ClientSample, FdClient, SweepDevice, card_name, cards_with_driver, first_dir,
        hwmon_u64, pdev_of, read_trim, read_u64,
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
        let devices = scan("/sys/class/drm");
        if devices.is_empty() {
            return None;
        }
        Some(Box::new(AmdBackend {
            pcie_state: vec![None; devices.len()],
            devices,
            engine_state: HashMap::new(),
            last_procs: Vec::new(),
        }))
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
    }

    struct AmdBackend {
        devices: Vec<AmdDevice>,
        /// (pid, drm-client-id) -> that client's engine counters at last scan.
        engine_state: HashMap<(u32, u64), EngineSample>,
        /// Per device: (rx count, tx count, sampled at) from `pcie_bw`.
        pcie_state: Vec<Option<(u64, u64, Instant)>>,
        /// Built during poll's fdinfo sweep, served by processes().
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
            // One fdinfo sweep per poll: per-process rows + device video util
            // (sysfs has gpu_busy_percent but nothing for the VCN engines).
            let media = self.sweep_clients();
            let now = Instant::now();
            let pcie_state = &mut self.pcie_state;
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
            let drivers: std::collections::BTreeSet<&str> =
                self.devices.iter().map(|d| d.driver.as_str()).collect();
            linux::driver_line(&drivers.into_iter().collect::<Vec<_>>().join("+"))
        }
    }

    impl AmdBackend {
        /// Returns per-device media-engine utilization; refreshes last_procs.
        fn sweep_clients(&mut self) -> HashMap<usize, MediaUtil> {
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
            let now = Instant::now();
            let engine_state = &mut self.engine_state;

            let mut sweep = linux::sweep_clients(&devices, |pid, gpu, client| {
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
            media
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
        let (pcie_gen, pcie_width, pcie_max_gen, pcie_max_width) = linux::pcie_link(&d.dev);

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
            pcie_max_gen,
            pcie_max_width,
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

    fn fan_pct(h: Option<&Path>) -> Option<f64> {
        let pwm = hwmon_u64(h, "pwm1")?;
        let max = hwmon_u64(h, "pwm1_max").filter(|v| *v > 0).unwrap_or(255);
        Some(pwm as f64 / max as f64 * 100.0)
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
        use crate::backend::linux::testing;

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

        /// Fake sysfs root for one test, wiped so a rerun can't see stale files.
        fn fake_sysfs(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(name);
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[test]
        fn power_gated_clock_falls_through_to_the_dpm_table() {
            let dir = fake_sysfs("gpur-clock-test");
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
            let dir = fake_sysfs("gpur-pciebw-test");
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
            let dir = std::env::temp_dir().join("gpur-dpm-test");
            std::fs::create_dir_all(&dir).unwrap();
            let f = dir.join("pp_dpm_mclk");
            std::fs::write(&f, "0: 96Mhz\n1: 3000Mhz *\n2: 1249Mhz\n").unwrap();
            assert_eq!(dpm_active_mhz(&f), Some(3000));
            std::fs::write(&f, "S: 0Mhz *\n").unwrap();
            assert_eq!(dpm_active_mhz(&f), Some(0));
        }

        #[test]
        #[ignore = "requires AMD hardware; run with --ignored --nocapture"]
        fn live_poll_reports_devices() {
            let mut backend = probe().expect("no amdgpu devices visible in /sys/class/drm");
            let gpus = backend.poll().unwrap();
            assert!(!gpus.is_empty());
            for g in &gpus {
                println!(
                    "{}: util={:?}% vram={:?}/{:?}MiB temp={:?}C power={:?}W fan={:?}% core={:?}MHz mem={:?}MHz video={:?} enc={:?} dec={:?} pcie_rx={:?} pcie_tx={:?}",
                    g.name,
                    g.utilization_pct,
                    g.vram_used_bytes.map(|b| b / 1024 / 1024),
                    g.vram_total_bytes.map(|b| b / 1024 / 1024),
                    g.temperature_c,
                    g.power_w,
                    g.fan_pct,
                    g.clock_mhz,
                    g.mem_clock_mhz,
                    g.video_util_pct,
                    g.enc_util_pct,
                    g.dec_util_pct,
                    g.pcie_rx_kbs,
                    g.pcie_tx_kbs,
                );
                assert!(
                    g.vram_total_bytes.is_some_and(|b| b > 0),
                    "vram total should be a real figure, not absent or zero"
                );
            }
            // Two samples so engine-ns deltas can produce utilization.
            let _ = backend.processes();
            std::thread::sleep(std::time::Duration::from_millis(300));
            backend.poll().unwrap();
            let procs = backend.processes();
            for p in &procs {
                println!(
                    "pid={} gpu={} kind={:?} util={:?}% vram={}MiB",
                    p.pid,
                    p.gpu_index,
                    p.kind,
                    p.gpu_util_pct,
                    p.gpu_mem_bytes / 1024 / 1024,
                );
            }
            println!("{} gpu processes", procs.len());
        }
    }
}
