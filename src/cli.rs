use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "gpur",
    version,
    about = "btop-style GPU monitor — NVIDIA, AMD, Apple Silicon",
    before_help = include_str!("art.txt")
)]
pub struct Cli {
    /// Use deterministic mock GPUs (demo the UI without hardware).
    /// Optionally pass how many, e.g. `--mock 6`.
    #[arg(long, num_args = 0..=1, default_missing_value = "2", value_name = "N")]
    pub mock: Option<usize>,

    /// Path to config.toml (default: $XDG_CONFIG_HOME/gpur/config.toml)
    #[arg(long, short)]
    pub config: Option<PathBuf>,

    /// Path to a theme TOML (overrides the config file)
    #[arg(long, short)]
    pub theme: Option<PathBuf>,

    /// Poll interval in milliseconds (overrides the config file)
    #[arg(long)]
    pub tick_ms: Option<u64>,

    /// Skip the startup splash
    #[arg(long)]
    pub no_splash: bool,

    /// Print one snapshot (two quick polls so utilization deltas are real)
    /// and exit — no TUI
    #[arg(long)]
    pub once: bool,

    /// Like --once but machine-readable JSON on stdout
    #[arg(long)]
    pub json: bool,

    /// Graph glyph set (overrides config `graphs`); ascii for terminals
    /// without braille/block fonts
    #[arg(long, value_enum)]
    pub graphs: Option<crate::app::GraphStyle>,

    /// Append one JSON line per poll to this file (sensor logging)
    #[arg(long, value_name = "FILE")]
    pub log: Option<PathBuf>,

    /// Print shell completions to stdout and exit
    #[arg(long, value_enum, value_name = "SHELL", hide = true)]
    pub completions: Option<clap_complete::Shell>,

    /// Print the man page (troff) to stdout and exit
    #[arg(long, hide = true)]
    pub man: bool,
}
