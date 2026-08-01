# gpur code review

Full-codebase pass over `src/` (10 modules, 8 backends) and `tests/`, at
v0.10.1. Findings are grouped by class and ordered by severity within each
group. Items already tracked in `backlog.md` are cross-referenced rather than
restated; this file covers what that document does not.

Baseline: `cargo clippy --all-targets -- -D warnings` is clean, and the suite
passes. Nothing here is a crash or a memory-safety problem.

## Status

Every finding below is now fixed. **C4** was held back longest because it breaks
log compatibility, which was the maintainer's call rather than a review finding
to apply unilaterally; it shipped in 0.11.0 once that call was made, and its
severity had been revised up in the meantime — see the entry. **D4** was judged
not worth the extraction and is recorded as considered-and-declined.

Two consequences of these fixes are behaviour changes in their own right and are
recorded under "Decisions taken deliberately" in `backlog.md`: an Intel GPU with
no visible DRM clients now reports `n/a` rather than `0%`, and a pid that leaves
the GPU while still running stays in the sysinfo cache until it exits.

The fixes landed as `c5bf0aa`, `3581995`, `12ca248`, `fba30d1`, `a48f14f`,
`c594203`, `c080385`, `37fe867`, `0b0cb28` and `79540ed`; the suite went from
170 to 178 tests, every one of them pinning a finding above. Two fixes are worth
reading in the commit rather than here, because the final shape differs from
what this document proposed:

- **C3** is fixed structurally rather than with a guard. A new
  `backend::BackendSource` carries the whole of how a session's backend is
  produced (`Live` / `Mock(n)` / `Replay(path)`), and startup and the failure
  re-detect run the same call — so a re-detect can only ever reproduce the kind
  of backend the session began with, and there is no rule left for a future edit
  to forget. A first attempt gated the re-detect on `can_signal()`, which was
  also correct but cost the `GPUR_MOCK_FAIL` PTY test its only route into the
  re-detect branch.
- **C5**'s eviction is a second no-field refresh over exactly the departed pids,
  since sysinfo exposes no way to drop a map entry and an `All` refresh is what
  this code exists to avoid. One residual is documented at the site: a pid that
  left the GPU but is still alive stays cached until it exits, because nothing
  short of `All` can evict a live process. That set is bounded by the machine's
  process table rather than by session length, which was the actual finding.

---

## Correctness

### C1. Intel reports a confident `0%` for a GPU it cannot see into

**Severity: medium. Fixed in `c5bf0aa`.** `src/backend/intel.rs:119`, `:147`.

```rust
utilization_pct: Some(clamp_pct(sweep.util.remove(&i).unwrap_or(0.0))),
...
gtt_used_bytes: Some(system_mem.remove(&i).unwrap_or(0)),
```

Intel has no device-level busy counter, so both figures are summed over the DRM
clients the `/proc` sweep could read. `linux::drm_clients` returns an empty vec
whenever `/proc/<pid>/fd` is unreadable — which is every process belonging to
another user, unless gpur runs as root:

```rust
let Ok(entries) = fs::read_dir(&fd_dir) else {
    return Vec::new(); // other users' processes without privileges
};
```

So an unprivileged gpur on a shared box renders another user's fully-loaded iGPU
as a measured, meter-drawn `0%` with an empty GTT pool. That is precisely the
fabrication `GpuSnapshot`'s doc comment, `vram_value`, `session_line` and
`draw_meter` all exist to prevent — the one backend that derives its headline
number from a privileged source is the one that hard-codes `Some(0.0)`.

The process pane already tells the truth here
(`"no GPU processes visible (need same-user or root for fdinfo)"`); the device
gauge above it does not.

**Fix:** distinguish "no client contributed" from "clients contributed zero".
`Sweep::seen` already carries per-client evidence — key the fallback off whether
any client was attributed to device `i` at all, and emit `None` when none was.
An idle device with visible clients keeps its honest `Some(0.0)`.

### C2. The kill dialog can send SIGKILL while reporting SIGTERM

**Severity: low (Windows-only today). Fixed in `fba30d1`.** `src/app.rs:972`.

```rust
let ok = p.kill_with(sig).unwrap_or_else(|| p.kill());
```

`kill_with` returns `None` when the signal is unsupported on the platform —
which is exactly the `Signal::Term`-on-Windows case the comment names. The
fallback `p.kill()` sends `Signal::Kill`. The confirmation dialog said
`send SIGTERM to 1234?`, the status line then says `sent SIGTERM to 1234`, and
the process was killed uncatchably.

