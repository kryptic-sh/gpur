mod app;
mod backend;
mod cli;
mod config;
mod keys;
mod splash;
mod theme;
mod ui;

use anyhow::Result;
use app::App;
use clap::Parser;
use cli::Cli;
use config::GpurConfig;
use crossterm::event::{self, Event, KeyEventKind};
use std::io::stdout;
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    let cli = Cli::parse();

    let cfg: GpurConfig = match &cli.config {
        Some(path) => hjkl_config::load_from(path)?,
        None => hjkl_config::load()?.0,
    };
    let tick_ms = cli.tick_ms.unwrap_or(cfg.tick_ms).max(50);
    let theme_path = cli.theme.clone().or(cfg.theme.clone());

    let theme = theme::load(theme_path.as_deref())?;
    let backend = backend::detect(cli.mock)?;

    let mut app = App::new(backend, theme, tick_ms, cfg.history_len, cli.no_splash);
    app.poll()?;

    let mut terminal = ratatui::init();
    hjkl_kitty::enable(&mut stdout())?;
    let result = run(&mut terminal, &mut app);
    let _ = hjkl_kitty::disable(&mut stdout());
    ratatui::restore();
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    let mut keymap = keys::default_keymap();
    let mut last_poll = Instant::now();

    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;

        let interval = if app.splash_active() {
            Duration::from_millis(60)
        } else {
            Duration::from_millis(app.tick_ms)
        };
        let timeout = interval.saturating_sub(last_poll.elapsed()).min(interval);

        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if app.splash_active() {
                        app.splash_skipped = true;
                        continue;
                    }
                    if let Some(action) = keys::resolve(&mut keymap, key)
                        && app.apply(action)
                    {
                        return Ok(());
                    }
                }
                _ => {}
            }
        }

        if last_poll.elapsed() >= Duration::from_millis(app.tick_ms) {
            app.poll()?;
            last_poll = Instant::now();
        }
    }
}
