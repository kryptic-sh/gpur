# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Container ids are resolved once per process instead of on every poll.** The
  Linux backends' rows arrive without container attribution, so each poll read
  `/proc/<pid>/cgroup` again for every GPU process. The cgroup path is ~static
  per process, so it is now cached per (pid, start time) and re-read only when
  the pid's identity changes; rows naming a pid sysinfo can't resolve still read
  it directly, and the cache is pruned of pids that leave the GPU table. Command
  lines still re-derive each poll from sysinfo's cached cmdline — an in-place
  `exec` must not leave a stale COMMAND column.
- **NVML session-static data is read once at probe.** The per-poll loop
  re-queried `name`, `bus_type` and the maximum PCIe link gen/width — values
  fixed for the life of the device — as driver round-trips on every poll, beside
  the UUID it already cached. They are now resolved at probe like the UUID, with
  a per-poll fallback only where the probe query failed. The enforced power
  limit still re-reads per poll: it changes when the user moves the cap.
- **The `/proc` fdinfo sweep no longer re-walks at the poll rate.** The Linux
  backends' per-process scan was re-requested on every poll, so at a fast tick
  on a busy box the worker could spend a whole core walking `/proc` (the walk is
  ~4.2 ms over 588 pids and scales with the process count). Walks are now paced
  to at most one every 200 ms and requests arriving mid-walk are absorbed, so
  the sweep-fed figures (the process table, Intel's gauges, AMD's video%)
  refresh at most every 200 ms on top of whatever the poll interval is — a
  slower refresh, never a wrong one, since utilization is measured between the
  walks' own timestamps.

- **The `hjkl-*` dependencies moved from 0.33 to 0.40** — `hjkl-config`,
  `hjkl-keymap`, `hjkl-keymap-tui`, `hjkl-kitty`, `hjkl-splash` and
  `hjkl-theme`. No call site changed: every symbol gpur uses is unchanged, and
  the one removal in the range (`hjkl_config::write_default`) was never called
  here. `hjkl-config` now resolves XDG paths through the new `hjkl-xdg` crate
  rather than inline, with the same policy — `~/.config/gpur` and
  `~/.cache/gpur` are where they were, and `XDG_CONFIG_HOME` / `XDG_CACHE_HOME`
  are still honoured. Packagers should note three new transitive dependencies:
  `hjkl-fs`, `hjkl-xdg` and `toml_edit`.

### Fixed

- **The power mini-spark no longer overflows on large replay values.** Its scale
  maximum is derived from the data (a replay can carry arbitrary `power_w`), and
  the level was computed as `value * 8` in `usize` before dividing — overflowing
  on 64-bit for values above `u64::MAX / 8` and panicking in checked builds. The
  scaling now happens in `u128`.

- **A mainline-i915 dGPU's process rows no longer under-report memory on the
  first sweep.** Such a card proves itself discrete only once a sweep shows
  local-memory residency, which upgraded the `discrete` flag only after that
  sweep's process rows were built — so the first poll charged its clients'
  `system` bytes (zero) instead of their device-local residency. A client that
  showed local regions this sweep is now charged its local bytes regardless of
  the stale flag.

- **PDH items marked invalid no longer become GPU measurements (Windows).**
  `PdhGetFormattedCounterArrayW` can report success while individual items carry
  `CStatus = PDH_CSTATUS_INVALID_DATA`; those items' values were read into
  utilization and memory figures as if real. Items whose status is not
  `PDH_CSTATUS_VALID_DATA` are now discarded.

- **Nouveau-driven NVIDIA cards are no longer hidden beside proprietary ones
  (Linux).** The NVML backend was returned as soon as one card on the
  proprietary driver was found, so a rig mixing `nvidia` and `nouveau` omitted
  every open-driver card. The nouveau sysfs scan now runs alongside NVML and its
  cards are appended to the NVML snapshots.