The user consented to a graceful terminate and got a hard kill, silently. Every
other guard on this path (start-time pinning, pid 1, self, exe check,
`can_signal`) is careful; this one is not.

**Fix:** report what was actually sent, or refuse the fallback for the non-force
path and tell the user to use `X`.

### C3. Re-detect discards the backend's provenance

**Severity: low (currently unreachable). Fixed in `a48f14f` — see Status.**
`src/app.rs:632`.

```rust
if self.poll_failures.is_multiple_of(5)
    && let Ok(fresh) = crate::backend::detect(self.mock, None)
```

`App` keeps `mock` for re-detection but not the replay path. A replay session
that hit five consecutive poll failures would silently swap itself for the live
hardware backend — which also flips `can_signal()` from false to true, undoing
the guarantee `ReplayBackend::can_signal` exists to make.

It cannot happen today only because `ReplayBackend::poll` never returns `Err`
(EOF holds the final frame, by design, and `replay.rs`'s module doc says so).
That is an implicit invariant in one file protecting a security property in
another.

**Fix:** carry the replay path alongside `mock`, or skip re-detect entirely when
`!self.backend.can_signal()` — a fabricated backend has no driver to reload.

### C4. Unknown per-process figures are indistinguishable from zero

**Severity: medium (revised up from low). OPEN — needs a call on breaking log
compatibility.** `src/backend/mod.rs:126`, `src/app.rs:880-881`. Tracked as item
0 in `backlog.md`.

Two corrections to this finding as first written.

**The field list.** In `GpuProcess`, `cpu_pct` and `host_mem_bytes` are already
`Option`; only `gpu_mem_bytes` is a bare `u64`. The `unwrap_or(0)` that destroys
the distinction is one layer up, where `ProcRow` is built. So it is `ProcRow`'s
three fields plus `GpuProcess::gpu_mem_bytes` — four fields across two types.

**The severity.** This first said `Unavailable` shows up on "MIG devices,
restricted-permission queries, some vGPU profiles", which was speculation. The
nvml-wrapper docs on `UsedGpuMemory::Unavailable` say:

> Under WDDM, `NVML_VALUE_NOT_AVAILABLE` is always reported because Windows KMD
> manages all the memory, not the NVIDIA driver.

WDDM is the ordinary consumer Windows configuration. Every NVIDIA process row on
Windows reads `0MiB`, always — a whole platform's default state, not an edge
case.

**Fix:** widen all four to `Option` and render `-`/`N/A` as the GPU% column
does. `gpu_util_pct` is the model: it is already `Option` and
`rebuild_proc_view` sinks unmeasured rows in both sort directions.

**Why it is not simply applied.** The record is written from `ProcRow` and read
back into `GpuProcess`, two types serde-compatible only by field-name overlap.
`#[serde(default)]` covers missing keys, not explicit nulls, so a `null`
`gpu_mem_bytes` fails to deserialize into `u64`. Measured against the real
binary: a log whose records all carry the null is rejected outright
(`no valid JSONL records`, exit 1); a log where only some do has those frames
silently skipped by `next_record` — a three-record log with one null replayed as
the other two, no warning. Both types must change together, and logs written by
the new version become unreadable to older gpur binaries. That is the real cost,
and why this wants a minor bump rather than a patch.

### C5. sysinfo's process cache grows for the life of the session

**Severity: low. Fixed in `79540ed` — see Status.** `src/app.rs:744`.

```rust
self.sys.refresh_processes_specifics(ProcessesToUpdate::Some(&pids), true, ...)
```

With `ProcessesToUpdate::Some`, sysinfo 0.39 only evicts dead processes _within
the supplied list_ (`common/system.rs:385`). A pid that stops using the GPU is
never in a later list, so its `Process` entry is retained forever. A node
churning through short GPU jobs accumulates one struct per pid ever seen.

`confirm_kill` deliberately depends on this non-eviction (its comment says so),
so the fix must keep the single-pid refresh it performs before signalling.

**Fix:** periodically `retain` the cache against the live pid set, or accept it
and document the ceiling. `MAX_ABSENT_DEVICES` bounds the equivalent per-device
maps already; this is the one unbounded map left.

### C6. CPU% is sampled below sysinfo's documented minimum interval

**Severity: low. Fixed in `79540ed`.** `src/app.rs:14` (`MIN_TICK_MS = 50`),
`:744`.

`cpu_usage()` is only meaningful when at least `MINIMUM_CPU_UPDATE_INTERVAL`
(200 ms) elapsed between refreshes of that process. At `--tick-ms 50` — a
supported and deliberately-preserved setting, per `backlog.md` — and at the 100
ms the PTY suite uses, the CPU% column is noise rather than a measurement.

**Fix:** rate-limit the CPU half of the refresh to ≥200 ms and hold the previous
value between, or drop the column below that tick rate.

### C7. Two `u16` sums in `draw_gpus` can overflow

**Severity: very low. Fixed in `3581995`.** `src/ui.rs:185`, `:222`.

```rust
let needed: u16 = (0..n).map(|i| height_of(app, i)).sum();
...
if used + h > area.height { break; }
```

`needed` overflows above ~8191 unfolded cards; `used + h` overflows when
`area.height` is within 8 of `u16::MAX`. Both are absurd inputs, but
`proc_pane_height` in the same file already computes in `u32` for exactly this
reason and carries a regression test (`proc_pane_height_never_overflows`) naming
programmatic PTY resize as the trigger. The treatment is inconsistent.

**Fix:** accumulate in `u32`, or use `saturating_add`.

### C8. Replay trusts `gpu_index` from the record

**Severity: very low. Fixed in `c594203`.** `src/backend/replay.rs:82`.

`processes()` hands back recorded rows verbatim. A recording whose `gpu_index`
exceeds the frame's GPU count — a truncated log, a hand-edited or third-party
JSONL — renders a `DEV 7` row against a two-card frame.
`CompositeBackend::processes` drops exactly this case for live children, but
replay is never composed, so nothing filters it.

**Fix:** drop rows whose `gpu_index` is outside the current frame, matching the
composite's rule.

---

## Security

The kill path was audited specifically and holds up: pid identity is pinned by
`(pid, start_time)` across the confirmation gap, pid 1 and self are refused,
`exe().is_none()` refuses kernel threads and unreadable processes,
`can_signal()` is re-checked inside `confirm_kill` (not only at `request_kill`),
and mock/replay backends refuse outright so a recording from a stranger is not a
one-keystroke signal primitive. `CompositeBackend::can_signal` is the AND of its
children. No findings there.

### S1. `--log` is created with default permissions

**Severity: low. Fixed in `c080385`.** `src/main.rs:70`.

```rust
std::fs::OpenOptions::new().create(true).append(true).open(path)?
```

The log records full command lines, usernames and container ids, one JSON line
per poll. Command lines routinely carry secrets in `argv` (`--api-key=…`,
`--token=…`). With a typical `umask 022` the file lands world-readable, and it
is meant to be left running for hours.

**Fix:** `OpenOptionsExt::mode(0o600)` on Unix. `state.json` (written by
`App::save_state`) deserves the same treatment for consistency, though its
contents are not sensitive.

### S2. Unit tests write to predictable paths in a world-writable directory

**Severity: low (test-only). Fixed in `12ca248`.** `src/backend/linux.rs:555`,
`:707`, `src/backend/amd.rs:533`, `:641`, `src/backend/intel.rs:409`.

```rust
let root = std::env::temp_dir().join(format!("gpur-drm-{name}"));
let _ = fs::remove_dir_all(&root);
```

These paths carry no pid and no counter: `/tmp/gpur-drm-partition`,
`/tmp/gpur-clock-test`, `/tmp/gpur-dpm-test`, `/tmp/gpur-intel-test-*`.

Two consequences. Concurrent `cargo test` runs — two checkouts, two users, a CI
matrix sharing a runner — race on the same directories and fail
nondeterministically. And on a shared host another user can pre-create the path
as a symlink: `remove_dir_all` fails on a symlink and the error is discarded by
`let _`, `create_dir_all` then succeeds through it, and every `fs::write` in the
test lands wherever the link points, as the test user.

`tests/smoke.rs` and `tests/tui.rs` already do this correctly, with
`std::process::id()` plus an `AtomicU32` counter and a `Drop` cleanup. The
in-crate tests should use the same pattern.

### S3. `state.json` is written non-atomically

**Severity: very low. Fixed in `37fe867`.** `src/app.rs:509`.

`fs::write` truncates before writing. A crash, a signal, or a full disk
mid-write leaves a truncated file, `load_state`'s
`serde_json::from_str(...).ok()?` returns `None`, and every persisted preference
is silently lost. The failure mode is benign but avoidable.

**Fix:** write to `state.json.tmp` and `rename` over the target.

---

## DRY

### D1. `fan_pct` is duplicated verbatim

**Fixed in `0b0cb28`.**

`src/backend/amd.rs:462` and `src/backend/nvidia.rs:306` (the nouveau module)
are the same six lines — `pwm1` over `pwm1_max`, defaulting the divisor to
hwmon's 255 and filtering a zero max. Both even carry the same explanatory
comment. `linux.rs` is where every other shared hwmon reader lives (`hwmon_u64`,
`pcie_link`, `driver_line`).

**Fix:** `linux::fan_pct(hwmon: Option<&Path>) -> Option<f64>`.

### D2. The multi-driver `driver_info` join is duplicated

**Fixed in `0b0cb28`.**

`src/backend/amd.rs:158` and `src/backend/intel.rs:160` build the identical
`BTreeSet` → `join("+")` → `linux::driver_line(..)` chain over their device
lists, for the identical reason (a box can run `amdgpu`+`radeon` or
`i915`+`xe`).

**Fix:** one helper taking an iterator of driver names.

### D3. `pci.ids` is read and parsed once per device

**Fixed in `0b0cb28`.**

`src/backend/linux.rs:483` — `card_name` reads the whole file (~1.5 MB on a
current hwdata) and scans it linearly, and it is called once per card inside
each backend's `scan`. A three-AMD-card box does three full reads and three
linear scans at probe; an AMD+Intel+nouveau box does one set per backend.

Probe-time only, so it is cost rather than a bug — but the file contents are
identical across every call in a process.

**Fix:** read the file once per `scan` (or once per process, lazily) and pass
the contents to `pci_device_name`, which already takes `&str`.

### D4. Sub-cell quantization is hand-rolled three times

**Declined.** The three sites quantize against different unit counts and
different clamp floors, so a shared helper would take both as parameters and
save nothing but the arithmetic itself.

`src/ui.rs:657` (`mini_spark`, both branches) and `:845` (`draw_waveform_cells`)
each open-code `value → filled sub-units, clamped` against different unit counts
(4, 8) and different clamp floors. Small enough that extraction may not pay, but
it is the kind of arithmetic that drifts.

---

## YAGNI

`backlog.md` §9 already tracks the three live items (`draw_meter`'s 8 arguments,
`UiTheme::temp_ok`'s asymmetric visibility, the single-variant `keys::Mode`
required by the `hjkl-keymap` API). No new dead code was found: every
`GpuSnapshot` field is rendered, every `Action` variant is bound, every helper
in `linux.rs` has at least two callers, and both `clamp_pct` and `join_throttle`
are used by more than one backend.

One near-miss worth naming:

### Y1. Braille-sized history retention is paid by all three graph styles

**Fixed in `3581995`.**

`src/ui.rs:66` — `app.history_need = area.width as usize * 2` is set
unconditionally, but only braille packs two samples per column; block and ascii
consume one. Under `--graphs ascii` on a 400-column terminal every device
retains twice the samples it can ever draw.

**Fix:** scale by the active `GraphStyle`.

---

## Verified, not findings

Recorded so the next pass does not re-open them.

- **Device identity.** `DeviceKey` / `device_keys` correctly degrade a duplicate
  or absent `device_id` to a positional key, positional keys are never
  persisted, and `CompositeBackend` namespaces every child's ids by child index.
  The vacated-slot de-duplication (`mod.rs:261`) is correct.
- **Process index rebasing.** `CompositeBackend::processes` accumulates by
  `slots` (the high-water mark), not by the current poll's device count, and
  drops rows outside their child's span. The three-child and middle-child-resize
  tests cover the off-by-one variants.
- **Mouse hit-testing.** `proc_visible` is bounded by both the drawn row count
  and `procs.len()`, so a click can never select an off-screen row; the
  `app.procs[scroll..scroll + visible]` slice is provably in range because
  `proc_scroll` is clamped to `total - visible` before it.
- **Numeric guards.** `gradient` handles empty and single-stop ramps,
  `parse_size` saturates, `vram_total_mb` saturates, `le_int` rejects
  non-scalars, `windowed` handles short data, and `proc_pane_height` computes in
  `u32`. All carry tests.
- **Modal input.** Control chords cannot reach the filter buffer, mouse events
  are ignored while a modal is up, and `Ctrl-C` in filter mode still quits.
- **PDH buffer handling** (`windows.rs:403`) allocates with the item type's
  alignment and sizes from the `PDH_MORE_DATA` byte count, which is the correct
  contract; the `Vec` outlives the raw pointer derived from it. </content>
  </invoke>
