use crate::backend::{GpuBackend, GpuSnapshot, ProcKind};
use crate::keys::Action;
use crate::theme::UiTheme;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use sysinfo::{
    MINIMUM_CPU_UPDATE_INTERVAL, Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind,
    Users,
};

const SPLASH_MS: u64 = 1500;
const STATUS_MS: u64 = 4000;

/// Poll-interval floor, shared by the CLI/config clamp and the `+` key.
/// Two different floors (50 vs 100) meant `--tick-ms 50` plus one `+`
/// *raised* the interval to 100 with no way back.
pub const MIN_TICK_MS: u64 = 50;

/// Poll-interval ceiling, shared by the CLI/config clamp and the `-` key. A
/// huge interval reads as a frozen monitor — the loop waits that long for its
/// first poll — so a value above this is clamped rather than honoured.
pub const MAX_TICK_MS: u64 = 10_000;

/// The startup poll-rate clamp: `--tick-ms`, the persisted rate and the config
/// value are unbounded above, so a huge one is brought back into range here.
pub fn clamp_tick_ms(v: u64) -> u64 {
    v.clamp(MIN_TICK_MS, MAX_TICK_MS)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Gpus,
    Procs,
}

/// Glyph set for graphs: braille needs good font coverage, block works on
/// most terminals, ascii works everywhere (Linux console, weird fonts).
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum GraphStyle {
    Braille,
    Block,
    Ascii,
}

impl GraphStyle {
    pub fn from_config(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "braille" => Some(Self::Braille),
            "block" => Some(Self::Block),
            "ascii" => Some(Self::Ascii),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    /// Typing in the process filter; raw keys go to the input buffer.
    Filter,
    /// Kill confirmation pending; y confirms, anything else cancels.
    Confirm,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum SortBy {
    #[default]
    GpuMem,
    GpuUtil,
    Cpu,
    HostMem,
    Pid,
}

impl SortBy {
    pub fn next(self) -> Self {
        match self {
            SortBy::GpuMem => SortBy::GpuUtil,
            SortBy::GpuUtil => SortBy::Cpu,
            SortBy::Cpu => SortBy::HostMem,
            SortBy::HostMem => SortBy::Pid,
            SortBy::Pid => SortBy::GpuMem,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SortBy::GpuMem => "gpu-mem",
            SortBy::GpuUtil => "gpu%",
            SortBy::Cpu => "cpu%",
            SortBy::HostMem => "host-mem",
            SortBy::Pid => "pid",
        }
    }
}

/// Per-device graph history. `None` is a sample the backend could not read,
/// kept distinct from a measured `0` all the way to the glyph: the waveform
/// draws an unknown column as a minimum sliver in the dim style rather than
/// in the gradient, and `mini_spark` leaves it blank.
#[derive(Default)]
pub struct History {
    pub util: Vec<Option<u64>>,
    /// Fill level of the pool the MEM meter shows — a dGPU's VRAM, an iGPU's
    /// share of system RAM. Named for the meter rather than for VRAM because
    /// on a unified-memory card there is no VRAM to plot.
    pub mem: Vec<Option<u64>>,
    pub power: Vec<Option<u64>>,
    pub temp: Vec<Option<u64>>,
}

/// What per-device state hangs off. Position is not identity: a device count
/// or order change reattaches one GPU's graphs and peaks to another, and no
/// amount of index bookkeeping in the backends can fix that.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DeviceKey {
    /// The backend's own opaque id — see [`GpuSnapshot::device_id`].
    Id(String),
    /// The backend cannot identify this device, so its slot is all there is.
    /// Session-only, never persisted: a bare position that outlives the
    /// process is exactly the bug this type replaced.
    Pos(usize),
}

/// One key per snapshot, in poll order. Ids must be unique within a poll —
/// two cards sharing a key would fold their samples into one history — so a
/// repeated id degrades to its position rather than colliding.
fn device_keys(gpus: &[GpuSnapshot]) -> Vec<DeviceKey> {
    let mut seen: HashSet<&str> = HashSet::new();
    gpus.iter()
        .enumerate()
        .map(|(i, g)| match g.device_id.as_deref() {
            Some(id) if seen.insert(id) => DeviceKey::Id(id.to_string()),
            _ => DeviceKey::Pos(i),
        })
        .collect()
}

/// Departed devices whose state is kept in case they come back — a driver
/// reset, an eGPU replugged, a card that missed one poll. Bounded so a long
/// session churning through devices cannot grow the maps without limit; the
/// least recently seen are dropped first.
const MAX_ABSENT_DEVICES: usize = 16;

/// Cap on folds written to `state.json`, for the same reason. Devices seen
/// this session win the cap.
const MAX_FOLDS_PERSISTED: usize = 32;

/// Full command line like nvtop; falls back to the process name for
/// kernel threads and stripped cmdlines.
fn command_of(p: &sysinfo::Process) -> String {
    let cmd = p
        .cmd()
        .iter()
        .map(|a| a.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    if cmd.trim().is_empty() {
        p.name().to_string_lossy().into_owned()
    } else {
        cmd
    }
}

/// Whether this poll should ask sysinfo for the CPU half of a process
/// refresh, given when it last asked.
///
/// `cpu_usage()` is a ratio: the process's own jiffy delta over the machine's
/// total jiffy delta read from `/proc/stat`. sysinfo refuses to re-read
/// `/proc/stat` more often than [`MINIMUM_CPU_UPDATE_INTERVAL`], but it does
/// re-read the process's own times on every refresh it is asked for — so
/// below that interval the numerator covers one tick while the denominator
/// still covers the last 200 ms, and the number that comes out is arithmetic
/// on two mismatched windows rather than a measurement. [`MIN_TICK_MS`] is 50
/// and `--tick-ms 50` is a capability this project kept deliberately, so at
/// the fast end four polls in five were producing exactly that.
///
/// Rationing the request rather than the poll is what keeps the column: on the
/// polls that skip it, sysinfo leaves the previously computed `cpu_usage()`
/// alone, so CPU% refreshes more slowly than the rest of the row instead of
/// blanking or lying.
fn cpu_sample_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.duration_since(last) >= MINIMUM_CPU_UPDATE_INTERVAL)
}

/// Running mean over the samples that actually carried a value. A sensor the
/// backend reports only intermittently must be averaged over its own readings,
/// not diluted toward zero by every sample that had none.
#[derive(Default)]
struct Mean {
    sum: f64,
    n: u64,
}

impl Mean {
    fn add(&mut self, v: f64) {
        self.sum += v;
        self.n += 1;
    }

    fn get(&self) -> Option<f64> {
        (self.n > 0).then(|| self.sum / self.n as f64)
    }
}

/// Fold `v` into a running peak that starts *unknown* rather than at 0.
fn peak(slot: &mut Option<f64>, v: f64) {
    *slot = Some(slot.map_or(v, |m| m.max(v)));
}

/// Running peak/mean per GPU since launch (HWiNFO-style session stats).
/// Every reading is optional: a backend with no thermal or power sensor would
/// otherwise report a peak of `0°C` / `0W`, which reads as a real measurement.
#[derive(Default)]
pub struct SessionStats {
    pub max_util_pct: Option<f64>,
    pub max_temp_c: Option<f64>,
    pub max_power_w: Option<f64>,
    util: Mean,
    power: Mean,
}

impl SessionStats {
    pub(crate) fn add(&mut self, g: &GpuSnapshot) {
        if let Some(u) = g.utilization_pct {
            peak(&mut self.max_util_pct, u);
            self.util.add(u);
        }
        if let Some(t) = g.temperature_c {
            peak(&mut self.max_temp_c, t);
        }
        if let Some(w) = g.power_w {
            peak(&mut self.max_power_w, w);
            self.power.add(w);
        }
    }

    pub fn avg_util_pct(&self) -> Option<f64> {
        self.util.get()
    }

    pub fn avg_power_w(&self) -> Option<f64> {
        self.power.get()
    }
}