- **Slowing the poll rate no longer overflows on a huge interval.** `--tick-ms`
  is unbounded above, so pressing `-` on a value above `u64::MAX / 2` wrapped
  `* 2` to zero (release; the loop then polled at maximum rate) or panicked
  (debug). The doubling is now saturating, so the 10 s clamp actually clamps.

- **Terminal setup failures now restore the terminal before exiting.** If
  enabling the kitty keyboard protocol or mouse capture failed after ratatui put
  the tty into raw mode and the alternate screen, gpur returned the error
  without undoing either — the invoking shell was left in raw mode. Both error
  paths now run the full teardown first.

- **Replay no longer skips the first recorded frame.** `load` consumed record 1
  to validate the file, then the first `poll` advanced past it, so an N-record
  log replayed records 2..N and headless `--once`/`--json` skipped the recorded
  first interval. The first poll now hands back the preloaded record.

- **Kill confirmation no longer signals a PID that was reused while the dialog
  was open (Linux).** The confirm path pinned the target with a
  seconds-resolution start-time comparison and then signalled by numeric PID, so
  a process that exited during the prompt and a replacement that took its number
  within the same second passed the check. `confirm_kill` now opens a `pidfd`
  (pin + send via `pidfd_send_signal`) and additionally re-reads
  `/proc/<pid>/stat` field 22 (clock-tick start time) at pin time. A reused PID
  fails the tick comparison at pin time, and a replacement after the pin cannot
  receive the signal — the pidfd still names the original, and the kernel
  answers `ESRCH`. Kernels or sandboxes without pidfd support fall through to
  the previous behaviour.

## [0.12.0] - 2026-08-02

### Changed

- **The `/proc` fdinfo sweep runs on a worker thread, and once per poll rather
  than once per vendor.** The Linux backends read per-process GPU state by
  walking every process's DRM fds, which ran inside `App::poll` — so
  `event::poll` could not run during it and keystrokes queued behind it. On a
  machine with thousands of processes at `--tick-ms 100` the walk can outlast
  the tick and the UI stops answering the keyboard. A `ProcScanner` thread now
  owns the walk and the render thread reads the newest finished one. The walk is
  also vendor-agnostic now, so an AMD + Intel box takes one walk per tick
  instead of one per backend, each of which used to discard the other's clients.
  Per-process figures can lag by up to one poll as a result; they are not
  smeared by it, because a utilization is measured between the two walks' own
  timestamps rather than against the clock when the backend got to them.
  `--once` and `--json` keep walking on the polling thread, since they report
  the delta across one known sleep and both walks have to bracket it.

- **The MEM meter now shows the memory a card actually spends, and says when
  those bytes are system RAM.** An Intel iGPU has no local pool, so `vram_*` is
  `None` and the meter read `MEM n/a` beside a card demonstrably holding memory;
  it now meters the system-backed pool as `703M/15G shared`. Apple Silicon and
  Windows' integrated adapters, whose unified memory arrives _through_ the VRAM
  fields, no longer read as dedicated VRAM — they gain the same `shared` marker.
  New `GpuSnapshot::mem_primary` / `mem_secondary` / `mem_pct` pick the pool
  once, for the meter, the graph and `--once` alike. The JSON record is
  unchanged.
- **The second memory pool moved from the footer onto the MEM row**, next to the
  meter whose pool it sits beside: `· gtt 3.0G/16G` on a card with real VRAM,
  where a rising figure means the working set spilled to host RAM across PCIe,
  and `· shared 3.0G/16G` on an APU, where nothing spilled anywhere because both
  pools are RAM.
- **The memory graph plots the metered pool**, so an iGPU's graph is no longer
  permanently blank; its caption is `mem%` rather than `vram%`.
- **`--once` prints `mem used/total` rather than `vram used/total`**, with the
  `shared` marker and the second pool where they apply.
- **`--mock`'s first card is now a unified-memory part**, so the demo and the
  test suite exercise the shape every iGPU, APU and Apple Silicon card has.

