//! Shared Linux DRM plumbing: sysfs readers, pci.ids lookup, and the
//! /proc fdinfo scan that powers per-process GPU attribution for both the
//! amdgpu and Intel (i915/xe) backends.
#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

pub const PCI_IDS_PATHS: &[&str] = &["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids"];
/// DRM character-device major.
const DRM_MAJOR: u64 = 226;

/// One DRM client (open fd) of a process, parsed from fdinfo.
#[derive(Debug, Default)]
pub struct FdClient {
    pub driver: String,
    pub id: u64,
    pub pdev: Option<String>,
    /// engine name -> cumulative busy ns ("gfx", "render", "dec", ...).
    pub engine_ns: HashMap<String, u64>,
    /// xe-style engine name -> (cycles, total_cycles).
    pub cycles: HashMap<String, (u64, u64)>,
    /// memory region -> bytes ("vram", "local", "system", "gtt", ...).
    pub memory: HashMap<String, u64>,
}

impl FdClient {
    /// Total busy time across all engines (i915/amdgpu accounting).
    pub fn total_engine_ns(&self) -> u64 {
        self.engine_ns.values().sum()
    }

    /// Busiest matching xe engine's cycles/total-cycles ratio since `prev`,
    /// as a fraction 0..=1. `pred` filters by engine name.
    pub fn xe_ratio(&self, prev: &FdClient, pred: impl Fn(&str) -> bool) -> f64 {
        let mut best = 0.0f64;
        for (name, (cyc, total)) in &self.cycles {
            if !pred(name) {
                continue;
            }
            let (pcyc, ptotal) = prev.cycles.get(name).copied().unwrap_or((0, 0));
            let dt = total.saturating_sub(ptotal);
            if dt == 0 {
                continue;
            }
            best = best.max(cyc.saturating_sub(pcyc) as f64 / dt as f64);
        }
        best
    }
}

pub fn proc_pids() -> Vec<u32> {
    fs::read_dir("/proc")
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().to_string_lossy().parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Parse every DRM client of `pid` whose fdinfo names `driver`. Restricted
/// to fds that stat as DRM character devices to avoid reading every fdinfo.
pub fn drm_clients(pid: u32, driver: &str) -> Vec<FdClient> {
    let fd_dir = format!("/proc/{pid}/fd");
    let Ok(entries) = fs::read_dir(&fd_dir) else {
        return Vec::new(); // other users' processes without privileges
    };
    entries
        .flatten()
        .filter_map(|e| {
            let meta = fs::metadata(e.path()).ok()?;
            if !meta.file_type().is_char_device() || linux_major(meta.rdev()) != DRM_MAJOR {
                return None;
            }
            let fd = e.file_name();
            let info =
                fs::read_to_string(format!("/proc/{pid}/fdinfo/{}", fd.to_string_lossy())).ok()?;
            let client = parse_fdinfo(&info)?;
            (client.driver == driver).then_some(client)
        })
        .collect()
}

fn linux_major(rdev: u64) -> u64 {
    ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff)
}

/// Parse a DRM fdinfo blob. Returns None when it isn't a DRM client file.
pub fn parse_fdinfo(info: &str) -> Option<FdClient> {
    let mut c = FdClient::default();
    let mut have_id = false;
    let mut resident: HashMap<String, u64> = HashMap::new();
    for line in info.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if key == "drm-driver" {
            c.driver = value.to_string();
        } else if key == "drm-client-id" {
            c.id = value.parse().ok()?;
            have_id = true;
        } else if key == "drm-pdev" {
            c.pdev = Some(value.to_string());
        } else if let Some(name) = key.strip_prefix("drm-engine-") {
            // Skip capacity lines like "drm-engine-capacity-render".
            if !name.starts_with("capacity") {
                c.engine_ns.insert(name.to_string(), parse_ns(value));
            }
        } else if let Some(name) = key.strip_prefix("drm-total-cycles-") {
            c.cycles.entry(name.to_string()).or_default().1 = parse_ns(value);
        } else if let Some(name) = key.strip_prefix("drm-cycles-") {
            c.cycles.entry(name.to_string()).or_default().0 = parse_ns(value);
        } else if let Some(region) = key.strip_prefix("drm-memory-") {
            c.memory.insert(region.to_string(), parse_kib(value));
        } else if let Some(region) = key.strip_prefix("drm-resident-") {
            resident.insert(region.to_string(), parse_kib(value));
        }
    }
    // Newer kernels emit drm-resident-*; older only drm-memory-*. Prefer the
    // explicit memory lines, fall back to resident.
    for (region, bytes) in resident {
        c.memory.entry(region).or_insert(bytes);
    }
    (have_id && !c.driver.is_empty()).then_some(c)
}

/// "123456 ns" or "123456" -> 123456
fn parse_ns(v: &str) -> u64 {
    v.split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// "12 KiB" -> 12288
fn parse_kib(v: &str) -> u64 {
    v.split_whitespace()
        .next()
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0)
        * 1024
}

pub fn read_u64(path: &Path) -> Option<u64> {
    read_trim(path)?.parse().ok()
}

pub fn read_trim(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

pub fn first_dir(path: &Path) -> Option<PathBuf> {
    fs::read_dir(path)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir())
}

/// "card1" -> Some(1); connectors ("card1-DP-1") and render nodes -> None.
pub fn card_index(file_name: &str) -> Option<u32> {
    file_name.strip_prefix("card")?.parse().ok()
}

