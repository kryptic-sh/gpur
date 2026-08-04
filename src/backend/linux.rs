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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

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

/// Every DRM client of every process this user can read, as one walk of
/// `/proc`, with no vendor filtering applied.
///
/// Vendor-agnostic on purpose. The walk is the expensive half — a readdir,
/// a stat per fd and a read per DRM fd, measured at 4.2 ms over 588 pids —
/// while deciding which card a client belongs to is a string compare. Each
/// Linux backend used to do its own full walk and throw away every client
/// belonging to a sibling, so an AMD + Intel + nouveau box paid that 4.2 ms
/// three times a tick to produce the identical set of clients. One walk feeds
/// all of them.
pub struct ProcSnapshot {
    /// (pid, client). A process appears once per DRM client it holds.
    pub clients: Vec<(u32, FdClient)>,
    /// When the walk finished.
    ///
    /// Load-bearing, not diagnostic: the fdinfo engine counters are cumulative,
    /// so a utilization is a counter delta over the wall-clock between the two
    /// readings. Attribution now runs later than collection and by a margin
    /// that varies, so measuring against `Instant::now()` at attribution time
    /// would divide this snapshot's counters by an interval they were never
    /// sampled over. Every caller stamps its delta state with this instead.
    pub at: Instant,
    /// Increments once per completed walk. A backend re-attributes only when
    /// this moves; see [`SweepCursor`].
    pub seq: u64,
}

/// Attribute a snapshot's clients to a backend's devices and aggregate.
/// Backends differ only in how one client's utilization and memory are
/// derived, which is what `per_client(pid, gpu, client)` supplies — it gets
/// the pid because both backends key their counter-delta state on
/// (pid, drm-client-id).
///
/// A client with several duplicated fds appears once per fd; it is counted
/// once, keyed on (pid, drm-client-id).
pub fn sweep_clients<F>(snap: &ProcSnapshot, devices: &[SweepDevice], mut per_client: F) -> Sweep
where
    F: FnMut(u32, usize, &FdClient) -> ClientSample,
{
    let mut sweep = Sweep::default();
    // (pid, gpu) -> aggregated stats across that process's DRM clients.
    let mut agg: HashMap<(u32, usize), (f64, u64, bool)> = HashMap::new();

    for (pid, client) in &snap.clients {
        let pid = *pid;
        let Some(gpu) = client_device(devices, client) else {
            continue;
        };
        if !sweep.seen.insert((pid, client.id)) {
            continue;
        }
        let s = per_client(pid, gpu, client);
        *sweep.util.entry(gpu).or_default() += s.util_pct;
        *sweep.video_util.entry(gpu).or_default() += s.video_pct;
        let e = agg.entry((pid, gpu)).or_insert((0.0, 0, false));
        e.0 += s.util_pct;
        e.1 += s.mem_bytes;
        e.2 |= s.graphics;
    }

    sweep.procs = agg
        .into_iter()
        .map(|((pid, gpu_index), (util, mem, graphics))| {
            build_proc(pid, gpu_index, util, mem, graphics)
        })
        .collect();
    sweep
}

/// Walk every readable process's DRM clients once.
fn scan_proc(seq: u64) -> ProcSnapshot {
    let mut clients = Vec::new();
    for pid in proc_pids() {
        // One walk of each pid's fd dir, however many drivers are in play: the
        // readdir+stat+read cost is per fd, so a pass per driver name paid it
        // again for every fd the process holds.
        clients.extend(drm_clients(pid).into_iter().map(|c| (pid, c)));
    }
    ProcSnapshot {
        clients,
        at: Instant::now(),
        seq,
    }
}

/// How long a caller waits for the first walk before giving up on it.
///
/// Only a caller that has nothing at all to fall back on ever waits, and only
/// because there is genuinely nothing to show until one walk has finished. The
/// bound exists so that a worker that never publishes — killed, or wedged on a
/// pathological `/proc` — costs one late frame rather than a frozen UI.
const FIRST_SCAN_WAIT: Duration = Duration::from_secs(2);

/// Walks requested more often than this are served the previous snapshot
/// instead; the worker never starts one sooner. Bounds the sweep's CPU cost
/// when the poll interval is shorter than the walk (a busy box at a fast tick
/// would otherwise re-walk `/proc` at the poll rate forever), and only widens
/// the measurement window — utilization is a counter delta over the actual
/// walk-to-walk interval, so a slower walk rate never corrupts it.
const MIN_WALK_INTERVAL: Duration = Duration::from_millis(200);