### Added

- **Hardware tests for the Intel backend** (`src/backend/intel.rs`), covering
  every value it reads from i915/xe: the card scan, gauge ranges, hwmon presence
  both ways, the PCIe attributes, pci.ids naming, VRAM and clock paths, the
  energy-delta power reading, per-client state pruning, and that the device
  gauges equal the sum of the process rows from the same sweep. They skip
  themselves where no i915/xe card is present; `GPUR_REQUIRE_INTEL=1` turns that
  skip into a failure for a runner that is supposed to have one.
- **Hardware tests for the AMD backend** (`src/backend/amd.rs`), on the same
  shape: the card scan and APU/discrete classification, gauge ranges, the sysfs
  and hwmon attributes checked both ways (a gauge missing where the file exists,
  and a gauge invented where none does), the labelled junction/memory
  temperature channels, the DPM-table clock fallback a power-gated `freq1_input`
  falls through to, the PCIe attributes, `pcie_bw` only where the ASIC counts
  it, pci.ids naming, per-client state pruning and the enc/dec split against the
  video total it came from. The `#[ignore]`d `live_poll_reports_devices`, which
  printed a poll and had to be run by hand, is gone: these replace it.

  The per-class memory rules — an APU charges a client `vram + gtt`, a discrete
  card `vram` alone — are checked against this machine's own fdinfo, with the
  test opening the card's render node read-only so there is a client to
  attribute. A headless box owns no DRM client at all, which is what used to
  make the rule impossible to check anywhere but on a running desktop.

  Three gates, so a runner with the hardware cannot silently stop testing it:
  `GPUR_REQUIRE_AMD`, plus `GPUR_REQUIRE_AMD_APU` and `GPUR_REQUIRE_AMD_DGPU`
  for the class-specific halves, which skip independently of each other.

## [0.11.1] - 2026-08-01

### Fixed

- **An unreadable graph sample drew as a flat `0`.** Graph history recorded an
  absent reading as zero, because a waveform has no glyph for "unknown" — the
  last place in the UI where a metric the backend could not read was drawn as a
  real measurement, after the device gauges (0.10.2) and the process table
  (0.11.0). It now draws the minimum sliver marked `·` in braille and block, or
  `_` in ascii, dimmed: a mark rather than a gap so the trace stays continuous,
  and one that belongs to no value ramp so it cannot be read as a magnitude.

  The glyph is the signal and the dimming only reinforces it, because colour and
  `DIM` are both lost under `NO_COLOR`, under `TERM=dumb`, on a terminal that
  ignores `DIM`, and in any screenshot or copy-paste. A measured `0` keeps its
  own glyph and its gradient, which is the distinction the change exists to
  make.

  The not-yet-filled left of a fresh graph is marked the same way, on the same
  grounds — no sample was taken there either — so a wide terminal now starts
  mostly dim and fills in with colour from the right.

- **Block graphs rendered their bottom half upside down without colour.**
  Unicode has upper partial blocks only at ⅛ and ½, so the down-growing half is
  drawn by painting the bar in the background and the hole in the foreground.
  That needs two distinct colours, and `NO_COLOR` / `TERM=dumb` has none — both
  collapse to the terminal default — so the hole was drawn as if it were the bar
  and a measured `0` filled seven eighths of its cell. That half now falls back
  to `▔`/`▀`/`█`, rounding to the nearest and rounding ties down: three levels
  instead of eight, but pointing the way the bar grows. Every other colour mode
  is unchanged.

## [0.11.0] - 2026-08-01

### Breaking

- **`--json` and `--log` emit `null` where they emitted `0`** for the four
  per-process metrics: `gpu_util_pct`, `gpu_mem_bytes`, `cpu_pct` and
  `host_mem_bytes`. A reading the backend could not take is now absent rather
  than zero, matching what the GPU-level metrics have always done.

  Consumers should read `null` as "not measured", not as zero. **A recording
  written by 0.11 or later cannot be replayed by an earlier gpur** — it rejects
  the file outright when every record carries a null, and skips the affected
  frames silently when only some do. The useful direction is unaffected: 0.11
  replays pre-0.11 recordings, since `0` remains a valid reading.

