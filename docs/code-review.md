# Code Review

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
The CLI accepts values above `u64::MAX / 2`, so release arithmetic can wrap to
zero and debug arithmetic can panic.

Repro: run `gpur --mock --no-splash --tick-ms 9223372036854775808`, then press
`-`

Expect: poll interval clamps to `10000ms`

Actual: release builds store `0ms`; overflow-checked builds panic

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
sample after casting it to `usize`. Valid replay numbers can overflow that
multiplication before division, producing the wrong glyph in release builds or a
panic in overflow-checked builds.

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

## Hardening

- Process identity is currently an observation rather than a pinned OS handle;
  even a finer-grained timestamp would leave the check-to-signal race.
- Windows-only PDH item filtering has no platform-independent helper test, so
  Linux CI cannot exercise that decision.

## Coverage

Scope: clean `main` working tree, so this review covered the full codebase
rather than a pending diff.

Reviewed: all Rust production modules under `src/`, Rust integration tests under
`tests/`, and runtime package templates under `pkg/`. Candidate findings were
re-traced against the cited project lines and the resolved dependency versions.

GAP: hardware behavior was not executed on AMD, Intel, NVIDIA, Apple, or Windows
GPUs. macOS- and Windows-gated code was read but not compiled on this Linux
host. Assets, licenses, prose documentation, and package metadata with no
runtime behavior were not correctness-reviewed.