/// Identify a container from /proc/<pid>/cgroup content: docker, podman,
/// cri-containerd (k8s), and crio scopes, cgroup v1 and v2 layouts.
#[cfg(any(target_os = "linux", test))]
fn container_of_cgroup(text: &str) -> Option<String> {
    for line in text.lines() {
        let path = line.rsplit(':').next().unwrap_or("");
        for (marker, runtime) in [
            ("docker-", "docker"),
            ("libpod-", "podman"),
            ("cri-containerd-", "k8s"),
            ("crio-", "k8s"),
        ] {
            if let Some(rest) = path.split('/').find_map(|seg| seg.strip_prefix(marker)) {
                let id: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_hexdigit())
                    .take(12)
                    .collect();
                if id.len() == 12 {
                    return Some(format!("{runtime}:{id}"));
                }
            }
        }
        // cgroup v1 flat layout: .../docker/<id>
        if let Some(idx) = path.find("/docker/") {
            let id: String = path[idx + 8..]
                .chars()
                .take_while(|c| c.is_ascii_hexdigit())
                .take(12)
                .collect();
            if id.len() == 12 {
                return Some(format!("docker:{id}"));
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn container_of_pid(pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    container_of_cgroup(&text)
}

#[cfg(not(target_os = "linux"))]
fn container_of_pid(_pid: u32) -> Option<String> {
    None
}

/// Resolve a pid's container id through a cache keyed on (pid, start time):
/// the cgroup path is ~static per process, so a fresh `/proc/<pid>/cgroup`
/// read per poll is wasted I/O. The resolver runs only on a miss.
fn cached_container(
    cache: &mut HashMap<(u32, u64), Option<String>>,
    pid: u32,
    start_time: u64,
    read: impl FnOnce() -> Option<String>,
) -> Option<String> {
    cache.entry((pid, start_time)).or_insert_with(read).clone()
}

/// One row of the process table: GPU stats + host-side enrichment.
#[derive(Clone, serde::Serialize)]
pub struct ProcRow {
    pub pid: u32,
    pub gpu_index: usize,
    pub kind: ProcKind,
    pub gpu_util_pct: Option<f64>,
    /// GPU memory held, straight off [`GpuProcess::gpu_mem_bytes`]. `None`
    /// means the figure could not be read — the whole of NVIDIA-under-WDDM,
    /// i.e. ordinary consumer Windows — and must render and serialize as
    /// unknown. `Some(0)` stays a measurement: this process holds nothing.
    pub gpu_mem_bytes: Option<u64>,
    pub user: String,
    /// Host CPU share. `None` only when the pid resolved to nothing at all:
    /// `sysinfo`'s `cpu_usage()` is a real reading, so a resolved process
    /// that happens to be asleep is `Some(0.0)`, not unknown.
    pub cpu_pct: Option<f32>,
    /// Host RSS, on the same terms as [`Self::cpu_pct`].
    pub host_mem_bytes: Option<u64>,
    pub command: String,
    /// Container runtime + short id ("docker:ab12cd34ef56"), Linux only.
    pub container: Option<String>,
}

impl ProcRow {
    /// Whether the column `by` sorts on is unreadable for this row rather
    /// than zero. `SortBy::Pid` is never unknown — every row has one.
    fn is_unmeasured(&self, by: SortBy) -> bool {
        match by {
            SortBy::GpuUtil => self.gpu_util_pct.is_none(),
            SortBy::GpuMem => self.gpu_mem_bytes.is_none(),
            SortBy::Cpu => self.cpu_pct.is_none(),
            SortBy::HostMem => self.host_mem_bytes.is_none(),
            SortBy::Pid => false,
        }
    }
}

/// Order an unreadable figure below a readable one; `None` when the two
/// sides are equally (un)known and the caller should compare the values.
///
/// A metric the backend could not read is unmeasured, not idle or empty, so
/// it has no place on the number line the other rows are sorted along.
/// Folding it in as 0 would hand it the *top* of an ascending sort — the
/// slot that reads "quietest process" — so callers must apply this before
/// any ascending/descending flip, which sinks it in both directions.
fn unmeasured_last(a_unknown: bool, b_unknown: bool) -> Option<std::cmp::Ordering> {
    match (a_unknown, b_unknown) {
        (false, true) => Some(std::cmp::Ordering::Less),
        (true, false) => Some(std::cmp::Ordering::Greater),
        _ => None,
    }
}

/// Parse `/proc/<pid>/stat` field 22 (process start time in clock ticks since
/// boot) from a raw stat line. The comm field (field 2) is parenthesized and
/// may itself contain spaces and `)` characters, so the comm closer is the
/// LAST `)`; everything after it is field 3 onwards. Field 22 is the 19th
/// whitespace-separated token after field 3.
#[cfg(target_os = "linux")]
fn parse_start_ticks(stat: &str) -> Option<u64> {
    let after_comm = stat.rsplit_once(')')?.1;
    after_comm.split_whitespace().nth(19)?.parse().ok()
}

/// Read `/proc/<pid>/stat` field 22 for a live local process.
#[cfg(target_os = "linux")]
fn proc_start_ticks(pid: u32) -> Option<u64> {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| parse_start_ticks(&stat))
}

/// A signal awaiting y/N confirmation. `start_time` pins the identity of the
/// target: a pid alone is not one, since the process can exit between the
/// prompt and the keystroke and the number be handed to something else.
pub struct PendingKill {
    pub pid: u32,
    /// `sysinfo` start time as read when the dialog opened.
    pub start_time: u64,
    /// SIGKILL rather than SIGTERM.
    pub force: bool,
    /// Truncated command line, for the dialog and the result message.
    pub command: String,
    /// Raw `/proc/<pid>/stat` field 22 (process start time in clock ticks
    /// since boot), captured when the dialog opened. Finer than the
    /// seconds-resolution `start_time`, and what the pidfd fast path re-reads
    /// to catch a same-second PID reuse.
    #[cfg(target_os = "linux")]
    pub start_ticks: Option<u64>,
}

/// UI state persisted across runs (folded cards, sort, poll rate) — the
/// tikr session.json pattern: auto-saved on clean quit into the cache dir,
/// never a config file.
#[derive(serde::Serialize, serde::Deserialize, Default)]
pub struct UiState {
    /// Folded cards, by device id.
    ///
    /// Deliberately a different key from the pre-identity `folded:
    /// [<index>]`: that field held bare positions, and a position recorded by
    /// a previous run cannot be mapped onto this run's devices without
    /// risking folding a different GPU than the user folded. An old file
    /// therefore keeps its `folded` key, serde ignores it, and nothing else
    /// in the file is lost — the one cost is that folds reset once, on the
    /// upgrade run.
    #[serde(default)]
    pub folded_devices: Vec<String>,
    pub sort_by: SortBy,
    pub sort_desc: bool,
    pub tick_ms: u64,
    /// Whether `tick_ms` was chosen interactively (`+`/`-`). A rate that only
    /// came from the config or `--tick-ms` must not outrank a later config
    /// edit — that made an edited `config.toml` silently do nothing forever.
    /// `None` = written before this flag existed, provenance unknown.
    #[serde(default)]
    pub tick_ms_explicit: Option<bool>,
}

impl UiState {
    /// The persisted rate, when it should still outrank `config.toml`. A
    /// pre-flag file is honoured once so nobody's rate changes on upgrade;
    /// the next clean quit records the truth and config edits work again.
    pub fn sticky_tick_ms(&self) -> Option<u64> {
        (self.tick_ms_explicit != Some(false) && self.tick_ms > 0).then_some(self.tick_ms)
    }

    /// Recorded as a genuine key press, so it stays sticky indefinitely.
    pub fn tick_chosen_by_key(&self) -> bool {
        self.tick_ms_explicit == Some(true)
    }
}

fn state_path() -> Option<std::path::PathBuf> {
    hjkl_config::cache_dir("gpur")
        .ok()
        .map(|d| d.join("state.json"))
}

pub fn load_state() -> Option<UiState> {
    read_state(&state_path()?)
}

/// Split out from [`load_state`] so the round-trip is testable: `state_path`
/// answers with the real cache dir, which a test must not write into.
fn read_state(path: &std::path::Path) -> Option<UiState> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Where the half-written state lives until it is complete. A sibling of the
/// target, so the rename that publishes it stays within one filesystem — that
/// is the condition for rename being atomic, and crossing a mount point would
/// turn it into a copy that can tear like the write we are trying to avoid.
/// The pid keeps two gpur instances quitting at the same moment from writing
/// through each other's partial file and renaming the loser's bytes into
/// place.
fn state_temp_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// Publish `state` at `path` without ever exposing a truncated file.
///
/// `fs::write` truncates first, so a crash, a `kill -9` or a full disk part
/// way through left a short file that [`read_state`] can only give up on —
/// and it gives up silently, so every fold, sort and poll rate the user had
/// accumulated vanished with no message. The quit path is precisely where
/// that is likeliest: `install_signal_teardown` calls `std::process::exit`
/// from a signal thread and does not wait for this. Writing a temp file and
/// renaming it over the target means a reader sees either the whole old file
/// or the whole new one, never the seam.
///
/// The temp file is created 0600 where the platform has modes, and the target
/// inherits that through the rename. Nothing in `state.json` is secret — it is
/// folds, a sort key and a tick rate — so this is least privilege on a
/// per-user cache file for its own sake and for consistency with the log,
/// not a defence against a specific disclosure.
///
/// Best-effort throughout, per [`App::save_state`]: every failure path leaves
/// the previous `state.json` untouched and returns, and takes the temp file
/// with it so a repeatedly failing rename cannot litter the cache dir.
fn write_state(path: &std::path::Path, state: &UiState) {
    let Ok(json) = serde_json::to_string_pretty(state) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = state_temp_path(path);
    if write_private(&tmp, json.as_bytes()).is_err() || std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// `fs::write`, but the file is created owner-only where the platform has
/// file modes. Windows has no equivalent knob here and writes as before.
fn write_private(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(contents)
}

/// Startup knobs for [`App::new`], resolved from CLI + config.
pub struct AppOptions {
    pub tick_ms: u64,
    /// `tick_ms` came from an interactive choice (a persisted `+`/`-`), so it
    /// stays persisted. See [`UiState::tick_ms_explicit`].
    pub tick_explicit: bool,
    pub history_len: usize,
    pub no_splash: bool,
    pub graph_style: GraphStyle,
    /// How this session's backend was produced, for re-detection.
    pub source: crate::backend::BackendSource,
    pub log: Option<std::io::BufWriter<std::fs::File>>,
}

pub struct App {
    pub backend: Box<dyn GpuBackend>,
    pub gpus: Vec<GpuSnapshot>,
    /// Key of each entry in `gpus`, same order. Everything per-device hangs
    /// off these rather than off the index — see [`DeviceKey`].
    keys: Vec<DeviceKey>,
    history: HashMap<DeviceKey, History>,
    /// Per-GPU peaks/averages since launch.
    session: HashMap<DeviceKey, SessionStats>,
    /// Poll number each key was last present in, for eviction.
    last_seen: HashMap<DeviceKey, u64>,
    /// Successful polls so far; the clock `last_seen` counts on.
    polls: u64,
    pub history_len: usize,
    pub selected: usize,
    pub paused: bool,
    pub tick_ms: u64,
    pub theme: UiTheme,
    pub started: Instant,
    pub splash_path: Vec<(u8, u8, char)>,
    pub splash_skipped: bool,
    /// Filtered + sorted view of the process rows.
    pub procs: Vec<ProcRow>,
    /// GPUs folded to a one-line summary (digit keys toggle).
    folded: HashSet<DeviceKey>,
    /// First visible GPU card when the card list overflows (clamped in draw).
    pub gpu_scroll: usize,
    /// First visible process row when the table overflows (clamped in draw).
    pub proc_scroll: usize,
    /// Cursor row in the process table (index into procs).
    pub proc_sel: usize,
    /// Data rows the process pane showed last draw. Click hit-tests bound
    /// against this so a stray click can never select an off-screen row.
    pub proc_visible: usize,
    /// Pane rectangles from the last draw, for routing mouse wheel events.
    pub gpus_rect: ratatui::layout::Rect,
    pub proc_rect: ratatui::layout::Rect,
    /// Which pane arrow keys act on.
    pub focus: Focus,
    /// Last backend poll failure; shown in the header, cleared on success.
    pub poll_error: Option<String>,
    pub input_mode: InputMode,
    /// Committed process filter (substring, case-insensitive).
    pub filter: String,
    /// Live edit buffer while `input_mode == Filter`.
    pub filter_input: String,
    pub sort_by: SortBy,
    pub sort_desc: bool,
    /// Signal awaiting y/N confirmation.
    pub pending_kill: Option<PendingKill>,
    /// Transient header status (kill results), with expiry.
    pub status: Option<(String, Instant)>,
    /// Help overlay visible; any key dismisses.
    pub show_help: bool,
    /// (rect, gpu index) of each card drawn last frame, for click hit-tests.
    pub card_rects: Vec<(ratatui::layout::Rect, usize)>,
    /// Samples the widest graph needs (2 per braille cell) — retention must
    /// cover this or wide terminals get a permanently empty left region and
    /// a "stuck" pad boundary. Set by the renderer each frame.
    pub history_need: usize,
    pub graph_style: GraphStyle,
    /// The `--mock`/`--replay` choice this session's backend came from, kept
    /// so a re-detect repeats that same detection rather than a bare one.
    source: crate::backend::BackendSource,
    /// Consecutive poll failures; triggers a re-detect (driver reload).
    poll_failures: u32,
    /// JSONL sink: one line per successful poll when --log is given.
    log: Option<std::io::BufWriter<std::fs::File>>,
    /// Unfiltered process rows in a stable order; `procs` is the
    /// filtered+sorted view. Machine-readable output comes from here.
    pub all_procs: Vec<ProcRow>,
    /// Whether the poll rate was set interactively this session (or in a
    /// previous one) — see [`UiState::tick_ms_explicit`].
    tick_explicit: bool,
    sys: System,
    /// When the CPU half of the process refresh was last asked for, so it can
    /// be rationed to the interval sysinfo can actually measure over — see
    /// [`cpu_sample_due`]. `None` until the first poll.
    last_cpu_sample: Option<Instant>,
    users: Users,
    /// Resolved container ids per (pid, start time). The cgroup path is ~static
    /// per process, so re-reading `/proc/<pid>/cgroup` every poll for every row
    /// the Linux backends emit is wasted I/O. Keyed on the same seconds-resolution
    /// start time the kill path pins, so a recycled pid re-resolves; pruned each
    /// poll to the pids still on a GPU.
    proc_text: HashMap<(u32, u64), Option<String>>,
}

impl App {
    pub fn new(backend: Box<dyn GpuBackend>, theme: UiTheme, opts: AppOptions) -> Self {
        let AppOptions {
            tick_ms,
            tick_explicit,
            history_len,
            no_splash,
            graph_style,
            source,
            log,
        } = opts;
        Self {
            tick_explicit,
            graph_style,
            source,
            poll_failures: 0,
            log,
            backend,
            gpus: Vec::new(),
            keys: Vec::new(),
            history: HashMap::new(),
            session: HashMap::new(),
            last_seen: HashMap::new(),
            polls: 0,
            history_len,
            selected: 0,
            paused: false,
            tick_ms,
            theme,
            started: Instant::now(),
            splash_path: crate::splash::build_path(),
            splash_skipped: no_splash,
            procs: Vec::new(),
            folded: HashSet::new(),
            gpu_scroll: 0,
            proc_scroll: 0,
            proc_sel: 0,
            proc_visible: 0,
            gpus_rect: ratatui::layout::Rect::default(),
            proc_rect: ratatui::layout::Rect::default(),
            focus: Focus::Gpus,
            poll_error: None,
            input_mode: InputMode::Normal,
            filter: String::new(),
            filter_input: String::new(),
            sort_by: SortBy::GpuMem,
            sort_desc: true,
            pending_kill: None,
            status: None,
            show_help: false,
            card_rects: Vec::new(),
            history_need: 0,
            all_procs: Vec::new(),
            sys: System::new(),
            last_cpu_sample: None,
            users: Users::new_with_refreshed_list(),
            proc_text: HashMap::new(),
        }
    }

    pub fn restore_state(&mut self, s: &UiState) {
        self.folded = s
            .folded_devices
            .iter()
            .cloned()
            .map(DeviceKey::Id)
            .collect();
        self.sort_by = s.sort_by;
        self.sort_desc = s.sort_desc;
    }

    /// Best-effort save on clean quit; silent on failure (a monitor must
    /// never refuse to exit over a full disk).
    pub fn save_state(&self) {
        let Some(path) = state_path() else { return };
        let state = UiState {
            folded_devices: self.persisted_folds(),
            sort_by: self.sort_by,
            sort_desc: self.sort_desc,
            tick_ms: self.tick_ms,
            tick_ms_explicit: Some(self.tick_explicit),
        };
        write_state(&path, &state);
    }

    /// Folds worth writing to `state.json`: only real device ids (a
    /// positional key is meaningless to the next run), sorted for a stable
    /// file, capped, with devices seen this session keeping their fold first.
    fn persisted_folds(&self) -> Vec<String> {
        let (mut seen, mut gone): (Vec<String>, Vec<String>) = self
            .folded
            .iter()
            .filter_map(|k| match k {
                DeviceKey::Id(id) => Some(id.clone()),
                DeviceKey::Pos(_) => None,
            })
            .partition(|id| self.last_seen.contains_key(&DeviceKey::Id(id.clone())));
        seen.sort();
        gone.sort();
        seen.into_iter()
            .chain(gone)
            .take(MAX_FOLDS_PERSISTED)
            .collect()
    }

    /// Forget the longest-absent departed devices once too many have piled
    /// up. Keeping some is the point — a card that misses one poll, or an
    /// eGPU replugged, comes back to its own graphs — but a machine that
    /// churns through devices must not grow these maps forever.
    fn evict_absent_devices(&mut self) {
        let live = self.keys.len();
        let Some(excess) = self.last_seen.len().checked_sub(live + MAX_ABSENT_DEVICES) else {
            return;
        };
        let mut absent: Vec<(u64, DeviceKey)> = self
            .last_seen
            .iter()
            .filter(|(k, _)| !self.keys.contains(k))
            .map(|(k, seen)| (*seen, k.clone()))
            .collect();
        // Oldest first; the key breaks ties so eviction is deterministic.
        absent.sort_unstable();
        for (_, k) in absent.into_iter().take(excess) {
            self.history.remove(&k);
            self.session.remove(&k);
            self.last_seen.remove(&k);
            // The fold goes with it: nothing else remembers this device, so
            // keeping the fold alone would silently fold it on return with no
            // history behind it.
            self.folded.remove(&k);
        }
    }

    /// Graph history of the card at `idx`, empty until its first poll.
    pub fn history_at(&self, idx: usize) -> Option<&History> {
        self.history.get(self.keys.get(idx)?)
    }

    /// Session peaks/averages of the card at `idx`.
    pub fn session_at(&self, idx: usize) -> Option<&SessionStats> {
        self.session.get(self.keys.get(idx)?)
    }

    pub fn is_folded(&self, idx: usize) -> bool {
        self.keys.get(idx).is_some_and(|k| self.folded.contains(k))
    }

    fn toggle_fold(&mut self, idx: usize) {
        let Some(key) = self.keys.get(idx).cloned() else {
            return;
        };
        if !self.folded.remove(&key) {
            self.folded.insert(key);
        }
    }

    pub fn splash_active(&self) -> bool {
        !self.splash_skipped && self.started.elapsed() < Duration::from_millis(SPLASH_MS)
    }

    pub fn status_line(&self) -> Option<&str> {
        match &self.status {
            Some((msg, at)) if at.elapsed() < Duration::from_millis(STATUS_MS) => {
                Some(msg.as_str())
            }
            _ => None,
        }
    }

    fn set_status(&mut self, msg: String) {
        self.status = Some((msg, Instant::now()));
    }

    /// Poll the backend. Failures degrade gracefully: the last good snapshot
    /// stays on screen and the error shows in the header until a poll
    /// succeeds again — a driver reset must not kill the monitor.
    pub fn poll(&mut self) {
        self.poll_inner(true);
    }

    /// Poll without emitting a log record. The headless path polls twice to
    /// give delta-based metrics a baseline; only the second sample is the
    /// snapshot `--once` promises, so only that one is recorded.
    pub fn poll_priming(&mut self) {
        self.poll_inner(false);
    }

    fn poll_inner(&mut self, log: bool) {
        if self.paused {
            return;
        }
        match self.backend.poll() {
            Ok(gpus) => {
                self.gpus = gpus;
                self.poll_error = None;
                self.poll_failures = 0;
            }
            Err(e) => {
                self.poll_error = Some(format!("poll failed: {e:#}"));
                self.poll_failures += 1;
                // A driver reload can permanently kill the old backend
                // handle (NVML especially). Try a fresh detect every 5th
                // consecutive failure — through the source this session was
                // started from, never a bare detect(). That is the whole
                // guarantee, and it is structural rather than a check anyone
                // has to remember: a bare detect() answers with the LIVE
                // backend whatever the session was, so a replay whose polls
                // began failing would trade a stranger's recording for this
                // machine's hardware, flipping the `can_signal()` the kill
                // path reads from false to true and leaving a table of
                // recorded, foreign pids aimed at local processes. Re-detecting
                // through the source can only ever produce the same kind of
                // backend it produced at startup — a recording re-opens that
                // recording, a mock builds another mock — so nothing here can
                // promote a fabricated backend to a live one.
                if self.poll_failures.is_multiple_of(5)
                    && let Ok(fresh) = self.source.detect()
                {
                    self.backend = fresh;
                    self.set_status(format!(
                        "backend re-detected ({}) after {} failed polls",
                        self.backend.name(),
                        self.poll_failures
                    ));
                }
                return; // keep previous snapshot and history
            }
        }
        // The cursor follows the device, not the slot: a hotplug that shifts
        // every later card must not silently move the selection onto a
        // different GPU.
        let selected_key = self.keys.get(self.selected).cloned();
        self.keys = device_keys(&self.gpus);
        self.selected = selected_key
            .and_then(|k| self.keys.iter().position(|x| *x == k))
            .unwrap_or_else(|| self.selected.min(self.gpus.len().saturating_sub(1)));

        self.polls += 1;
        // Split borrow: the per-device maps are updated while `gpus` is read.
        let Self {
            gpus,
            keys,
            history,
            session,
            last_seen,
            polls,
            history_len,
            history_need,
            ..
        } = self;
        // Config history_len is a MINIMUM; keep at least what the widest
        // graph can display (+slack for resize wiggle).
        let cap = (*history_len).max(*history_need + 8);
        for (gpu, key) in gpus.iter().zip(keys.iter()) {
            last_seen.insert(key.clone(), *polls);
            session.entry(key.clone()).or_default().add(gpu);
            let hist = history.entry(key.clone()).or_default();
            // An unreadable metric is recorded as `None`, not as a 0: the
            // waveform draws such a column as a dim minimum sliver and the
            // mini sparks leave it blank, so the graph makes the same
            // distinction the meter above it does with `n/a`.
            let sample = |v: Option<f64>| v.map(|v| v.round() as u64);
            hist.util.push(sample(gpu.utilization_pct));
            hist.mem.push(sample(gpu.mem_pct()));
            hist.power.push(sample(gpu.power_w));
            hist.temp.push(sample(gpu.temperature_c));
            let overflow = hist.util.len().saturating_sub(cap);
            if overflow > 0 {
                hist.util.drain(..overflow);
                hist.mem.drain(..overflow);
                hist.power.drain(..overflow);
                hist.temp.drain(..overflow);
            }
        }
        self.evict_absent_devices();
        self.refresh_processes();
        if log {
            self.write_log();
        }
    }

    /// The one machine-readable shape: `--log` lines and `--json` snapshots
    /// are the same payload, so a recording can be diffed against a snapshot
    /// and `--replay` reads back everything it needs. `processes` is the
    /// UNFILTERED table in a stable order — a UI filter must not silently
    /// drop rows from a bug report, and a script must not have its ordering
    /// depend on whether a human ever pressed `s`.
    pub fn record(&self) -> serde_json::Value {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        serde_json::json!({
            "ts_ms": ts,
            "backend": self.backend.name(),
            "driver": self.backend.driver_info(),
            "gpus": self.gpus,
            "processes": self.all_procs,
        })
    }

    /// Append one JSONL record per successful poll. A write error drops the
    /// logger with a status message instead of spamming or crashing.
    fn write_log(&mut self) {
        use std::io::Write;
        if self.log.is_none() {
            return;
        }
        let rec = self.record();
        let Some(w) = self.log.as_mut() else { return };
        let ok = serde_json::to_writer(&mut *w, &rec).is_ok()
            && writeln!(w).is_ok()
            && w.flush().is_ok();
        if !ok {
            self.log = None;
            self.set_status("log write failed — logging disabled".into());
        }
    }

    fn refresh_processes(&mut self) {
        let gpu_procs = self.backend.processes();
        // Dedupe: a process on N GPUs appears N times, and sysinfo removes a
        // process refreshed twice in one pass with remove_dead=true.
        let mut pids: Vec<Pid> = gpu_procs.iter().map(|p| Pid::from_u32(p.pid)).collect();
        pids.sort_unstable();
        pids.dedup();
        // CPU is the one column that cannot be sampled at the poll rate; the
        // rest of the row is a plain read and is asked for every time.
        let now = Instant::now();
        let sample_cpu = cpu_sample_due(self.last_cpu_sample, now);
        if sample_cpu {
            self.last_cpu_sample = Some(now);
        }
        // The plain refresh_processes() kind omits user and cmd — ask for
        // exactly what the table shows.
        let kind = ProcessRefreshKind::nothing()
            .with_memory()
            .with_user(UpdateKind::OnlyIfNotSet)
            .with_cmd(UpdateKind::OnlyIfNotSet);
        let kind = if sample_cpu { kind.with_cpu() } else { kind };
        self.sys
            .refresh_processes_specifics(ProcessesToUpdate::Some(&pids), true, kind);
        self.evict_departed_processes(&pids);

        self.all_procs = gpu_procs
            .into_iter()
            .map(|gp| {
                let p = self.sys.process(Pid::from_u32(gp.pid));
                ProcRow {
                    // Live sysinfo enrichment; backend-recorded values (the
                    // replay path) win because this host's pids are unrelated.
                    user: gp
                        .user
                        .clone()
                        .or_else(|| {
                            p.and_then(|p| p.user_id())
                                .and_then(|uid| self.users.get_user_by_id(uid))
                                .map(|u| u.name().to_string())
                        })
                        .unwrap_or_else(|| "-".into()),
                    // Left None when neither the backend nor sysinfo could
                    // supply the figure — i.e. the pid resolved to nothing.
                    // Both sysinfo calls are real readings, so a live process
                    // sitting idle still lands here as a measured zero.
                    cpu_pct: gp.cpu_pct.or(p.map(|p| p.cpu_usage())),
                    host_mem_bytes: gp.host_mem_bytes.or(p.map(|p| p.memory())),
                    command: gp
                        .command
                        .clone()
                        .or(p.map(command_of))
                        .unwrap_or_else(|| "?".into()),
                    container: match (&gp.container, p) {
                        // A backend that pre-enriches rows (replay) owns the
                        // attribution.
                        (Some(c), _) => Some(c.clone()),
                        // Live row with a resolvable pid: cache on the pid's
                        // identity.
                        (None, Some(p)) => {
                            cached_container(&mut self.proc_text, gp.pid, p.start_time(), || {
                                container_of_pid(gp.pid)
                            })
                        }
                        // Unresolvable pid: no identity to key on, read
                        // directly as today.
                        (None, None) => container_of_pid(gp.pid),
                    },
                    pid: gp.pid,
                    gpu_index: gp.gpu_index,
                    kind: gp.kind,
                    gpu_util_pct: gp.gpu_util_pct,
                    gpu_mem_bytes: gp.gpu_mem_bytes,
                }
            })
            .collect();
        // Backend enumeration order is an implementation detail (hash maps,
        // sysfs readdir). Pin one order here so `--json` and `--log` records
        // are reproducible and diffable regardless of UI sort state.
        //
        // Rows whose GPU memory could not be read sort below every row where
        // it could, then by pid and gpu index like the rest — the same rule
        // `rebuild_proc_view` applies to the table, so a record and the
        // display agree on where an unknown belongs. Ordering them among the
        // measured rows would need a number they do not have, and putting
        // them first would give the top of the record to the rows carrying
        // the least information.
        self.all_procs.sort_by(|a, b| {
            unmeasured_last(a.gpu_mem_bytes.is_none(), b.gpu_mem_bytes.is_none())
                .unwrap_or_else(|| b.gpu_mem_bytes.cmp(&a.gpu_mem_bytes))
                .then(a.pid.cmp(&b.pid))
                .then(a.gpu_index.cmp(&b.gpu_index))
        });
        // Cache hygiene, same rule as `evict_departed_processes`: entries for
        // pids no longer on a GPU would otherwise accumulate for the life of
        // the session.
        let live: HashSet<u32> = pids.iter().map(|p| p.as_u32()).collect();
        self.proc_text.retain(|(pid, _), _| live.contains(pid));
        self.rebuild_proc_view();
    }

    /// Drop the cached `sysinfo` entries for pids that are no longer on a GPU.
    ///
    /// [`Self::refresh_processes`] names only the pids the backend lists this
    /// poll, because `ProcessesToUpdate::All` would stat every process on the
    /// box several times a second. The cost of that choice is that sysinfo
    /// only ever evicts pids from *inside* the set it is handed: a pid that
    /// stops using the GPU is never in a later set, so the cache kept one
    /// `Process` per pid ever seen and gave none of it back. A node churning
    /// through short GPU jobs grew that map for the life of the session.
    ///
    /// sysinfo exposes no way to remove an entry outright, so the eviction is
    /// a second refresh over exactly the pids that have dropped off the table,
    /// with `remove_dead_processes` doing the work — the ones that have exited
    /// are gone from the OS and fall out of the map. It asks for no fields at
    /// all: this pass exists only for its removal half, and in particular must
    /// not request CPU, which would recompute every process's usage against a
    /// `/proc/stat` delta the caller may just have decided was too young to
    /// use.
    ///
    /// A departed pid whose process is still alive stays cached — nothing
    /// short of an `All` refresh can evict a live process — but that set is
    /// bounded by the machine's process table rather than by session length,
    /// and each member is swept again every poll and drops out on the first
    /// one after it exits.
    fn evict_departed_processes(&mut self, live: &[Pid]) {
        let live: HashSet<Pid> = live.iter().copied().collect();
        let departed: Vec<Pid> = self
            .sys
            .processes()
            .keys()
            .copied()
            .filter(|pid| !live.contains(pid))
            .collect();
        if departed.is_empty() {
            return;
        }
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&departed),
            true,
            ProcessRefreshKind::nothing(),
        );
    }

    /// Re-apply filter + sort to the raw rows, keeping the cursor on the
    /// same (pid, gpu) when it survives the rebuild.
    pub fn rebuild_proc_view(&mut self) {
        let cursor_key = self.procs.get(self.proc_sel).map(|p| (p.pid, p.gpu_index));

        let needle = self.filter.to_lowercase();
        let mut rows: Vec<ProcRow> = self
            .all_procs
            .iter()
            .filter(|p| {
                needle.is_empty()
                    || p.command.to_lowercase().contains(&needle)
                    || p.user.to_lowercase().contains(&needle)
                    || p.pid.to_string().contains(&needle)
                    || p.container
                        .as_deref()
                        .is_some_and(|c| c.to_lowercase().contains(&needle))
            })
            .cloned()
            .collect();

        let by = self.sort_by;
        rows.sort_by(|a, b| {
            // Returned ahead of the direction flip below, so the rule holds
            // in BOTH directions for every sortable column.
            if let Some(ord) = unmeasured_last(a.is_unmeasured(by), b.is_unmeasured(by)) {
                return ord;
            }
            // Both sides are equally (un)known by here, so the placeholders
            // below only ever compare two unknowns against each other.
            let ord = match by {
                SortBy::GpuMem => a.gpu_mem_bytes.cmp(&b.gpu_mem_bytes),
                SortBy::GpuUtil => a
                    .gpu_util_pct
                    .unwrap_or(0.0)
                    .total_cmp(&b.gpu_util_pct.unwrap_or(0.0)),
                SortBy::Cpu => a
                    .cpu_pct
                    .unwrap_or(0.0)
                    .total_cmp(&b.cpu_pct.unwrap_or(0.0)),
                SortBy::HostMem => a.host_mem_bytes.cmp(&b.host_mem_bytes),
                SortBy::Pid => a.pid.cmp(&b.pid),
            };
            let ord = if self.sort_desc { ord.reverse() } else { ord };
            ord.then(a.pid.cmp(&b.pid))
        });
        self.procs = rows;

        self.proc_sel = cursor_key
            .and_then(|key| self.procs.iter().position(|p| (p.pid, p.gpu_index) == key))
            .unwrap_or_else(|| self.proc_sel.min(self.procs.len().saturating_sub(1)));
    }

    /// Readline `<C-w>`: drop trailing blanks, then the word before them.
    pub fn filter_delete_word(&mut self) {
        while self.filter_input.ends_with(char::is_whitespace) {
            self.filter_input.pop();
        }
        while !self.filter_input.is_empty() && !self.filter_input.ends_with(char::is_whitespace) {
            self.filter_input.pop();
        }
    }

    /// Commit the filter edit buffer (Enter in filter mode).
    pub fn commit_filter(&mut self) {
        self.filter = self.filter_input.trim().to_string();
        self.input_mode = InputMode::Normal;
        self.rebuild_proc_view();
    }

    /// Whether the current backend's pids name processes on this machine.
    pub fn can_signal(&self) -> bool {
        self.backend.can_signal()
    }

    /// Open the kill dialog for the cursor row, after the checks that don't
    /// need the process table: backend provenance and pane focus.
    fn request_kill(&mut self, force: bool) {
        if !self.can_signal() {
            self.set_status(format!(
                "kill disabled: {} pids don't name processes on this machine",
                self.backend.name()
            ));
            return;
        }
        // The cursor row is invisible from the GPU pane; signalling a row the
        // user isn't looking at is never right. Refuse instead of stealing
        // focus, so the keystroke can't become a kill by accident.
        if self.focus != Focus::Procs {
            self.set_status("kill: focus the process pane first (p)".into());
            return;
        }
        let Some(row) = self.procs.get(self.proc_sel) else {
            return;
        };
        let pid = row.pid;
        let command: String = row.command.chars().take(40).collect();
        let Some(start_time) = self.sys.process(Pid::from_u32(pid)).map(|p| p.start_time()) else {
            self.set_status(format!("kill: pid {pid} is not a live local process"));
            return;
        };
        #[cfg(target_os = "linux")]
        let start_ticks = proc_start_ticks(pid);
        self.pending_kill = Some(PendingKill {
            pid,
            start_time,
            force,
            command,
            #[cfg(target_os = "linux")]
            start_ticks,
        });
        self.input_mode = InputMode::Confirm;
    }

    /// Send the pending signal (y in confirm mode). Every guard lives here
    /// too, not just in `request_kill` — this is the one place that signals.
    pub fn confirm_kill(&mut self) {
        let Some(k) = self.pending_kill.take() else {
            return;
        };
        self.input_mode = InputMode::Normal;
        let PendingKill {
            pid,
            start_time,
            force,
            command,
            #[cfg(target_os = "linux")]
            start_ticks,
        } = k;
        let sig_name = if force { "SIGKILL" } else { "SIGTERM" };
        // A failed poll can swap the backend under a pending dialog.
        if !self.can_signal() {
            self.set_status(format!(
                "kill: {} pids are not signalable",
                self.backend.name()
            ));
            return;
        }
        if pid == 1 {
            self.set_status("kill: refusing to signal pid 1 (init)".into());
            return;
        }
        if pid == std::process::id() {
            self.set_status("kill: refusing to signal gpur itself".into());
            return;
        }
        // What the cache says about this pid proves nothing on its own: a
        // process that exited after the dialog opened may still be sitting
        // there from the poll that listed it, may have been swept out by
        // `evict_departed_processes` since, or may have had its number handed
        // to something else entirely. Refresh this one pid — remove_dead
        // evicts it if it is gone, and a live process missing from the map is
        // re-added, so an eviction in the meantime costs nothing here — and
        // demand the same start time, or we'd signal whoever inherited the
        // number.
        let target = Pid::from_u32(pid);
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[target]),
            true,
            ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
        );
        let Some(p) = self.sys.process(target) else {
            self.set_status(format!("kill: pid {pid} no longer exists"));
            return;
        };
        if p.start_time() != start_time {
            self.set_status(format!("kill: pid {pid} was reused by another process"));
            return;
        }
        // Kernel threads have no executable. They can't be killed anyway, and
        // an unreadable exe (another user's process, no ptrace access) means
        // the signal would only earn an EPERM — refuse either way.
        if p.exe().is_none() {
            self.set_status(format!(
                "kill: pid {pid} has no executable (kernel thread?) — refusing"
            ));
            return;
        }
        // pidfd fast path (Linux): pidfd_open pins the process identity at
        // this instant, so a replacement that reuses the pid after the open
        // can never receive the signal (the pidfd still names the original,
        // and pidfd_send_signal answers ESRCH once it exits), and the
        // clock-tick start comparison catches a same-second replacement at
        // pin time. Fall through to the kill_with path below when the kernel
        // or sandbox has no pidfd support.
        #[cfg(target_os = "linux")]
        {
            let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) }
                as libc::c_int;
            if pidfd >= 0 {
                // Re-read identity at pin time: field 22 differs for any
                // process that took the number after the dialog opened.
                let cur = proc_start_ticks(pid);
                let start = start_ticks;
                if cur != start {
                    unsafe { libc::close(pidfd) };
                    self.set_status(format!("kill: pid {pid} was reused by another process"));
                    return;
                }
                let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
                let ret = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        pidfd,
                        sig,
                        std::ptr::null_mut::<libc::siginfo_t>(),
                        0,
                    )
                };
                unsafe { libc::close(pidfd) };
                if ret == 0 {
                    self.set_status(format!("sent {sig_name} to {pid} ({command})"));
                    return;
                }
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ESRCH) {
                    // The pidfd pins the ORIGINAL process, so an ESRCH here
                    // means it exited — never that a reused pid got signalled.
                    self.set_status(format!("kill: pid {pid} exited before the signal was sent"));
                } else {
                    self.set_status(format!(
                        "{sig_name} to {pid} failed (permission? try as root)"
                    ));
                }
                return;
            }
        }
        // kill_with returns None when the signal isn't supported on this
        // platform (Term on Windows). Falling back to plain kill() there sent
        // Signal::Kill while the dialog had asked about SIGTERM and the status
        // line went on to report SIGTERM: the user consented to a signal the
        // process could catch, clean up after and ignore, and silently got one
        // it could not. Refuse instead — every other guard on this path errs
        // towards not signalling, and an escalation to SIGKILL has to be the
        // user's own keystroke.
        let sig = if force {
            sysinfo::Signal::Kill
        } else {
            sysinfo::Signal::Term
        };
        let Some(ok) = p.kill_with(sig) else {
            // Kill is the one signal sysinfo supports everywhere, so in
            // practice this is the SIGTERM arm; p.kill() is exactly the
            // Signal::Kill we are declining to send behind the user's back.
            let hint = if force {
                ""
            } else {
                " — use K to send SIGKILL explicitly"
            };
            self.set_status(format!(
                "kill: {sig_name} is unsupported on this platform{hint}"
            ));
            return;
        };
        if ok {
            self.set_status(format!("sent {sig_name} to {pid} ({command})"));
        } else {
            self.set_status(format!(
                "{sig_name} to {pid} failed (permission? try as root)"
            ));
        }
    }

    /// Apply a key action. Returns true when the app should quit.
    pub fn apply(&mut self, action: Action) -> bool {
        match action {
            Action::Quit => return true,
            Action::TogglePause => self.paused = !self.paused,
            Action::NextItem => match self.focus {
                Focus::Gpus => self.next_gpu(),
                Focus::Procs => self.proc_down(),
            },
            Action::PrevItem => match self.focus {
                Focus::Gpus => self.prev_gpu(),
                Focus::Procs => self.proc_up(),
            },
            Action::NextGpu => self.next_gpu(),
            Action::PrevGpu => self.prev_gpu(),
            Action::TickFaster => {
                self.tick_ms = (self.tick_ms / 2).max(MIN_TICK_MS);
                self.tick_explicit = true;
            }
            Action::TickSlower => {
                self.tick_ms = self.tick_ms.saturating_mul(2).min(MAX_TICK_MS);
                self.tick_explicit = true;
            }
            Action::Digit(i) => {
                if i < self.gpus.len() {
                    if self.focus == Focus::Gpus && self.selected == i {
                        // Second press on the selected GPU folds it.
                        self.toggle_fold(i);
                    } else {
                        self.focus = Focus::Gpus;
                        self.selected = i;
                    }
                }
            }
            Action::FocusProcs => self.focus = Focus::Procs,
            Action::Help => self.show_help = true,
            Action::ProcScrollDown => self.proc_down(),
            Action::ProcScrollUp => self.proc_up(),
            Action::SortCycle => {
                self.sort_by = self.sort_by.next();
                self.rebuild_proc_view();
            }
            Action::SortReverse => {
                self.sort_desc = !self.sort_desc;
                self.rebuild_proc_view();
            }
            Action::FilterOpen => {
                self.focus = Focus::Procs;
                self.filter_input = self.filter.clone();
                self.input_mode = InputMode::Filter;
            }
            Action::KillTerm | Action::KillForce => {
                self.request_kill(matches!(action, Action::KillForce))
            }
        }
        false
    }

    fn proc_down(&mut self) {
        self.proc_sel = (self.proc_sel + 1).min(self.procs.len().saturating_sub(1));
    }

    fn proc_up(&mut self) {
        self.proc_sel = self.proc_sel.saturating_sub(1);
    }

    fn next_gpu(&mut self) {
        // Clamp at the ends — no wrap-around.
        self.selected = (self.selected + 1).min(self.gpus.len().saturating_sub(1));
    }

    fn prev_gpu(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::GpuBackend;

    /// Stand-in for a live backend: the only one whose pids may be signalled.
    struct LocalBackend;

    impl GpuBackend for LocalBackend {
        fn name(&self) -> &'static str {
            "test"
        }
        fn poll(&mut self) -> anyhow::Result<Vec<GpuSnapshot>> {
            Ok(Vec::new())
        }
    }

    /// Stand-in for the replay backend: rows arrive pre-enriched, in an
    /// order the recording happened to have.
    struct RecordedBackend;

    impl GpuBackend for RecordedBackend {
        fn name(&self) -> &'static str {
            "recorded"
        }
        fn poll(&mut self) -> anyhow::Result<Vec<GpuSnapshot>> {
            Ok(vec![GpuSnapshot::default()])
        }
        fn driver_info(&self) -> Option<String> {
            Some("recorded driver 1.2".into())
        }
        fn processes(&mut self) -> Vec<crate::backend::GpuProcess> {
            vec![
                crate::backend::GpuProcess {
                    pid: 1,
                    gpu_mem_bytes: Some(1),
                    command: Some("idle".into()),
                    user: Some("root".into()),
                    ..Default::default()
                },
                crate::backend::GpuProcess {
                    pid: 4242,
                    gpu_mem_bytes: Some(2 << 30),
                    command: Some("train.py".into()),
                    user: Some("bob".into()),
                    container: Some("docker:abcdef123456".into()),
                    ..Default::default()
                },
            ]
        }
    }

    /// Rows emitted out of order, two of which the backend cannot account
    /// for at all — the NVML-under-WDDM shape, where `UsedGpuMemory` comes
    /// back `Unavailable`.
    struct UnaccountedBackend;

    impl GpuBackend for UnaccountedBackend {
        fn name(&self) -> &'static str {
            "unaccounted"
        }
        fn poll(&mut self) -> anyhow::Result<Vec<GpuSnapshot>> {
            Ok(vec![GpuSnapshot::default()])
        }
        fn processes(&mut self) -> Vec<crate::backend::GpuProcess> {
            let row = |pid, gpu_mem_bytes| crate::backend::GpuProcess {
                pid,
                gpu_mem_bytes,
                user: Some("me".into()),
                command: Some("x".into()),
                ..Default::default()
            };
            // Deliberately neither sorted nor grouped: a pass-through order
            // would fail the assertion, and so would one that only happens to
            // work because the unknowns arrived together.
            vec![
                row(30, None),
                row(10, Some(1 << 20)),
                row(20, None),
                row(40, Some(4 << 20)),
            ]
        }
    }

    /// Scripted GPU process table: one pid list per poll, so a pid can be on
    /// the table for one poll and gone from the next. The last entry repeats
    /// once the script runs out.
    ///
    /// Gated with its only test, which needs a real child process to watch
    /// leave the table — otherwise this is dead code on Windows and the
    /// `-D warnings` build fails there rather than here.
    #[cfg(unix)]
    struct ChurningBackend {
        ticks: Vec<Vec<u32>>,
        tick: usize,
    }

    #[cfg(unix)]
    impl GpuBackend for ChurningBackend {
        fn name(&self) -> &'static str {
            "churning"
        }
        fn poll(&mut self) -> anyhow::Result<Vec<GpuSnapshot>> {
            Ok(Vec::new())
        }
        fn processes(&mut self) -> Vec<crate::backend::GpuProcess> {
            let i = self.tick.min(self.ticks.len() - 1);
            self.tick += 1;
            self.ticks[i]
                .iter()
                .map(|pid| crate::backend::GpuProcess {
                    pid: *pid,
                    ..Default::default()
                })
                .collect()
        }
    }

    /// Scripted device list: one entry per poll, each `(device id, util%)`.
    /// The last entry repeats once the script runs out. `None` for the id is
    /// a backend that cannot identify the device.
    struct ScriptedBackend {
        ticks: Vec<Vec<(Option<&'static str>, f64)>>,
        tick: usize,
    }

    impl ScriptedBackend {
        fn boxed(ticks: Vec<Vec<(Option<&'static str>, f64)>>) -> Box<dyn GpuBackend> {
            Box::new(Self { ticks, tick: 0 })
        }
    }

    impl GpuBackend for ScriptedBackend {
        fn name(&self) -> &'static str {
            "scripted"
        }
        fn poll(&mut self) -> anyhow::Result<Vec<GpuSnapshot>> {
            let i = self.tick.min(self.ticks.len() - 1);
            self.tick += 1;
            Ok(self.ticks[i]
                .iter()
                .map(|(id, util)| GpuSnapshot {
                    name: id.unwrap_or("anon").to_string(),
                    device_id: id.map(str::to_string),
                    utilization_pct: Some(*util),
                    ..Default::default()
                })
                .collect())
        }
    }

    fn util_history(app: &App, idx: usize) -> Vec<Option<u64>> {
        app.history_at(idx).expect("history for card").util.clone()
    }

    /// A scratch dir, wiped when the guard drops.
    /// Mirrors `backend::linux::testing::Sandbox` — that one is Linux-only and
    /// lives behind a private module, so this shares the pattern rather than
    /// the type. The pid and the counter are the point: a fixed name under the
    /// world-writable temp dir makes two concurrent `cargo test` runs fight
    /// over one directory, and lets anyone else on the host pre-create that
    /// name as a symlink the test would write through.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "gpur-app-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn replay_log(tag: &str, body: &str) -> Self {
            let scratch = Self::new(tag);
            std::fs::write(scratch.path(), body).unwrap();
            scratch
        }

        fn join(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }

        fn path(&self) -> std::path::PathBuf {
            self.join("rec.jsonl")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn app_with(backend: Box<dyn GpuBackend>) -> App {
        app_from(backend, crate::backend::BackendSource::Live)
    }

    /// Same, for the tests that care where the session's backend came from —
    /// the failure re-detect is the only path that reads it.
    fn app_from(backend: Box<dyn GpuBackend>, source: crate::backend::BackendSource) -> App {
        App::new(
            backend,
            crate::theme::load(None, crate::theme::detect_color_mode()).unwrap(),
            AppOptions {
                tick_ms: 1000,
                tick_explicit: false,
                history_len: 60,
                no_splash: true,
                graph_style: GraphStyle::Ascii,
                source,
                log: None,
            },
        )
    }

    /// Arm the dialog directly — `request_kill` needs a populated table.
    fn arm(app: &mut App, pid: u32, start_time: u64) {
        app.pending_kill = Some(PendingKill {
            pid,
            start_time,
            force: false,
            command: "victim".into(),
            #[cfg(target_os = "linux")]
            start_ticks: proc_start_ticks(pid),
        });
        app.input_mode = InputMode::Confirm;
    }

    fn status_of(app: &App) -> String {
        app.status_line().unwrap_or("").to_string()
    }

    #[test]
    fn fabricated_backends_refuse_to_signal() {
        assert!(!crate::backend::detect(Some(2), None).unwrap().can_signal());
        assert!(LocalBackend.can_signal());
        // Replay is private; drive it through detect() on a one-line log.
        let scratch = Scratch::replay_log("can-signal", "{\"gpus\":[],\"processes\":[]}\n");
        let replay = crate::backend::detect(None, Some(&scratch.path())).unwrap();
        assert_eq!(replay.name(), "replay");
        assert!(!replay.can_signal());
    }

    /// The failure re-detect must reproduce the session, not replace it. A
    /// recording whose polls started failing has to come back as that same
    /// recording: a bare `detect()` would answer with this machine's hardware,
    /// and `can_signal()` — which is what the kill path asks before it will
    /// signal anything — would flip false to true, leaving a stranger's
    /// recorded pids aimed at local processes.
    #[test]
    fn a_failing_replay_re_detects_to_a_replay_not_to_live_hardware() {
        /// A replay backend whose log has gone away mid-session — the only
        /// way a replay reaches the re-detect at all, since `ReplayBackend`
        /// holds its last frame instead of failing.
        struct DeadRecording;
        impl GpuBackend for DeadRecording {
            fn name(&self) -> &'static str {
                "recording"
            }
            fn poll(&mut self) -> anyhow::Result<Vec<GpuSnapshot>> {
                anyhow::bail!("the log went away")
            }
            fn can_signal(&self) -> bool {
                false
            }
        }

        let scratch = Scratch::replay_log("re-detect", "{\"gpus\":[],\"processes\":[]}\n");
        let mut app = app_from(
            Box::new(DeadRecording),
            crate::backend::BackendSource::Replay(scratch.path()),
        );
        // Re-detect fires on every 5th consecutive failure; go well past it.
        for _ in 0..12 {
            app.poll();
        }
        assert_eq!(
            app.backend.name(),
            "replay",
            "a re-detect swapped a recording for something else"
        );
        assert!(!app.can_signal(), "a re-detect made a recording signalable");

        // And the kill dialog still refuses, off that same flag.
        arm(&mut app, 424242, 0);
        app.confirm_kill();
        assert!(
            status_of(&app).contains("not signalable"),
            "{}",
            status_of(&app)
        );
    }

    #[test]
    fn kill_refuses_init_and_self() {
        let mut app = app_with(Box::new(LocalBackend));

        arm(&mut app, 1, 0);
        app.confirm_kill();
        assert!(status_of(&app).contains("pid 1"), "{}", status_of(&app));
        assert!(app.pending_kill.is_none());
        assert!(app.input_mode == InputMode::Normal);

        arm(&mut app, std::process::id(), 0);
        app.confirm_kill();
        assert!(
            status_of(&app).contains("gpur itself"),
            "{}",
            status_of(&app)
        );
    }

    #[test]
    fn kill_refuses_when_the_backend_is_not_local() {
        let mut app = app_with(crate::backend::detect(Some(1), None).unwrap());
        arm(&mut app, 424242, 0);
        app.confirm_kill();
        assert!(
            status_of(&app).contains("not signalable"),
            "{}",
            status_of(&app)
        );
    }

    #[test]
    fn kill_refuses_a_pid_that_no_longer_exists() {
        let mut app = app_with(Box::new(LocalBackend));
        // Above any plausible live pid on a test host.
        arm(&mut app, 4_194_300, 0);
        app.confirm_kill();
        assert!(
            status_of(&app).contains("no longer exists"),
            "{}",
            status_of(&app)
        );
    }

    /// The core of finding 33: a pid whose recorded start time no longer
    /// matches is a different process wearing the same number.
    #[test]
    #[cfg(unix)]
    fn kill_refuses_a_recycled_pid() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = Pid::from_u32(child.id());
        let mut app = app_with(Box::new(LocalBackend));
        app.sys
            .refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        let real = app.sys.process(pid).expect("child visible").start_time();

        arm(&mut app, child.id(), real + 1);
        app.confirm_kill();
        assert!(
            status_of(&app).contains("reused by another process"),
            "{}",
            status_of(&app)
        );
        assert!(
            matches!(child.try_wait(), Ok(None)),
            "child was signalled despite the start-time mismatch"
        );

        // Same pid, matching start time: the guards must not block a real kill.
        arm(&mut app, child.id(), real);
        app.confirm_kill();
        assert!(
            status_of(&app).contains("sent SIGTERM"),
            "{}",
            status_of(&app)
        );
        let _ = child.wait();
    }

    /// The comm field (field 2) is parenthesized and may embed spaces and `)`
    /// characters; only the LAST `)` closes it. Field 22 (starttime) is the
    /// 19th whitespace token after field 3 (state).
    #[test]
    #[cfg(target_os = "linux")]
    fn parse_start_ticks_handles_comm_with_spaces_and_parens() {
        // Comm `(a)b) c)`; tokens after the last `)` run S, 3, 4, …, so the
        // 22nd field (starttime) is the value at index 19, i.e. "21".
        let stat = "1 (a)b) c) S 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23";
        assert_eq!(parse_start_ticks(stat), Some(21));
        // A plain comm parses the same way: field 22 is "19" here.
        let plain = "9 (init) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20";
        assert_eq!(parse_start_ticks(plain), Some(19));
        // Short or malformed stat lines yield None, never a panic.
        assert_eq!(parse_start_ticks(""), None);
        assert_eq!(parse_start_ticks("1"), None);
        assert_eq!(parse_start_ticks("1 (a) S 3 4"), None);
    }

    #[test]
    fn kill_needs_the_process_pane_focused() {
        let mut app = app_with(Box::new(LocalBackend));
        app.all_procs = vec![ProcRow {
            pid: std::process::id(),
            gpu_index: 0,
            kind: ProcKind::Compute,
            gpu_util_pct: None,
            gpu_mem_bytes: Some(0),
            user: "me".into(),
            cpu_pct: Some(0.0),
            host_mem_bytes: Some(0),
            command: "gpur".into(),
            container: None,
        }];
        app.rebuild_proc_view();

        app.focus = Focus::Gpus;
        app.apply(Action::KillTerm);
        assert!(app.pending_kill.is_none());
        assert!(
            status_of(&app).contains("focus the process pane"),
            "{}",
            status_of(&app)
        );

        // With the pane focused the dialog opens (and pins the start time).
        app.focus = Focus::Procs;
        app.sys.refresh_processes(
            ProcessesToUpdate::Some(&[Pid::from_u32(std::process::id())]),
            true,
        );
        app.apply(Action::KillTerm);
        assert!(app.pending_kill.is_some());
        assert!(app.input_mode == InputMode::Confirm);
    }

    /// The process cache is refreshed with `ProcessesToUpdate::Some`, and
    /// sysinfo evicts nothing outside the set it is handed — so without an
    /// explicit sweep every short-lived job the node ever ran stayed in the
    /// map until gpur exited.
    #[test]
    #[cfg(unix)]
    fn a_pid_that_leaves_the_gpu_table_is_evicted_from_the_process_cache() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        let mut app = app_with(Box::new(ChurningBackend {
            ticks: vec![vec![pid], vec![]],
            tick: 0,
        }));

        app.poll();
        assert!(
            app.sys.process(Pid::from_u32(pid)).is_some(),
            "the child never reached the cache, so the eviction would prove nothing"
        );

        child.kill().expect("kill child");
        child.wait().expect("reap child");

        // Second poll: the backend has stopped listing the pid, so nothing
        // else will ever name it again.
        app.poll();
        assert!(
            app.sys.process(Pid::from_u32(pid)).is_none(),
            "a departed pid accumulated in the sysinfo cache"
        );
    }

    /// The container cache resolves once per (pid, start time): a second row
    /// for the same process must not re-read `/proc`, and a recycled pid (new
    /// start time) must re-resolve rather than inherit the old process's
    /// cgroup.
    #[test]
    fn cached_container_resolves_once_per_process_identity() {
        let mut cache: HashMap<(u32, u64), Option<String>> = HashMap::new();
        // Cell so the resolver closure can be passed by value (Copy) per call
        // while the counter stays readable between calls.
        let reads = std::cell::Cell::new(0);
        let resolve = || {
            reads.set(reads.get() + 1);
            Some("docker:abcdef123456".into())
        };
        assert_eq!(
            cached_container(&mut cache, 7, 100, resolve),
            Some("docker:abcdef123456".into())
        );
        assert_eq!(
            cached_container(&mut cache, 7, 100, resolve),
            Some("docker:abcdef123456".into())
        );
        assert_eq!(
            reads.get(),
            1,
            "the resolver ran again for the same process"
        );
        // Same pid, different start time: a different process.
        assert_eq!(
            cached_container(&mut cache, 7, 200, resolve),
            Some("docker:abcdef123456".into())
        );
        assert_eq!(reads.get(), 2);
        // A None reading is cached too — a containerless process stays a miss.
        let mut cache2: HashMap<(u32, u64), Option<String>> = HashMap::new();
        let reads2 = std::cell::Cell::new(0);
        let none = || {
            reads2.set(reads2.get() + 1);
            None
        };
        assert_eq!(cached_container(&mut cache2, 9, 1, none), None);
        assert_eq!(cached_container(&mut cache2, 9, 1, none), None);
        assert_eq!(reads2.get(), 1);
    }

    /// The container cache is pruned alongside the sysinfo process cache:
    /// a pid that leaves the GPU table must not leave its (pid, start time)
    /// entry accumulating for the life of the session.
    #[test]
    #[cfg(unix)]
    fn a_pid_that_leaves_the_gpu_table_is_pruned_from_the_container_cache() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        let mut app = app_with(Box::new(ChurningBackend {
            ticks: vec![vec![pid], vec![]],
            tick: 0,
        }));

        app.poll();
        // The entry exists whether or not the cgroup read succeeds
        // (`or_insert_with` always inserts), so this holds on any machine.
        assert_eq!(
            app.proc_text.len(),
            1,
            "the poll never resolved the child's container"
        );

        child.kill().expect("kill child");
        child.wait().expect("reap child");

        // Second poll: the backend has stopped listing the pid, so the entry
        // has no live process to belong to anymore.
        app.poll();
        assert!(
            app.proc_text.is_empty(),
            "a departed pid left a stale container entry in the cache"
        );
    }

    /// CPU% is a delta against a `/proc/stat` reading sysinfo refuses to
    /// retake more often than `MINIMUM_CPU_UPDATE_INTERVAL`, and `MIN_TICK_MS`
    /// is a quarter of that — asking for CPU on every poll at the fast end
    /// divided one tick of process time by 200 ms of machine time.
    #[test]
    fn a_cpu_sample_is_due_only_once_sysinfos_minimum_interval_has_elapsed() {
        let last = Instant::now();

        assert!(
            cpu_sample_due(None, last),
            "the first poll has no previous sample to be too close to"
        );
        assert!(
            !cpu_sample_due(Some(last), last + Duration::from_millis(MIN_TICK_MS)),
            "a poll at the tick floor was taken as a CPU sample"
        );
        assert!(
            !cpu_sample_due(
                Some(last),
                last + MINIMUM_CPU_UPDATE_INTERVAL - Duration::from_millis(1)
            ),
            "a sample one millisecond short of the interval still counted"
        );
        assert!(
            cpu_sample_due(Some(last), last + MINIMUM_CPU_UPDATE_INTERVAL),
            "the documented minimum interval must be enough"
        );
    }

    /// `--tick-ms` and config values are unbounded above, so the startup clamp is
    /// the only thing between a typo'd interval and a monitor that waits years for
    /// its first poll.
    #[test]
    fn the_startup_tick_clamp_floors_and_caps() {
        assert_eq!(clamp_tick_ms(0), MIN_TICK_MS);
        assert_eq!(clamp_tick_ms(50), 50);
        assert_eq!(clamp_tick_ms(9_999_999_999), MAX_TICK_MS);
    }

    /// `+` must never *raise* the interval: the key floor and the CLI floor
    /// are one constant now.
    #[test]
    fn tick_faster_stops_at_the_shared_floor() {
        let mut app = app_with(Box::new(LocalBackend));
        app.tick_ms = MIN_TICK_MS;
        app.apply(Action::TickFaster);
        assert_eq!(app.tick_ms, MIN_TICK_MS);

        app.tick_ms = MIN_TICK_MS * 2;
        app.apply(Action::TickFaster);
        assert_eq!(app.tick_ms, MIN_TICK_MS);
        // An interactive change is the only thing that makes the rate sticky.
        assert!(app.tick_explicit);
    }

    /// `-` on a tick too large to double must clamp to 10 s, not wrap to 0
    /// (release) or panic (debug): `--tick-ms` is unbounded above and only
    /// floored, so `* 2` on `9_223_372_036_854_775_808` (2^63, CLI-parseable)
    /// used to wrap to exactly 0.
    #[test]
    fn tick_slower_clamps_a_huge_tick_instead_of_wrapping() {
        let mut app = app_with(Box::new(LocalBackend));
        app.tick_ms = 9_223_372_036_854_775_808;
        app.apply(Action::TickSlower);
        assert_eq!(app.tick_ms, MAX_TICK_MS);
        assert!(app.tick_explicit);
    }

    #[test]
    fn tick_stays_non_sticky_until_a_key_changes_it() {
        let app = app_with(Box::new(LocalBackend));
        assert!(!app.tick_explicit);
    }

    /// A pre-flag state file is honoured once but is not a key press, so the
    /// next clean quit demotes it and `config.toml` gets its say back.
    #[test]
    fn legacy_tick_state_is_honoured_once_then_demoted() {
        let legacy: UiState = serde_json::from_str(
            r#"{"folded":[],"sort_by":"Pid","sort_desc":false,"tick_ms":300}"#,
        )
        .unwrap();
        assert_eq!(legacy.sticky_tick_ms(), Some(300));
        assert!(!legacy.tick_chosen_by_key());

        let chosen: UiState = serde_json::from_str(
            r#"{"folded":[],"sort_by":"Pid","sort_desc":false,"tick_ms":300,"tick_ms_explicit":true}"#,
        )
        .unwrap();
        assert_eq!(chosen.sticky_tick_ms(), Some(300));
        assert!(chosen.tick_chosen_by_key());

        // Recorded as config/CLI provenance: config.toml wins from now on.
        let config_only: UiState = serde_json::from_str(
            r#"{"folded":[],"sort_by":"Pid","sort_desc":false,"tick_ms":300,"tick_ms_explicit":false}"#,
        )
        .unwrap();
        assert_eq!(config_only.sticky_tick_ms(), None);
    }

    #[test]
    fn filter_delete_word_removes_one_word_and_its_blanks() {
        let mut app = app_with(Box::new(LocalBackend));
        app.filter_input = "python train.py  ".into();
        app.filter_delete_word();
        assert_eq!(app.filter_input, "python ");
        app.filter_delete_word();
        assert_eq!(app.filter_input, "");
        // Empty buffer: a no-op, not a panic.
        app.filter_delete_word();
        assert_eq!(app.filter_input, "");
    }

    /// A backend that pre-enriches rows (replay) owns them: the container
    /// must come off the record, never off this host's /proc.
    #[test]
    fn recorded_rows_win_over_local_resolution() {
        let mut app = app_with(Box::new(RecordedBackend));
        app.poll();
        assert_eq!(app.all_procs.len(), 2);
        assert_eq!(
            app.all_procs[0].container.as_deref(),
            Some("docker:abcdef123456")
        );
        assert_eq!(app.all_procs[0].command, "train.py");
    }

    /// One shape for `--log` and `--json`, attribution included, rows
    /// unfiltered and in a stable order whatever the UI sort says.
    #[test]
    fn record_carries_attribution_and_stable_unfiltered_rows() {
        let mut app = app_with(Box::new(RecordedBackend));
        app.poll();
        app.filter = "train".into();
        app.sort_by = SortBy::Pid;
        app.sort_desc = false;
        app.rebuild_proc_view();
        assert_eq!(app.procs.len(), 1, "filter should narrow the UI view");

        let rec = app.record();
        assert_eq!(rec["backend"], "recorded");
        assert_eq!(rec["driver"], "recorded driver 1.2");
        assert!(rec["ts_ms"].as_u64().unwrap() > 0);
        assert_eq!(rec["gpus"].as_array().unwrap().len(), 1);
        let procs = rec["processes"].as_array().unwrap();
        assert_eq!(procs.len(), 2, "record must not inherit the UI filter");
        // Descending gpu-mem regardless of the backend's emission order.
        assert_eq!(procs[0]["pid"], 4242);
        assert_eq!(procs[0]["container"], "docker:abcdef123456");
    }

    /// C4: the record's rows are pinned gpu-mem descending, and a row whose
    /// GPU memory could not be read has no figure to sort by. It sits below
    /// every row that has one, ties among the unknowns break on pid, and the
    /// field serializes as `null` rather than the `0` it used to flatten to —
    /// so the order stays reproducible and a consumer can still tell an
    /// unreadable row from an empty one.
    #[test]
    fn record_rows_sink_unknown_gpu_mem_below_every_known_figure() {
        let mut app = app_with(Box::new(UnaccountedBackend));
        app.poll();
        assert_eq!(
            app.all_procs.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![40, 10, 20, 30],
            "unknown gpu-mem rows are not last, or did not tie-break on pid"
        );

        let rec = app.record();
        let procs = rec["processes"].as_array().unwrap();
        assert_eq!(procs[0]["gpu_mem_bytes"], 4 << 20);
        assert_eq!(procs[1]["gpu_mem_bytes"], 1 << 20);
        assert!(
            procs[2]["gpu_mem_bytes"].is_null() && procs[3]["gpu_mem_bytes"].is_null(),
            "an unreadable figure serialized as a measurement: {rec}"
        );
    }

    /// Finding 11: a backend with no thermal or power sensor must leave those
    /// peaks unknown rather than reporting a run that never got above 0°C/0W.
    #[test]
    fn session_peaks_stay_unknown_without_a_sensor() {
        let mut s = SessionStats::default();
        for util in [10.0, 90.0, 40.0] {
            s.add(&GpuSnapshot {
                utilization_pct: Some(util),
                ..Default::default()
            });
        }
        assert_eq!(s.max_util_pct, Some(90.0));
        assert_eq!(s.avg_util_pct(), Some(140.0 / 3.0));
        assert_eq!(s.max_temp_c, None);
        assert_eq!(s.max_power_w, None);
        assert_eq!(s.avg_power_w(), None);

        // And a backend reporting nothing at all contributes nothing.
        let mut blind = SessionStats::default();
        blind.add(&GpuSnapshot::default());
        assert_eq!(blind.max_util_pct, None);
        assert_eq!(blind.avg_util_pct(), None);
    }

    /// An intermittently-reported sensor is averaged over its own readings;
    /// folding the silent samples in as 0 diluted the average toward zero.
    #[test]
    fn averages_ignore_samples_with_no_reading() {
        let mut s = SessionStats::default();
        let sample = |w| GpuSnapshot {
            utilization_pct: Some(50.0),
            power_w: w,
            ..Default::default()
        };
        s.add(&sample(Some(100.0)));
        s.add(&sample(None));
        s.add(&sample(None));
        s.add(&sample(Some(200.0)));
        assert_eq!(s.avg_power_w(), Some(150.0));
        assert_eq!(s.max_power_w, Some(200.0));
        // Utilization was present every time, so its own count is unaffected.
        assert_eq!(s.avg_util_pct(), Some(50.0));
    }

    /// Every metric measured, so a test can blank exactly the one it is about.
    fn sortable_row(pid: u32) -> ProcRow {
        ProcRow {
            pid,
            gpu_index: 0,
            kind: ProcKind::Compute,
            gpu_util_pct: Some(0.0),
            gpu_mem_bytes: Some(0),
            user: "me".into(),
            cpu_pct: Some(0.0),
            host_mem_bytes: Some(0),
            command: "x".into(),
            container: None,
        }
    }

    /// Pids 1 and 3 carry a low and a high reading of whichever column
    /// `sort_by` orders on; pid 2 carries none of it. Pid 2 belongs last
    /// both ways up: descending because it has no claim on the top, and
    /// ascending because "unread" is not the smallest reading.
    fn assert_unknown_sinks(sort_by: SortBy, set: impl Fn(&mut ProcRow, Option<f64>)) {
        let mut app = app_with(Box::new(LocalBackend));
        let row = |pid, v| {
            let mut r = sortable_row(pid);
            set(&mut r, v);
            r
        };
        app.all_procs = vec![row(1, Some(10.0)), row(2, None), row(3, Some(90.0))];
        app.sort_by = sort_by;

        app.sort_desc = true;
        app.rebuild_proc_view();
        assert_eq!(
            app.procs.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![3, 1, 2],
            "{sort_by:?} descending did not sink the unmeasured row"
        );

        app.sort_desc = false;
        app.rebuild_proc_view();
        assert_eq!(
            app.procs.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![1, 3, 2],
            "{sort_by:?} ascending sorted the unmeasured row as if it were 0"
        );
    }

    /// Finding 7 in the process table: "unmeasured" must not win an ascending
    /// GPU% sort by masquerading as 0.
    #[test]
    fn rows_with_unknown_gpu_util_sink_in_both_directions() {
        assert_unknown_sinks(SortBy::GpuUtil, |r, v| r.gpu_util_pct = v);
    }

    /// C4: the same rule for GPU memory, which NVML leaves unreadable for
    /// every process under WDDM — so on Windows this is the common case, and
    /// an ascending GPU MEM sort would otherwise open on the rows nothing is
    /// known about rather than on the genuinely smallest allocations.
    #[test]
    fn rows_with_unknown_gpu_mem_sink_in_both_directions() {
        assert_unknown_sinks(SortBy::GpuMem, |r, v| {
            r.gpu_mem_bytes = v.map(|v| v as u64 * 1024 * 1024)
        });
    }

    /// C4: and for the host columns, unreadable when the pid resolves to
    /// nothing at all.
    #[test]
    fn rows_with_unknown_cpu_sink_in_both_directions() {
        assert_unknown_sinks(SortBy::Cpu, |r, v| r.cpu_pct = v.map(|v| v as f32));
    }

    #[test]
    fn rows_with_unknown_host_mem_sink_in_both_directions() {
        assert_unknown_sinks(SortBy::HostMem, |r, v| {
            r.host_mem_bytes = v.map(|v| v as u64 * 1024 * 1024)
        });
    }

    /// Backlog 6: the history ring has to carry the same distinction the
    /// meters do. A metric the backend could not read is `None`; a metric it
    /// read as empty is `Some(0)`, and the graph renders the two differently.
    #[test]
    fn history_separates_an_unreadable_sample_from_a_measured_zero() {
        struct HalfSensored;
        impl GpuBackend for HalfSensored {
            fn name(&self) -> &'static str {
                "half"
            }
            fn poll(&mut self) -> anyhow::Result<Vec<GpuSnapshot>> {
                Ok(vec![GpuSnapshot {
                    device_id: Some("a".into()),
                    // Unreadable: no utilization counter, no power sensor.
                    utilization_pct: None,
                    power_w: None,
                    // Measured, and the measurement is zero.
                    vram_used_bytes: Some(0),
                    vram_total_bytes: Some(8 << 30),
                    temperature_c: Some(0.0),
                    ..Default::default()
                }])
            }
        }

        let mut app = app_with(Box::new(HalfSensored));
        app.poll();
        let h = app.history_at(0).expect("history for card");
        assert_eq!(h.util, vec![None], "an unreadable utilization became a 0");
        assert_eq!(h.power, vec![None], "a missing power sensor became a 0");
        assert_eq!(h.mem, vec![Some(0)], "a measured-empty pool lost its 0");
        assert_eq!(h.temp, vec![Some(0)], "a measured 0°C became unknown");
    }

    /// The heart of it: a GPU that changes position between polls keeps its
    /// own waveform history and its own session peaks.
    #[test]
    fn per_device_state_follows_the_device_across_a_reorder() {
        let mut app = app_with(ScriptedBackend::boxed(vec![
            vec![(Some("a"), 10.0), (Some("b"), 90.0)],
            // A hotplug (or a differently-ordered enumeration) swaps them.
            vec![(Some("b"), 20.0), (Some("a"), 30.0)],
        ]));
        app.poll();
        app.poll();

        assert_eq!(app.gpus[0].name, "b");
        assert_eq!(util_history(&app, 0), vec![Some(90), Some(20)]);
        assert_eq!(app.session_at(0).unwrap().max_util_pct, Some(90.0));
        assert_eq!(util_history(&app, 1), vec![Some(10), Some(30)]);
        assert_eq!(app.session_at(1).unwrap().max_util_pct, Some(30.0));
    }

    /// A device that drops off the bus and comes back is the same device.
    #[test]
    fn a_returning_device_is_re_identified_not_treated_as_new() {
        let mut app = app_with(ScriptedBackend::boxed(vec![
            vec![(Some("a"), 10.0), (Some("b"), 90.0)],
            vec![(Some("b"), 20.0)],
            vec![(Some("b"), 30.0), (Some("a"), 40.0)],
        ]));
        app.poll();
        app.poll();
        app.poll();

        assert_eq!(app.gpus[1].name, "a");
        assert_eq!(util_history(&app, 1), vec![Some(10), Some(40)]);
        assert_eq!(app.session_at(1).unwrap().max_util_pct, Some(40.0));
        // And the device that stayed is untouched by the churn.
        assert_eq!(util_history(&app, 0), vec![Some(90), Some(20), Some(30)]);
    }

    /// The cursor names a GPU, not a slot.
    #[test]
    fn the_selection_follows_the_selected_device() {
        let mut app = app_with(ScriptedBackend::boxed(vec![
            vec![(Some("a"), 1.0), (Some("b"), 2.0)],
            vec![(Some("c"), 3.0), (Some("a"), 1.0), (Some("b"), 2.0)],
        ]));
        app.poll();
        app.selected = 1; // "b"
        app.poll();
        assert_eq!(app.gpus[app.selected].name, "b");

        // A selected device that leaves clamps into range rather than
        // pointing past the end.
        let mut app = app_with(ScriptedBackend::boxed(vec![
            vec![(Some("a"), 1.0), (Some("b"), 2.0)],
            vec![(Some("a"), 1.0)],
        ]));
        app.poll();
        app.selected = 1;
        app.poll();
        assert_eq!(app.selected, 0);
    }

    /// Folding is a property of the GPU, so adding or removing a card must
    /// not move the fold onto a different one.
    #[test]
    fn a_fold_survives_a_device_being_added_or_removed() {
        let mut app = app_with(ScriptedBackend::boxed(vec![
            vec![(Some("a"), 1.0), (Some("b"), 2.0)],
            // "c" arrives ahead of both, pushing "b" from slot 1 to slot 2.
            vec![(Some("c"), 3.0), (Some("a"), 1.0), (Some("b"), 2.0)],
            // ...and then "a" leaves.
            vec![(Some("c"), 3.0), (Some("b"), 2.0)],
        ]));
        app.poll();
        app.selected = 1;
        app.apply(Action::Digit(1)); // fold "b"
        assert!(app.is_folded(1));

        app.poll();
        assert!(app.is_folded(2), "the fold did not follow the device");
        assert!(!app.is_folded(0) && !app.is_folded(1));

        app.poll();
        assert!(app.is_folded(1));
        assert!(!app.is_folded(0));
    }

    /// Folds are persisted by id; a positional key means nothing to the next
    /// run, so it never reaches the file.
    #[test]
    fn only_identified_devices_persist_their_fold() {
        let mut app = app_with(ScriptedBackend::boxed(vec![vec![
            (Some("a"), 1.0),
            (None, 2.0),
        ]]));
        app.poll();
        app.selected = 0;
        app.apply(Action::Digit(0));
        app.selected = 1;
        app.apply(Action::Digit(1));
        assert!(app.is_folded(0) && app.is_folded(1));
        assert_eq!(app.persisted_folds(), vec!["a".to_string()]);
    }

    /// A pre-identity state file carries `folded` as bare positions. It must
    /// load — losing the sort and the poll rate too would be a second bug —
    /// and those positions must not fold anything.
    #[test]
    fn legacy_positional_folds_are_dropped_not_applied() {
        let legacy: UiState = serde_json::from_str(
            r#"{"folded":[1],"sort_by":"Pid","sort_desc":false,"tick_ms":300}"#,
        )
        .expect("a pre-identity state file must still parse");
        assert!(legacy.folded_devices.is_empty());
        assert_eq!(legacy.sort_by, SortBy::Pid);
        assert_eq!(legacy.sticky_tick_ms(), Some(300));

        let mut app = app_with(ScriptedBackend::boxed(vec![vec![
            (Some("a"), 1.0),
            (Some("b"), 2.0),
        ]]));
        app.restore_state(&legacy);
        app.poll();
        assert!(!app.is_folded(0) && !app.is_folded(1));
    }

    #[test]
    fn state_restored_by_id_folds_that_device_wherever_it_sits() {
        let state: UiState = serde_json::from_str(
            r#"{"folded_devices":["b"],"sort_by":"Pid","sort_desc":false,"tick_ms":0}"#,
        )
        .unwrap();
        let mut app = app_with(ScriptedBackend::boxed(vec![vec![
            (Some("c"), 3.0),
            (Some("a"), 1.0),
            (Some("b"), 2.0),
        ]]));
        app.restore_state(&state);
        app.poll();
        assert!(app.is_folded(2));
        assert!(!app.is_folded(0) && !app.is_folded(1));
    }

    /// A backend that cannot identify its devices gets positional keys, and
    /// so does the second of two devices claiming the same id — sharing one
    /// key would fold two cards' samples into one history.
    #[test]
    fn unidentifiable_and_duplicate_ids_fall_back_to_position() {
        let snap = |id: Option<&str>| GpuSnapshot {
            device_id: id.map(str::to_string),
            ..Default::default()
        };
        assert_eq!(
            device_keys(&[snap(Some("a")), snap(None), snap(Some("a"))]),
            vec![
                DeviceKey::Id("a".into()),
                DeviceKey::Pos(1),
                DeviceKey::Pos(2)
            ]
        );
    }

    /// Keying by id means a departed GPU's state is no longer dropped by a
    /// shorter vec, so it has to be evicted deliberately.
    #[test]
    fn state_for_departed_devices_is_bounded() {
        /// Pathological hotplug: every poll shows one brand-new device and
        /// the previous one is gone for good.
        struct ChurnBackend(u64);
        impl GpuBackend for ChurnBackend {
            fn name(&self) -> &'static str {
                "churn"
            }
            fn poll(&mut self) -> anyhow::Result<Vec<GpuSnapshot>> {
                self.0 += 1;
                Ok(vec![GpuSnapshot {
                    device_id: Some(format!("dev-{}", self.0)),
                    ..Default::default()
                }])
            }
        }

        let churn = MAX_ABSENT_DEVICES * 3;
        let mut app = app_with(Box::new(ChurnBackend(0)));
        for _ in 0..churn {
            app.poll();
        }
        // Exactly the cap: one live device plus the retained departed ones,
        // proving eviction ran rather than the churn simply being short.
        assert_eq!(app.history.len(), 1 + MAX_ABSENT_DEVICES);
        assert_eq!(app.history.len(), app.session.len());
        assert_eq!(app.history.len(), app.last_seen.len());
        // A device seen recently is still there to come back to.
        assert!(app.history.contains_key(app.keys.last().unwrap()));
    }

    #[test]
    fn cgroup_container_detection() {
        assert_eq!(
            container_of_cgroup(
                "0::/system.slice/docker-abcdef123456789000000000000000000000000000000000000000000000dead.scope"
            )
            .as_deref(),
            Some("docker:abcdef123456")
        );
        assert_eq!(
            container_of_cgroup("0::/machine.slice/libpod-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.scope")
                .as_deref(),
            Some("podman:0123456789ab")
        );
        assert_eq!(
            container_of_cgroup("0::/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod1234.slice/cri-containerd-fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210.scope")
                .as_deref(),
            Some("k8s:fedcba987654")
        );
        assert_eq!(
            container_of_cgroup(
                "12:pids:/docker/00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff"
            )
            .as_deref(),
            Some("docker:00ff00ff00ff")
        );
        assert_eq!(
            container_of_cgroup("0::/user.slice/user-1000.slice/session-2.scope"),
            None
        );
        assert_eq!(container_of_cgroup("0::/system.slice/sshd.service"), None);
    }

    /// Everything the TUI persists has to come back on the next run. The
    /// atomic-write rework moved the writing side onto a temp file and a
    /// rename; if the published name or the serialisation drifted from what
    /// the loader reads, the only symptom would be folds, sort order and
    /// poll rate quietly resetting on every launch.
    #[test]
    fn a_saved_state_is_read_back_unchanged_by_the_loader() {
        let scratch = Scratch::new("state-roundtrip");
        let path = scratch.join("state.json");
        let state = UiState {
            folded_devices: vec!["GPU-abc".into(), "GPU-def".into()],
            sort_by: SortBy::Pid,
            sort_desc: false,
            tick_ms: 250,
            tick_ms_explicit: Some(true),
        };
        write_state(&path, &state);

        let back = read_state(&path).expect("a state that was just saved would not load");
        assert_eq!(back.folded_devices, state.folded_devices);
        assert_eq!(back.sort_by, state.sort_by);
        assert_eq!(back.sort_desc, state.sort_desc);
        assert_eq!(back.tick_ms, state.tick_ms);
        assert_eq!(back.tick_ms_explicit, state.tick_ms_explicit);
        // The scratch file is an implementation detail of the save, not
        // something every quit should leave lying in the cache dir.
        assert!(
            !state_temp_path(&path).exists(),
            "the temp file outlived a successful save"
        );
    }

    /// A save that cannot complete must not cost the user the state they
    /// already had. `fs::write` truncated the target before writing a byte,
    /// so anything going wrong past that point left a short file that
    /// `read_state` can only discard — silently, since a monitor never
    /// refuses to quit over a bad save. The temp file plus rename never
    /// touches the target until every byte is on disk.
    ///
    /// The failure is staged by putting a *directory* where the temp file
    /// wants to be: deterministic on every platform, and unlike a read-only
    /// parent it still fails when the suite happens to run as root.
    #[test]
    fn a_failed_save_leaves_the_previous_state_file_intact() {
        let scratch = Scratch::new("state-failed-save");
        let path = scratch.join("state.json");
        write_state(
            &path,
            &UiState {
                tick_ms: 250,
                ..Default::default()
            },
        );

        std::fs::create_dir_all(state_temp_path(&path)).unwrap();
        write_state(
            &path,
            &UiState {
                tick_ms: 999,
                ..Default::default()
            },
        );

        let back = read_state(&path).expect("a failed save destroyed the previous state file");
        assert_eq!(
            back.tick_ms, 250,
            "a save that never completed was published anyway"
        );
    }
}
