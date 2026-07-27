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
    /// Optionally pass how many (1-16), e.g. `--mock 6`.
    // Range-checked here so an out-of-range count is rejected with a clap
    // error instead of being silently rewritten by a clamp.
    #[arg(
        long,
        num_args = 0..=1,
        default_missing_value = "2",
        value_name = "N",
        value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..=16),
    )]
    pub mock: Option<usize>,

    /// Path to config.toml (default: $XDG_CONFIG_HOME/gpur/config.toml)
    #[arg(long, short)]
    pub config: Option<PathBuf>,

    /// Path to a theme TOML (overrides the config file)
    #[arg(long, short)]
    pub theme: Option<PathBuf>,

    /// Poll interval in milliseconds, floor 50 (overrides the config file)
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

    /// Replay a --log recording instead of live GPUs (one record per tick)
    #[arg(long, value_name = "FILE", conflicts_with = "mock")]
    pub replay: Option<PathBuf>,

    /// Print shell completions to stdout and exit
    #[arg(long, value_enum, value_name = "SHELL", hide = true)]
    pub completions: Option<CompletionShell>,

    /// Print the man page (troff) to stdout and exit
    #[arg(long, hide = true)]
    pub man: bool,
}

/// Shells `--completions` can generate for: clap_complete's five core
/// shells plus nushell (separate generator crate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    Powershell,
    Elvish,
    Nushell,
}

impl CompletionShell {
    pub fn generate(self, cmd: &mut clap::Command) {
        use clap_complete::Shell;
        let out = &mut std::io::stdout();
        match self {
            CompletionShell::Bash => clap_complete::generate(Shell::Bash, cmd, "gpur", out),
            CompletionShell::Zsh => clap_complete::generate(Shell::Zsh, cmd, "gpur", out),
            CompletionShell::Fish => clap_complete::generate(Shell::Fish, cmd, "gpur", out),
            CompletionShell::Powershell => {
                clap_complete::generate(Shell::PowerShell, cmd, "gpur", out)
            }
            CompletionShell::Elvish => clap_complete::generate(Shell::Elvish, cmd, "gpur", out),
            CompletionShell::Nushell => {
                clap_complete::generate(clap_complete_nushell::Nushell, cmd, "gpur", out)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Out-of-range counts used to be clamped silently (0 → 1, 100 → 16).
    #[test]
    fn mock_count_is_range_checked() {
        assert_eq!(
            Cli::try_parse_from(["gpur", "--mock", "6"]).unwrap().mock,
            Some(6)
        );
        // No value keeps the two-GPU default.
        assert_eq!(
            Cli::try_parse_from(["gpur", "--mock"]).unwrap().mock,
            Some(2)
        );
        assert!(Cli::try_parse_from(["gpur", "--mock", "0"]).is_err());
        assert!(Cli::try_parse_from(["gpur", "--mock", "17"]).is_err());
        assert!(Cli::try_parse_from(["gpur", "--mock", "-1"]).is_err());
    }
}