/// The `/proc` walk, moved off the render thread.
///
/// The scan is synchronous I/O over every process on the machine, and it used
/// to run inside `App::poll` — so `event::poll` could not run while it did and
/// keystrokes queued behind it. On an ordinary desktop that is invisible; on a
/// node with thousands of processes at `--tick-ms 100` the walk can outlast the
/// tick, and the UI stops answering the keyboard.
///
/// So a worker thread owns the walk and the render thread only ever reads the
/// most recent finished one. The cost is that a snapshot is up to one poll old
/// by the time it is drawn. That is a delay, not an error: [`ProcSnapshot::at`]
/// stamps when the counters were actually read, so the utilizations derived
/// from it still cover the interval they were sampled over.
///
/// Not every caller wants that trade — see [`ProcScanner::set_synchronous`].
pub struct ProcScanner {
    state: Mutex<ScanState>,
    /// Signals the worker that someone wants a fresh walk.
    wanted: Condvar,
    /// Signals waiters that a walk has been published.
    published: Condvar,
    /// Walk on the calling thread and return only when it is done. See
    /// [`ProcScanner::set_synchronous`].
    synchronous: AtomicBool,
}

#[derive(Default)]
struct ScanState {
    latest: Option<Arc<ProcSnapshot>>,
    /// A walk has been asked for and not yet started.
    wanted: bool,
    /// The worker is mid-walk. Requests arriving while it is are absorbed
    /// rather than queued, so a walk that outlasts the tick cannot make the
    /// worker start another the moment it finishes.
    walking: bool,
    /// The highest sequence number reserved for a walk. Minted under the lock
    /// by `next_seq`; a snapshot can publish with a number below a later
    /// reservation, so this is a reservation counter, not the newest published
    /// seq.
    seq: u64,
}