### Fixed

- **Per-process GPU memory read `0MiB` for every NVIDIA process on Windows.**
  NVML answers `NVML_VALUE_NOT_AVAILABLE` when it cannot account for a process,
  and nvml-wrapper documents that as _always_ reported under WDDM — the ordinary
  consumer Windows configuration, where the Windows kernel-mode driver owns the
  memory rather than NVIDIA's. gpur folded that to `0`, so the GPU MEM column
  was a confident, permanent lie on that platform rather than a rare unknown.

  The same `unwrap_or(0)` hid an unresolvable pid's CPU% and host memory behind
  `0%` and `0MiB`. All of it now renders as `N/A` in the table and `-` in
  `--once`, the spelling each already used for an unreadable utilization.

  Only genuinely unreadable figures become absent. The Linux fdinfo sweeps still
  report a real `Some(0)`: fdinfo names a memory region only when the client
  holds something in it, so a client with no `vram` or `gtt` region really does
  hold nothing there. On Windows, PDH reports unknown only when neither memory
  counter carries the process at all.

- Unmeasured rows sink below every measured row in the process table in **both**
  sort directions, for GPU memory, CPU% and host memory — the rule GPU% already
  had. Folding them in as `0` handed them the top of an ascending sort. The
  stable order `--json` and `--log` use sinks them the same way, so the record
  and the table agree.

## [0.10.3] - 2026-08-01

### Changed

- Dependencies refreshed to their latest compatible versions: clap 4.6.4 to
  4.6.5, clap_builder 4.6.2 to 4.6.5, serial2 0.2.37 to 0.2.38 and toml 1.1.3 to
  1.1.4. No manifest ranges and no code changed.

  The hjkl crates stay on 0.33. Their 0.39 is semver-incompatible and spans
  config, keymap, theme and splash — config and cache paths, keybindings,
  theming — so it remains a behavioural change deliberately left for its own
  release, as it was in 0.10.1.

## [0.10.2] - 2026-08-01

A full-codebase review pass; the suite grew from 170 to 178 tests, each one
pinning a finding below. The review had its own document while findings were
still open; once they had all shipped it was folded into `docs/backlog.md`,
whose "Settled by review" section records what was checked and still holds.

### Fixed

- **An Intel GPU whose processes gpur could not read was drawn as a measured
  `0%`.** Intel publishes no device-level busy counter, so utilization is summed
  over the DRM clients found by the `/proc` fdinfo sweep — and `/proc/<pid>/fd`
  is unreadable for every process another user owns. An unprivileged gpur
  therefore painted a confident, meter-drawn `0%` over someone else's saturated
  iGPU, and an empty GTT pool beside it. A device the sweep attributed no client
  to now reports `n/a`; one whose clients were read and summed to zero keeps its
  honest `0%`, so an idle GPU still draws an empty meter.
- **The kill dialog could send SIGKILL while reporting SIGTERM.**
  `Process::kill_with` answers `None` where the signal is unsupported — SIGTERM
  on Windows — and the fallback was a plain `kill()`, which is `SIGKILL`. The
  dialog asked about SIGTERM, the status line then said SIGTERM, and the process
  was killed uncatchably. gpur now refuses and says the signal is unsupported;
  escalating has to be the user's own keystroke.
- **A failing `--replay` session could re-detect itself into live hardware.**
  The five-consecutive-failure re-detect kept `--mock` but not `--replay`, so a
  bare detect would have answered with this machine's backend — flipping
  `can_signal()` from false to true and leaving a stranger's recorded pids aimed
  at local processes. The whole choice is now carried in one value, and startup
  and re-detect run the same call, so a re-detect can only reproduce the kind of
  backend the session began with.
