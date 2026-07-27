use crate::backend::{GpuBackend, GpuSnapshot, ProcKind};
use crate::keys::Action;
use crate::theme::UiTheme;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind, Users};

const SPLASH_MS: u64 = 1500;
const STATUS_MS: u64 = 4000;

/// Poll-interval floor, shared by the CLI/config clamp and the `+` key.
/// Two different floors (50 vs 100) meant `--tick-ms 50` plus one `+`
/// *raised* the interval to 100 with no way back.
pub const MIN_TICK_MS: u64 = 50;

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

#[derive(Default)]
pub struct History {
    pub util: Vec<u64>,
    pub vram: Vec<u64>,
    pub power: Vec<u64>,
    pub temp: Vec<u64>,
}

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

/// One row of the process table: GPU stats + host-side enrichment.
#[derive(Clone, serde::Serialize)]
pub struct ProcRow {
    pub pid: u32,
    pub gpu_index: usize,
    pub kind: ProcKind,
    pub gpu_util_pct: Option<f64>,
    pub gpu_mem_bytes: u64,
    pub user: String,
    pub cpu_pct: f32,
    pub host_mem_bytes: u64,
    pub command: String,
    /// Container runtime + short id ("docker:ab12cd34ef56"), Linux only.
    pub container: Option<String>,
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
}

/// UI state persisted across runs (folded cards, sort, poll rate) — the
/// tikr session.json pattern: auto-saved on clean quit into the cache dir,
/// never a config file.
#[derive(serde::Serialize, serde::Deserialize, Default)]
pub struct UiState {
    pub folded: Vec<usize>,
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
    let text = std::fs::read_to_string(state_path()?).ok()?;
    serde_json::from_str(&text).ok()
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
    pub mock: Option<usize>,
    pub log: Option<std::io::BufWriter<std::fs::File>>,
}

pub struct App {
    pub backend: Box<dyn GpuBackend>,
    pub gpus: Vec<GpuSnapshot>,
    pub history: Vec<History>,
    /// Per-GPU peaks/averages since launch.
    pub session: Vec<SessionStats>,
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
    /// GPU indices folded to a one-line summary (digit keys toggle).
    pub folded: std::collections::HashSet<usize>,
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
    /// The --mock argument, kept for backend re-detection.
    mock: Option<usize>,
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
    users: Users,
}

impl App {
    pub fn new(backend: Box<dyn GpuBackend>, theme: UiTheme, opts: AppOptions) -> Self {
        let AppOptions {
            tick_ms,
            tick_explicit,
            history_len,
            no_splash,
            graph_style,
            mock,
            log,
        } = opts;
        Self {
            tick_explicit,
            graph_style,
            mock,
            poll_failures: 0,
            log,
            backend,
            gpus: Vec::new(),
            history: Vec::new(),
            session: Vec::new(),
            history_len,
            selected: 0,
            paused: false,
            tick_ms,
            theme,
            started: Instant::now(),
            splash_path: crate::splash::build_path(),
            splash_skipped: no_splash,
            procs: Vec::new(),
            folded: std::collections::HashSet::new(),
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
            users: Users::new_with_refreshed_list(),
        }
    }

    pub fn restore_state(&mut self, s: &UiState) {
        self.folded = s.folded.iter().copied().collect();
        self.sort_by = s.sort_by;
        self.sort_desc = s.sort_desc;
    }

