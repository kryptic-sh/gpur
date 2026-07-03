# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.0] - 2026-07-03

### Added

- Container attribution: process rows show a CONTAINER column (docker/podman/k8s
  runtime + short id from /proc cgroups, Linux) whenever any GPU process is
  containerized; the filter matches it too.
- Replay mode: `--replay session.jsonl` re-drives the full TUI (or `--json`
  snapshot) from a `--log` recording — recorded user/command enrichment is
  preserved instead of resolving foreign pids; last frame holds at EOF. Makes
  bug reports replayable.
- AMD sensor depth: junction + memory temperatures (labelled hwmon channels),
  GTT usage, VDDGFX voltage, and a warning badge when the DPM performance level
  is forced off auto.
- Fan RPM alongside fan %, on AMD (fan1_input) and NVIDIA (NVML RPM API).
- Driver/kernel version in the header (NVML driver version; kernel release for
  the Linux sysfs backends; mock included).

## [0.6.0] - 2026-07-03

## [0.5.0] - 2026-07-02

### Added

- Terminal teardown hardening: a chained panic hook and a Unix signal handler
  (SIGTERM/SIGHUP/SIGINT) now restore mouse capture, the kitty keyboard
  protocol, raw mode, and the alt screen — external kills and panics no longer
  leave the shell with mouse reporting garbage.
- PTY integration tests in CI (`tests/tui.rs`, Unix): the real binary runs
  against a pseudo-terminal with a vt100 emulator asserting rendered content,
  fold/filter/quit key flows, and teardown escape sequences on both clean quit
  and SIGTERM.
- Invalid `graphs` config values are now a startup error instead of a silent
  fallback to braille.

- Video engine utilization in the info line: NVIDIA shows split `enc`/`dec`
  (NVML), AMD shows unified VCN `video %` (fdinfo engine deltas), Intel shows
  media-engine `video %` (i915 video/video-enhance ns, xe vcs/vecs cycles),
  Windows shows `enc`/`dec` from the PDH videoencode/videodecode engine types.
- Throttle badge shows on folded card summaries too.
- Throttle badge: red `⚠thermal`/`⚠power-limit` in the card info line. NVIDIA
  uses the real NVML throttle-reason mask; AMD uses an at-limit heuristic (power
  ≥99% of cap, or temp within 3°C of the hwmon critical trip).
- AMD backend now does its fdinfo sweep once per poll (Intel-style), halving
  /proc scanning and enabling the device-level VCN readout.

- Graph glyph fallback: `graphs = "braille"|"block"|"ascii"` in config (or
  `--graphs`) switches the waveform, mini-sparks, and meters — block for
  terminals with patchy braille fonts, ascii for the Linux console.
- Sensor logging: `--log FILE` appends one JSON line per poll (`ts_ms`, gpus,
  processes); works in TUI and `--once` modes, disables itself with a status
  message on write errors.

## [0.4.0] - 2026-07-02

### Changed

- GPU selection no longer wraps: j/k and the wheel stop at the first/last card.

### Added

- Headless snapshot mode: `--once` prints one text snapshot, `--json` emits
  machine-readable JSON (backend, gpus, processes) — two quick polls so
  delta-based utilizations are real; built for waybar/polybar and scripting.
- PCIe downgrade indicator: yellow `(max X.0@Nx)` in the card caption when the
  link runs below its maximum (AMD sysfs + NVML max-link data).
- Session stats per GPU: peak util/temp/power and averages since launch, shown
  as a card line when space allows.

- `=` is an unshifted alias for `+` (poll faster).

## [0.3.0] - 2026-07-02

### Added

- Process actions: `s` cycles the sort column (gpu-mem → gpu% → cpu% → host-mem
  → pid, arrow shown in the header and caption), `r` reverses, `/` opens a
  filter input (case-insensitive substring on command/user/pid, Enter applies,
  empty clears, Esc cancels), `x`/`X` send SIGTERM/SIGKILL to the selected
  process behind a y/N confirmation popup; results show as a transient header
  status. Cursor stays on the same process across re-sorts/filters.

## [0.2.0] - 2026-07-02

### Added

- Intel Linux backend (i915 + xe): device utilization aggregated from per-client
  fdinfo engine counters (i915 busy-ns deltas, xe cycles ratios — the nvtop
  approach, since Intel has no sysfs busy%), power from the hwmon cumulative
  energy-counter delta, gt clock (i915 + xe paths), pci.ids names, Arc dGPU vs
  iGPU detection via `lmem_total_bytes`. Probe order is now nvml → amdgpu →
  intel → ioaccel → pdh.
- Shared Linux DRM module (`backend/linux.rs`): generic fdinfo client parser
  (engine-ns, xe cycles, memory regions with drm-resident fallback), pci.ids
  lookup, card scanning — amdgpu backend refactored onto it; fixture unit tests
  for amdgpu/i915/xe fdinfo formats.
- Graceful poll degradation: a backend poll failure keeps the last snapshot on
  screen and shows a red header warning, cleared on the next successful poll —
  driver resets no longer exit the TUI. `GPUR_MOCK_FAIL=N` fails every Nth mock
  poll to exercise the path.
- Process table row cursor: j/k/arrows (and wheel/J/K) move a highlighted row
  when the pane is focused, viewport follows, click selects a row; highlight
  uses the theme surface color.