- **The process cache grew for the life of the session.** `sysinfo` only evicts
  pids from inside the set it is handed, and gpur hands it only the pids
  currently on a GPU — so every short-lived job a node ever ran stayed cached
  until gpur exited. Departed pids are now swept each poll, without the
  all-processes refresh the narrow update set exists to avoid.
- **CPU% was sampled faster than it can be measured.** `cpu_usage()` divides a
  process's own jiffy delta by a `/proc/stat` delta `sysinfo` will not retake
  inside 200 ms, so at `--tick-ms 50` four polls in five divided one tick of
  process time by 200 ms of machine time. The CPU half of the refresh is now
  rationed to that minimum; the column keeps its last value in between rather
  than blanking.
- Recorded process rows naming a GPU outside the replayed frame — a truncated or
  hand-edited log — are dropped instead of drawn against a card that was never
  recorded, matching the rule the composite backend already applies to its live
  children.
- Two card-height sums in the GPU pane wrapped `u16`: past 8191 cards the total
  read as "everything fits", and a pane within one card of `u16::MAX` kept
  admitting cards past its own bottom.

### Security

- **`--log` is created readable only by its owner** where the platform has file
  modes. Every record carries the full process table — command lines, usernames,
  container ids — and argv routinely holds `--api-key=`, `--token=` or a
  database URL with its password in it. Under the usual `umask 022` the file
  landed world-readable, on exactly the kind of shared machine where `--log` is
  left running for hours. A log that already exists keeps the permissions its
  owner gave it.
- **`state.json` is published through a temp file and a rename.** `fs::write`
  truncates first, so a crash, a `kill -9` or a full disk part way through left
  a short file that the loader could only discard — silently, taking every fold,
  sort key and poll rate with it. The quit path is where that was likeliest: the
  signal handler exits without waiting for the save.

### Changed

- Graph history is retained per what the active glyph set can draw. Only braille
  packs two samples into a terminal column, so `--graphs block` and
  `--graphs ascii` were holding twice the samples any graph could ever show.
- `pci.ids` is read at most once per process rather than once per card by each
  Linux backend — a mixed AMD + Intel + nouveau rig re-read the same ~1.5 MB
  several times over at probe. `fan_pct` and the multi-driver header line, both
  duplicated verbatim between backends, now live beside the other shared sysfs
  readers and have their first test coverage.
- Unit-test fixtures no longer build fixed paths under the shared temp
  directory. Concurrent `cargo test` runs raced on one directory, and the name
  could be pre-created as a symlink the tests would then write through.

## [0.10.1] - 2026-07-28

### Changed

- Dependencies refreshed to their latest compatible versions across ~45 crates —
  clap, serde, regex, sysinfo, libc, toml, and the pest and futures trees, with
  hjkl moving 0.33.3 to 0.33.6. No manifest ranges and no code changed.

  The hjkl crates have 0.39 available, which is semver-incompatible with the
  0.33 range pinned here and spans config, keymap, theme and splash — config and
  cache paths, keybindings, theming. That is a behavioural change rather than a
  refresh, so it is deliberately left for its own release.

## [0.10.0] - 2026-07-28

### Added

- **NVIDIA cards on the nouveau driver are listed.** NVML exists only with the
  proprietary driver, and the AMD and Intel scans correctly filter on their own
  PCI vendor, so a nouveau-bound card was claimed by no backend at all — on a
  mixed-vendor machine gpur showed the other cards and gave no hint one was
  missing. A sysfs backend now runs behind the NVML probe, so it can never
  double-list a card NVML already reported. It carries name, PCI id, hwmon
  temperature/power/fan/voltage and PCIe link state; utilization, VRAM and
  clocks stay absent, because nouveau publishes none of them and a fabricated
  `0%` would read as an idle GPU.
- Pre-GCN AMD cards on the `radeon` driver are listed, with the gauges that
  driver actually supports.

