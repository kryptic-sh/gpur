use crate::backend::{GpuBackend, GpuSnapshot, ProcKind};
use crate::keys::Action;
use crate::theme::UiTheme;
use anyhow::Result;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessesToUpdate, System, Users};

const SPLASH_MS: u64 = 1500;

#[derive(Default)]
pub struct History {
    pub util: Vec<u64>,
    pub vram: Vec<u64>,
    pub power: Vec<u64>,
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
            sys: System::new(),
            users: Users::new_with_refreshed_list(),
        }
    }

    pub fn splash_active(&self) -> bool {
        !self.splash_skipped && self.started.elapsed() < Duration::from_millis(SPLASH_MS)
    }

    pub fn poll(&mut self) -> Result<()> {
        if self.paused {
            return Ok(());
        }
        self.gpus = self.backend.poll()?;
        self.history.resize_with(self.gpus.len(), History::default);
        if self.selected >= self.gpus.len() {
            self.selected = self.gpus.len().saturating_sub(1);
        }
        for (gpu, hist) in self.gpus.iter().zip(&mut self.history) {
            hist.util.push(gpu.utilization_pct.round() as u64);
            hist.vram.push(gpu.vram_pct().round() as u64);
            hist.power.push(gpu.power_w.unwrap_or(0.0).round() as u64);
            let overflow = hist.util.len().saturating_sub(self.history_len);
            if overflow > 0 {
                hist.util.drain(..overflow);
                hist.vram.drain(..overflow);
                hist.power.drain(..overflow);
            }
        }
        self.refresh_processes();
        Ok(())
    }

    fn refresh_processes(&mut self) {
        let gpu_procs = self.backend.processes();
        let pids: Vec<Pid> = gpu_procs.iter().map(|p| Pid::from_u32(p.pid)).collect();
        self.sys
            .refresh_processes(ProcessesToUpdate::Some(&pids), true);

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
                    command: p
                        .map(|p| p.name().to_string_lossy().into_owned())
                        .unwrap_or_else(|| "?".into()),
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
            Action::NextGpu => {
                if !self.gpus.is_empty() {
                    self.selected = (self.selected + 1) % self.gpus.len();
                }
            }
            Action::PrevGpu => {
                if !self.gpus.is_empty() {
                    self.selected = (self.selected + self.gpus.len() - 1) % self.gpus.len();
                }
            }
            Action::TickFaster => self.tick_ms = (self.tick_ms / 2).max(100),
            Action::TickSlower => self.tick_ms = (self.tick_ms * 2).min(10_000),
        }
        false
    }
}
