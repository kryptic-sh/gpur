# gpur

btop-style GPU monitor for the terminal. An `nvtop` replacement that runs
everywhere: NVIDIA, AMD, Intel, and Apple GPUs on Linux, macOS, and Windows.

Mirrored braille waveforms, gradient meters, an nvtop-style process table with
per-process GPU attribution, mouse support, foldable GPU cards, and theming via
the hjkl stack.

> **Status: beta.** The AMD Linux path is battle-tested on real hardware; the
> Intel/macOS/Windows backends are implemented and CI-built but still collecting
> real-hardware mileage. Reports welcome.

## Install

```sh
cargo install gpur              # crates.io
yay -S gpur-bin                 # Arch (AUR)
brew install kryptic-sh/tap/gpur  # macOS
scoop install kryptic/gpur      # Windows
```

Debian/RPM packages, musl builds, and an Alpine `.apk` are attached to each
[GitHub release](https://github.com/kryptic-sh/gpur/releases).

## Run

```sh
gpur              # real GPUs
gpur --mock       # fake GPUs, works anywhere
gpur --mock 6     # demo a 6-GPU rig
gpur --once       # one text snapshot, no TUI (scripts, quick checks)
gpur --json       # machine-readable snapshot (waybar/polybar, monitoring)
gpur --log x.jsonl  # append one JSON record per poll (benchmark/sensor logs)
gpur --graphs ascii # graph glyphs: braille (default) | block | ascii
```

## Keys & mouse

| Input              | Action                                                   |
| ------------------ | -------------------------------------------------------- |
| `q` / `Esc`        | quit                                                     |
| `Space`            | pause/resume polling                                     |
| `0`-`9`            | focus GPU list + select GPU N; same digit again folds it |
| `p`                | focus process list                                       |
| `j`/`k`, arrows    | move within the focused pane                             |
| `J`/`K`, PgUp/PgDn | move the process cursor from anywhere                    |
| `s` / `r`          | cycle process sort column / reverse it                   |
| `/`                | filter processes (Enter applies, empty clears)           |
| `x` / `X`          | SIGTERM / SIGKILL the selected process (with confirm)    |
| `+`/`=` / `-`      | poll rate faster/slower                                  |
| wheel / click      | scroll + focus the pane under the cursor; click selects  |

## Configuration

`gpur` reads `$XDG_CONFIG_HOME/gpur/config.toml` (falls back to
`~/.config/gpur/config.toml`). No file is written automatically; missing file
means built-in defaults.

```toml
tick_ms = 1000
history_len = 300
# graph glyphs: "braille" (default), "block", or "ascii" for fonts
# without braille coverage
graphs = "braille"
# hjkl-theme TOML; omit for the built-in theme
theme = "/path/to/theme.toml"
```

Themes use the [hjkl-theme](https://crates.io/crates/hjkl-theme) schema:
`[palette]` of hex colors plus `[ui]` styles. Any hjkl theme file works — the
meters, waveform gradients, and highlights all derive from the palette.

## Backends

| Backend | Platform       | Source                           | Status |
| ------- | -------------- | -------------------------------- | ------ |
| nvml    | Linux, Windows | NVML (dynamic load)              | done   |
| amdgpu  | Linux          | sysfs `/sys/class/drm` + fdinfo  | done   |
| intel   | Linux          | i915/xe fdinfo + hwmon           | done   |
| pdh     | Windows        | GPU Engine/Adapter Memory + DXGI | done   |
| ioaccel | macOS          | IOAccelerator PerfStats (IOKit)  | done   |
| mock    | all            | deterministic waves (`--mock`)   | done   |

Probe order: nvml → amdgpu → intel → ioaccel → pdh. Intel utilization is derived
from per-client fdinfo engine counters (i915 busy-ns, xe cycles) the same way
nvtop does it; power comes from the hwmon energy counter delta. PDH is the
vendor-generic Windows fallback (Task Manager's counters) and reports
utilization and memory only.

Backend poll failures degrade gracefully: the last snapshot stays on screen with
a header warning until polling recovers — a driver reset won't kill the monitor.

Extras: each GPU card shows session peaks/averages (util, temp, power) when tall
enough, and the PCIe caption flags a link running below its maximum
(`PCIe 3.0@8x (max 4.0@16x)`) — a classic symptom of a bad riser or wrong slot.

Planned depth: Apple temperature/power (SMC/IOReport), Windows AMD ADLX
(temps/fans/clocks), encoder/decoder utilization display.

## Per-process GPU attribution

The process table (PID, user, device, type, GPU%, GPU memory, CPU%, host memory,
command) works on Linux (AMD + Intel via `/proc` fdinfo — same-user processes
unless run as root), NVIDIA everywhere (NVML), and Windows (PDH per-pid
counters). macOS has no public per-process GPU API.

## License

MIT