    /// Best-effort save on clean quit; silent on failure (a monitor must
    /// never refuse to exit over a full disk).
    pub fn save_state(&self) {
        let Some(path) = state_path() else { return };
        let state = UiState {
            folded: self.folded.iter().copied().collect(),
            sort_by: self.sort_by,
            sort_desc: self.sort_desc,
            tick_ms: self.tick_ms,
            tick_ms_explicit: Some(self.tick_explicit),
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(&state) {
            let _ = std::fs::write(path, json);
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
                // consecutive failure.
                if self.poll_failures.is_multiple_of(5)
                    && let Ok(fresh) = crate::backend::detect(self.mock, None)
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
        self.history.resize_with(self.gpus.len(), History::default);
        self.session
            .resize_with(self.gpus.len(), SessionStats::default);
        if self.selected >= self.gpus.len() {
            self.selected = self.gpus.len().saturating_sub(1);
        }
        for (gpu, sess) in self.gpus.iter().zip(&mut self.session) {
            sess.add(gpu);
        }
        for (gpu, hist) in self.gpus.iter().zip(&mut self.history) {
            // A waveform has no glyph for "unknown", so an unreadable metric
            // records as a flat 0 line. The meter above it says `n/a`, which
            // is where the distinction is actually made.
            hist.util
                .push(gpu.utilization_pct.unwrap_or(0.0).round() as u64);
            hist.vram.push(gpu.vram_pct().unwrap_or(0.0).round() as u64);
            hist.power.push(gpu.power_w.unwrap_or(0.0).round() as u64);
            hist.temp
                .push(gpu.temperature_c.unwrap_or(0.0).round() as u64);
            // Config history_len is a MINIMUM; keep at least what the
            // widest graph can display (+slack for resize wiggle).
            let cap = self.history_len.max(self.history_need + 8);
            let overflow = hist.util.len().saturating_sub(cap);
            if overflow > 0 {
                hist.util.drain(..overflow);
                hist.vram.drain(..overflow);
                hist.power.drain(..overflow);
                hist.temp.drain(..overflow);
            }
        }
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
        // The plain refresh_processes() kind omits user and cmd — ask for
        // exactly what the table shows.
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            true,
            ProcessRefreshKind::nothing()
                .with_memory()
                .with_cpu()
                .with_user(UpdateKind::OnlyIfNotSet)
                .with_cmd(UpdateKind::OnlyIfNotSet),
        );

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
                    cpu_pct: gp.cpu_pct.or(p.map(|p| p.cpu_usage())).unwrap_or(0.0),
                    host_mem_bytes: gp.host_mem_bytes.or(p.map(|p| p.memory())).unwrap_or(0),
                    command: gp
                        .command
                        .clone()
                        .or(p.map(command_of))
                        .unwrap_or_else(|| "?".into()),
                    container: gp.container.clone().or_else(|| container_of_pid(gp.pid)),
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
        self.all_procs.sort_by(|a, b| {
            b.gpu_mem_bytes
                .cmp(&a.gpu_mem_bytes)
                .then(a.pid.cmp(&b.pid))
                .then(a.gpu_index.cmp(&b.gpu_index))
        });
        self.rebuild_proc_view();
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

        rows.sort_by(|a, b| {
            // A row with no GPU% is unmeasured, not idle. Sink it below every
            // measured row in BOTH directions — folding it in as 0.0 would
            // hand it the top of an ascending sort.
            if self.sort_by == SortBy::GpuUtil {
                match (a.gpu_util_pct.is_none(), b.gpu_util_pct.is_none()) {
                    (false, true) => return std::cmp::Ordering::Less,
                    (true, false) => return std::cmp::Ordering::Greater,
                    _ => {}
                }
            }
            let ord = match self.sort_by {
                SortBy::GpuMem => a.gpu_mem_bytes.cmp(&b.gpu_mem_bytes),
                SortBy::GpuUtil => a
                    .gpu_util_pct
                    .unwrap_or(0.0)
                    .total_cmp(&b.gpu_util_pct.unwrap_or(0.0)),
                SortBy::Cpu => a.cpu_pct.total_cmp(&b.cpu_pct),
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
        self.pending_kill = Some(PendingKill {
            pid,
            start_time,
            force,
            command,
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
        // `refresh_processes` only ever refreshes the pids the backend still
        // lists, and sysinfo never evicts a pid outside the update set — so a
        // process that exited after the dialog opened is still cached here.
        // Refresh this one pid (remove_dead evicts it if gone) and demand the
        // same start time, or we'd signal whoever inherited the number.
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
        // kill_with returns None when the signal isn't supported on this
        // platform (e.g. Term on Windows) — fall back to plain kill().
        let sig = if force {
            sysinfo::Signal::Kill
        } else {
            sysinfo::Signal::Term
        };
        let ok = p.kill_with(sig).unwrap_or_else(|| p.kill());
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
                self.tick_ms = (self.tick_ms * 2).min(10_000);
                self.tick_explicit = true;
            }
            Action::Digit(i) => {
                if i < self.gpus.len() {
                    if self.focus == Focus::Gpus && self.selected == i {
                        // Second press on the selected GPU folds it.
                        if !self.folded.remove(&i) {
                            self.folded.insert(i);
                        }
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
                    gpu_mem_bytes: 1,
                    command: Some("idle".into()),
                    user: Some("root".into()),
                    ..Default::default()
                },
                crate::backend::GpuProcess {
                    pid: 4242,
                    gpu_mem_bytes: 2 << 30,
                    command: Some("train.py".into()),
                    user: Some("bob".into()),
                    container: Some("docker:abcdef123456".into()),
                    ..Default::default()
                },
            ]
        }
    }

    fn app_with(backend: Box<dyn GpuBackend>) -> App {
        App::new(
            backend,
            crate::theme::load(None, crate::theme::detect_color_mode()).unwrap(),
            AppOptions {
                tick_ms: 1000,
                tick_explicit: false,
                history_len: 60,
                no_splash: true,
                graph_style: GraphStyle::Ascii,
                mock: None,
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
        let dir = std::env::temp_dir().join(format!("gpur-can-signal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("rec.jsonl");
        std::fs::write(&log, "{\"gpus\":[],\"processes\":[]}\n").unwrap();
        let replay = crate::backend::detect(None, Some(&log)).unwrap();
        assert_eq!(replay.name(), "replay");
        assert!(!replay.can_signal());
        std::fs::remove_dir_all(&dir).ok();
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

    #[test]
    fn kill_needs_the_process_pane_focused() {
        let mut app = app_with(Box::new(LocalBackend));
        app.all_procs = vec![ProcRow {
            pid: std::process::id(),
            gpu_index: 0,
            kind: ProcKind::Compute,
            gpu_util_pct: None,
            gpu_mem_bytes: 0,
            user: "me".into(),
            cpu_pct: 0.0,
            host_mem_bytes: 0,
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

    /// Finding 7 in the process table: "unmeasured" must not win an ascending
    /// GPU% sort by masquerading as 0.
    #[test]
    fn rows_with_unknown_gpu_util_sink_in_both_directions() {
        let mut app = app_with(Box::new(LocalBackend));
        let row = |pid, util| ProcRow {
            pid,
            gpu_index: 0,
            kind: ProcKind::Compute,
            gpu_util_pct: util,
            gpu_mem_bytes: 0,
            user: "me".into(),
            cpu_pct: 0.0,
            host_mem_bytes: 0,
            command: "x".into(),
            container: None,
        };
        app.all_procs = vec![row(1, Some(10.0)), row(2, None), row(3, Some(90.0))];
        app.sort_by = SortBy::GpuUtil;

        app.sort_desc = true;
        app.rebuild_proc_view();
        assert_eq!(
            app.procs.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![3, 1, 2]
        );

        app.sort_desc = false;
        app.rebuild_proc_view();
        assert_eq!(
            app.procs.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![1, 3, 2],
            "an unmeasured row sorted as if it were 0%"
        );
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
}