impl ProcScanner {
    /// The one scanner, and the one worker thread, for this process. Shared
    /// rather than per backend: two backends asking on the same tick is
    /// exactly the duplicate walk this exists to remove.
    pub fn shared() -> &'static Arc<ProcScanner> {
        static SCANNER: OnceLock<Arc<ProcScanner>> = OnceLock::new();
        SCANNER.get_or_init(|| {
            let scanner = Arc::new(ProcScanner::new());
            scanner.spawn_worker();
            scanner
        })
    }

    fn new() -> ProcScanner {
        ProcScanner {
            state: Mutex::new(ScanState {
                // The first walk is wanted before anyone asks for it, so the
                // worker starts on it while the UI is still drawing its first
                // frame rather than after the first poll asks.
                wanted: true,
                ..ScanState::default()
            }),
            wanted: Condvar::new(),
            published: Condvar::new(),
            synchronous: AtomicBool::new(false),
        }
    }

    /// Start the worker, degrading to synchronous mode if the OS refuses the
    /// thread.
    ///
    /// Without that fallback a refused spawn is worse than never having had a
    /// worker: nothing would ever publish, so every `latest` would wait out
    /// `FIRST_SCAN_WAIT` and return nothing, and the process table would be
    /// permanently empty behind a UI that stalls once per tick. Walking on the
    /// calling thread is what this code did before the worker existed — a UI
    /// that stutters beats one that hangs and shows nothing.
    fn spawn_worker(self: &Arc<Self>) {
        let scanner = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("gpur-proc-sweep".into())
            .spawn(move || scanner.run())
            .is_ok();
        self.on_spawn_outcome(spawned);
    }

    /// The refused-spawn degradation, split out so a test can force the
    /// branch that only thread exhaustion otherwise reaches. A worker the OS
    /// refused means nothing will ever publish, so every `latest` would wait
    /// out `FIRST_SCAN_WAIT` and return nothing — the process table would be
    /// permanently empty behind a UI that stalls once per tick. Walking on
    /// the calling thread is what this code did before the worker existed: a
    /// UI that stutters beats one that hangs and shows nothing.
    fn on_spawn_outcome(self: &Arc<Self>, spawned: bool) {
        if !spawned {
            self.set_synchronous(true);
        }
    }

    /// Walk on the calling thread instead, blocking until it finishes.
    ///
    /// For `--once` and `--json`, which have no UI to keep responsive and are
    /// asked for one measurement rather than a stream of them. Those take two
    /// polls a fixed sleep apart and report the delta between them, so the two
    /// walks must bracket that sleep — a walk that merely happens to be the
    /// newest one finished would leave the sleep outside the interval being
    /// measured, and the answer would cover a few milliseconds of the wrong
    /// moment.
    ///
    /// Also what the hardware tests use, for the same reason: a test that opens
    /// a DRM client and polls is asserting on that client, so it needs the walk
    /// to happen after the open rather than whenever a worker got to it.
    pub fn set_synchronous(&self, on: bool) {
        self.synchronous.store(on, Ordering::Relaxed);
    }

    /// The newest finished walk, and a request for another.
    ///
    /// Never blocks once a walk has landed — that is the whole point — so the
    /// snapshot it returns may be the same one as last poll. Callers detect
    /// that through `seq` rather than re-deriving identical numbers; see
    /// [`SweepCursor`].
    pub fn latest(&self) -> Option<Arc<ProcSnapshot>> {
        if self.synchronous.load(Ordering::Relaxed) {
            return Some(self.scan_here());
        }
        let mut st = self.lock();
        // Pace the worker: a walk younger than MIN_WALK_INTERVAL, or one still
        // in flight, already covers this poll — requesting another would walk
        // `/proc` at the poll rate, which on a busy box at a fast tick spends a
        // whole core re-reading fds. The caller detects the unchanged seq and
        // redraws the previous figures, as it does for any poll that finds no
        // new walk.
        let fresh = st
            .latest
            .as_ref()
            .is_some_and(|s| Instant::now().saturating_duration_since(s.at) < MIN_WALK_INTERVAL);
        if !st.walking && !fresh {
            st.wanted = true;
            self.wanted.notify_one();
        }
        if st.latest.is_none() {
            // Nothing has ever been published: there is no stale answer to
            // fall back on, so this one call waits for the first walk.
            let (guard, _) = self
                .published
                .wait_timeout_while(st, FIRST_SCAN_WAIT, |st| st.latest.is_none())
                .unwrap_or_else(|e| e.into_inner());
            st = guard;
        }
        st.latest.clone()
    }

    /// The next walk's sequence number, minted under the lock so the worker and
    /// the synchronous walk can never reserve the same number. A snapshot is
    /// published with the number minted for it; nothing ever writes the counter
    /// back, so a slow walk cannot regress it past what a sibling producer
    /// already reserved.
    fn next_seq(&self) -> u64 {
        let mut st = self.lock();
        st.seq += 1;
        st.seq
    }

    /// Walk on the calling thread and publish, advancing `seq` exactly as the
    /// worker does so a cursor cannot tell the two apart.
    fn scan_here(&self) -> Arc<ProcSnapshot> {
        let seq = self.next_seq();
        let snap = Arc::new(scan_proc(seq));
        self.lock().latest = Some(Arc::clone(&snap));
        snap
    }

    /// Locks through a poisoned mutex rather than panicking. The only state
    /// behind it is a snapshot and two counters — a thread that died mid-update
    /// leaves them consistent, and refusing to read them afterwards would take
    /// the process table down with the worker.
    fn lock(&self) -> std::sync::MutexGuard<'_, ScanState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The worker loop: walk when asked, publish, wait to be asked again.
    /// Idle between polls, so a paused UI costs nothing.
    fn run(self: Arc<Self>) {
        loop {
            let mut st = self.lock();
            while !st.wanted {
                st = self.wanted.wait(st).unwrap_or_else(|e| e.into_inner());
            }
            st.wanted = false;
            st.walking = true;
            drop(st);

            let seq = self.next_seq();
            let snap = Arc::new(scan_proc(seq));

            let mut st = self.lock();
            st.latest = Some(snap);
            st.walking = false;
            self.published.notify_all();
        }
    }
}

/// Test hooks: a scanner with no worker thread, whose walks are supplied by
/// hand. The worker publishes when it finishes a walk of the real `/proc`,
/// which is neither deterministic nor a machine-independent input — so the
/// behaviour that depends on *which* walk a caller gets is driven from here
/// instead, and the worker gets its own test.
#[cfg(test)]
impl ProcScanner {
    pub fn detached() -> Arc<ProcScanner> {
        Arc::new(ProcScanner::new())
    }

    /// A scanner wired the way production wires it — its own worker walking
    /// the real `/proc` — but separate from the shared one, whose mode the
    /// hardware tests have already pinned to synchronous.
    pub fn detached_with_worker() -> Arc<ProcScanner> {
        let scanner = ProcScanner::detached();
        scanner.spawn_worker();
        scanner
    }

    /// Publish a walk as a finished one, advancing `seq` exactly as both real
    /// producers do.
    pub fn publish(&self, clients: Vec<(u32, FdClient)>, at: Instant) {
        let mut st = self.lock();
        st.seq += 1;
        st.latest = Some(Arc::new(ProcSnapshot {
            clients,
            at,
            seq: st.seq,
        }));
    }
}

