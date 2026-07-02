use hjkl_config::AppConfig;
use serde::Deserialize;
use std::path::PathBuf;

/// On-disk config. Missing file means these defaults; nothing is ever written.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GpurConfig {
    /// Poll interval in milliseconds.
    pub tick_ms: u64,
    /// Path to a theme TOML. None means the built-in theme.
    pub theme: Option<PathBuf>,
    /// History window kept per GPU (sparkline samples).
    pub history_len: usize,
    /// Graph glyph set: "braille" (default), "block", or "ascii".
    pub graphs: String,
}

impl Default for GpurConfig {
    fn default() -> Self {
        Self {
            tick_ms: 1000,
            theme: None,
            history_len: 300,
            graphs: "braille".into(),
        }
    }
}

impl AppConfig for GpurConfig {
    const APPLICATION: &'static str = "gpur";
}