/// Sorted (card index, device dir) pairs whose vendor file matches.
pub fn cards_with_vendor(drm: &str, vendor: &str) -> Vec<(u32, PathBuf)> {
    let Ok(entries) = fs::read_dir(drm) else {
        return Vec::new();
    };
    let mut cards: Vec<(u32, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let idx = card_index(&e.file_name().to_string_lossy())?;
            Some((idx, e.path().join("device")))
        })
        .filter(|(_, dev)| read_trim(&dev.join("vendor")).as_deref() == Some(vendor))
        .collect();
    cards.sort_by_key(|(idx, _)| *idx);
    cards
}

/// The PCI address ("0000:75:00.0") a card's device dir resolves to; this is
/// what fdinfo reports as drm-pdev.
pub fn pdev_of(dev: &Path) -> Option<String> {
    fs::canonicalize(dev)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
}

/// Look up a device's marketing name in pci.ids. Vendor/device ids are
/// lowercase hex without the 0x prefix.
pub fn pci_device_name(ids: &str, vendor: &str, device: &str) -> Option<String> {
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

/// Resolve a card's marketing name from pci.ids with a readable fallback.
pub fn card_name(dev: &Path, idx: u32, vendor_hex: &str, fallback_brand: &str) -> String {
    let device_id = read_trim(&dev.join("device")).unwrap_or_default();
    PCI_IDS_PATHS
        .iter()
        .find_map(|p| fs::read_to_string(p).ok())
        .as_deref()
        .and_then(|ids| pci_device_name(ids, vendor_hex, device_id.trim_start_matches("0x")))
        .unwrap_or_else(|| format!("{fallback_brand} GPU {device_id} (card{idx})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMD_FDINFO: &str = "\
drm-driver:\tamdgpu
drm-client-id:\t7568
drm-pdev:\t0000:75:00.0
drm-engine-gfx:\t123456789 ns
drm-engine-dec:\t1000 ns
drm-memory-vram:\t12 KiB
drm-memory-gtt: \t2048 KiB
";

    const I915_FDINFO: &str = "\
drm-driver:\ti915
drm-client-id:\t42
drm-pdev:\t0000:00:02.0
drm-engine-render:\t9876543 ns
drm-engine-video:\t100 ns
drm-engine-capacity-video:\t2
drm-total-local0:\t256 MiB
drm-resident-local0:\t131072 KiB
";

    const XE_FDINFO: &str = "\
drm-driver:\txe
drm-client-id:\t7
drm-pdev:\t0000:03:00.0
drm-cycles-rcs:\t500
drm-total-cycles-rcs:\t1000
drm-cycles-vcs:\t10
drm-total-cycles-vcs:\t1000
drm-resident-vram0:\t4096 KiB
";

    #[test]
    fn parses_amdgpu_client() {
        let c = parse_fdinfo(AMD_FDINFO).unwrap();
        assert_eq!(c.driver, "amdgpu");
        assert_eq!(c.id, 7568);
        assert_eq!(c.pdev.as_deref(), Some("0000:75:00.0"));
        assert_eq!(c.engine_ns["gfx"], 123_456_789);
        assert_eq!(c.total_engine_ns(), 123_456_789 + 1000);
        assert_eq!(c.memory["vram"], 12 * 1024);
    }

    #[test]
    fn parses_i915_client_skipping_capacity() {
        let c = parse_fdinfo(I915_FDINFO).unwrap();
        assert_eq!(c.driver, "i915");
        assert_eq!(c.engine_ns["render"], 9_876_543);
        assert!(!c.engine_ns.contains_key("capacity-video"));
        // resident fallback populates the region
        assert_eq!(c.memory["local0"], 131_072 * 1024);
    }

    #[test]
    fn xe_cycles_utilization() {
        let prev = parse_fdinfo(XE_FDINFO).unwrap();
        let mut cur = parse_fdinfo(XE_FDINFO).unwrap();
        cur.cycles.insert("rcs".into(), (800, 2000));
        cur.cycles.insert("vcs".into(), (110, 2000));
        // rcs: (800-500)/(2000-1000) = 0.3 ; vcs: (110-10)/1000 = 0.1
        assert!((cur.xe_ratio(&prev, |_| true) - 0.3).abs() < 1e-9);
        // video-only filter picks the vcs engine
        assert!((cur.xe_ratio(&prev, |n| n.starts_with("vcs")) - 0.1).abs() < 1e-9);
        assert_eq!(cur.memory["vram0"], 4096 * 1024);
    }

    #[test]
    fn non_drm_fdinfo_is_none() {
        assert!(parse_fdinfo("pos:\t0\nflags:\t0100002\n").is_none());
    }

    #[test]
    fn card_index_filters_connectors_and_render_nodes() {
        assert_eq!(card_index("card0"), Some(0));
        assert_eq!(card_index("card12"), Some(12));
        assert_eq!(card_index("card1-DP-1"), None);
        assert_eq!(card_index("renderD128"), None);
        assert_eq!(card_index("version"), None);
    }

    const IDS: &str = "\
# comment
1002  Advanced Micro Devices, Inc. [AMD/ATI]
\t13c0  Phoenix2
\t744c  Navi 31 [Radeon RX 7900 XT/7900 XTX/7900M]
\t\t1002 0e3b  Some subsystem
8086  Intel Corporation
\t56a0  DG2 [Arc A770]
";

    #[test]
    fn pci_lookup_finds_device_in_vendor_section() {
        assert_eq!(
            pci_device_name(IDS, "1002", "744c").as_deref(),
            Some("Navi 31 [Radeon RX 7900 XT/7900 XTX/7900M]")
        );
        assert_eq!(
            pci_device_name(IDS, "8086", "56a0").as_deref(),
            Some("DG2 [Arc A770]")
        );
        assert_eq!(pci_device_name(IDS, "1002", "0e3b"), None);
    }
}
