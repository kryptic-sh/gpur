# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- **The process-kill path is now guarded.** `x`/`X` refuse to open the dialog
  unless the process pane has focus, and a new `GpuBackend::can_signal()` (false
  for `--mock` and `--replay`) blocks signalling pids that name fabricated or
  foreign processes — a shared recording was otherwise a one-keystroke kill
  primitive against whoever opened it. `confirm_kill` also refuses pid 1, gpur's
  own pid, and processes with no readable executable (kernel threads), and pins
  the target's `start_time` when the dialog opens so a recycled pid is rejected
  instead of signalled; the old "pid no longer exists" guard could never fire,
  because `sysinfo` never evicts a pid outside the refresh set. Mock pids now
  start at `1_000_000` with fabricated host columns instead of emitting the
  literal integers `1..3n`, which the demo UI had been enriching from the real
  host table (init and kernel threads shown as killable rows).
- Windows PDH resolved no adapters at all: `luid_prefix` sliced a fixed 22
  characters out of the counter instance name while the LUID token is 26, so no
  lookup against `luid_key` could ever match and utilization, dedicated/shared
  memory, encode/decode rates and the entire process table read zero on every
  Windows machine with an AMD or Intel GPU. Matching is now structural over the
  `_`-separated tokens, and the pure parsers moved out of the `cfg(windows)`
  module so they compile and are unit-tested on any host.
- i915/xe memory figures were off by up to 1024×: `parse_kib` multiplied every
  fdinfo value by 1024 regardless of the unit the kernel printed, so a client
  holding 512 MiB reported 0 MiB and an unaligned 1234567 bytes reported 1.15
  GiB. The suffix is now parsed, saturating so a bogus unit cannot overflow.
- Intel GPUs no longer show a permanent `0M/0M`. VRAM total is chained by driver
  (xe `device/tileN/physical_vram_size_bytes` summed over tiles, i915
  `lmem_total_bytes` on the card kobject rather than the PCI directory) and left
  absent instead of zero when no source answers; discrete-vs-integrated is
  derived from local-memory evidence and made sticky, so an Arc dGPU is no
  longer filed as an iGPU with its whole PCIe caption suppressed; and the fdinfo
  memory filter now classifies the region names i915/xe actually emit on
  integrated parts, routing system memory into the GTT fields the UI already
  renders.
- NVIDIA per-process GPU% worked only on device 0: the
  `process_utilization_stats` watermark was one backend-wide field advanced
  inside the per-device loop, so device 0 raised it to "now" and every later
  device saw nothing. It is now per device, and the many samples NVML returns
  per pid per call are folded to the newest instead of "whichever arrived last".
- A NVIDIA device that errors mid-poll is replaced by a placeholder instead of
  skipped, so a transient driver reset no longer shifts every later snapshot
  down a slot — which destroyed the last card's waveform history and session
  peaks while rendering its values over another card's graph.
- The splash screen busy-looped: the event timeout was computed against
  `last_poll`, which only advances on a poll, so during the splash it went to
  zero for the rest of every tick (~15,840 frames in 1.4 s, a pegged core, a
  flood of repaints on every launch). Frames now pace off `last_draw`, polls off
  `last_poll`, timeout is the minimum of the two.
- Running with stdout redirected produced a raw ratatui panic and exit 101; it
  now bails with
  `stdout is not a terminal — use --once or --json for non-interactive output`,
  like every other startup failure.
- `Ctrl-C` in the filter input inserted a literal `c` and, with no SIGINT under
  raw mode, left `Esc` as the only way out. `Ctrl-C` (quit), `Ctrl-U` (clear)
  and `Ctrl-W` (delete word) are handled; other chords are ignored.
- `+` (poll faster) raised the interval below 100 ms — the keybinding floored at
  100 while the CLI floored at 50, and no key sequence returned to 50. Both now
  share `MIN_TICK_MS = 50`.
- AMD clock readings never fell back: the comment noted `freq1_input` reads 0
  when the domain is power-gated, but `.map()` on `Some(0)` is `Some(0)`, so
  `or_else` only ran when the file was absent. The zero is filtered first.
- AMD per-process memory counted only the `vram` region, so on an APU — where
  almost everything lands in GTT — the process rows contradicted the card's own
  `gtt` line. Measured against a live VA-API encode on a Phoenix2 APU: the same
  pid goes from 58 MiB to 79 MiB once GTT is counted.
- Clicking the process pane's bottom border selected a row that was never
  visible; hit-testing is now bounded by the rows the last draw actually showed.
  Mouse events also no longer move the selection underneath an open modal.
- `--mock` silently rewrote out-of-range counts (0 → 1, 100 → 16); the count is
  now range-checked by clap and rejected with an error.
- The test suite read and overwrote the developer's real
  `~/.cache/gpur/state.json`, so `sort_cycle` and the smoke sort assertion
  failed on any machine whose cached sort differed from the default. Each test
  now runs against a sandboxed XDG tree under the system temp dir.

### Changed

