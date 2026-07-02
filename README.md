# gpur

btop-style GPU monitor for the terminal. An `nvtop` replacement that aims to run
everywhere: NVIDIA, AMD, and Apple Silicon GPUs on Linux, macOS, and Windows.

## Status

All platforms have a working backend: NVML (NVIDIA), sysfs/amdgpu (AMD Linux),
IOKit (macOS), PDH counters (Windows AMD/Intel). Depth varies — see the backend
table below.

```sh
cargo run            # real GPUs
cargo run -- --mock  # fake GPUs, works anywhere
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

| Backend | Platform       | Source                           | Status |
| ------- | -------------- | -------------------------------- | ------ |
| nvml    | Linux, Windows | NVML (dynamic load)              | done   |
| amdgpu  | Linux          | sysfs `/sys/class/drm`           | done   |
| pdh     | Windows        | GPU Engine/Adapter Memory + DXGI | done   |
| ioaccel | macOS          | IOAccelerator PerfStats (IOKit)  | done   |
| mock    | all            | deterministic waves (`--mock`)   | done   |

Probe order: nvml → amdgpu → ioaccel → pdh. PDH is the vendor-generic Windows
fallback (Task Manager's counters) covering AMD/Intel; it reports utilization
and memory only. Apple temperature/power (SMC/IOReport) and Windows AMD ADLX
(temps/fans/clocks) are planned.

## License

MIT
