//! AMD backend. Linux: sysfs/amdgpu. Windows: ADLX (not yet implemented).

use super::GpuBackend;

pub fn probe() -> Option<Box<dyn GpuBackend>> {
    #[cfg(target_os = "linux")]
    if let Some(b) = linux_impl::probe() {
        return Some(b);
    }
    // TODO Windows: ADLX bindings.
    None
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use crate::backend::linux::{
        self, card_name, cards_with_vendor, first_dir, pdev_of, read_trim, read_u64,
    };
    use crate::backend::{GpuBackend, GpuProcess, GpuSnapshot, ProcKind};
    use anyhow::Result;
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    const AMD_VENDOR: &str = "0x1002";

    pub fn probe() -> Option<Box<dyn GpuBackend>> {
        let devices = scan("/sys/class/drm");
        if devices.is_empty() {
            return None;
        }
        Some(Box::new(AmdBackend {
            devices,
            engine_state: HashMap::new(),
        }))
    }

    struct AmdDevice {
        name: String,
        dev: PathBuf,
        hwmon: Option<PathBuf>,
        /// PCI address ("0000:75:00.0"), matched against fdinfo drm-pdev.
        pdev: Option<String>,
    }

    struct AmdBackend {
        devices: Vec<AmdDevice>,
        /// (pid, drm-client-id) -> cumulative engine ns at last scan.
        engine_state: HashMap<(u32, u64), (u64, Instant)>,
    }

    impl GpuBackend for AmdBackend {
        fn name(&self) -> &'static str {
            "amdgpu"
        }

        fn poll(&mut self) -> Result<Vec<GpuSnapshot>> {
            Ok(self.devices.iter().map(sample).collect())
        }

        fn processes(&mut self) -> Vec<GpuProcess> {
            let pdev_to_gpu: HashMap<&str, usize> = self
                .devices
                .iter()
                .enumerate()
                .filter_map(|(i, d)| d.pdev.as_deref().map(|p| (p, i)))
                .collect();

            // (pid, gpu) -> aggregated stats across that process's DRM clients.
            let mut agg: HashMap<(u32, usize), (f64, u64, bool)> = HashMap::new();
            let mut seen_clients: HashSet<(u32, u64)> = HashSet::new();
            let now = Instant::now();

            for pid in linux::proc_pids() {
                for client in linux::drm_clients(pid, "amdgpu") {
                    let Some(&gpu) = client.pdev.as_deref().and_then(|p| pdev_to_gpu.get(p)) else {
                        continue;
                    };
                    // The same client shows once per duplicated fd — count once.
                    if !seen_clients.insert((pid, client.id)) {
                        continue;
                    }
                    let engine_ns = client.total_engine_ns();
                    let util = match self.engine_state.get(&(pid, client.id)) {
                        Some((prev_ns, prev_at)) => {
                            let wall = now.duration_since(*prev_at).as_nanos() as f64;
                            if wall > 0.0 {
                                (engine_ns.saturating_sub(*prev_ns) as f64 / wall * 100.0)
                                    .clamp(0.0, 100.0)
                            } else {
                                0.0
                            }
                        }
                        None => 0.0,
                    };
                    self.engine_state.insert((pid, client.id), (engine_ns, now));

                    let e = agg.entry((pid, gpu)).or_insert((0.0, 0, false));
                    e.0 += util;
                    e.1 += client.memory.get("vram").copied().unwrap_or(0);
                    e.2 |= client.engine_ns.get("gfx").copied().unwrap_or(0) > 0;
                }
            }

            // Forget clients that vanished so the map doesn't grow forever.
            self.engine_state.retain(|k, _| seen_clients.contains(k));

            agg.into_iter()
                .map(|((pid, gpu_index), (util, vram, graphics))| GpuProcess {
                    pid,
                    gpu_index,
                    kind: if graphics {
                        ProcKind::Graphics
                    } else {
                        ProcKind::Compute
                    },
                    gpu_util_pct: Some(util.min(100.0)),
                    gpu_mem_bytes: vram,
                })
                .collect()
        }
    }

    fn scan(drm: &str) -> Vec<AmdDevice> {
        cards_with_vendor(drm, AMD_VENDOR)
            .into_iter()
            .map(|(idx, dev)| {
                let name = card_name(&dev, idx, "1002", "AMD");
                let hwmon = first_dir(&dev.join("hwmon"));
                let pdev = pdev_of(&dev);
                AmdDevice {
                    name,
                    dev,
                    hwmon,
                    pdev,
                }
            })
            .collect()
    }

    fn sample(d: &AmdDevice) -> GpuSnapshot {
        let h = d.hwmon.as_deref();
        GpuSnapshot {
            name: d.name.clone(),
            integrated: is_apu(&d.dev),
            utilization_pct: read_u64(&d.dev.join("gpu_busy_percent")).unwrap_or(0) as f64,
            mem_util_pct: read_u64(&d.dev.join("mem_busy_percent")).map(|v| v as f64),
            vram_used_bytes: read_u64(&d.dev.join("mem_info_vram_used")).unwrap_or(0),
            vram_total_bytes: read_u64(&d.dev.join("mem_info_vram_total")).unwrap_or(0),
            // Millidegrees C. temp1 is the "edge" sensor on amdgpu.
            temperature_c: hwmon_u64(h, "temp1_input").map(|v| v as f64 / 1000.0),
            // Microwatts; power1_average is absent on APUs (power1_input instead).
            power_w: hwmon_u64(h, "power1_average")
                .or_else(|| hwmon_u64(h, "power1_input"))
                .map(|v| v as f64 / 1e6),
            // A cap of 0 means "not reporting" (seen on idle Navi 31), not 0 W.
            power_limit_w: hwmon_u64(h, "power1_cap")
                .filter(|v| *v > 0)
                .or_else(|| hwmon_u64(h, "power1_cap_default").filter(|v| *v > 0))
                .map(|v| v as f64 / 1e6),
            fan_pct: fan_pct(h),
            // Hz. Reads 0 when the clock domain is power-gated at idle.
            clock_mhz: hwmon_u64(h, "freq1_input")
                .map(|v| v / 1_000_000)
                .or_else(|| dpm_active_mhz(&d.dev.join("pp_dpm_sclk"))),
            mem_clock_mhz: hwmon_u64(h, "freq2_input")
                .map(|v| v / 1_000_000)
                // APUs have no freq2_input; the active DPM level has it.
                .or_else(|| dpm_active_mhz(&d.dev.join("pp_dpm_mclk"))),
            pcie_gen: read_trim(&d.dev.join("current_link_speed"))
                .as_deref()
                .and_then(gts_to_gen),
            pcie_width: read_trim(&d.dev.join("current_link_width")).and_then(|w| w.parse().ok()),
            // amdgpu does not expose PCIe throughput counters.
            pcie_rx_kbs: None,
            pcie_tx_kbs: None,
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

    /// "16.0 GT/s PCIe" -> Some(4). Gen1=2.5, doubling each gen after Gen2.
    fn gts_to_gen(speed: &str) -> Option<u8> {
        let gts: f64 = speed.split_whitespace().next()?.parse().ok()?;
        Some(match gts {
            s if s >= 128.0 => 7,
            s if s >= 64.0 => 6,
            s if s >= 32.0 => 5,
            s if s >= 16.0 => 4,
            s if s >= 8.0 => 3,
            s if s >= 5.0 => 2,
            _ => 1,
        })
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

    fn hwmon_u64(hwmon: Option<&Path>, file: &str) -> Option<u64> {
        read_u64(&hwmon?.join(file))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn pcie_gen_from_gts_string() {
            assert_eq!(gts_to_gen("2.5 GT/s PCIe"), Some(1));
            assert_eq!(gts_to_gen("8.0 GT/s PCIe"), Some(3));
            assert_eq!(gts_to_gen("16.0 GT/s PCIe"), Some(4));
            assert_eq!(gts_to_gen("32.0 GT/s PCIe"), Some(5));
            assert_eq!(gts_to_gen("garbage"), None);
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
                    "{}: util={}% vram={}/{}MiB temp={:?}C power={:?}W fan={:?}% core={:?}MHz mem={:?}MHz",
                    g.name,
                    g.utilization_pct,
                    g.vram_used_bytes / 1024 / 1024,
                    g.vram_total_bytes / 1024 / 1024,
                    g.temperature_c,
                    g.power_w,
                    g.fan_pct,
                    g.clock_mhz,
                    g.mem_clock_mhz,
                );
                assert!(g.vram_total_bytes > 0, "vram total should be nonzero");
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