- **`--json` and `--log` now emit the same record shape**:
  `{ts_ms, backend, driver, gpus, processes}`. `--json` gained `ts_ms` and
  `driver`; `--log` records gained `backend` and `driver` (the two facts a
  maintainer needs first in a bug report, previously recorded by neither).
- **`processes[].container` round-trips**: `GpuProcess` gained a `container`
  field, so a recording's container attribution is read back instead of being
  dropped by serde and re-resolved from the recorded, foreign pid against the
  replaying host's `/proc`.
- **Headless output is deterministic**: `--once` and `--json` skip `state.json`
  entirely, and the `processes` array is the unfiltered table in a fixed order
  (GPU memory descending, then pid, then GPU index) instead of following
  whatever sort a human last chose in the TUI. `--once --log` writes one record
  rather than two.
- **`state.json` gained `tick_ms_explicit`**: the poll rate is persisted as
  sticky only when it was chosen interactively with `+`/`-`. A rate that came
  from `config.toml` or `--tick-ms` no longer shadows a later config edit
  forever after the first clean quit; a state file written before the flag
  existed is honoured once, then rewritten with real provenance.
- Windows no longer hides idle GPU processes — no other backend does — and its
  integrated-GPU heuristic no longer files small discrete cards as iGPUs and
  hands them the shared memory pool as their VRAM total. The PDH counter array
  is allocated as `Vec<PDH_FMT_COUNTERVALUE_ITEM_W>` so the buffer carries the
  item's alignment instead of relying on the allocator.
- AMD device utilization is routed through `clamp_pct` like every other backend.

### Added

- AMD PCIe throughput: `pcie_bw` is read as a counter delta against the max
  packet size where the ASIC implements `get_pcie_usage`, degrading to none on
  APUs and RDNA3 where the kernel marks it unsupported. The code comment
  asserting amdgpu exposes no throughput counters was wrong.
- AMD encoder/decoder split alongside the unified `video` figure — the predicate
  already existed and the split was being discarded. Both stay absent unless a
  client actually used that engine class, so no card grows a permanent `enc 0%`.
- NVIDIA memory temperature (via `field_values_for`), the real fan count instead
  of assuming fan 0, and the performance state. Each degrades to none on
  unsupported hardware, including NVML's zero "no reading" answer.
- Intel now reads the four generic PCI link attributes, so the PCIe downgrade
  warning finally fires for Arc; `gts_to_gen`, `pcie_link` and the fdinfo sweep
  moved into `backend/linux.rs` for amdgpu to share.
- Test coverage for sort actually reordering rows, pause, the tick keys, GPU
  card overflow scrolling, and the `GPUR_MOCK_FAIL` degradation and re-detect
  paths; four assertions that passed vacuously (dashboard, filter, row content,
  completions) are now pinned to behavior.

## [0.8.1] - 2026-07-03

### Fixed

- Status icons now sit apart from their text: the throttle warning (`⚠`) and the
  PCIe throughput arrows (`▼`/`▲`) gained a space before the value, so they read
  as `⚠ power-limit` and `▼ 1.2GiB/s` instead of hugging the glyph.

### Changed

- Internal DRY/YAGNI refactor, no behavior change: hoisted the shared
  amdgpu/i915 fdinfo delta math and process-row assembly into the Linux DRM
  layer (`ns_delta_util`, `build_proc`, `engine_ns_where`, a shared `hwmon_u64`)
  and added `clamp_pct` / `join_throttle` to the backend module (used by the
  nvidia, amd, intel and windows backends); deduplicated the UI scrollbars,
  popup centering, waveform mirror geometry and history sampling into
  `draw_scrollbar`, `centered`, `waveform_halves`, `windowed` and `draw_card`.

## [0.8.0] - 2026-07-03

### Fixed

- Wide-terminal graphs no longer freeze at the left edge: history retention was
  a fixed 300 samples while braille graphs need 2×width — on terminals wider
  than 150 columns the left region could never fill and long activity bursts
  pinned at the pad boundary looking stuck. Retention now adapts to the widest
  graph seen (config `history_len` acts as a minimum).

### Added

- `--completions nushell` via `clap_complete_nushell` (a `CompletionShell`
  bridge enum — clap's core Shell has no nushell variant); the release
  completions tarball now carries all six shells.

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

### Added

- `?` help overlay, driven by the same binding table as the keymap, and UI state
  persistence across runs (folded cards, sort column/direction, poll rate) in
  `state.json` under the cache dir.
- Hidden `--completions <SHELL>` and `--man` generators for packaging, plus
  Windows console teardown on exit.
- Backend re-detect after repeated poll failures, and color degradation for
  `NO_COLOR` and 256/16-color terminals.
- PTY resize-storm test coverage.

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

[Unreleased]: https://github.com/kryptic-sh/gpur/compare/v0.8.1...HEAD
[0.8.1]: https://github.com/kryptic-sh/gpur/releases/tag/v0.8.1
[0.8.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.8.0
[0.7.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.7.0
[0.6.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.6.0
[0.5.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.5.0
[0.4.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.4.0
[0.3.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.3.0
[0.2.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.2.0
[0.1.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.1.0
