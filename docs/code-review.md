# Code Review

Full-codebase review of the `main` tree at 0.12.0, 2026-08-04 (refresh of the
review at the same tag). Working tree clean, so the scope is the whole codebase:
every production module under `src/` read in full, both integration test files
skimmed for what they pin, and the resolved dependency interfaces (nvml-wrapper
0.12.1, sysinfo 0.39.5/0.39.6) spot-checked where a units or API claim was
load-bearing. Every candidate below was traced against the cited lines in this
tree before publication.

This refresh is the tree that landed the fixes for every finding of the previous
pass — the eight commits between the two reviews map one-to-one onto its eight
findings, and each fix ships a regression test that I re-traced:

- `ac7cde7` pidfd pin → closes the reused-PID signal race (`app.rs:1218`).
- `d3664f7` replay `first` flag → closes the skipped-preload (`replay.rs:30`).
- `9f35fdb` restore on setup errors → closes the terminal-teardown bypass
  (`main.rs:119`).
- `a32d18e` saturating tick clamp → closes the interval overflow
  (`app.rs:1316`).
- `c338a8a` MergedNvidiaBackend → closes the nouveau-hidden-by-NVML omission
  (`nvidia.rs:26`).
- `03e16a1` per-item `CStatus` check → closes the invalid-PDH-item leak
  (`windows.rs:465`).
- `4ac1121` `saw_local` charging → closes the first-sweep i915 dGPU memory
  mischarge (`intel.rs:282`).
- `005112a` u128 spark arithmetic → closes the mini-spark overflow
  (`ui.rs:763`).

## Findings

None. No correctness defect survived verification in the current tree: no logic
error, broken edge case, swallowed error path, resource leak, or surprising
behavior change was traced to a reachable failure.

## Cleared

The suspicious paths, each traced and disproved:

- **Kill path (highest-risk code).** The pidfd fast path pins identity at
  confirm time and re-reads `/proc/<pid>/stat` field 22 against the dialog-open
  value, so a same-second reuse is caught; `ESRCH` from `pidfd_send_signal`
  means the pinned original exited, never that a reused pid was hit. The
  `kill_with(None)` fallback refuses rather than escalating to `Signal::Kill`
  behind the user's back. `start_ticks` being `None` on both sides of the
  comparison is the only pass-through, and the sysinfo start-time check plus
  pidfd pin still guard it.
- **`parse_start_ticks` field offset.** Field 22 is the 19th whitespace token
  after the comm closer (field 3), so `nth(19)` after `rsplit_once(')')` is
  right; comm names embedding spaces and `)` are handled by the last-`)` rule.
  Tested with both shapes.
- **Braille spark orientation.** `mini_spark` grows dots from `bit_col[3]` (dot
  7/8, the bottom row) upward — a measured 0 draws `⣀` and 100 draws `⣿`, which
  is what the tests assert. `DOT_BITS` columns match the Unicode braille dot
  layout exactly.
- **`windowed` bounds.** Every caller indexes below its window (`cx*2+s < n`,
  `c < CELLS`, `cx < cols`), so neither the `data.len() >= window` nor the pad
  branch can index out of range; the left pad is `None`, never a fabricated
  zero.
- **PDH `read_array` sizing.** `count ≤ n` is guaranteed by the status check: a
  fill that would overflow returns `PDH_MORE_DATA` (non-zero) and the function
  returns an empty table rather than reading past the allocation. `size` is
  bytes on input, items on output, and `div_ceil` never under-allocates.
