mod app;
mod backend;
mod cli;
mod config;
mod keys;
mod splash;
mod theme;
mod ui;

use anyhow::Result;
use app::{App, Focus, InputMode};
use clap::Parser;
use cli::Cli;
use config::GpurConfig;
use crossterm::event::KeyCode;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseButton, MouseEventKind,
};
use keys::Action;
use ratatui::layout::Position;
use std::io::stdout;
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Packaging helpers (hidden): emit completions / man page and exit.
    if let Some(shell) = cli.completions {
        use clap::CommandFactory;
        clap_complete::generate(shell, &mut Cli::command(), "gpur", &mut stdout());
        return Ok(());
    }
    if cli.man {
        use clap::CommandFactory;
        clap_mangen::Man::new(Cli::command()).render(&mut stdout())?;
        return Ok(());
    }

    let cfg: GpurConfig = match &cli.config {
        Some(path) => hjkl_config::load_from(path)?,
        None => hjkl_config::load()?.0,
    };
    // Precedence: CLI flag > last-used (persisted state) > config default.
    let state = app::load_state();
    let tick_ms = cli
        .tick_ms
        .or(state.as_ref().map(|s| s.tick_ms).filter(|t| *t > 0))
        .unwrap_or(cfg.tick_ms)
        .max(50);
    let theme_path = cli.theme.clone().or(cfg.theme.clone());

    let theme = theme::load(theme_path.as_deref(), theme::detect_color_mode())?;
    let backend = backend::detect(cli.mock, cli.replay.as_deref())?;
    let graph_style = match cli.graphs {
        Some(s) => s,
        None => app::GraphStyle::from_config(&cfg.graphs).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown graphs value {:?} in config (expected braille, block, or ascii)",
                cfg.graphs
            )
        })?,
    };
    let log = match &cli.log {
        Some(path) => Some(std::io::BufWriter::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?,
        )),
        None => None,
    };

    let mut app = App::new(
        backend,
        theme,
        app::AppOptions {
            tick_ms,
            history_len: cfg.history_len,
            no_splash: cli.no_splash,
            graph_style,
            mock: cli.mock,
            log,
        },
    );
    if let Some(s) = &state {
        app.restore_state(s);
    }
    app.poll();

    if cli.once || cli.json {
        return snapshot(&mut app, cli.json, tick_ms);
    }

    // ratatui::init installs a panic hook restoring raw mode + alt screen;
    // it knows nothing about mouse capture or the kitty protocol, so chain
    // our teardown in front of it — a panic must not leave the shell with
    // mouse reporting on.
    let mut terminal = ratatui::init();
    hjkl_kitty::enable(&mut stdout())?;
    crossterm::execute!(stdout(), EnableMouseCapture)?;
    {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_extras();
            prev(info);
        }));
    }
    install_signal_teardown();

    let result = run(&mut terminal, &mut app);
    app.save_state();
    restore_extras();
    ratatui::restore();
    result
}

/// Undo what we set up beyond ratatui's own raw-mode/alt-screen handling.
/// Safe to call more than once — both sequences are idempotent pops.
fn restore_extras() {
    let _ = crossterm::execute!(stdout(), DisableMouseCapture);
    let _ = hjkl_kitty::disable(&mut stdout());
}

/// External SIGTERM/SIGHUP/SIGINT would otherwise kill the process with raw
/// mode, the alt screen, and mouse capture still active. (Ctrl-C arrives as
/// a key event under raw mode, so SIGINT here means an outside `kill -INT`.)
#[cfg(unix)]
fn install_signal_teardown() {
    use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;
    let Ok(mut signals) = Signals::new([SIGTERM, SIGHUP, SIGINT]) else {
        return;
    };
    std::thread::spawn(move || {
        if let Some(sig) = signals.forever().next() {
            restore_extras();
            ratatui::restore();
            // Conventional 128+signal exit code.
            std::process::exit(128 + sig);
        }
    });
}

/// Windows: console close / logoff / shutdown deliver CTRL_*_EVENTs on a
/// system thread — the only interception point for a vanishing console.
/// (Ctrl-C itself arrives as a key event under raw mode.)
#[cfg(windows)]
fn install_signal_teardown() {
    use windows::Win32::System::Console::SetConsoleCtrlHandler;

    unsafe extern "system" fn handler(_ctrl_type: u32) -> windows::core::BOOL {
        restore_extras();
        ratatui::restore();
        std::process::exit(130);
    }
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(handler), true);
    }
}

#[cfg(not(any(unix, windows)))]
fn install_signal_teardown() {}

/// Headless one-shot: a second poll after a short gap makes the delta-based
/// utilizations (Intel, per-process) real instead of zero.
fn snapshot(app: &mut App, json: bool, tick_ms: u64) -> Result<()> {
    std::thread::sleep(Duration::from_millis(tick_ms.clamp(100, 1000)));
    app.poll();

    if json {
        let out = serde_json::json!({
            "backend": app.backend.name(),
            "gpus": app.gpus,
            "processes": app.procs,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    for (i, g) in app.gpus.iter().enumerate() {
        let mut line = format!(
            "{i}  {}  util {:>3.0}%  vram {}/{}MiB",
            g.name,
            g.utilization_pct,
            g.vram_used_bytes / 1024 / 1024,
            g.vram_total_bytes / 1024 / 1024,
        );
        if let Some(t) = g.temperature_c {
            line.push_str(&format!("  {t:.0}°C"));
        }
        if let Some(w) = g.power_w {
            line.push_str(&format!("  {w:.0}W"));
        }
        println!("{line}");
    }
    for p in &app.procs {
        println!(
            "  pid {:>7}  gpu {}  {:>4}  {:>5}MiB  {}",
            p.pid,
            p.gpu_index,
            p.gpu_util_pct
                .map(|u| format!("{u:.0}%"))
                .unwrap_or_else(|| "-".into()),
            p.gpu_mem_bytes / 1024 / 1024,
            p.command,
        );
    }
    Ok(())
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
                    match app.input_mode {
                        // Filter editing: raw input, bypasses the keymap.
                        InputMode::Filter => match key.code {
                            KeyCode::Enter => app.commit_filter(),
                            KeyCode::Esc => app.input_mode = InputMode::Normal,
                            KeyCode::Backspace => {
                                app.filter_input.pop();
                            }
                            KeyCode::Char(c) => app.filter_input.push(c),
                            _ => {}
                        },
                        // Kill confirmation: y confirms, anything else cancels.
                        InputMode::Confirm => {
                            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                                app.confirm_kill();
                            } else {
                                app.pending_kill = None;
                                app.input_mode = InputMode::Normal;
                            }
                        }
                        InputMode::Normal => {
                            // Any key dismisses the help overlay.
                            if app.show_help {
                                app.show_help = false;
                                continue;
                            }
                            if let Some(action) = keys::resolve(&mut keymap, key)
                                && app.apply(action)
                            {
                                return Ok(());
                            }
                        }
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
