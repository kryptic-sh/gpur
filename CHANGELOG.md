# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/kryptic-sh/gpur/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.2.0
[0.1.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.1.0