- **`Instant::duration_since` panics.** Every delta pair (`ns_delta_util`,
  `xe_ratio`'s callers, intel `power_w`, `pcie_kbs`) comes from two
  monotonically increasing `Instant`s stamped by one clock — the scanner worker
  for the fdinfo walks (which also guards `wall <= 0`), the same poll clock for
  energy and pcie deltas (which guard `secs > 0`).
- **Counter resets.** `pcie_kbs` saturates a reset to a 0 delta; intel `power_w`
  treats `uj < prev_uj` as "no delta this poll" and re-seeds the baseline;
  `ns_delta_util` saturates busy-time subtraction.
- **Layout arithmetic.** `stacked_height`, `cards_that_fit`, `proc_pane_height`
  all compute in `u32`/`u64` and are pinned by tests at the exact `u16::MAX`
  wrap points; degenerate panes trace to early returns.
- **The `attributed`/`video` invariant in the Intel backend.** Both
  `utilization_pct` and `video_util_pct` are `Some` exactly when the sweep
  attributed a client to the device: the only non-gated read is
  `s.video_util.get(&i)`, which is empty for an unattributed device, so the
  invariant the hardware test asserts holds by construction.
- **Composite slot bookkeeping.** Placeholders inherit names/ids, vacated slots
  stop claiming a live device, process indices rebase on the high-water mark,
  `can_signal` is the AND of the children, and a partially failing poll keeps
  the survivors' cards — all pinned by the Stub-driven suite, including the
  tri-vendor and hotplug cases.
- **Sweep state pruning.** `engine_state`/`i915_state`/`xe_state` are retained
  against `sweep.seen` each walk, so short-lived DRM clients cannot grow the
  maps unboundedly; `evict_absent_devices` bounds departed-GPU state; the
  departed-pid sysinfo eviction sweeps only what left the table.
- **State-file atomicity.** Temp file + same-filesystem rename, 0600 creation,
  failure leaves the prior file intact — tested including the staged failure
  (directory planted at the temp path).
- **Unknown vs zero everywhere.** `MemReadout::pct` refuses a `total == 0` pool;
  history records `None` for unreadable samples and renders it as a distinct
  glyph in all three graph styles; `n/a`/`N/A`/`null`/`-` spellings per surface
  are pinned by unit and smoke tests; PDH process memory is `None` only when
  neither counter names the pid.
- **`gts_to_gen` thresholds** match PCIe per-lane speeds (2.5/5/8/16/32/64/128
  GT/s); `linux_major` matches glibc's `major()` encoding of `dev_t`.
- **Windows LUID matching.** DXGI formats `0x%08x` lowercase, PDH instance names
  are lowercased in `read_array`, `is_hex32` accepts either case, and the key is
  matched structurally over `_`-tokens rather than by width.
- **Mock safety.** `PID_BASE` sits above `pid_max`, `can_signal()` is false, and
  the one real pid (gpur itself) is blocked by both the signalability gate and
  the self-kill guard.
- **`--once`/`--json` flow.** Two polls bracket the sleep, only the second is
  logged (smoke test pins exactly one record), synchronous scan walks on the
  calling thread so the deltas span the real interval.
- **Replay framing.** The preloaded first record is handed back by the first
  poll; EOF holds the final frame; out-of-frame rows are dropped; pre-`Option`
  recordings still deserialize (`serde(default)` + explicit `null` handling
  pinned by the double-round-trip smoke test).

## Hardening

Correct today, fragile by convention rather than by type:

- `--tick-ms` has no startup ceiling: `--tick-ms 99999999999` is floored but
  never capped, so the event loop waits ~3 years for the first poll and gpur
  reads as frozen. (Carried from the previous pass; `+`/`-` are capped.)
- Intel `power_w` keeps its `(µJ, at)` baseline when `energy1_input` is
  transiently unreadable, so the next successful read reports average power over
  the outage window rather than over one interval.
- NVML `pcie_throughput` returns decimal KB/s while the snapshot field and UI
  label say KiB/s — a 2.4% drift on the PCIe readout.
- Headless `--once`/`--json` on a mixed AMD+Intel box walks `/proc` once per
  backend cursor per poll (the shared-walk dedup is lost in synchronous mode),
  and the scanner worker thread stays alive through the one-shot run.
- A re-detect whose child set changed (a driver appearing or disappearing)
  renumbers child indices, changing every namespaced device id and resetting
  graph/session history — the "survives a re-detect" guarantee holds only while
  the child set is unchanged.
- `first_dir` trusts sysfs `read_dir` order for a card with multiple hwmon
  children; amdgpu/i915 register one per device in practice.
- `le_int` zero-extends sub-8-byte blobs, so a negative 1/2/4-byte device-tree
  value would decode as a large positive; only positive props occur today.
- PDH `read_array` does not loop: if the instance count grows between the sizing
  call and the fill, the second call returns `PDH_MORE_DATA` and that counter
  reads as empty for one poll (all-None gauges for a tick) instead of retrying.
  No overflow — just a lost tick.
- Two `gpur --log` processes appending to one file can tear a record mid-line (a
  flushed `BufWriter` record is not one atomic write); the replay reader skips
  the malformed line, so the cost is one lost record.

## Coverage

Scope: clean `main` working tree, so this review covered the full codebase
rather than a pending diff.

Reviewed: all Rust production modules under `src/` (backend composition,
Linux/mock/replay, AMD/Intel/NVIDIA/Windows/Apple backends, app, main, ui, keys,
config, theme, splash, cli) read in full, line by line; both integration test
files (`tests/smoke.rs` in full, `tests/tui.rs` via its test names and the
harness) skimmed to confirm what invariants are pinned; installed sources of
nvml-wrapper and sysinfo consulted for the API/units claims above.

GAP: hardware behavior was not executed on any GPU, and the macOS- and
Windows-gated code (`apple.rs` IOKit, `windows.rs` PDH live paths) was read but
not compiled on this Linux host — those paths are verified statically only, and
their pure-function halves are what the test suite actually runs. GAP: `pkg/`
templates, `assets/`, prose documentation, and the `CHANGELOG.md` were not
correctness-reviewed (no runtime-code changes landed in them since the previous
pass). GAP: dependency internals beyond the two spot-checks above
(hjkl-keymap/kitty/splash dispatch, ratatui layout for overflowing constraints)
were taken on trust rather than read.
