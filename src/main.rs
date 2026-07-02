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
use app::Focus;
use clap::Parser;
use cli::Cli;
use config::GpurConfig;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseButton, MouseEventKind,
};
use keys::Action;
use ratatui::layout::Position;
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
    app.poll();

    let mut terminal = ratatui::init();
    hjkl_kitty::enable(&mut stdout())?;
    crossterm::execute!(stdout(), EnableMouseCapture)?;
    let result = run(&mut terminal, &mut app);
    let _ = crossterm::execute!(stdout(), DisableMouseCapture);
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
                Event::Mouse(m) => {
                    if app.splash_active() {
                        app.splash_skipped = true;
                        continue;
                    }
                    let pos = Position::new(m.column, m.row);
                    let in_procs = app.proc_rect.contains(pos);
                    let in_gpus = app.gpus_rect.contains(pos);
                    let action = match m.kind {
                        // Wheel and click both focus the pane under the cursor.
                        MouseEventKind::ScrollDown if in_procs => {
                            app.focus = Focus::Procs;
                            Some(Action::ProcScrollDown)
                        }
                        MouseEventKind::ScrollUp if in_procs => {
                            app.focus = Focus::Procs;
                            Some(Action::ProcScrollUp)
                        }
                        MouseEventKind::ScrollDown if in_gpus => {
                            app.focus = Focus::Gpus;
                            Some(Action::NextGpu)
                        }
                        MouseEventKind::ScrollUp if in_gpus => {
                            app.focus = Focus::Gpus;
                            Some(Action::PrevGpu)
                        }
                        MouseEventKind::Down(MouseButton::Left) if in_procs => {
                            app.focus = Focus::Procs;
                            // Rows start after the top border + header line.
                            let first_row_y = app.proc_rect.y + 2;
                            if m.row >= first_row_y {
                                let clicked = app.proc_scroll + (m.row - first_row_y) as usize;
                                if clicked < app.procs.len() {
                                    app.proc_sel = clicked;
                                }
                            }
                            None
                        }
                        MouseEventKind::Down(MouseButton::Left) if in_gpus => {
                            app.focus = Focus::Gpus;
                            if let Some(&(_, i)) =
                                app.card_rects.iter().find(|(rect, _)| rect.contains(pos))
                            {
                                app.selected = i;
                            }
                            None
                        }
                        _ => None,
                    };
                    if let Some(action) = action {
                        app.apply(action);
                    }
                }
                _ => {}
            }
        }

        if last_poll.elapsed() >= Duration::from_millis(app.tick_ms) {
            app.poll();
            last_poll = Instant::now();
        }
    }
}