### Fixed

- **The AMD scan claimed cards by PCI vendor alone**, so it took `radeon` cards
  and cards with no driver bound, then read them through amdgpu's sysfs layout.
  Both Linux scans now claim by vendor _and_ driver, and each device carries its
  own driver rather than the hardcoded `"amdgpu"` that made the fdinfo sweep's
  driver check a lie for any `radeon` card.
- **A vacated slot in the composite backend could inherit a live device's
  identity.** When a child shrank and the survivor landed on a lower slot, one
  poll emitted two rows with the same `device_id`. The TUI was unaffected —
  `App` degrades a repeated id to a positional key — but `--json` and `--log`
  recorded the duplicate and `--replay` read it back.
- `poll()` returned an empty device list with the error swallowed when one child
  errored and another reported no devices; it now surfaces the failure, while a
  genuine partial failure still keeps the surviving vendors' cards on screen.
- On an Intel Mac the iGPU was filed as discrete while being handed all of
  system RAM as its VRAM total, and the header driver line was fixed at probe so
  an eGPU's kext never joined it.

## [0.9.0] - 2026-07-27

### Fixed

- **Devices now carry a stable identity.** `GpuSnapshot` gained an opaque
  `device_id` — NVML UUID, PCI address on Linux, IOKit registry entry id on
  macOS, adapter LUID on Windows, namespaced per child in the composite backend
  — and `App` keys waveform history, session peaks, folding and the selection
  cursor on it instead of on position. A hotplug, a driver reset or a
  differently-ordered enumeration previously reattached one GPU's graphs and
  peaks to another with no visual cue. State for a departed device is kept (so a
  card that vanishes and returns picks its own graphs back up) and bounded, so
  hotplug churn cannot grow the maps without limit. Folds persist as
  `folded_devices` (device ids) rather than the old `folded` array of positions,
  which folded the wrong card whenever the device set changed between runs; a
  pre-identity `state.json` still loads, its positions dropped rather than
  misapplied.
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
- **CoreFoundation values are type-checked before use.** `dict_get_dict` and
  `dict_get_i64` reinterpreted an arbitrary `CFTypeRef` as a concrete type
  without checking, while `dict_get_string` alongside them already downcast
  properly. Reading a `CFData` as a `CFNumber` is undefined behaviour and not
  hypothetical: device-tree properties such as `gpu-core-count` are commonly
  published as a raw little-endian blob.
- The process pane could take the whole body, leaving the GPU pane zero rows and
  rendering nothing at all on a short terminal, and the same expression
  overflowed `u16` above 21845 rows and panicked.
- `theme::gradient` panicked on ramps shorter than two stops; the scroll caption
  rendered an inverted range when nothing was visible; the confirm popup was
  sized in bytes rather than characters.
- An empty process list blamed fdinfo permissions even when the user's own
  filter was what emptied it.
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
- AMD device utilization is routed through `clamp_pct` like every other backend,
  as is Windows per-process utilization — `proc_engine` sums engine instances of
  the same type and could exceed 100%.
- **Every colour is quantized.** The block waveform rebuilt its background as
  24-bit RGB from a hardcoded fallback, and the splash trail emitted raw RGB, so
  16- and 256-colour terminals — the ordinary tmux and ssh case — received
  truecolor escapes they cannot render. Measured with `--graphs block`: 158
  escapes under `TERM=xterm` before, none after.
- Session peaks omit sensors that never reported instead of showing `0°C` /
  `0W`, and the power average covers only the samples that carried a reading
  rather than being diluted toward zero by those that did not.
- Apple accelerators are ordered by registry entry id. IOKit does not document
  its iteration order, and `App` keys history and session peaks positionally, so
  a reorder silently swapped two GPUs' graphs mid-session.
- Per-process memory on APUs counts GTT, where an APU puts almost everything —
  the process rows previously contradicted the card's own GTT line. Verified
  against a live VAAPI encode: the same pid goes from 58 MiB to 79 MiB.