/// One backend's place in the stream of snapshots: hands back a walk only
/// when it is one this backend has not already attributed.
///
/// Two backends on one machine share the scanner but keep their own cursor,
/// since each has its own delta state to advance. Returning `None` is not an
/// error — it means no new walk has finished since the last poll, and the
/// caller must redraw its previous reading. Re-deriving the same snapshot
/// instead would divide the same counters by a zero interval and report every
/// process as idle, so a slow walk would render as a GPU that flickers to 0%.
pub struct SweepCursor {
    scanner: Arc<ProcScanner>,
    seen_seq: Option<u64>,
}

impl Default for SweepCursor {
    fn default() -> Self {
        SweepCursor::on(Arc::clone(ProcScanner::shared()))
    }
}

impl SweepCursor {
    /// A cursor over a specific scanner. The backends all take the shared one;
    /// this is what lets a test drive a scanner of its own.
    pub fn on(scanner: Arc<ProcScanner>) -> Self {
        SweepCursor {
            scanner,
            seen_seq: None,
        }
    }

    /// The newest walk, if it is newer than the one this cursor last returned.
    pub fn next(&mut self) -> Option<Arc<ProcSnapshot>> {
        let snap = self.scanner.latest()?;
        if self.seen_seq == Some(snap.seq) {
            return None;
        }
        self.seen_seq = Some(snap.seq);
        Some(snap)
    }
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

/// The hwmon child directory of a card, preferring the one whose `name` file
/// identifies the device's bound driver. amdgpu/i915/xe/nouveau register one
/// hwmon per device in practice, so the first child in readdir order has never
/// been the wrong one; a card that ever exposes several (a second chip, a
/// backlight) must not have its sensors picked by directory order. Falls back
/// to the first child when none names the driver.
pub fn hwmon_dir(dev: &Path, driver: &str) -> Option<PathBuf> {
    let dirs: Vec<PathBuf> = fs::read_dir(dev.join("hwmon"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.iter()
        .find(|p| {
            read_trim(&p.join("name"))
                .is_some_and(|n| n == driver || n.starts_with(&format!("{driver}_")))
        })
        .cloned()
        .or_else(|| dirs.into_iter().next())
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

/// Current negotiated PCIe link as (gen, width). These are PCI-core
/// attributes (`drivers/pci/pci-sysfs.c`), identical for every vendor's
/// endpoint, so one reader serves all sysfs backends. Each element is None
/// when its file is missing or unparseable — e.g. on integrated devices,
/// which are not PCIe endpoints in any meaningful sense.
pub fn pcie_current_link(dev: &Path) -> (Option<u8>, Option<u32>) {
    let speed = |f: &str| read_trim(&dev.join(f)).as_deref().and_then(gts_to_gen);
    let width = |f: &str| read_trim(&dev.join(f)).and_then(|w| w.parse().ok());
    (speed("current_link_speed"), width("current_link_width"))
}

/// Maximum supported PCIe link as (gen, width), from the same PCI-core
/// attributes as the current link. Each element is None when its file is
/// missing or unparseable — e.g. on integrated devices, which are not PCIe
/// endpoints in any meaningful sense.
///
/// The maximum is a fixed capability, resolved once at scan time and cached
/// per device rather than re-read every poll.
pub fn pcie_max_link(dev: &Path) -> (Option<u8>, Option<u32>) {
    let speed = |f: &str| read_trim(&dev.join(f)).as_deref().and_then(gts_to_gen);
    let width = |f: &str| read_trim(&dev.join(f)).and_then(|w| w.parse().ok());
    (speed("max_link_speed"), width("max_link_width"))
}

/// Current and maximum PCIe link as (gen, width, max gen, max width) — the
/// four-field view for callers that want everything at once. These are
/// PCI-core attributes (`drivers/pci/pci-sysfs.c`), identical for every
/// vendor's endpoint, so one reader serves all sysfs backends. Each element
/// is None when its file is missing or unparseable — e.g. on integrated
/// devices, which are not PCIe endpoints in any meaningful sense.
#[cfg(test)]
pub fn pcie_link(dev: &Path) -> (Option<u8>, Option<u32>, Option<u8>, Option<u32>) {
    let (cur_gen, cur_width) = pcie_current_link(dev);
    let (max_gen, max_width) = pcie_max_link(dev);
    (cur_gen, cur_width, max_gen, max_width)
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

/// "Navi 31 [Radeon RX 7900 XT/7900 XTX/7900M]" -> "Radeon RX 7900 XT/7900 XTX/7900M".
/// pci.ids lists most modern parts as "codename [marketing name]"; the bracketed
/// name is what the user recognises, so prefer it and fall back to the whole
/// entry (the codename) when there is no bracket.
pub fn marketing_name(entry: &str) -> String {
    entry
        .rsplit_once('[')
        .and_then(|(_, rest)| rest.strip_suffix(']'))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| entry.to_string())
}

/// Resolve a card's marketing name from pci.ids with a readable fallback.
pub fn card_name(dev: &Path, idx: u32, vendor_hex: &str, fallback_brand: &str) -> String {
    let device_id = read_trim(&dev.join("device")).unwrap_or_default();
    pci_ids()
        .and_then(|ids| pci_device_name(ids, vendor_hex, device_id.trim_start_matches("0x")))
        .map(|name| marketing_name(&name))
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

    /// The link readers split by direction: current reads only the negotiated
    /// pair, max only the capability — so a scan can cache the maxes and a poll
    /// re-read only the currents.
    #[test]
    fn pcie_link_splits_current_from_max() {
        let dev = scratch("pcie-halves");
        fs::write(dev.join("current_link_speed"), "8.0 GT/s PCIe\n").unwrap();
        fs::write(dev.join("current_link_width"), "8\n").unwrap();
        assert_eq!(pcie_current_link(&dev), (Some(3), Some(8)));
        assert_eq!(pcie_max_link(&dev), (None, None));
        fs::write(dev.join("max_link_speed"), "16.0 GT/s PCIe\n").unwrap();
        fs::write(dev.join("max_link_width"), "16\n").unwrap();
        assert_eq!(pcie_max_link(&dev), (Some(4), Some(16)));
        assert_eq!(pcie_link(&dev), (Some(3), Some(8), Some(4), Some(16)));
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

    /// A card with several hwmon children (a second chip, a backlight) must
    /// read its sensors from the child the driver itself registered, not from
    /// whichever child readdir happens to list first.
    #[test]
    fn hwmon_dir_prefers_the_driver_named_child_over_readdir_order() {
        let dev = scratch("hwmon-driver");
        let hwmon = dev.join("hwmon");
        fs::create_dir_all(hwmon.join("hwmon0")).unwrap();
        fs::create_dir_all(hwmon.join("hwmon1")).unwrap();
        // The foreign child is created first; the driver's own hwmon second.
        fs::write(hwmon.join("hwmon0/name"), "backlight\n").unwrap();
        fs::write(hwmon.join("hwmon1/name"), "amdgpu\n").unwrap();
        assert_eq!(hwmon_dir(&dev, "amdgpu"), Some(hwmon.join("hwmon1")));
    }

    /// amdgpu on pre-GCN cards names its hwmon `amdgpu_legacy`; the `_`-suffix
    /// rule is what keeps those cards on their sensors instead of the fallback.
    #[test]
    fn hwmon_dir_matches_the_legacy_driver_name() {
        let dev = scratch("hwmon-legacy");
        let hwmon = dev.join("hwmon");
        fs::create_dir_all(hwmon.join("hwmon0")).unwrap();
        fs::write(hwmon.join("hwmon0/name"), "amdgpu_legacy\n").unwrap();
        assert_eq!(hwmon_dir(&dev, "amdgpu"), Some(hwmon.join("hwmon0")));
    }

    /// No child naming the driver is today's behaviour, pinned: the first
    /// child in readdir order, not None — a card that merely lacks a `name`
    /// file must still report its sensors.
    #[test]
    fn hwmon_dir_falls_back_to_the_first_child_when_none_names_the_driver() {
        let dev = scratch("hwmon-fallback");
        let hwmon = dev.join("hwmon");
        fs::create_dir_all(hwmon.join("hwmon0")).unwrap();
        fs::create_dir_all(hwmon.join("hwmon1")).unwrap();
        fs::write(hwmon.join("hwmon0/name"), "backlight\n").unwrap();
        fs::write(hwmon.join("hwmon1/name"), "another_chip\n").unwrap();
        let first = fs::read_dir(&hwmon)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.is_dir())
            .unwrap();
        assert_eq!(hwmon_dir(&dev, "amdgpu"), Some(first));
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
        // The proprietary-driver card belongs to NVML when NVML is alive —
        // nothing else may pick it up. The NVML-absent fallback in nvidia.rs
        // claims it.
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

    #[test]
    fn marketing_name_prefers_the_bracketed_marketing_name() {
        assert_eq!(
            marketing_name("Navi 31 [Radeon RX 7900 XT/7900 XTX/7900M]"),
            "Radeon RX 7900 XT/7900 XTX/7900M"
        );
        assert_eq!(marketing_name("DG2 [Arc A770]"), "Arc A770");
        assert_eq!(
            marketing_name("Vega 10 XT [Radeon RX Vega 64]"),
            "Radeon RX Vega 64"
        );
    }

    #[test]
    fn marketing_name_falls_back_without_a_bracket() {
        assert_eq!(
            marketing_name("VGA compatible controller"),
            "VGA compatible controller"
        );
        assert_eq!(marketing_name(""), "");
    }

    #[test]
    fn marketing_name_keeps_an_empty_codename_out_of_the_result() {
        assert_eq!(marketing_name("[Radeon RX 7900 XT]"), "Radeon RX 7900 XT");
    }

    /// A walk carrying `clients`, stamped now.
    fn snapshot(clients: Vec<(u32, FdClient)>) -> ProcSnapshot {
        ProcSnapshot {
            clients,
            at: Instant::now(),
            seq: 1,
        }
    }

    fn sweep_dev(pdev: &str, driver: &str) -> SweepDevice {
        SweepDevice {
            pdev: Some(pdev.to_string()),
            driver: driver.to_string(),
        }
    }

    /// Every client of one process on one card folds into a single row, and
    /// the per-device totals are the sum over the clients of every process.
    /// Only reachable as a test now that the walk is an input rather than
    /// something this function goes and does.
    #[test]
    fn the_sweep_totals_per_device_and_rows_per_process() {
        let devices = [
            sweep_dev("0000:75:00.0", "amdgpu"),
            sweep_dev("0000:00:02.0", "i915"),
        ];
        let amd = || parse_fdinfo(AMD_FDINFO).unwrap();
        let i915 = || parse_fdinfo(I915_FDINFO).unwrap();
        // pid 10 holds two clients of card 0; pid 11 one of card 0 and one of
        // card 1. Client ids differ, so nothing here is a duplicate.
        let mut second = amd();
        second.id = 999;
        let snap = snapshot(vec![(10, amd()), (10, second), (11, amd()), (11, i915())]);

        // Each client contributes a fixed 10% and 1 MiB, so the sums below are
        // a count of the clients that reached each bucket.
        let sweep = sweep_clients(&snap, &devices, |_, _, _| ClientSample {
            util_pct: 10.0,
            video_pct: 1.0,
            mem_bytes: 1 << 20,
            graphics: true,
        });

        assert_eq!(sweep.util[&0], 30.0, "three amdgpu clients");
        assert_eq!(sweep.util[&1], 10.0, "one i915 client");
        assert_eq!(sweep.video_util[&0], 3.0);

        let mut rows: Vec<(u32, usize, Option<u64>)> = sweep
            .procs
            .iter()
            .map(|p| (p.pid, p.gpu_index, p.gpu_mem_bytes))
            .collect();
        rows.sort_unstable();
        assert_eq!(
            rows,
            [
                // pid 10's two clients of card 0 are one row, summed.
                (10, 0, Some(2 << 20)),
                (11, 0, Some(1 << 20)),
                // The same process on a second card is a second row.
                (11, 1, Some(1 << 20)),
            ]
        );
        assert_eq!(sweep.seen.len(), 4, "one entry per (pid, client id)");
    }

    /// A client reachable through several fds appears once per fd in the walk
    /// and must be counted once — otherwise a process that dup'd its DRM fd
    /// reads as using the card twice over.
    #[test]
    fn one_client_seen_twice_is_counted_once() {
        let devices = [sweep_dev("0000:75:00.0", "amdgpu")];
        let client = || parse_fdinfo(AMD_FDINFO).unwrap();
        let snap = snapshot(vec![(10, client()), (10, client())]);

        let mut calls = 0;
        let sweep = sweep_clients(&snap, &devices, |_, _, _| {
            calls += 1;
            ClientSample {
                util_pct: 10.0,
                mem_bytes: 1 << 20,
                ..ClientSample::default()
            }
        });

        assert_eq!(calls, 1, "the per-client closure ran for the duplicate");
        assert_eq!(sweep.util[&0], 10.0);
        assert_eq!(sweep.procs.len(), 1);
        assert_eq!(sweep.procs[0].gpu_mem_bytes, Some(1 << 20));
    }

    /// A walk carries every vendor's clients, since it is taken once for all
    /// of them. Each backend must take only its own — and must not even ask
    /// its closure about a sibling's client, which is where the delta state
    /// for a foreign client would be minted.
    #[test]
    fn a_backend_attributes_only_its_own_vendors_clients() {
        let devices = [sweep_dev("0000:75:00.0", "amdgpu")];
        let mut orphan = parse_fdinfo(AMD_FDINFO).unwrap();
        orphan.pdev = None;
        let snap = snapshot(vec![
            (10, parse_fdinfo(AMD_FDINFO).unwrap()),
            (11, parse_fdinfo(I915_FDINFO).unwrap()),
            (12, parse_fdinfo(XE_FDINFO).unwrap()),
            (13, orphan),
        ]);

        let mut seen_pids = Vec::new();
        let sweep = sweep_clients(&snap, &devices, |pid, _, _| {
            seen_pids.push(pid);
            ClientSample::default()
        });

        assert_eq!(seen_pids, [10]);
        assert_eq!(sweep.procs.len(), 1);
        assert_eq!(sweep.seen.len(), 1);
    }

    /// The cursor's whole job: a walk is attributed once. Handing the same one
    /// back would divide identical counters by a zero interval, and the card
    /// would flicker to 0% every time a walk ran late.
    #[test]
    fn a_cursor_attributes_each_walk_once() {
        let scanner = ProcScanner::detached();
        let mut cursor = SweepCursor::on(Arc::clone(&scanner));
        let now = Instant::now();

        scanner.publish(vec![(10, parse_fdinfo(AMD_FDINFO).unwrap())], now);
        let first = cursor.next().expect("a published walk");
        assert_eq!(first.clients.len(), 1);
        assert!(cursor.next().is_none(), "the same walk came back twice");
        assert!(cursor.next().is_none(), "and again");

        scanner.publish(Vec::new(), now + Duration::from_secs(1));
        let second = cursor.next().expect("the newer walk");
        assert!(second.seq > first.seq);
        assert!(cursor.next().is_none());

        // Two backends share one scanner and each attributes every walk, so a
        // second cursor is not affected by the first having consumed it.
        let mut other = SweepCursor::on(Arc::clone(&scanner));
        assert_eq!(other.next().map(|s| s.seq), Some(second.seq));
    }

    /// The walk-count half of "one walk serves every vendor": the amdgpu and
    /// Intel backends each hold a `SweepCursor::default()` — the shared scanner —
    /// so two consumers polling one worker must cost one walk per pacing interval,
    /// not one per consumer. Sharing is what makes a mixed AMD+Intel box walk
    /// `/proc` once a tick instead of twice; the two-LIVE-backends observation
    /// still needs a mixed box, but the count property is checkable anywhere with
    /// two cursors over one worker-backed scanner.
    #[test]
    fn two_cursors_over_one_worker_share_each_walk() {
        let scanner = ProcScanner::detached_with_worker();
        let mut a = SweepCursor::on(Arc::clone(&scanner));
        let mut b = SweepCursor::on(Arc::clone(&scanner));
        // Warm the first walk: the first request waits for it, and the loop below
        // measures only the walks published after this one.
        let _ = a.next();

        let deadline = Instant::now() + MIN_WALK_INTERVAL * 3;
        let mut walks: HashSet<u64> = HashSet::new();
        while Instant::now() < deadline {
            if let Some(s) = a.next() {
                walks.insert(s.seq);
            }
            if let Some(s) = b.next() {
                walks.insert(s.seq);
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        // One paced worker over ~3 intervals publishes at most ~4 walks total,
        // and BOTH cursors share them (dedup by seq). Two workers — one per
        // cursor, i.e. the sharing lost — would publish up to twice as many in
        // the same window, so a bound of 5 sits between the shared maximum and
        // the first non-shared value.
        assert!(
            walks.len() <= 5,
            "{} distinct walks served to two cursors in ~3 pacing intervals",
            walks.len()
        );
    }

    /// The worker path end to end: a thread does the walking, and successive
    /// requests come back as successively newer walks of the real `/proc`.
    #[test]
    fn the_worker_thread_publishes_successive_walks() {
        let scanner = ProcScanner::detached();
        scanner.spawn_worker();
        let mut cursor = SweepCursor::on(Arc::clone(&scanner));

        // The first call waits for the first walk, because there is nothing
        // to fall back on. Anything after it may or may not find a new walk
        // ready — that is the point of the worker — so take walks as they
        // land rather than assuming one per call.
        let first = cursor.next().expect("the worker published a first walk");
        let deadline = Instant::now() + FIRST_SCAN_WAIT;
        let mut walks = vec![first];
        while walks.len() < 3 && Instant::now() < deadline {
            if let Some(s) = cursor.next() {
                walks.push(s);
            }
        }

        assert_eq!(walks.len(), 3, "the worker stopped publishing");
        for w in walks.windows(2) {
            assert!(w[1].seq > w[0].seq, "a walk was published twice");
            assert!(w[1].at > w[0].at, "walks are not ordered in time");
        }
    }

    /// Synchronous mode is what `--once` rests on: the walk happens when the
    /// poll asks for it, so two polls a known interval apart bracket that
    /// interval. A caller must never be handed a walk older than its request.
    #[test]
    fn synchronous_mode_walks_on_the_calling_thread() {
        let scanner = ProcScanner::detached();
        scanner.set_synchronous(true);
        let mut cursor = SweepCursor::on(Arc::clone(&scanner));

        // No worker was spawned, so anything returned here was walked by this
        // thread; every call yields a walk newer than the last.
        let asked = Instant::now();
        let first = cursor.next().expect("walked here");
        let second = cursor.next().expect("walked here again");
        assert!(second.seq > first.seq, "the same walk was served twice");
        assert!(first.at >= asked, "served a walk older than the request");
        assert!(second.at > first.at);
    }

    /// The refused-spawn branch: an OS that will not give the worker a thread
    /// must degrade to walking on the polling thread, not leave the process
    /// table waiting out FIRST_SCAN_WAIT forever. Only reachable under real
    /// thread exhaustion, so the decision is split out and driven here.
    #[test]
    fn a_refused_worker_spawn_degrades_to_synchronous_mode() {
        let refused = ProcScanner::detached();
        refused.on_spawn_outcome(false);
        assert!(
            refused.synchronous.load(Ordering::Relaxed),
            "a refused spawn did not switch to synchronous mode"
        );
        // A successful spawn keeps the async worker.
        let accepted = ProcScanner::detached();
        accepted.on_spawn_outcome(true);
        assert!(
            !accepted.synchronous.load(Ordering::Relaxed),
            "a successful spawn switched to synchronous mode"
        );
    }

    /// The worker is paced: requests arriving faster than MIN_WALK_INTERVAL are
    /// served the previous walk, never a fresh one — a poll hammering the scanner
    /// must not walk `/proc` at the poll rate.
    #[test]
    fn the_worker_serves_paced_walks_not_one_per_request() {
        let scanner = ProcScanner::detached_with_worker();
        let mut cursor = SweepCursor::on(Arc::clone(&scanner));
        let first = cursor.next().expect("a first walk");
        // Request continuously for ~1.5 floors; a paced worker produces at most a
        // couple more walks, an unpaced one produces dozens.
        let deadline = Instant::now() + MIN_WALK_INTERVAL * 3 / 2;
        let mut walks = vec![first];
        while Instant::now() < deadline {
            if let Some(s) = cursor.next() {
                walks.push(s);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(walks.len() <= 4, "walked per request: {}", walks.len());
        for w in walks.windows(2) {
            let gap = w[1].at.saturating_duration_since(w[0].at);
            assert!(
                gap >= MIN_WALK_INTERVAL.saturating_sub(Duration::from_millis(100)),
                "walks {gap:?} apart"
            );
        }
    }

    /// The seq counter must never hand two walks the same number, even when the
    /// worker and the synchronous path interleave: the worker reserves its number
    /// at grab time and the publish never writes the counter back, so a slow
    /// first walk cannot regress it. Before the fix the worker grabbed `st.seq+1`
    /// without committing and wrote it back on publish, so a worker whose first
    /// walk finished after two synchronous polls made the next poll mint a
    /// duplicate seq — `SweepCursor::next`'s equality check saw a seq it had
    /// already consumed and skipped a genuinely fresh walk (a synchronous poll
    /// that returned `None`).
    #[test]
    fn the_seq_counter_never_regresses_across_worker_and_sync_walks() {
        for _ in 0..25 {
            let scanner = ProcScanner::detached();
            scanner.spawn_worker();
            // Let the worker's first, cold walk start before the polls do: it
            // runs ~2x slower than a warm poll walk, so it spans two of the
            // test's mints and its publish lands mid-sequence — the interleave
            // that regressed the counter. Without the head start the worker's
            // walk finishes just before the next mint and the bug never shows.
            std::thread::sleep(Duration::from_millis(1));
            scanner.set_synchronous(true);
            let mut cursor = SweepCursor::on(Arc::clone(&scanner));

            let mut last = 0u64;
            for _ in 0..4 {
                // In synchronous mode every call walks fresh, so None (a skipped
                // walk) and a non-increasing seq are both the bug.
                let snap = cursor
                    .next()
                    .expect("a synchronous poll always walks fresh");
                assert!(snap.seq > last, "seq regressed: {last} -> {}", snap.seq);
                last = snap.seq;
            }
        }
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
