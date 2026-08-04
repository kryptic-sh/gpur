# Performance review — gpur 0.12.0 (2026-08-04)

Perf pass over the whole codebase at `main` (8 fix commits since the 0.12.0
review, all pushed; tree otherwise clean). Findings ranked by impact. No code
changed.

All three findings were fixed the same day in three commits, each shipping its
own regression test, reviewed and pushed after the fact:

- `fe37c82` → closes finding 1 (paced sweep, `linux.rs`).
- `f5d76f0` → closes finding 2 (NVML static cache, `nvidia.rs`).
- `2d63c5e` → closes finding 3 (container-id cache, `app.rs`).

The hardening notes and coverage section below are unchanged by the fixes: the
hardening items were judged not worth the churn, and the coverage gaps
(PDH/IOKit live paths, no profiling) are about verification, not code.

## Findings

### 1. The /proc fdinfo sweep is re-requested every poll with no pacing — a busy box at a fast tick burns a full core

**Status: fixed in `fe37c82`** — `latest()` now requests a walk only when the
last published one is older than 200 ms and none is in flight; the worker tracks
`walking` under the same lock that clears `wanted`. Sync mode
(`--once`/`--json`) still walks on demand. Test:
`the_worker_serves_paced_walks_not_one_per_request`.

`src/backend/linux.rs:354-355` — `ProcScanner::latest()` sets `wanted` and wakes
the worker on **every** call, and the worker (linux.rs:391-408) does one full
walk per request. Each Linux poll calls it once per live backend (`amd.rs:152`,
`intel.rs:132` via `SweepCursor::next`, linux.rs:476), so walk rate == poll
rate. The walk is the costliest thing in the process: a readdir, a stat per fd
and a read per DRM fd, "measured at 4.2 ms over 588 pids" (linux.rs:153-154) —
roughly 7 µs/pid.

Why it matters: the two rates multiply. At the default 1000 ms tick that is 0.4%
of a core; at the 50 ms floor, 8.4%; on a 5000-pid GPU node the walk is ~36 ms,
so at any tick below that the worker walks back-to-back forever — a core pegged
at ~100% by a monitoring tool that is supposed to be idle, and the util% columns
end up measured over a ~walk-duration window regardless of the tick the user
chose.

Fix: pace the requests. Do not set `wanted` while a walk is already in flight,
and/or do not request a fresh walk until the last published one is older than a
floor (e.g. `min(tick_ms, 200ms)`). Utilization is a counter delta over the
actual walk-to-walk interval (linux.rs:75-92 divides by the walks' own `at`
stamps), so a slower walk rate only widens the measurement window — it never
corrupts the number. `wanted` already coalesces two backends on one tick into
one walk; the same coalescing just needs a time component.

### 2. NVML re-queries session-static values every poll

**Status: fixed in `f5d76f0`** — `name`, `bus_type`, `pcie_max_gen` and
`pcie_max_width` resolve once at probe into per-index fields, with a per-poll
fallback only when a probe query failed. `enforced_power_limit` deliberately
stays per-poll (it changes when the user moves the cap). Compiler-verified; no
hardware-backed test exists for NVML on this repo.

