# Code Review

Full-codebase review of the `main` tree at 0.12.0, 2026-08-04. Four parallel
review passes (backend composition + Linux/mock/replay; vendor backends; app +
main; ui + support + tests); every finding below was re-traced against the cited
lines in this tree before publication. Severity is relative to a real user
hitting it.

## Findings

### High — process confirmation can signal a reused PID

`src/app.rs:1167` compares second-resolution process start times, then
`src/app.rs:1193` signals by numeric PID in a separate operation. A replacement
created within the same second passes the identity check; any replacement
between the check and signal also bypasses it.

Repro: open kill confirmation for PID 500; the target exits; PID 500 is reused
within the same second; press `y`

Expect: replacement process remains untouched and gpur reports PID reuse

Actual: the replacement passes the start-time check and receives the signal

### Medium — replay skips its preloaded first record

`src/backend/replay.rs:64` advances the input before returning the record loaded
at startup. The first interactive poll therefore replaces record one with record
two; headless mode advances twice and returns record three from a three-record
log.

Repro: replay JSONL records whose GPU names are `first`, then `second`

Expect: the initial TUI frame shows `first`

Actual: the initial TUI frame shows `second`; `first` is never displayed

### Medium — partial terminal setup errors bypass teardown

`src/main.rs:116` and `src/main.rs:117` can return after ratatui enabled raw
mode and the alternate screen, but teardown runs only after the event loop. An
error enabling Kitty keys or mouse capture can leave the invoking shell in
altered terminal state.

Repro: interactive TTY where `ratatui::try_init()` succeeds and
`hjkl_kitty::enable()` returns `EIO`

Expect: gpur returns the error after restoring cooked mode and the main screen

Actual: gpur returns immediately without calling `restore_extras()` or
`ratatui::restore()`

### Medium — slowing an oversized interval overflows before capping

`src/app.rs:1236` multiplies an unrestricted `u64` before applying the maximum.
The CLI accepts values above `u64::MAX / 2` (`cli.rs:36`, floor-only clamp at
`main.rs:60`), so release arithmetic can wrap to zero and debug arithmetic can
panic.

Repro: run `gpur --mock --no-splash --tick-ms 9223372036854775808`, then press
`-`

Expect: poll interval clamps to `10000ms`

Actual: release builds store `0ms` (the event loop busy-spins, polling the
backend every iteration); overflow-checked builds panic

### Medium — NVML success hides nouveau-driven cards

`src/backend/nvidia.rs:20` returns the NVML backend as soon as one proprietary
NVIDIA card is found, so the nouveau scan never runs. Because detection asks for
only one NVIDIA backend, a mixed-driver NVIDIA rig omits every nouveau-bound
card.

Repro: Linux host with card A bound to `nvidia`, card B bound to `nouveau`, and
NVML reporting only card A

Expect: snapshots for cards A and B

Actual: only card A is returned

### Medium — invalid PDH items are published as measurements

`src/backend/windows.rs:463` reads every wildcard item's `doubleValue` without
checking its per-item `FmtValue.CStatus`. PDH can return a successful array call
containing invalid items, whose values then become utilization or memory
measurements.

Repro: PDH returns a valid GPU Engine instance name with
`CStatus = PDH_CSTATUS_INVALID_DATA` and `doubleValue = 73.0`

Expect: item is discarded and utilization remains unknown

Actual: item contributes `73.0` utilization

### Low — first Intel dGPU sweep reports system memory for processes

`src/backend/intel.rs:278` chooses process memory using the device's pre-sweep
`discrete` flag. Mainline i915 cards without a published VRAM total begin as
integrated; local-memory evidence updates the flag only after that sweep's
process rows are built.

Repro: first readable i915 dGPU sweep with `local0 = 536870912`,
`system0 = 2097152`, and no published VRAM total

Expect: process `gpu_mem_bytes = Some(536870912)`

Actual: process `gpu_mem_bytes = Some(2097152)` until the next sweep

### Low — mini-spark scaling overflows on large replay values

`src/ui.rs:763`, `src/ui.rs:766`, and `src/ui.rs:783` multiply a bounded `u64`
sample after casting it to `usize`. The power spark's `max` (`ui.rs:581`) is
unbounded data, and valid replay numbers can overflow that multiplication before
division, producing the wrong glyph in release builds or a panic in
overflow-checked builds.

Repro: replay `power_w = 2305843009213693952` with `--graphs block`

Expect: a sample equal to the scale maximum renders a full-height cell

Actual: 64-bit release arithmetic wraps the level numerator to zero; checked
arithmetic panics

## Cleared

- Composite backend child failures preserve slot offsets and do not duplicate a
  surviving device identity.
- Replay/mock process rows cannot reach signaling because those backends remain
  non-signalable across re-detection.
- Replay rows whose `gpu_index` exceeds the frame are filtered before display.
- Linux fdinfo duplicate descriptors and reused asynchronous snapshots are
  deduplicated before attribution.
- NVML process-utilization watermarks are per device, so one GPU does not starve
  later devices' samples.
- Empty GPU/process views, stale cursors, zero-height panes, and graph unknowns
  stay within bounds and preserve unknown-versus-zero semantics.
- State-file publication preserves the prior file when writing or renaming the
  temporary file fails.
