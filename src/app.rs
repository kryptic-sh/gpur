use crate::backend::{GpuBackend, GpuSnapshot, ProcKind};
use crate::keys::Action;
use crate::theme::UiTheme;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind, Users};

const SPLASH_MS: u64 = 1500;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Gpus,
    Procs,
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

/// One row of the process table: GPU stats + host-side enrichment.
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
}

pub struct App {
    pub backend: Box<dyn GpuBackend>,
    pub gpus: Vec<GpuSnapshot>,
    pub history: Vec<History>,
    pub history_len: usize,
    pub selected: usize,
    pub paused: bool,
    pub tick_ms: u64,
    pub theme: UiTheme,
    pub started: Instant,
    pub splash_path: Vec<(u8, u8, char)>,
    pub splash_skipped: bool,
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
    /// (rect, gpu index) of each card drawn last frame, for click hit-tests.
    pub card_rects: Vec<(ratatui::layout::Rect, usize)>,
    sys: System,
    users: Users,
}

impl App {
    pub fn new(
        backend: Box<dyn GpuBackend>,
        theme: UiTheme,
        tick_ms: u64,
        history_len: usize,
        no_splash: bool,
    ) -> Self {
        Self {
            backend,
            gpus: Vec::new(),
            history: Vec::new(),
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
            card_rects: Vec::new(),
            sys: System::new(),
            users: Users::new_with_refreshed_list(),
        }
    }

    pub fn splash_active(&self) -> bool {
        !self.splash_skipped && self.started.elapsed() < Duration::from_millis(SPLASH_MS)
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
            }
            Err(e) => {
                self.poll_error = Some(format!("poll failed: {e:#}"));
                return; // keep previous snapshot and history
            }
        }
        self.history.resize_with(self.gpus.len(), History::default);
        if self.selected >= self.gpus.len() {
            self.selected = self.gpus.len().saturating_sub(1);
        }
        for (gpu, hist) in self.gpus.iter().zip(&mut self.history) {
            hist.util.push(gpu.utilization_pct.round() as u64);
            hist.vram.push(gpu.vram_pct().round() as u64);
            hist.power.push(gpu.power_w.unwrap_or(0.0).round() as u64);
            hist.temp
                .push(gpu.temperature_c.unwrap_or(0.0).round() as u64);
            let overflow = hist.util.len().saturating_sub(self.history_len);
            if overflow > 0 {
                hist.util.drain(..overflow);
                hist.vram.drain(..overflow);
                hist.power.drain(..overflow);
                hist.temp.drain(..overflow);
            }
        }
        self.refresh_processes();
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

        let mut rows: Vec<ProcRow> = gpu_procs
            .into_iter()
            .map(|gp| {
                let p = self.sys.process(Pid::from_u32(gp.pid));
                ProcRow {
                    user: p
                        .and_then(|p| p.user_id())
                        .and_then(|uid| self.users.get_user_by_id(uid))
                        .map(|u| u.name().to_string())
                        .unwrap_or_else(|| "-".into()),
                    cpu_pct: p.map(|p| p.cpu_usage()).unwrap_or(0.0),
                    host_mem_bytes: p.map(|p| p.memory()).unwrap_or(0),
                    command: p.map(command_of).unwrap_or_else(|| "?".into()),
                    pid: gp.pid,
                    gpu_index: gp.gpu_index,
                    kind: gp.kind,
                    gpu_util_pct: gp.gpu_util_pct,
                    gpu_mem_bytes: gp.gpu_mem_bytes,
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            b.gpu_mem_bytes
                .cmp(&a.gpu_mem_bytes)
                .then(a.pid.cmp(&b.pid))
        });
        self.procs = rows;
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
            Action::ProcScrollDown => self.proc_down(),
            Action::ProcScrollUp => self.proc_up(),
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
        if !self.gpus.is_empty() {
            self.selected = (self.selected + 1) % self.gpus.len();
        }
    }

    fn prev_gpu(&mut self) {
        if !self.gpus.is_empty() {
            self.selected = (self.selected + self.gpus.len() - 1) % self.gpus.len();
        }
    }
}
