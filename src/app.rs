use crate::backend::{GpuBackend, GpuSnapshot};
use crate::keys::Action;
use crate::theme::UiTheme;
use anyhow::Result;
use std::time::{Duration, Instant};

const SPLASH_MS: u64 = 1500;

#[derive(Default)]
pub struct History {
    pub util: Vec<u64>,
    pub vram: Vec<u64>,
    pub power: Vec<u64>,
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
        Ok(())
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