- Package template artifact names and installed executable paths match the
  release layout.
- A stale `proc_visible` click cannot set `proc_sel` out of range: `run()` draws
  at the top of every loop iteration (`main.rs:311`) and polls only at the
  bottom (`main.rs:441`), so a shrink-poll is always followed by a draw that
  recomputes `proc_visible` from the current row count (`ui.rs:1118`) before the
  next event dispatch; `rebuild_proc_view` additionally clamps `proc_sel`
  (`app.rs:1056`).
- The kill path's start-time refresh is sound as far as sysinfo goes: it
  re-reads `start_time` on every targeted refresh and rebuilds the entry when a
  PID was recycled, so a recycled pid with a different start time fails the
  check (the same-second case is the finding above).
- `xe_ratio` on an engine new to a client's `cycles` map reports the counter's
  lifetime average; the xe caller guards the client-level first sample and new
  engines' counters are minted when the engine started, so the interval is the
  correct one.
- PDH `read_array` buffer sizing: the first call sizes in bytes, the item
  allocation divides up (never down), and the second call re-sets `count` before
  the read loop — no OOB, no truncation.
- LUID keys meet across DXGI and PDH: DXGI formats `0x%08x`, PDH instance names
  are lowercased, and `is_hex32` accepts either case.
- Division-by-zero candidates are all guarded: intel `power_w` (`secs > 0` and
  counter-reset), `pcie_kbs`/`ns_delta_util`, `fan_pct` (`pwm1_max > 0`),
  `MemReadout::pct` (`total > 0`).
- `le_int` bounds-checked (`bytes.len() <= 8` before copy); NVML
  `field_values_for` chain matches the resolved nvml-wrapper 0.12.1 API.
- Windows PDH query closes exactly once on the probe error path; IOKit iterator
  is released on the normal path and zero on every early return.
- Missing/unparseable sysfs reads become `None`, never a confident 0; the only
  fabricated-zero paths are deliberate and tested.
- Graph history is a `Vec` + `drain(..overflow)`, not a wrap-around ring — no
  head/tail arithmetic, all vectors pushed in lockstep.
- `proc_scroll..proc_scroll+visible` and card indices are clamped on every path
  that can shrink the table; the ratatui scrollbar returns early on a
  zero-height track.
- Degenerate terminal sizes (1×1 resize storms, zero-height panes) trace to
  early returns, never a panic; splash coordinates are compile-time range
  checked.
- CPU rationing aligns with sysinfo's internal 200 ms minimum update interval.
- Panic/teardown ordering is correct: extras → raw-off/alt-leave → default hook;
  the normal quit path saves state before restoring.

## Hardening

- Process identity is currently an observation rather than a pinned OS handle;
  even a finer-grained timestamp would leave the check-to-signal race (the
  finding above; a pidfd pin closes it on Linux).
- Windows-only PDH item filtering has no platform-independent helper test, so
  Linux CI cannot exercise that decision.
- An engine that first appears mid-session in an xe client's `cycles` map gets a
  one-poll lifetime-average ratio (`linux.rs:59`) — self-corrects, same as
  nvtop-style tools.
- intel `power_w` keeps a stale `(µJ, at)` baseline if `energy1_input` is
  transiently unreadable, reporting average power over the outage window on the
  next successful read.
- `first_dir` trusts sysfs `read_dir` order for a card with multiple hwmon
  children; amdgpu/i915 register one per device in practice.
- `le_int` zero-extends sub-8-byte blobs, so a negative value in 1/2/4 bytes
  decodes as a large positive; only positive device-tree props occur today.
- NVML `pcie_throughput` reports KB/s (×1000) while the field and docs say KiB/s
  (×1024) — a 2.4% drift if the UI labels it KiB/s.
- One `PdhCollectQueryData` at probe: the first poll can read `PDH_NO_DATA` and
  render a fully-`None` first frame on Windows.
- `tick_ms` has no startup ceiling — `--tick-ms 99999999999` silently never
  polls (same family as the overflow finding).
- `--once`/`--json` synchronous mode does one `/proc` walk per backend cursor
  per poll, defeating the shared-walk dedup; the worker thread also stays alive
  through a one-shot run.
- Re-detect with a changed child set renumbers child indices, so device ids
  change and graph/session history resets despite the "survives re-detect"
  comment — only holds while the child set is unchanged.

## Coverage

Scope: clean `main` working tree, so this review covered the full codebase
rather than a pending diff.

Reviewed: all Rust production modules under `src/` (backend composition,
Linux/mock/replay, AMD/Intel/NVIDIA/Windows/Apple backends, app, main, ui, keys,
config, theme, splash, cli) and both integration test files under `tests/`.
Candidate findings were re-traced against the cited project lines and the
resolved dependency versions (sysinfo 0.39.6, ratatui 0.30.2, nvml-wrapper
0.12.1, hjkl-kitty).

GAP: hardware behavior was not executed on AMD, Intel, NVIDIA, Apple, or Windows
GPUs. macOS- and Windows-gated code was read but not compiled on this Linux
host. Runtime package templates under `pkg/` were spot-checked only (the
previous pass cleared their artifact/layout claims; no correctness changes
landed in them since). Assets, prose documentation, and package metadata with no
runtime behavior were not correctness-reviewed.
