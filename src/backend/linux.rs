//! Shared Linux DRM plumbing: sysfs readers, pci.ids lookup, and the
//! /proc fdinfo scan that powers per-process GPU attribution for both the
//! amdgpu and Intel (i915/xe) backends.
#![cfg(target_os = "linux")]

use super::{GpuProcess, ProcKind, clamp_pct};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

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

    /// Busy ns summed over engines whose name matches `pred` (e.g. the media
    /// engines, for video-utilization attribution).
    pub fn engine_ns_where(&self, pred: impl Fn(&str) -> bool) -> u64 {
        self.engine_ns
            .iter()
            .filter(|(k, _)| pred(k))
            .map(|(_, v)| *v)
            .sum()
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

/// Utilization% and video-util% from an engine-ns counter delta against the
/// prior scan. Both amdgpu and i915 accumulate busy time in ns; percent =
/// busy delta / wall-clock delta. Returns (0, 0) on the first sample or a
/// zero interval. The caller stores `(engine_ns, video_ns, now)` as the next
/// `prev`.
pub fn ns_delta_util(
    prev: Option<&(u64, u64, Instant)>,
    engine_ns: u64,
    video_ns: u64,
    now: Instant,
) -> (f64, f64) {
    let Some((prev_ns, prev_video, prev_at)) = prev else {
        return (0.0, 0.0);
    };
    let wall = now.duration_since(*prev_at).as_nanos() as f64;
    if wall <= 0.0 {
        return (0.0, 0.0);
    }
    (
        (engine_ns.saturating_sub(*prev_ns) as f64 / wall * 100.0).clamp(0.0, 100.0),
        (video_ns.saturating_sub(*prev_video) as f64 / wall * 100.0).clamp(0.0, 100.0),
    )
}

/// Assemble one process-table row from aggregated sweep stats. Shared by the
/// amdgpu and Intel sweeps, which build identical rows.
pub fn build_proc(pid: u32, gpu_index: usize, util: f64, mem: u64, graphics: bool) -> GpuProcess {
    GpuProcess {
        pid,
        gpu_index,
        kind: if graphics {
            ProcKind::Graphics
        } else {
            ProcKind::Compute
        },
        gpu_util_pct: Some(clamp_pct(util)),
        gpu_mem_bytes: mem,
        ..Default::default()
    }
}

/// What the shared fdinfo sweep needs to know about one device.
pub struct SweepDevice {
    /// PCI address as fdinfo reports it in `drm-pdev`.
    pub pdev: Option<String>,
    /// DRM driver name a client must report to belong to this device.
    pub driver: String,
}

/// One client's contribution, as computed by the backend's per-client closure.
#[derive(Debug, Default, Clone, Copy)]
pub struct ClientSample {
    pub util_pct: f64,
    pub video_pct: f64,
    /// Bytes to charge the owning process's table row.
    pub mem_bytes: u64,
    /// Client touched a graphics (rather than compute-only) engine.
    pub graphics: bool,
}

/// Aggregate of one /proc fdinfo sweep.
#[derive(Debug, Default)]
pub struct Sweep {
    /// gpu index -> summed client utilization %.
    pub util: HashMap<usize, f64>,
    /// gpu index -> summed client video-engine utilization %.
    pub video_util: HashMap<usize, f64>,
    pub procs: Vec<GpuProcess>,
    /// (pid, drm-client-id) seen this pass. Callers `retain` their delta-state
    /// maps against it so vanished clients don't leak.
    pub seen: HashSet<(u32, u64)>,
}

/// Walk every process's DRM clients once, attribute each to a device by
/// `drm-pdev`, and aggregate. Backends differ only in how one client's
/// utilization and memory are derived, which is what `per_client(pid, gpu,
/// client)` supplies — it gets the pid because both backends key their
/// counter-delta state on (pid, drm-client-id).
///
/// A client with several duplicated fds appears once per fd; it is counted
/// once, keyed on (pid, drm-client-id).
pub fn sweep_clients<F>(devices: &[SweepDevice], mut per_client: F) -> Sweep
where
    F: FnMut(u32, usize, &FdClient) -> ClientSample,
{
    let pdev_to_gpu: HashMap<&str, usize> = devices
        .iter()
        .enumerate()
        .filter_map(|(i, d)| d.pdev.as_deref().map(|p| (p, i)))
        .collect();
    // One fdinfo pass per distinct driver, not per device.
    let mut drivers: Vec<&str> = devices.iter().map(|d| d.driver.as_str()).collect();
    drivers.sort_unstable();
    drivers.dedup();

    let mut sweep = Sweep::default();
    // (pid, gpu) -> aggregated stats across that process's DRM clients.
    let mut agg: HashMap<(u32, usize), (f64, u64, bool)> = HashMap::new();

    for pid in proc_pids() {
        for driver in &drivers {
            for client in drm_clients(pid, driver) {
                let Some(&gpu) = client.pdev.as_deref().and_then(|p| pdev_to_gpu.get(p)) else {
                    continue;
                };
                if devices[gpu].driver != *driver {
                    continue;
                }
                if !sweep.seen.insert((pid, client.id)) {
                    continue;
                }
                let s = per_client(pid, gpu, &client);
                *sweep.util.entry(gpu).or_default() += s.util_pct;
                *sweep.video_util.entry(gpu).or_default() += s.video_pct;
                let e = agg.entry((pid, gpu)).or_insert((0.0, 0, false));
                e.0 += s.util_pct;
                e.1 += s.mem_bytes;
                e.2 |= s.graphics;
            }
        }
    }

    sweep.procs = agg
        .into_iter()
        .map(|((pid, gpu_index), (util, mem, graphics))| {
            build_proc(pid, gpu_index, util, mem, graphics)
        })
        .collect();
    sweep
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
            c.memory.insert(region.to_string(), parse_size(value));
        } else if let Some(region) = key.strip_prefix("drm-resident-") {
            resident.insert(region.to_string(), parse_size(value));
        }
    }
    // Newer kernels emit drm-resident-*; older only drm-memory-*. Prefer the
    // explicit memory lines, fall back to resident. drm-total-* is deliberately
    // ignored: it counts allocated-but-possibly-evicted pages, not what the
    // region actually holds.
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