- The fdinfo sweep reads each pid's `fd` directory once rather than once per
  driver name, roughly halving sweep cost on an i915+xe host (4.2 ms against
  7.8-9.9 ms over 588 pids).

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
- **All vendors are enumerated at once.** `detect()` returned the first backend
  that probed, so a hybrid machine — an NVIDIA dGPU beside an AMD APU or Intel
  iGPU — showed one vendor and hid the other. Successful probes are now
  composed; a single-vendor machine is handed its backend unwrapped and behaves
  identically. A child that fails or shrinks keeps its slots as placeholders so
  it cannot shift another vendor's indices and detach its graphs, `poll()` fails
  only when every child fails, and `can_signal()` is the AND of the children.
  The composite reports `multi` and carries vendor identity in `driver_info()`.
- Apple `driver_info()`, from the OS product and build plus the accelerators'
  kext bundle ids — macOS ships no per-GPU driver version.
- **Mixed-vendor Windows rigs list every card.** The PDH backend was taken only
  when no vendor backend probed, because DXGI enumerates every adapter and would
  list an NVIDIA card twice next to NVML's richer entry — so an NVIDIA laptop
  with an AMD or Intel iGPU showed only the discrete card. PDH is now a peer,
  told at probe time which PCI vendors the vendor backends claimed, and drops
  those adapters. Matching is by vendor rather than by adapter, since
  `DXGI_ADAPTER_DESC1` carries no bus/device/function; an adapter its own
  vendor's backend does not report is hidden rather than duplicated.
- Mouse input has test coverage for the first time: clicks on process rows and
  GPU cards, the pane border that must select nothing, wheel routing and its
  clamps at both ends, and the guard that ignores mouse events under an open
  modal. Each is proven load-bearing against a deliberate source mutation.

### Breaking

- **`utilization_pct`, `vram_used_bytes` and `vram_total_bytes` are now
  optional** and serialize as `null` in `--json` and `--log` when the backend
  genuinely cannot read them; `--once` prints `n/a` and the TUI draws no meter
  track at all. They were the only headline metrics that substituted `0` for a
  missing source, so an unreadable sensor rendered as a confident idle GPU —
  which is why a truncated LUID key, an Arc card with no VRAM total, and an iGPU
  whose memory regions never matched all looked like ordinary idle behaviour. A
  measured zero is still `0`. Consumers that assumed a number must handle
  `null`.
- The `--once` VRAM pair carries its unit on each side (`vram 40MiB/24560MiB`)
  so `n/a` reads correctly in either position.
- **`state.json` stores folded cards by device id** in a new `folded_devices`
  key. The old positional `folded` list is dropped rather than migrated — which
  GPU a bare index meant is precisely what was unknowable, so honouring it would
  re-apply the bug being fixed. Folds must be set once more after upgrading;
  sort order and poll rate carry over untouched.

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

[Unreleased]: https://github.com/kryptic-sh/gpur/compare/v0.12.0...HEAD
[0.12.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.12.0
[0.11.1]: https://github.com/kryptic-sh/gpur/releases/tag/v0.11.1
[0.11.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.11.0
[0.10.3]: https://github.com/kryptic-sh/gpur/releases/tag/v0.10.3
[0.10.2]: https://github.com/kryptic-sh/gpur/releases/tag/v0.10.2
[0.10.1]: https://github.com/kryptic-sh/gpur/releases/tag/v0.10.1
[0.10.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.10.0
[0.9.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.9.0
[0.8.1]: https://github.com/kryptic-sh/gpur/releases/tag/v0.8.1
[0.8.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.8.0
[0.7.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.7.0
[0.6.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.6.0
[0.5.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.5.0
[0.4.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.4.0
[0.3.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.3.0
[0.2.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.2.0
[0.1.0]: https://github.com/kryptic-sh/gpur/releases/tag/v0.1.0
