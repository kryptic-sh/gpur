//! Shared Linux DRM plumbing: sysfs readers, pci.ids lookup, the
//! `/sys/class/drm` card scan every vendor backend partitions, and the /proc
//! fdinfo scan that powers per-process GPU attribution for the amdgpu and
//! Intel (i915/xe) backends.
//!
//! Gated once, by `#[cfg(target_os = "linux")] mod linux;` in the parent.

use super::{GpuProcess, ProcKind, clamp_pct};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
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
        // Stays `Some` even at zero, unlike the NVML and PDH paths. This sum
        // is over the fdinfo memory regions the sweep actually read for this
        // process's clients, and fdinfo names a region only when the client
        // holds something in it — so a client that lists no `vram`/`gtt` (amd)
        // or `local`/`system` (intel) region is one holding nothing in that
        // pool. That is a reading of zero, not an absence of a reading, and
        // flattening it to `None` would hide real idle clients behind `n/a`.
        gpu_mem_bytes: Some(mem),
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
    let mut sweep = Sweep::default();
    // (pid, gpu) -> aggregated stats across that process's DRM clients.
    let mut agg: HashMap<(u32, usize), (f64, u64, bool)> = HashMap::new();

    for pid in proc_pids() {
        // One walk of each pid's fd dir, however many drivers are in play: the
        // readdir+stat+read cost is per fd, so a pass per driver name paid it
        // again for every fd the process holds.
        for client in drm_clients(pid) {
            let Some(gpu) = client_device(devices, &client) else {
                continue;
            };
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

    sweep.procs = agg
        .into_iter()
        .map(|((pid, gpu_index), (util, mem, graphics))| {
            build_proc(pid, gpu_index, util, mem, graphics)
        })
        .collect();
    sweep
}

/// Which of `devices` a DRM client belongs to — an index into the *calling
/// backend's own* device list, which the composite then re-bases onto that
/// child's offset.
///
/// Both halves of the match are load-bearing on a mixed-vendor box. The PCI
/// address is what actually names the card; the driver check is what stops a
/// client of a card this backend does not own from landing on whichever of its
/// devices happened to share the address — the backends are disjoint, so a
/// mismatch here means the client belongs to a sibling backend and this one
/// must not claim it. A client with no `drm-pdev` line is unattributable and
/// belongs to nobody.
pub fn client_device(devices: &[SweepDevice], client: &FdClient) -> Option<usize> {
    let pdev = client.pdev.as_deref()?;
    devices
        .iter()
        .position(|d| d.pdev.as_deref() == Some(pdev) && d.driver == client.driver)
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

/// Parse every DRM client of `pid`, whatever driver it belongs to — callers
/// filter on `FdClient::driver`. Restricted to fds that stat as DRM character
/// devices to avoid reading every fdinfo.
pub fn drm_clients(pid: u32) -> Vec<FdClient> {
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
            parse_fdinfo(&info)
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

/// Fan duty as a percentage of this card's own pwm scale. amdgpu, radeon and
/// nouveau all drive their fan through the same hwmon attributes, so one reader
/// serves them.
///
/// The divisor is `pwm1_max` where the chip publishes one, and 255 otherwise —
/// not a guess, but the range the hwmon ABI documents for `pwmN`, which most
/// drivers therefore never bother restating in a file. A published max of 0
/// would divide the duty into an infinity and paint a meaningless meter, so it
/// falls back to the documented 255 as well.
///
/// None when there is no `pwm1` at all: the card reports no fan, which the UI
/// must keep distinct from a fan sitting at 0%.
pub fn fan_pct(hwmon: Option<&Path>) -> Option<f64> {
    let pwm = hwmon_u64(hwmon, "pwm1")?;
    let max = hwmon_u64(hwmon, "pwm1_max")
        .filter(|v| *v > 0)
        .unwrap_or(255);
    Some(pwm as f64 / max as f64 * 100.0)
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

/// The kernel driver bound to a card's PCI device, from the `device/driver`
/// symlink ("amdgpu", "radeon", "i915", "xe", "nouveau", "nvidia"). None when
/// the device is unbound or the link is unreadable, which is the honest answer:
/// an unbound device publishes no telemetry for anyone to read.
pub fn driver_of(dev: &Path) -> Option<String> {
    Some(
        fs::read_link(dev.join("driver"))
            .ok()?
            .file_name()?
            .to_string_lossy()
            .into_owned(),
    )
}
/// Sorted (card index, device dir, bound driver) for every DRM card under `drm`
/// whose PCI vendor is `vendor` and whose driver `accept`s.
///
/// Every Linux backend readdirs this one directory, so both filters are what
/// keeps them from stepping on each other:
///
/// - The `cardN` name filter drops the render nodes and connectors that share
///   the directory. `renderD128/device` resolves to the very same PCI device as
///   its card and its `vendor` reads the same id, so a vendor-only scan lists
///   every card twice.
/// - The driver filter is what makes the vendor backends provably disjoint and
///   keeps each one to devices it can actually read. Vendor id alone would hand
///   amdgpu's sysfs reader a pre-GCN card on `radeon`, whose layout it does not
///   speak, as a row of empty gauges.
pub fn cards_with_driver(
    drm: &str,
    vendor: &str,
    accept: impl Fn(&str) -> bool,
) -> Vec<(u32, PathBuf, String)> {
    let Ok(entries) = fs::read_dir(drm) else {
        return Vec::new();
    };
    let mut cards: Vec<(u32, PathBuf, String)> = entries
        .flatten()
        .filter_map(|e| {
            let idx = card_index(&e.file_name().to_string_lossy())?;
            let dev = e.path().join("device");
            if read_trim(&dev.join("vendor")).as_deref() != Some(vendor) {
                return None;
            }
            let driver = driver_of(&dev).filter(|d| accept(d))?;
            Some((idx, dev, driver))
        })
        .collect();
    cards.sort_by_key(|(idx, _, _)| *idx);
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

/// The UI's driver line, "amdgpu · kernel 6.12.1-arch1-1". Every Linux backend
/// has the same answer to "which driver, which kernel"; only the prefix, which
/// may name several drivers, differs. None when the kernel release is unreadable.
pub fn driver_line(prefix: &str) -> Option<String> {
    sysinfo::System::kernel_version().map(|k| format!("{prefix} · kernel {k}"))
}

/// The driver line for a backend whose cards are not all on one driver, as
/// "amdgpu+radeon · kernel 6.12.1-arch1-1".
///
/// A single box can run `amdgpu` beside `radeon`, or `i915` beside `xe`, and
/// the two halves of such a pair do not publish the same telemetry — a header
/// naming only one of them attributes to that driver the gauges the other
/// card's driver is the reason for lacking. So every driver actually in use is
/// named. Deduplicated through a `BTreeSet`, which also fixes the order: the
/// same set of cards renders the same string on every poll, whatever order the
/// scan happened to list them in.
pub fn driver_line_for<'a>(drivers: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let unique: BTreeSet<&str> = drivers.into_iter().collect();
    driver_line(&unique.into_iter().collect::<Vec<_>>().join("+"))
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

/// Stable device id from a PCI address. The BDF names the slot the card sits
/// in, which survives re-enumeration, driver reload and card renumbering —
/// unlike the `cardN` index. `None` keeps "unidentifiable" distinct from a
/// fabricated id; see [`crate::backend::GpuSnapshot::device_id`].
pub fn pci_device_id(pdev: Option<&str>) -> Option<String> {
    pdev.map(|p| format!("pci:{p}"))
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

/// The pci.ids database, read from disk at most once for the lifetime of the
/// process.
///
/// The file is ~1.5 MB on current hwdata and `pci_device_name` scans it
/// linearly, and `card_name` is called once per card by each of the three Linux
/// backends — so a mixed AMD+Intel+nouveau rig used to read and re-read the
/// same megabyte and a half several times over at probe. The contents cannot
/// differ between those calls, so every read after the first is pure waste. The
/// cache is process-wide rather than per backend for the same reason: the
/// backends are reading one file, not one file each.
///
/// The `Option` inside the cell is the load-bearing part. A host with no
/// hwdata package installed has neither path, and caching that miss is what
/// stops it from stat-ing both paths again for every card it owns; only a
/// successful read is worth remembering otherwise. The cost is that installing
/// hwdata under a running gpur does not take effect until restart, which is a
/// trade a probe-time lookup can afford.
static PCI_IDS: OnceLock<Option<String>> = OnceLock::new();

fn pci_ids() -> Option<&'static str> {
    PCI_IDS
        .get_or_init(|| {
            PCI_IDS_PATHS
                .iter()
                .find_map(|p| fs::read_to_string(p).ok())
        })
        .as_deref()
}

/// Resolve a card's marketing name from pci.ids with a readable fallback.
pub fn card_name(dev: &Path, idx: u32, vendor_hex: &str, fallback_brand: &str) -> String {
    let device_id = read_trim(&dev.join("device")).unwrap_or_default();
    pci_ids()
        .and_then(|ids| pci_device_name(ids, vendor_hex, device_id.trim_start_matches("0x")))
        .unwrap_or_else(|| format!("{fallback_brand} GPU {device_id} (card{idx})"))
}

/// Fake `/sys/class/drm` trees, shared by every Linux backend's tests. All of
/// them readdir the same real directory, so they are only provably disjoint
/// when each is tested against one tree that holds every other vendor's cards
/// too.
#[cfg(test)]
pub mod testing {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A scratch directory for one test, wiped when the guard drops.
    ///
    /// Mirrors the `Sandbox` in `tests/smoke.rs` and `tests/tui.rs` — those are
    /// integration tests in another crate, so the type cannot be shared, only
    /// the pattern. The pid and the counter are the point: a fixed name under
    /// the world-writable temp dir makes two concurrent `cargo test` runs (two
    /// checkouts, two users on one box, a CI matrix on one runner) fight over
    /// one directory, and lets anyone else on the host pre-create that name as
    /// a symlink the test would then write through.
    ///
    /// Every fixture helper in the crate's unit tests builds on this one, so
    /// there is a single place for that guarantee to live.
    pub struct Sandbox(PathBuf);

    impl Sandbox {
        /// `tag` only names the fixture for a human reading a failure; the pid
        /// and counter are what make the path unique.
        pub fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "gpur-unit-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Sandbox(dir)
        }
    }

    impl std::ops::Deref for Sandbox {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// One DRM card, laid out the way sysfs lays it out: a PCI device dir named
    /// by its BDF, with the `cardN` entry pointing at it through a `device`
    /// symlink. Going through the symlink is what makes `pdev_of` — which
    /// canonicalizes — return a real address, and the BDF is what the fdinfo
    /// sweep matches on.
    ///
    /// Alongside the card it plants the entries that share `/sys/class/drm`: a
    /// render node and a connector, both reaching the same PCI device, so a
    /// scan that filters on vendor alone claims this card three times.
    pub fn card(root: &Path, idx: u32, bdf: &str, vendor: &str, device: &str, driver: &str) {
        let pci = root.join("pci").join(bdf);
        fs::create_dir_all(&pci).unwrap();
        fs::write(pci.join("vendor"), format!("{vendor}\n")).unwrap();
        fs::write(pci.join("device"), format!("{device}\n")).unwrap();
        if !driver.is_empty() {
            let target = root.join("pci/drivers").join(driver);
            fs::create_dir_all(&target).unwrap();
            std::os::unix::fs::symlink(&target, pci.join("driver")).unwrap();
        }
        let drm = root.join("drm");
        // The card itself, plus the neighbours only the `cardN` name filter
        // keeps out: a render node and a connector on the same device.
        for entry in [
            format!("card{idx}"),
            format!("renderD{}", 128 + idx),
            format!("card{idx}-DP-1"),
        ] {
            let d = drm.join(entry);
            fs::create_dir_all(&d).unwrap();
            std::os::unix::fs::symlink(&pci, d.join("device")).unwrap();
        }
    }

    /// A DRM card on something that is not a PCI device — `simpledrm` on the
    /// EFI framebuffer, `vkms`. Its `device` dir has no `vendor` file at all,
    /// which every vendor scan has to read as "not mine" rather than trip over.
    pub fn platform_card(root: &Path, idx: u32) {
        let plat = root.join("platform/simple-framebuffer.0");
        fs::create_dir_all(&plat).unwrap();
        let d = root.join("drm").join(format!("card{idx}"));
        fs::create_dir_all(&d).unwrap();
        std::os::unix::fs::symlink(&plat, d.join("device")).unwrap();
    }

    /// A tri-vendor rig, plus the awkward cards a real one carries: an NVIDIA
    /// card on the proprietary driver *and* one on nouveau, a pre-GCN AMD card
    /// on `radeon`, one device per vendor whose `device/driver` symlink does not
    /// resolve, and a non-PCI DRM device with no vendor at all.
    ///
    /// Card indices deliberately do not follow vendor order — the kernel
    /// numbers them in probe order — so a scan leaning on the index to tell
    /// vendors apart trips here.
    pub fn tri_vendor(name: &str) -> Sandbox {
        let root = Sandbox::new(name);
        card(&root, 0, "0000:00:02.0", "0x8086", "0x7d55", "i915");
        card(&root, 1, "0000:03:00.0", "0x1002", "0x744c", "amdgpu");
        card(&root, 2, "0000:01:00.0", "0x10de", "0x2684", "nvidia");
        card(&root, 3, "0000:04:00.0", "0x10de", "0x1c03", "nouveau");
        card(&root, 4, "0000:05:00.0", "0x1002", "0x6779", "radeon");
        card(&root, 5, "0000:06:00.0", "0x8086", "0xe20b", "xe");
        // No driver symlink: unbound, or one this process cannot read.
        card(&root, 6, "0000:07:00.0", "0x1002", "0x1636", "");
        card(&root, 7, "0000:08:00.0", "0x8086", "0x9a49", "");
        card(&root, 8, "0000:09:00.0", "0x10de", "0x2504", "");
        platform_card(&root, 9);
        root
    }

    /// The `drm` root to hand a `scan`, as the `&str` the scans take.
    pub fn drm(root: &Path) -> String {
        root.join("drm").to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The BDF names the slot, so it is the same key across driver reloads
    /// and card renumbering; no address means no identity, not a made-up one.
    #[test]
    fn pci_device_ids_are_namespaced_and_optional() {
        assert_eq!(
            pci_device_id(Some("0000:75:00.0")).as_deref(),
            Some("pci:0000:75:00.0")
        );
        assert_eq!(pci_device_id(None), None);
    }

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
    fn scratch(name: &str) -> testing::Sandbox {
        testing::Sandbox::new(name)
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

    /// Every card's pwm is read against its own ceiling, and the ceiling is
    /// hwmon's documented 255 whenever the chip does not publish a usable one.
    #[test]
    fn fan_duty_is_a_percentage_of_this_cards_own_pwm_scale() {
        let h = scratch("fan");
        // The common case: no pwm1_max file, so the hwmon-documented 0..255.
        fs::write(h.join("pwm1"), "128\n").unwrap();
        assert!((fan_pct(Some(&h)).unwrap() - 128.0 / 255.0 * 100.0).abs() < 1e-9);

        // A chip with a scale of its own must be divided by that scale, not by
        // 255 — half duty on a 0..100 fan is 50%, not 128%.
        fs::write(h.join("pwm1_max"), "100\n").unwrap();
        fs::write(h.join("pwm1"), "50\n").unwrap();
        assert!((fan_pct(Some(&h)).unwrap() - 50.0).abs() < 1e-9);

        // A published max of 0 would divide the duty into an infinity, so it
        // falls back to 255 like an absent file.
        fs::write(h.join("pwm1_max"), "0\n").unwrap();
        assert!((fan_pct(Some(&h)).unwrap() - 50.0 / 255.0 * 100.0).abs() < 1e-9);

        // No pwm1 at all is a card that reports no fan — unknown, not a fan
        // that has stopped, which would draw a confident empty meter.
        fs::remove_file(h.join("pwm1")).unwrap();
        assert_eq!(fan_pct(Some(&h)), None);
        assert_eq!(fan_pct(None), None);
    }

    /// A box running two of a vendor's drivers at once has to say so: the
    /// header names each driver in play exactly once, in an order that does not
    /// depend on the scan's.
    #[test]
    fn the_driver_line_names_every_driver_in_use_once() {
        let line = |drivers: &[&str]| driver_line_for(drivers.to_vec());
        let Some(kernel) = sysinfo::System::kernel_version() else {
            return; // no /proc/sys/kernel/osrelease: nothing to compare against
        };

        // amdgpu beside a pre-GCN radeon card, listed in either scan order.
        assert_eq!(
            line(&["amdgpu", "radeon"]),
            Some(format!("amdgpu+radeon · kernel {kernel}"))
        );
        assert_eq!(line(&["radeon", "amdgpu"]), line(&["amdgpu", "radeon"]));

        // Two cards on one driver name it once, not "i915+i915".
        assert_eq!(
            line(&["i915", "i915", "xe"]),
            Some(format!("i915+xe · kernel {kernel}"))
        );

        // The single-driver case is exactly what driver_line alone renders.
        assert_eq!(line(&["nouveau"]), driver_line("nouveau"));
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

    /// The scan claims cards, not the render nodes and connectors sharing the
    /// directory, and not a card whose driver it does not speak. Both filters
    /// are what keeps two backends off one device.
    #[test]
    fn card_scan_filters_by_name_and_by_driver() {
        let root = testing::tri_vendor("scan-filters");
        let drm = testing::drm(&root);

        let amd = cards_with_driver(&drm, "0x1002", |d| d == "amdgpu");
        assert_eq!(
            amd.iter()
                .map(|(i, _, d)| (*i, d.as_str()))
                .collect::<Vec<_>>(),
            [(1, "amdgpu")],
            "renderD129/device and card1-DP-1/device read vendor 0x1002 too"
        );
        // Widening the predicate is the only thing that adds the radeon card:
        // nothing else about the tree changed.
        let both = cards_with_driver(&drm, "0x1002", |d| d == "amdgpu" || d == "radeon");
        assert_eq!(
            both.iter()
                .map(|(i, _, d)| (*i, d.as_str()))
                .collect::<Vec<_>>(),
            [(1, "amdgpu"), (4, "radeon")]
        );
        // The device whose driver symlink does not resolve (card6) is in
        // neither: nothing bound means nothing to read, from anyone.
        assert!(!both.iter().any(|(i, _, _)| *i == 6));
        // A non-PCI DRM device (simpledrm, card9) has no vendor file; reading
        // one must be a miss, not a panic.
        assert!(
            cards_with_driver(&drm, "0x1002", |_| true)
                .iter()
                .all(|(i, _, _)| *i != 9)
        );

        assert_eq!(
            cards_with_driver(&drm, "0x8086", |d| d == "i915" || d == "xe")
                .iter()
                .map(|(i, _, d)| (*i, d.as_str()))
                .collect::<Vec<_>>(),
            [(0, "i915"), (5, "xe")]
        );
        assert_eq!(
            cards_with_driver(&drm, "0x10de", |d| d == "nouveau")
                .iter()
                .map(|(i, _, d)| (*i, d.as_str()))
                .collect::<Vec<_>>(),
            [(3, "nouveau")]
        );
        // Sorted by card index regardless of readdir order, so the device
        // order is the same on every poll and every restart.
        assert!(
            cards_with_driver(&drm, "0x1002", |_| true)
                .windows(2)
                .all(|w| w[0].0 < w[1].0)
        );
    }

    /// The tri-vendor verdict: every Linux backend readdirs one directory, so
    /// this is the test that they partition it — each card claimed exactly
    /// once, no id claimed twice, and the NVIDIA card on nouveau not silently
    /// dropped just because NVML could not initialise.
    #[test]
    fn the_linux_backends_partition_one_drm_directory() {
        let root = testing::tri_vendor("partition");
        let drm = testing::drm(&root);
        let amd = crate::backend::amd::claimed_ids(&drm);
        let intel = crate::backend::intel::claimed_ids(&drm);
        let nvidia = crate::backend::nvidia::claimed_ids(&drm);

        assert_eq!(amd, ["pci:0000:03:00.0", "pci:0000:05:00.0"]);
        assert_eq!(intel, ["pci:0000:00:02.0", "pci:0000:06:00.0"]);
        assert_eq!(nvidia, ["pci:0000:04:00.0"]);

        // Five of the ten entries, once each: card2 is NVML's, cards 6-8 have
        // no driver bound, and card9 is not a PCI device.
        let all: Vec<&String> = amd.iter().chain(&intel).chain(&nvidia).collect();
        let unique: HashSet<&&String> = all.iter().collect();
        assert_eq!(all.len(), 5);
        assert_eq!(unique.len(), all.len(), "a device claimed by two backends");
        // The proprietary-driver card belongs to NVML, which is not scanned
        // here — nothing else may pick it up.
        assert!(!all.iter().any(|id| id.ends_with("0000:01:00.0")));
    }

    /// The BDF the card resolves to is what fdinfo reports as `drm-pdev`, and
    /// it is the whole of the device identity — so it has to survive the walk
    /// through `cardN/device`.
    #[test]
    fn card_scan_resolves_the_pci_address() {
        let root = testing::tri_vendor("scan-pdev");
        let cards = cards_with_driver(&testing::drm(&root), "0x1002", |d| d == "amdgpu");
        let (_, dev, _) = &cards[0];
        assert_eq!(pdev_of(dev).as_deref(), Some("0000:03:00.0"));
        assert_eq!(
            pci_device_id(pdev_of(dev).as_deref()).as_deref(),
            Some("pci:0000:03:00.0")
        );
    }

    /// Attribution is by PCI address *and* driver. A backend must never claim a
    /// client of a card it does not own just because the address collides with
    /// one of its own slots — on a mixed rig that draws an Intel process's row
    /// against an AMD card.
    #[test]
    fn clients_attribute_by_address_and_driver() {
        let dev = |pdev: &str, driver: &str| SweepDevice {
            pdev: Some(pdev.to_string()),
            driver: driver.to_string(),
        };
        let amd = [dev("0000:03:00.0", "amdgpu")];
        let intel = [dev("0000:00:02.0", "i915"), dev("0000:06:00.0", "xe")];

        let i915_client = parse_fdinfo(I915_FDINFO).unwrap();
        assert_eq!(client_device(&intel, &i915_client), Some(0));
        assert_eq!(
            client_device(&amd, &i915_client),
            None,
            "an Intel client must not land on the AMD backend's only card"
        );

        let amd_client = parse_fdinfo(AMD_FDINFO).unwrap();
        // Same address as the AMD card, wrong driver: a sibling backend's.
        let impostor = [dev("0000:75:00.0", "i915")];
        assert_eq!(client_device(&impostor, &amd_client), None);
        assert_eq!(
            client_device(&[dev("0000:75:00.0", "amdgpu")], &amd_client),
            Some(0)
        );

        // The second device of a backend is index 1 *within that backend* — the
        // composite re-bases it; an off-by-one here would be silent.
        let xe_client = parse_fdinfo(XE_FDINFO).unwrap();
        assert_eq!(
            client_device(
                &[dev("0000:00:02.0", "xe"), dev("0000:03:00.0", "xe")],
                &xe_client
            ),
            Some(1)
        );
        // No address at all is unattributable, not device 0.
        let mut anon = parse_fdinfo(AMD_FDINFO).unwrap();
        anon.pdev = None;
        assert_eq!(client_device(&[dev("0000:75:00.0", "amdgpu")], &anon), None);
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

    /// The fdinfo sweep sums regions it read, so its zero is a reading. Unlike
    /// the NVML and PDH paths, this one must not degrade to `None` — a client
    /// naming no memory region holds nothing, and `n/a` there would hide a
    /// real idle process behind an "unknown".
    #[test]
    fn a_swept_row_reports_zero_memory_as_a_measurement() {
        assert_eq!(build_proc(1, 0, 0.0, 0, false).gpu_mem_bytes, Some(0));
        assert_eq!(
            build_proc(2, 0, 50.0, 1 << 30, true).gpu_mem_bytes,
            Some(1 << 30)
        );
    }
}
