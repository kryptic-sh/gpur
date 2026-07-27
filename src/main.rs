mod app;
mod backend;
mod cli;
mod config;
mod keys;
mod splash;
mod theme;
mod ui;

use anyhow::{Context, Result};
use app::{App, Focus, InputMode};
use clap::Parser;
use cli::Cli;
use config::GpurConfig;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use keys::Action;
use ratatui::layout::Position;
use std::io::{IsTerminal, stdout};
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Packaging helpers (hidden): emit completions / man page and exit.
    if let Some(shell) = cli.completions {
        use clap::CommandFactory;
        shell.generate(&mut Cli::command());
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
    // Headless output must not depend on what a human once did in the TUI:
    // sort/fold state is a UI preference, not part of the data.
    let headless = cli.once || cli.json;
    // Precedence: CLI flag > last-used (persisted state) > config default.
    // Only an *interactively* chosen rate is "last-used" — otherwise the
    // state file shadows config.toml forever after the first run.
    let state = app::load_state().filter(|_| !headless);
    let saved_tick = state.as_ref().and_then(app::UiState::sticky_tick_ms);
    let tick_ms = cli
        .tick_ms
        .or(saved_tick)
        .unwrap_or(cfg.tick_ms)
        .max(app::MIN_TICK_MS);
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
            // Sticky only if it was already an interactive choice; a config,
            // a --tick-ms value, or a pre-flag state file stays non-sticky.
            tick_explicit: cli.tick_ms.is_none()
                && state.as_ref().is_some_and(app::UiState::tick_chosen_by_key),
            history_len: cfg.history_len,
            no_splash: cli.no_splash,
            graph_style,
            mock: cli.mock,
            log,
        },
    );
    if headless {
        return snapshot(&mut app, cli.json, tick_ms);
    }
    if let Some(s) = &state {
        app.restore_state(s);
    }
    app.poll();

    // A pipe, a cron job or a CI runner has no terminal to put into raw
    // mode; ratatui::init() would unwrap the error into a panic backtrace.
    if !stdout().is_terminal() {
        anyhow::bail!("stdout is not a terminal — use --once or --json for non-interactive output");
    }
    // ratatui::init installs a panic hook restoring raw mode + alt screen;
    // it knows nothing about mouse capture or the kitty protocol, so chain
    // our teardown in front of it — a panic must not leave the shell with
    // mouse reporting on.
    let mut terminal = ratatui::try_init().context("initializing the terminal")?;
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
/// utilizations (Intel, per-process) real instead of zero. The priming poll
/// is not logged — `--once --log` must write exactly one record.
fn snapshot(app: &mut App, json: bool, tick_ms: u64) -> Result<()> {
    app.poll_priming();
    std::thread::sleep(Duration::from_millis(tick_ms.clamp(100, 1000)));
    app.poll();

    if json {
        println!("{}", serde_json::to_string_pretty(&app.record())?);
        return Ok(());
    }

    // `n/a` rather than a fabricated 0: a metric the backend cannot read must
    // not print as a confident "idle"/"empty" in a scriptable output either.
    let mib = |b: Option<u64>| {
        b.map(|b| format!("{}MiB", b / 1024 / 1024))
            .unwrap_or_else(|| "n/a".to_string())
    };
    for (i, g) in app.gpus.iter().enumerate() {
        let mut line = format!(
            "{i}  {}  util {}  vram {}/{}",
            g.name,
            g.utilization_pct
                .map(|u| format!("{u:>3.0}%"))
                .unwrap_or_else(|| "n/a".to_string()),
            mib(g.vram_used_bytes),
            mib(g.vram_total_bytes),
        );
        if let Some(t) = g.temperature_c {
            line.push_str(&format!("  {t:.0}°C"));
        }
        if let Some(w) = g.power_w {
            line.push_str(&format!("  {w:.0}W"));
        }
        println!("{line}");
    }
    // Same rows the JSON record carries: unfiltered, deterministic order.
    for p in &app.all_procs {
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

/// Splash animation frame budget (~16 fps).
const SPLASH_FRAME_MS: u64 = 60;

/// How long to wait for input before the next frame is due. Frames and
/// polls run on separate clocks: the splash animates at ~16 fps while polls
/// stay at `tick_ms`, and pacing frames off the poll clock spun the loop at
/// full speed for the rest of every tick.
fn event_timeout(
    splash: bool,
    tick_ms: u64,
    since_draw: Duration,
    since_poll: Duration,
) -> Duration {
    let frame_interval = if splash {
        Duration::from_millis(SPLASH_FRAME_MS)
    } else {
        Duration::from_millis(tick_ms)
    };
    let tick = Duration::from_millis(tick_ms);
    frame_interval
        .saturating_sub(since_draw)
        .min(tick.saturating_sub(since_poll))
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    let mut keymap = keys::default_keymap();
    let mut last_poll = Instant::now();

    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;
        let last_draw = Instant::now();

        let timeout = event_timeout(
            app.splash_active(),
            app.tick_ms,
            last_draw.elapsed(),
            last_poll.elapsed(),
        );

        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if app.splash_active() {
                        app.splash_skipped = true;
                        continue;
                    }
                    match app.input_mode {
                        // Filter editing: raw input, bypasses the keymap.
                        // Control chords must never reach the buffer — raw
                        // mode gives no SIGINT, so a literal 'c' from Ctrl-C
                        // left the app with no way out but Esc.
                        InputMode::Filter => {
                            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                            match key.code {
                                KeyCode::Char('c') if ctrl => return Ok(()),
                                KeyCode::Char('u') if ctrl => app.filter_input.clear(),
                                KeyCode::Char('w') if ctrl => app.filter_delete_word(),
                                // Any other control chord is ignored, not typed.
                                KeyCode::Char(_) if ctrl => {}
                                KeyCode::Enter => app.commit_filter(),
                                KeyCode::Esc => app.input_mode = InputMode::Normal,
                                KeyCode::Backspace => {
                                    app.filter_input.pop();
                                }
                                KeyCode::Char(c) => app.filter_input.push(c),
                                _ => {}
                            }
                        }
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
                    // Modals own the screen: a wheel or click must not move
                    // the selection underneath the kill dialog, the filter
                    // input, or the help overlay.
                    if app.show_help || app.input_mode != InputMode::Normal {
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
                            // Rows start after the top border + header line
                            // and stop before the bottom border — which
                            // proc_rect.contains() counts as inside.
                            let first_row_y = app.proc_rect.y + 2;
                            let border_y = app.proc_rect.bottom().saturating_sub(1);
                            if m.row >= first_row_y && m.row < border_y {
                                let clicked = app.proc_scroll + (m.row - first_row_y) as usize;
                                // Only rows actually on screen: a click must
                                // select what was clicked, never scroll.
                                if clicked < app.proc_scroll + app.proc_visible {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The splash busy-loop: with frames paced off the *poll* clock, the
    /// timeout collapsed to zero for the rest of every tick and the loop
    /// spun at ~1000 fps. Frames get their own clock now.
    #[test]
    fn splash_frames_wait_a_frame_not_zero() {
        // 100 ms into a 1000 ms tick, 40 ms into a 60 ms frame.
        let t = event_timeout(
            true,
            1000,
            Duration::from_millis(40),
            Duration::from_millis(100),
        );
        assert_eq!(t, Duration::from_millis(SPLASH_FRAME_MS - 40));
    }

    #[test]
    fn a_due_poll_wins_over_a_later_frame() {
        // 990 ms into the tick: the poll is 10 ms away, the frame 60 ms.
        let t = event_timeout(true, 1000, Duration::ZERO, Duration::from_millis(990));
        assert_eq!(t, Duration::from_millis(10));
    }

    /// Outside the splash both clocks are the tick, so a fresh draw waits a
    /// whole tick and an overdue poll returns immediately.
    #[test]
    fn steady_state_paces_on_the_tick() {
        assert_eq!(
            event_timeout(false, 500, Duration::ZERO, Duration::ZERO),
            Duration::from_millis(500)
        );
        assert_eq!(
            event_timeout(false, 500, Duration::ZERO, Duration::from_millis(700)),
            Duration::ZERO
        );
    }
}