/// Bytes from a DRM size value. `drm_fdinfo_print_size` scales the number down
/// while it stays 1 KiB-aligned, so the same key yields bare bytes, "KiB" or
/// "MiB" depending on the allocation — the suffix must be honoured, not assumed.
/// Saturating so a bogus suffix on a huge number can't overflow.
fn parse_size(v: &str) -> u64 {
    let mut it = v.split_whitespace();
    let n: u64 = it.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    match it.next() {
        Some("KiB") => n.saturating_mul(1 << 10),
        Some("MiB") => n.saturating_mul(1 << 20),
        Some("GiB") => n.saturating_mul(1 << 30),
        _ => n,
    }
}

pub fn read_u64(path: &Path) -> Option<u64> {
    read_trim(path)?.parse().ok()
}

/// Read a numeric hwmon attribute (`temp1_input`, `power1_average`, ...) from
/// an optional hwmon directory. None when there's no hwmon or the file is
/// missing/unparseable.
pub fn hwmon_u64(hwmon: Option<&Path>, file: &str) -> Option<u64> {
    read_u64(&hwmon?.join(file))
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

/// "16.0 GT/s PCIe" -> Some(4). Gen1 = 2.5 GT/s, doubling from Gen2 on.
/// Reads "Unknown" on links the bridge won't report, which parses to None.
pub fn gts_to_gen(speed: &str) -> Option<u8> {
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

/// Current and maximum PCIe link as (gen, width, max gen, max width). These
/// are PCI-core attributes (`drivers/pci/pci-sysfs.c`), identical for every
/// vendor's endpoint, so one reader serves all sysfs backends. Each element is
/// None when its file is missing or unparseable — e.g. on integrated devices,
/// which are not PCIe endpoints in any meaningful sense.
pub fn pcie_link(dev: &Path) -> (Option<u8>, Option<u32>, Option<u8>, Option<u32>) {
    let speed = |f: &str| read_trim(&dev.join(f)).as_deref().and_then(gts_to_gen);
    let width = |f: &str| read_trim(&dev.join(f)).and_then(|w| w.parse().ok());
    (
        speed("current_link_speed"),
        width("current_link_width"),
        speed("max_link_speed"),
        width("max_link_width"),
    )
}

/// Total system RAM in bytes. i915 and xe size their system memory region at
/// `totalram_pages()`, so this is the real ceiling on system-backed (GTT-style)
/// graphics memory for devices that publish no total of their own.
pub fn sys_mem_total_bytes() -> Option<u64> {
    parse_meminfo_total(&fs::read_to_string("/proc/meminfo").ok()?)
}

/// "MemTotal:       32770396 kB" -> bytes. The unit is always kB here.
fn parse_meminfo_total(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kb.checked_mul(1024)
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
drm-total-local0:\t512 MiB
drm-resident-local0:\t256 MiB
drm-resident-stolen-local0:\t131072 KiB
drm-resident-system0:\t1234567
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
        // amdgpu's legacy printer always tags KiB, on both lines.
        assert_eq!(c.memory["vram"], 12 << 10);
        assert_eq!(c.memory["gtt"], 2048 << 10);
    }

    #[test]
    fn parses_i915_client_skipping_capacity() {
        let c = parse_fdinfo(I915_FDINFO).unwrap();
        assert_eq!(c.driver, "i915");
        assert_eq!(c.engine_ns["render"], 9_876_543);
        assert!(!c.engine_ns.contains_key("capacity-video"));
        // resident fallback populates the region, honouring each unit the DRM
        // printer may pick: MiB, KiB, and bare bytes when not 1 KiB-aligned.
        assert_eq!(c.memory["local0"], 256 << 20);
        assert_eq!(c.memory["stolen-local0"], 131_072 << 10);
        assert_eq!(c.memory["system0"], 1_234_567);
        // drm-total-* is allocated, not resident: never counted.
        assert!(!c.memory.contains_key("total-local0"));
    }

    #[test]
    fn parse_size_honours_unit_suffix() {
        assert_eq!(parse_size("1234567"), 1_234_567); // bare bytes
        assert_eq!(parse_size("12 KiB"), 12 << 10);
        assert_eq!(parse_size("512 MiB"), 512 << 20);
        assert_eq!(parse_size("2 GiB"), 2 << 30);
        assert_eq!(parse_size("0"), 0);
        assert_eq!(parse_size("0 KiB"), 0);
        assert_eq!(parse_size(""), 0);
        // unknown suffix falls back to bytes; absurd input saturates
        assert_eq!(parse_size("42 TiB"), 42);
        assert_eq!(parse_size(&format!("{} GiB", u64::MAX)), u64::MAX);
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

    /// Fresh scratch dir under the system temp dir, one per test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gpur-linux-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn pcie_gen_from_gts_string() {
        assert_eq!(gts_to_gen("2.5 GT/s PCIe"), Some(1));
        assert_eq!(gts_to_gen("8.0 GT/s PCIe"), Some(3));
        assert_eq!(gts_to_gen("16.0 GT/s PCIe"), Some(4));
        assert_eq!(gts_to_gen("32.0 GT/s PCIe"), Some(5));
        assert_eq!(gts_to_gen("64.0 GT/s PCIe"), Some(6));
        assert_eq!(gts_to_gen("Unknown"), None);
        assert_eq!(gts_to_gen("garbage"), None);
    }

    #[test]
    fn pcie_link_reads_current_and_max() {
        let dev = scratch("pcie");
        fs::write(dev.join("current_link_speed"), "8.0 GT/s PCIe\n").unwrap();
        fs::write(dev.join("current_link_width"), "8\n").unwrap();
        fs::write(dev.join("max_link_speed"), "16.0 GT/s PCIe\n").unwrap();
        fs::write(dev.join("max_link_width"), "16\n").unwrap();
        // A card in a x8 Gen3 slot: the downgrade the UI flags.
        assert_eq!(pcie_link(&dev), (Some(3), Some(8), Some(4), Some(16)));
    }

    #[test]
    fn pcie_link_absent_files_are_none() {
        let dev = scratch("pcie-missing");
        assert_eq!(pcie_link(&dev), (None, None, None, None));
        // A partially populated endpoint keeps the fields it does have.
        fs::write(dev.join("current_link_width"), "4\n").unwrap();
        fs::write(dev.join("max_link_speed"), "Unknown\n").unwrap();
        assert_eq!(pcie_link(&dev), (None, Some(4), None, None));
    }

    #[test]
    fn meminfo_total_parses_kb() {
        assert_eq!(
            parse_meminfo_total("MemTotal:       32770396 kB\nMemFree:  100 kB\n"),
            Some(32_770_396 * 1024)
        );
        assert_eq!(parse_meminfo_total("MemFree:  100 kB\n"), None);
        assert_eq!(parse_meminfo_total("MemTotal:\n"), None);
    }

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