`src/backend/nvidia.rs:126` (`dev.name()`), `:132` (`dev.bus_type()`),
`:161-164` (max PCIe gen/width) — a driver round trip per card per tick for data
fixed for the life of the session. The backend already caches the UUID for
precisely this reason (nvidia.rs:59-63: "a per-poll query is a driver round-trip
per card"), but these five calls per card survive in the poll loop alongside the
~20 that genuinely change (util, memory, temp, power, clocks, throttle, fan,
throughput). `dev.enforced_power_limit()` (:156) changes only when the user
moves the cap and belongs in the same bucket.

Why it matters: NVML calls are cheap individually, but each is a driver
round-trip, and at the 50 ms tick a two-card rig is making ~200 of them a
second. Removing the static ones cuts the NVML portion of the per-poll work by
~20%.

Fix: resolve `name`, `bus_type`, `pcie_max_gen`, `pcie_max_width` once at probe
into struct fields (alongside `uuids`); re-poll the power limit on a slow
schedule or on demand.

### 3. Process enrichment re-reads /proc and rebuilds cmdlines per row per poll

**Status: fixed in `2d63c5e`** — the container id is cached per (pid, start
time) and re-read only when the pid's identity changes, pruned to the pids still
on a GPU each poll. The command join is deliberately NOT cached: it is an
in-memory alloc over sysinfo's already-cached cmdline, and an in-place `exec`
must not leave a stale COMMAND column. Tests:
`cached_container_resolves_once_per_process_identity`,
`a_pid_that_leaves_the_gpu_table_is_pruned_from_the_container_cache`.

`src/app.rs:961` — `container_of_pid(gp.pid)` does a fresh `/proc/<pid>/cgroup`
file read (app.rs:274-278) for every row of every poll, and `:956-960` —
`command_of` joins sysinfo's cmdline `Vec` into a `String` (app.rs:140-152) per
row per poll. Both run unconditionally because the Linux backends emit rows with
no user/command/container (`linux.rs:96-116`, `build_proc`), so
`gp.container`/`gp.command` are `None` on every live row.

Why it matters: per-poll, per-row I/O and allocation for data that is ~static
per process. A cgroup path does not change between polls of the same pid. At a
desktop's handful of GPU processes this is noise; at the 50 ms tick over a
hundred containerized rows it is a few hundred extra file reads and joins a
second.

Fix: cache the container/command on `(pid, start-time)` and re-resolve only when
the pid changes identity, or when it first appears. `refresh_processes` already
has sysinfo's cached `Process` for the row; the only _I/O_ saved is the cgroup
read, the join is a pure allocation.

## Hardening (correct, not defects)

- **PCIe link re-read per tick** — `pcie_link` does 4 sysfs reads per GPU per
  tick (amd.rs:350, intel.rs:150, nouveau nvidia.rs:330) for values that are
  almost static: max gen/width never change, current gen/width change on
  negotiation only. Cache the maxes at probe; re-read the current pair on a slow
  schedule. Small but pure waste at fast ticks.
- **History front-drain** — `hist.*.drain(..overflow)` (app.rs:856-862) is an
  O(cap) memmove per vec per poll once the ring is full (~4 × 16 B × cap × GPUs
  per poll; still single-digit MB/s at real widths). A `VecDeque` makes it O(1)
  if the cap ever grows with terminal width.
- **Meter spans** — `draw_meter` pushes one `Span` per meter column per frame
  (ui.rs:725-739), ~2 × meter width × cards spans/frame on a wide terminal.
  Idiomatic ratatui and small in absolute terms; only worth revisiting if the
  frame rate becomes a goal.
- **Per-frame string rebuilds** — `footer_hints()` (ui.rs:81, keys.rs:151-161)
  and `driver_info()` (ui.rs:41, composite joins children each call) rebuild
  static text every frame. Trivial to cache; noise at any realistic frame rate.

## Coverage

- **Traced fully:** the `App::poll` → `refresh_processes` → `rebuild_proc_view`
  path, the shared /proc scanner and sweep attribution (the dominant cost in the
  process), amd/intel/nouveau per-tick sysfs reads, NVML's per-poll query set,
  the whole `ui.rs` frame path, the main-loop event/draw/poll pacing, and
  replay/mock.
- **Not run (platform-gated, statically reviewed only):** `windows.rs` PDH and
  `apple.rs` IOKit. PDH collects each counter's full formatted array per poll
  (windows.rs:432) and Apple re-enumerates IOKit per poll (apple.rs:212-225,
  documented as cheap); neither is compiled on this Linux host, so no cost
  figure was verified for either.
- **Not settled without profiling:** NVML round-trip latency, the real /proc
  walk cost on a busy box (the 4.2 ms/588-pids figure is the code's own
  measurement, linux.rs:154), and the render path's per-frame microseconds.
  Finding 1's severity on a given machine depends on its process count — the
  defect (walk rate == poll rate, no pacing) is structural and independent of
  that count.
