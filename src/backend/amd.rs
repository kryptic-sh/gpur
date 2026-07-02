//! AMD backend. Linux: sysfs/amdgpu. Windows: ADLX (not yet implemented).

use super::GpuBackend;

pub fn probe() -> Option<Box<dyn GpuBackend>> {
    #[cfg(target_os = "linux")]
    if let Some(b) = linux::probe() {
        return Some(b);
    }
    // TODO Windows: ADLX bindings.
    None
}

#[cfg(target_os = "linux")]
mod linux {
    use crate::backend::{GpuBackend, GpuSnapshot};
    use anyhow::Result;
    use std::fs;
    use std::path::{Path, PathBuf};

    const AMD_VENDOR: &str = "0x1002";
    const PCI_IDS_PATHS: &[&str] = &["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids"];

    pub fn probe() -> Option<Box<dyn GpuBackend>> {
        let devices = scan("/sys/class/drm");
        if devices.is_empty() {
            return None;
        }
        Some(Box::new(AmdBackend { devices }))
    }

    struct AmdDevice {
        name: String,
        dev: PathBuf,
        hwmon: Option<PathBuf>,
    }

    struct AmdBackend {
        devices: Vec<AmdDevice>,
    }

    impl GpuBackend for AmdBackend {
        fn name(&self) -> &'static str {
            "amdgpu"
        }

        fn poll(&mut self) -> Result<Vec<GpuSnapshot>> {
            Ok(self.devices.iter().map(sample).collect())
        }
    }

    fn scan(drm: &str) -> Vec<AmdDevice> {
        let Ok(entries) = fs::read_dir(drm) else {
            return Vec::new();
        };
        let mut cards: Vec<(u32, PathBuf)> = entries
            .flatten()
            .filter_map(|e| {
                let idx = card_index(&e.file_name().to_string_lossy())?;
                Some((idx, e.path().join("device")))
            })
            .collect();
        cards.sort_by_key(|(idx, _)| *idx);

        let pci_ids = PCI_IDS_PATHS
            .iter()
            .find_map(|p| fs::read_to_string(p).ok());

        cards
            .into_iter()
            .filter(|(_, dev)| read_trim(&dev.join("vendor")).as_deref() == Some(AMD_VENDOR))
            .map(|(idx, dev)| {
                let device_id = read_trim(&dev.join("device")).unwrap_or_default();
                let name = pci_ids
                    .as_deref()
                    .and_then(|ids| {
                        pci_device_name(ids, "1002", device_id.trim_start_matches("0x"))
                    })
                    .unwrap_or_else(|| format!("AMD GPU {device_id} (card{idx})"));
                let hwmon = first_dir(&dev.join("hwmon"));
                AmdDevice { name, dev, hwmon }
            })
            .collect()
    }

    /// "card1" -> Some(1); connectors ("card1-DP-1") and render nodes -> None.
    fn card_index(file_name: &str) -> Option<u32> {
        file_name.strip_prefix("card")?.parse().ok()
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

    fn read_u64(path: &Path) -> Option<u64> {
        read_trim(path)?.parse().ok()
    }

    fn read_trim(path: &Path) -> Option<String> {
        fs::read_to_string(path).ok().map(|s| s.trim().to_string())
    }

    fn first_dir(path: &Path) -> Option<PathBuf> {
        fs::read_dir(path)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .find(|p| p.is_dir())
    }

    /// Look up a device's marketing name in pci.ids. Vendor/device ids are
    /// lowercase hex without the 0x prefix.
    fn pci_device_name(ids: &str, vendor: &str, device: &str) -> Option<String> {
        let mut in_vendor = false;
        for line in ids.lines() {
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            if !line.starts_with('\t') {
                in_vendor = line
                    .split_whitespace()
                    .next()
                    .is_some_and(|v| v.eq_ignore_ascii_case(vendor));
                continue;
            }
            if !in_vendor || line.starts_with("\t\t") {
                continue; // subsystem lines
            }
            let rest = line.trim_start();
            if let Some((id, name)) = rest.split_once(char::is_whitespace)
                && id.eq_ignore_ascii_case(device)
            {
                return Some(name.trim().to_string());
            }
        }
        None
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const IDS: &str = "\
# comment
1002  Advanced Micro Devices, Inc. [AMD/ATI]
\t13c0  Phoenix2
\t744c  Navi 31 [Radeon RX 7900 XT/7900 XTX/7900M]
\t\t1002 0e3b  Some subsystem
10de  NVIDIA Corporation
\t744c  Not an AMD device
";

        #[test]
        fn pci_lookup_finds_device_in_vendor_section() {
            assert_eq!(
                pci_device_name(IDS, "1002", "744c").as_deref(),
                Some("Navi 31 [Radeon RX 7900 XT/7900 XTX/7900M]")
            );
        }

        #[test]
        fn pci_lookup_ignores_other_vendors_and_subsystems() {
            assert_eq!(pci_device_name(IDS, "1002", "0e3b"), None);
            assert_eq!(
                pci_device_name(IDS, "10de", "744c").as_deref(),
                Some("Not an AMD device")
            );
        }

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
        }

        #[test]
        fn card_index_filters_connectors_and_render_nodes() {
            assert_eq!(card_index("card0"), Some(0));
            assert_eq!(card_index("card12"), Some(12));
            assert_eq!(card_index("card1-DP-1"), None);
            assert_eq!(card_index("renderD128"), None);
            assert_eq!(card_index("version"), None);
        }
    }
}