- Fixed scrollbars: `content_length` must be the number of scroll positions
  (`max_scroll + 1`) — ratatui only lets the thumb reach the track end when
  `position == content_length - 1`. With viewport length = visible rows the
  thumb keeps the visible/total proportion and reaches both track extremes. The
  process track also no longer overlaps the header row.
- Pane focus model: `p` focuses the process list, digits 0-9 focus the GPU list
  and select that GPU (same digit again folds/unfolds), arrows/j/k act on the
  focused pane, left click focuses the pane under the cursor (and selects the
  clicked GPU card); pause moved to Space; focused process pane gets the accent
  border.
- Mouse wheel support: scrolling over the process pane scrolls the table, over
  the GPU area moves the selection (mouse capture on, released at exit).
- Dynamic layout: process pane sizes to content capped at 30% of the body with
  J/K + PgUp/PgDn scrolling and a scrollbar; GPU card list scrolls whole cards
  with a scrollbar when they overflow (selection stays visible, visible cards
  stretch to fill). `--mock` now takes an optional GPU count (`--mock 6`) and
  fakes 3 processes per GPU for demoing overflow.
- Digit keys 0-9 fold/unfold a GPU card to a one-line summary
  (`▸ 0·name GPU% MEM temp power`); remaining cards absorb the space.
- btop-inspired chrome: `┐caption┌` titles embedded in borders (GPU name left,
  PCIe/integrated right, process count on the table), `■■■·····` meters with
  position gradient replacing the gauges, inline 5-cell braille mini-sparks next
  to temp and power, PCIe RX/TX moved to the info line as `▼/▲`.
- btop-style mirrored braille waveform per GPU: gpu% grows up from the midline,
  vram% mirrors down, vertical color gradient toward the edges (green→yellow→red
  / blue→accent), idle keeps a thin center line; rounded borders on all panes.
- nvtop-style process table: PID/USER/DEV/TYPE/GPU%/GPU MEM/CPU%/HOST
  MEM/COMMAND, sorted by GPU memory. Sources: AMD Linux via `/proc` fdinfo
  (drm-client-id dedupe, engine-busy-ns deltas for per-process GPU%,
  `drm-memory-vram`); NVML `running_graphics/compute_processes` +
  `process_utilization_stats`; Windows PDH per-pid GPU Engine instances +
  `GPU Process Memory` counters. Host user/CPU%/RSS/command via `sysinfo`. Apple
  has no public per-process GPU API — table is empty there.

## [0.1.0] - 2026-07-02

### Added

- Org-style release pipeline in `ci.yml`: 7-target build matrix (linux gnu/musl
  x86_64+aarch64 via cargo-zigbuild glibc 2.28, windows msvc, both mac arches
  with `MACOSX_DEPLOYMENT_TARGET`), `.deb`/`.rpm` on gnu targets, sha256
  sidecars, dry-run builds on every main push with tag-gated publishing: GitHub
  Release, crates.io, AUR (`gpur-bin`), Homebrew tap, Scoop bucket, Alpine
  `.apk`. Templates under `pkg/`.
- NVIDIA backend: NVML via `nvml-wrapper` (Linux/Windows) — utilization, VRAM,
  temperature, power + limit, fan, core/mem clocks, PCIe gen/width and RX/TX
  throughput. Driver library loaded dynamically; probe fails soft.
- Apple backend (macOS): IOKit IOAccelerator `PerformanceStatistics` —
  utilization + memory for Apple Silicon (AGX, SoC-derived name with GPU core
  count, unified-memory totals) and Intel-Mac GPUs.
- Windows generic backend: PDH `GPU Engine`/`GPU Adapter Memory` counters (Task
  Manager semantics: busiest-engine sum per adapter LUID) + DXGI for names/VRAM
  totals; covers AMD/Intel where NVML is absent.
- nvtop-style header details: integrated-GPU tag, PCIe gen@width, PCIe RX/TX,
  memory-controller busy %, plus a second per-GPU VRAM% sparkline.
- AMD: APU detection via `gpu_metrics` format revision, PCIe link speed/width
  from sysfs, APU memory clock via `pp_dpm_mclk` active level.
- AMD backend (Linux): sysfs/amdgpu — utilization (`gpu_busy_percent`), VRAM
  (`mem_info_vram_*`), edge temperature, power draw + cap, PWM fan %, core/mem
  clocks via hwmon; multi-card (iGPU + dGPU), marketing names from `pci.ids`.
  Zero power caps and gated clocks at idle are handled.
- Initial scaffold: `GpuBackend` trait with nvidia/amd/apple probe stubs and a
  deterministic mock backend (`--mock`).
- btop-style ratatui dashboard: per-GPU utilization/VRAM gauges, history
  sparklines, temperature/power/clock readouts.
- hjkl stack integration: `hjkl-theme` theming, `hjkl-config` XDG config
  loading, `hjkl-keymap` chord keybindings, `hjkl-kitty` keyboard protocol,
  `hjkl-splash` startup screen.
- CI (`ci.yml`) with lint/test/smoke across Linux/macOS/Windows and tag-driven
  release workflow (`release.yml`).

[Unreleased]: https://github.com/kryptic-sh/gpur/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.7.0
[0.6.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.6.0
[0.5.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.5.0
[0.4.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.4.0
[0.3.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.3.0
[0.2.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.2.0
[0.1.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.1.0
