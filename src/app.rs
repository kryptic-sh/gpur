use crate::backend::{GpuBackend, GpuSnapshot, ProcKind};
use crate::keys::Action;
use crate::theme::UiTheme;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind, Users};

const SPLASH_MS: u64 = 1500;
const STATUS_MS: u64 = 4000;

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

/// Running min/max/avg per GPU since launch (HWiNFO-style session stats).
#[derive(Default, Clone, serde::Serialize)]
pub struct SessionStats {
    pub max_util_pct: f64,
    pub max_temp_c: f64,
    pub max_power_w: f64,
    sum_util: f64,
    sum_power: f64,
    samples: u64,
}

impl SessionStats {
    fn add(&mut self, g: &GpuSnapshot) {
        self.max_util_pct = self.max_util_pct.max(g.utilization_pct);
        if let Some(t) = g.temperature_c {
            self.max_temp_c = self.max_temp_c.max(t);
        }
        if let Some(w) = g.power_w {
            self.max_power_w = self.max_power_w.max(w);
        }
        self.sum_util += g.utilization_pct;
        self.sum_power += g.power_w.unwrap_or(0.0);
        self.samples += 1;
    }

    pub fn avg_util_pct(&self) -> f64 {
        self.sum_util / self.samples.max(1) as f64
    }

    pub fn avg_power_w(&self) -> f64 {
        self.sum_power / self.samples.max(1) as f64
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

/// UI state persisted across runs (folded cards, sort, poll rate) — the
/// tikr session.json pattern: auto-saved on clean quit into the cache dir,
/// never a config file.
#[derive(serde::Serialize, serde::Deserialize, Default)]
pub struct UiState {
    pub folded: Vec<usize>,
    pub sort_by: SortBy,
    pub sort_desc: bool,
    pub tick_ms: u64,
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
    /// (pid, force, command) awaiting y/N confirmation.
    pub pending_kill: Option<(u32, bool, String)>,
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
    /// Unfiltered process rows; `procs` is the filtered+sorted view.
    all_procs: Vec<ProcRow>,
    sys: System,
    users: Users,
}

impl App {
    pub fn new(backend: Box<dyn GpuBackend>, theme: UiTheme, opts: AppOptions) -> Self {
        let AppOptions {
            tick_ms,
            history_len,
            no_splash,
            graph_style,
            mock,
            log,
        } = opts;
        Self {
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
            hist.util.push(gpu.utilization_pct.round() as u64);
            hist.vram.push(gpu.vram_pct().round() as u64);
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
        self.write_log();
    }

    /// Append one JSONL record per successful poll. A write error drops the
    /// logger with a status message instead of spamming or crashing.
    fn write_log(&mut self) {
        use std::io::Write;
        let Some(w) = self.log.as_mut() else { return };
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let rec = serde_json::json!({
            "ts_ms": ts,
            "gpus": self.gpus,
            "processes": self.all_procs,
        });
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
                    container: container_of_pid(gp.pid),
                    pid: gp.pid,
                    gpu_index: gp.gpu_index,
                    kind: gp.kind,
                    gpu_util_pct: gp.gpu_util_pct,
                    gpu_mem_bytes: gp.gpu_mem_bytes,
                }
            })
            .collect();
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

    /// Commit the filter edit buffer (Enter in filter mode).
    pub fn commit_filter(&mut self) {
        self.filter = self.filter_input.trim().to_string();
        self.input_mode = InputMode::Normal;
        self.rebuild_proc_view();
    }

    /// Send the pending signal (y in confirm mode).
    pub fn confirm_kill(&mut self) {
        let Some((pid, force, cmd)) = self.pending_kill.take() else {
            return;
        };
        self.input_mode = InputMode::Normal;
        let sig_name = if force { "SIGKILL" } else { "SIGTERM" };
        let Some(p) = self.sys.process(Pid::from_u32(pid)) else {
            self.set_status(format!("kill: pid {pid} no longer exists"));
            return;
        };
        // kill_with returns None when the signal isn't supported on this
        // platform (e.g. Term on Windows) — fall back to plain kill().
        let sig = if force {
            sysinfo::Signal::Kill
        } else {
            sysinfo::Signal::Term
        };
        let ok = p.kill_with(sig).unwrap_or_else(|| p.kill());
        if ok {
            self.set_status(format!("sent {sig_name} to {pid} ({cmd})"));
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
            Action::TickFaster => self.tick_ms = (self.tick_ms / 2).max(100),
            Action::TickSlower => self.tick_ms = (self.tick_ms * 2).min(10_000),
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
                if let Some(row) = self.procs.get(self.proc_sel) {
                    self.pending_kill = Some((
                        row.pid,
                        matches!(action, Action::KillForce),
                        row.command.chars().take(40).collect(),
                    ));
                    self.input_mode = InputMode::Confirm;
                }
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
    use super::container_of_cgroup;

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
