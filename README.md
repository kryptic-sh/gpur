# gpur

btop-style GPU monitor for the terminal. An `nvtop` replacement that aims to run
everywhere: NVIDIA, AMD, and Apple Silicon GPUs on Linux, macOS, and Windows.

## Status

Early scaffold. The TUI, theming, config, and keybinding plumbing are in place;
vendor backends are stubs. Run with `--mock` to see the dashboard with fake
GPUs.

```sh
cargo run -- --mock
```

## Keys

| Key         | Action           |
| ----------- | ---------------- |
| `q` / `Esc` | quit             |
| `p`         | pause/resume     |
| `j` / `k`   | select GPU       |
| `+` / `-`   | poll rate adjust |

## Configuration

`gpur` reads `$XDG_CONFIG_HOME/gpur/config.toml` (falls back to
`~/.config/gpur/config.toml`). No file is written automatically; missing file
means built-in defaults.

```toml
tick_ms = 1000
history_len = 300
# hjkl-theme TOML; omit for the built-in theme
theme = "/path/to/theme.toml"
```

Themes use the [hjkl-theme](https://crates.io/crates/hjkl-theme) schema:
`[palette]` of hex colors plus `[ui]` styles. Any hjkl theme file works.

## Backends

| Backend | Platform              | Source                         | Status  |
| ------- | --------------------- | ------------------------------ | ------- |
| nvml    | Linux, Windows        | NVML                           | planned |
| amdgpu  | Linux                 | sysfs `/sys/class/drm`         | planned |
| adlx    | Windows               | ADLX                           | planned |
| metal   | macOS (Apple Silicon) | IOReport/IOKit                 | planned |
| mock    | all                   | deterministic waves (`--mock`) | done    |

## License

MIT
