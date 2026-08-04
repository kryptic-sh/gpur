# gpur backlog

Open work from the review passes, the multi-vendor detection audit and the
full-codebase review of v0.10.1, whose separate document was folded into this
one once every finding it raised had shipped. The 0.12.0 code and performance
reviews were folded in the same way: their open items are entries 9-18 below,
and their verdicts and decisions live in the settled-by-review and decisions
sections at the bottom. Closed items are removed rather than struck through —
what was fixed, and why, lives in `CHANGELOG.md`. Anything from a closed item
that is still a live constraint on future work is kept below, either as a
deliberate decision or in the settled-by-review list, rather than as a task.

Roughly ordered by what is worth doing first. Nothing open here is a correctness
bug; the remainder is granularity, cost, coverage and polish. Every item that is
still open is open because it turns on a product or architecture call rather
than on effort.

No fabricated readings remain anywhere in the UI: the device gauges stopped
inventing a `0` in 0.10.2, the process table in 0.11.0, and the graph history —
the last holdout, because a waveform has no glyph for absent — in 0.11.1.

---

## Pending: the v0.12.0 AUR publish

**Everything else in the 0.12.0 release landed.** The GitHub release carries all
25 assets, crates.io reports 0.12.0 as its max version, and the Homebrew, Scoop
and Alpine jobs succeeded. Only `Publish AUR (gpur-bin)` failed, on
`git clone ssh://aur@aur.archlinux.org/gpur-bin.git`:

> The AUR is down due to maintenance. We will be back soon.

Not a configuration fault. The same `git ls-remote` fails identically from a
developer machine with a working AUR account, while `ssh aur@aur.archlinux.org`
answers its greeter normally — the interactive shell is up and git-upload-pack
is not. Re-run attempts at the time all hit the same banner, and a re-check the
following day (2026-08-03) got the same one — so this is an outage measured in
days, not minutes.

**To finish:** once `git ls-remote ssh://aur@aur.archlinux.org/gpur-bin.git`
succeeds, `gh run rerun 30735988517 --failed`. The job is idempotent — it exits
early with "nothing to push" if the PKGBUILD is already current — so re-running
it is safe whatever state the AUR repo is in. Delete this section once it is
green.

## 1. Windows vendor exclusion is per vendor, not per adapter

**Severity: low.** `src/backend/mod.rs` `compose_with_generic`,
`src/backend/windows.rs` `parse::retain_unclaimed`.

The mixed-rig case is fixed: PDH is now a peer rather than a fallback, told at
probe time which PCI vendors the vendor backends claimed, and drops those
adapters. An NVIDIA + Intel Windows laptop lists both cards, each once.

What remains is the granularity of the match. DXGI gives an adapter LUID (plus
`VendorId`/`DeviceId`); NVML gives a UUID or PCI BDF. `DXGI_ADAPTER_DESC1`
carries no bus/device/function, so nothing links a specific adapter to a
specific NVML device without SetupAPI or WMI to resolve LUID → BDF. The filter
therefore works at vendor granularity, and an adapter DXGI enumerates that its
own vendor's backend does not report is hidden rather than duplicated — the
choice being deliberate, since a card listed twice under two backends is the
worse bug.

**Fix, if it ever matters:** resolve LUID → PCI BDF via SetupAPI
(`SPDRP_LOCATION_INFORMATION`) or `Win32_PnPEntity`, and match that against
`Device::pci_info().bus_id`, falling back to the vendor filter when either side
declines. Not worth the dependency until someone hits the blind spot.

Unverifiable without Windows hardware: only `retain_unclaimed` and the
composition wiring are covered by unit tests. That DXGI's `VendorId` matches
`0x10DE`/`0x1002`/`0x8086` on a real adapter, and that NVML and DXGI agree on
which cards exist, rest on inspection.

## 2. An NVIDIA card is still invisible when NVML is broken rather than absent

**Severity: low.** `src/backend/nvidia.rs`.

The nouveau case is covered: a card on the open driver is picked up from sysfs
behind the NVML probe. What remains is a card bound to the _proprietary_ driver
on a host where NVML cannot initialise — a container without `/dev/nvidiactl`, a
mismatched driver/userspace pair. The card is in `/sys/class/drm` and no backend
claims it.

The reason it is not simply fixed the same way: an `nvidia`-bound card exposes
essentially nothing in sysfs — no busy counter, no VRAM, no hwmon in the usual
place — so listing it would add a name and a PCI address and nothing else.
Whether a row of `n/a` beats an absent card is a product call, not a bug.

## 3. Remaining test gaps

- **The kill path's signal branch.** Unit tests in `app.rs` cover every refusal
  and one successful signal against a spawned `sleep`, but no PTY test reaches
  it, because mock and replay now correctly refuse to signal. End-to-end
  coverage needs a backend stub the test binary can inject — which would also
  close the one mouse case left untested, the kill dialog as a modal. The filter
  prompt covers the identical `input_mode != Normal` condition meanwhile. Note
  the obvious shortcut, making the mock signalable, would weaken the guard that
  exists to stop exactly that.
- **Mouse kinds with no behaviour attached** — drag, middle and right button,
  and moves all fall to the `_ => None` arm. Untested because untested is what
  they are: there is nothing to assert yet.
- **The threaded sweep reaches a real card only through the AMD backend.**
  `the_worker_thread_feeds_the_backend_on_real_hardware` in `amd.rs` gives a
  backend a `ProcScanner` wired the production way and asserts its own render
  node reaches a row; `intel.rs` has no counterpart, so on Intel the worker path
  is proven only against hand-published walks. The other hardware tests pin the
  shared scanner to synchronous through `amd()` / `intel()`, deliberately — a
  test that opens a client and polls is asserting on that client, so the walk
  has to happen after the open. Worth adding the Intel twin next time that
  machine is available.
- **One walk serving every vendor is unverified on a mixed-vendor box.** Both
  backends take `SweepCursor::default()` (`amd.rs`, `intel.rs`), which is the
  shared `ProcScanner`, so structurally an AMD + Intel machine takes one walk a
  tick rather than one per backend. Nothing has observed it: the machine this
  was written on has AMD cards only, and no test asserts a walk count across two
  live backends. `a_cursor_attributes_each_walk_once` covers the half that is
  checkable anywhere — two cursors over one scanner each receive the same walk.
- **No before/after measurement of the responsiveness this bought.** The 4.2 ms
  over 588 pids in the old entry measured the walk itself, and the walk's code
  is unchanged — what moved is which thread runs it. That keystrokes no longer
  queue behind it follows from `App::poll` no longer walking, not from anything
  timed, and the pathological case that motivated the work (a 10k-process node
  at `--tick-ms 100`) has never been run.
- **Nothing forces a refused thread spawn.** `ProcScanner::spawn_worker`
  degrades to synchronous mode when the OS will not give it a thread, and that
  degraded path is the one `synchronous_mode_walks_on_the_calling_thread` covers
  — but the branch that chooses it is only reachable under thread exhaustion,
  which no test creates.
- **The hjkl 0.40 path re-home was read, not run, off Linux.** `hjkl-config`
  0.40 delegates config/cache resolution to the new `hjkl-xdg` crate instead of
  its own inline `xdg_base`. The two resolvers were compared side by side and
  implement the same policy — XDG vars honoured on every platform when absolute
  and non-empty, otherwise `~/.config` / `~/.cache` via `dirs::home_dir()`,
  deliberately not `%APPDATA%` or `~/Library/Application Support` — so no path
  should move anywhere. What was actually exercised is Linux only:
  `tests/tui.rs` points `XDG_CONFIG_HOME` / `XDG_CACHE_HOME` at a sandbox,
  drives the real binary through a PTY and reads `state.json` back out of it.
  The Windows and macOS halves rest on reading the resolver.
- **No CI job runs against real GPU hardware.** Inherent to hosted runners; the
  mitigation was fixture-testing every backend's pure parsers, which is why
  `windows.rs` and `apple.rs` have unit tests that run on any host. Closing it
  properly needs a self-hosted runner.

  This is the gap behind every "unverifiable without hardware" note in this
  file, and it is load-bearing: the WDDM per-process-memory bug fixed in 0.11.0
  was diagnosed from NVIDIA's documentation, not from an observation, and
  nothing in CI could have caught it or can confirm the fix.

  **The Linux half is verified, by hand rather than by CI.** The `hardware`
  modules in `intel.rs` and `amd.rs` run against whatever card the developer's
  machine has and skip otherwise. Both have been run green on real silicon —
  `amd.rs` on the amdgpu APU-plus-discrete machine this backlog is written on,
  with `GPUR_REQUIRE_AMD`, `GPUR_REQUIRE_AMD_APU` and `GPUR_REQUIRE_AMD_DGPU`
  all set; `intel.rs` on a separate Intel machine with `GPUR_REQUIRE_INTEL`. The
  gates are not vacuous: setting `GPUR_REQUIRE_INTEL` on the AMD machine fails
  the Intel suite outright rather than skipping it.

  So what remains here is not that the Linux backends are unverified but that
  **nothing enforces the verification** — the skip is still the default on any
  machine, and NVIDIA, Windows and Apple have no equivalent live suite at all. A
  self-hosted runner setting those variables is what makes the pass a standing
  guarantee rather than a thing someone remembered to do.

- **What the AMD hardware tests still do not reach**, on the machine they were
  written against (an amdgpu APU plus an amdgpu discrete card):
  - **`radeon`.** No pre-GCN card to run it, so `is_amd_driver`'s second half is
    fixture-only. The class tests would file such a card as discrete (`is_apu`
    sees no `gpu_metrics`), which is right, but nothing confirms it.
  - **`pcie_bw`.** Neither card implements the counter — the APU because the
    kernel marks it unsupported, Navi 31 because RDNA3 never implemented
    `get_pcie_usage` — so `pcie_kbs`'s live path is fixture-only.
    `pcie_bandwidth_is_reported_only_where_this_asic_counts_it` asserts the
    absence, which is the half that is checkable here. Needs a Vega or Navi
    1x/2x card.
  - **Engine utilization and the enc/dec split.** The tests open a render node
    to have a client to attribute, but a bare open submits no work: every
    `drm-engine-*` counter stays absent, so the ns-delta path is exercised only
    for its zero case and `enc`/`dec` never appear. Closing it means submitting
    real GPU work from a test, which is a dependency (a compute runtime) rather
    than an afternoon.
  - **Throttling.** `the_throttle_label_only_names_reasons_this_card_measures`
    checks that a label can only name a reason whose inputs exist; it cannot
    make a card hit its cap, so the branch that produces a label is never taken
    on an idle box.

- **The perf review's cost figures were never profiled.** The walk figure (4.2
  ms over 588 pids) is the code's own comment, and NVML round-trip latency and
  per-frame render time were never measured. The 0.12.0 perf pass paced the walk
  structurally (a 200 ms floor, `fe37c82`), which bounds the cost, but the
  numbers the review quoted are not measurements taken on this machine.

## 4. NVIDIA temperature threshold unread

**Severity: low.** `Device::temperature_threshold()` is available in
`nvml-wrapper 0.12.1` (`device.rs:4048`) and would let each card carry its own
thermal limits.

Note this is not a meter rescale: temperature has no meter. `src/ui.rs`
`temp_span` colours the reading through `UiTheme::temp_style`, which thresholds
at a fixed 75 °C and 90 °C for every device. So wiring this up changes **when a
card reads as hot**, which differs by part — a laptop GPU throttling at 87 °C
and a workstation card rated to 100 °C currently share one scale. Needs a new
`GpuSnapshot` field and a `temp_style` that takes the card's own limits, and it
is a product call rather than a mechanical change.

Junction temperature is genuinely unavailable rather than merely unimplemented:
the only temperature field ids in `nvml-wrapper-sys 0.9.1` are
`NVML_FI_DEV_MEMORY_TEMP` (wired up) and the four `*_TLIMIT` margins, which are
thresholds, not a hotspot reading.

## 5. Device naming differs per backend

**Severity: low, cosmetic.** `nvidia.rs` gives the marketing name
(`NVIDIA GeForce RTX 4090`), the Linux backends the pci.ids codename
(`Navi 31 [Radeon RX 7900 XT/...]`), Windows the DXGI description, Apple the SoC
brand. Fallbacks differ too: `NVIDIA GPU 0` (index) against
`AMD GPU 0x744c (card1)` (device id plus card).

**Fix:** settle on a convention — prefer the marketing name, fall back to the
codename, always suffix the card or index — and apply it uniformly.

## 6. Intel memory totals could come from the DRM query ioctl

**Severity: low, accuracy.** The Intel backend reads its system-pool total from
`/proc/meminfo` `MemTotal`, which is exactly what i915 reports for its system
region — verified against `/sys/kernel/debug/dri/*/i915_gem_objects` on a Tiger
Lake part, where `system: total:0x3d5cc9000` equals `MemTotal` to the byte. So
the total is right; what sysfs cannot give is anything more.

`DRM_I915_QUERY_MEMORY_REGIONS` and xe's `DRM_XE_DEVICE_QUERY_MEM_REGIONS` on
`/dev/dri/renderD*` (mode `0666`, so no privileges needed) publish per-region
`probed_size` and `unallocated_size`. That would add the stolen region as its
own figure instead of folding it into the system bucket, and — the real prize —
give an Arc's local memory a _free_ figure rather than a sum over the fdinfo of
processes this user happens to be able to read.

**Cost:** a raw ioctl and per-driver structs in a backend that is otherwise pure
sysfs reads, plus opening a device node. **Fix:** worth it only if the free-VRAM
figure on discrete Arc is wanted; the iGPU totals would not change.

## 7. Residual YAGNI

- `src/theme.rs:89` `UiTheme::temp_ok` is `pub` but used only by `temp_style` at
  `:173` in the same file, unlike its peers `temp_warn` / `temp_crit`. Narrowing
  it alone would be asymmetric.
- `src/keys.rs:7` `enum Mode { Normal }` has one variant threaded through
  `Keymap<Action, Mode>`. Required by the `hjkl-keymap` API, so it cannot go,
  but no second mode is planned — filter and confirm bypass the keymap entirely.

## 8. Noted upstream, not gpur's to fix

- **`hjkl-splash`'s changelog files a removal against the wrong release.** Its
  0.40.0 entry says the `ratatui` feature and `start_screen::render` were
  dropped there, but neither exists in any 0.33.x published to crates.io either
  — no `pub fn render` in the source and no `ratatui` in the manifest, in 0.33.3
  through 0.33.6 — and that crate's `src` is byte-identical between 0.33.6 and
  0.40.0. So the removal shipped at or before 0.33.3 and the entry followed
  late, which the crate's changelog jumping straight from 0.2.0 to 0.40.0
  explains: the whole 0.33 series was published without entries. Harmless for
  gpur, which renders the splash itself and never called `start_screen::render`,
  but it misleads the next consumer reading that changelog to decide whether an
  upgrade is safe — and it means the 0.33 series has no changelog at all. Fix
  belongs in the hjkl repo.

## 9. `--tick-ms` has no startup ceiling

**Severity: low.** `main.rs` clamps the resolved tick with `.max(MIN_TICK_MS)`
only, so `--tick-ms 99999999999` is accepted and the event loop waits ~3 years
for its first poll — gpur reads as frozen. `+`/`-` are capped (10 s); the CLI
value is not. Fix: clamp to the same 10 s ceiling at startup. Carried from both
the 0.10.1 and 0.12.0 review passes.

## 10. A re-detect whose child set changed resets device-keyed state

**Severity: low.** `CompositeBackend` (`src/backend/mod.rs`) namespaces device
ids by child index, so a re-detect (after five consecutive poll failures) whose
child set changed — a driver appearing or disappearing — renumbers every
namespaced id and restarts graph/session history. The "survives a re-detect"
guarantee holds only while the child set is unchanged. Fix: derive the id
namespace from something stable across re-detects, or document the reset.

## 11. PDH `read_array` does not retry on a growth race

**Severity: low (Windows).** `src/backend/windows.rs`: if a counter's instance
count grows between the sizing call and the fill, the second call returns
`PDH_MORE_DATA` and that counter reads as empty (all-`None` gauges) for one
poll. No overflow — just a lost tick. Fix: loop size+fill until the count
stabilises.

## 12. Two concurrent `--log` appenders can tear a record

**Severity: low.** A flushed `BufWriter` record is not one atomic write, so two
`gpur --log` processes appending to one file can interleave mid-line; the replay
reader skips the malformed line, so the cost is one lost record. Fix: take an
advisory lock around the record write, or document single-writer.

## 13. `le_int` zero-extends sub-8-byte device-tree values

**Severity: low (macOS).** `src/backend/apple.rs` decodes 1/2/4-byte `CFData`
blobs by zero-extension, so a negative value would read as a large positive;
only positive properties occur today. Fix: sign-extend.

## 14. Intel `power_w` averages over a sensor-outage window

**Severity: low.** `src/backend/intel.rs` keeps its `(µJ, at)` baseline when
`energy1_input` is transiently unreadable, so the next successful read reports
average power over the whole outage window rather than over one interval. Fix:
drop the baseline on a failed read, or bound the window.

## 15. NVML PCIe throughput units drift 2.4%

**Severity: low.** `nvml_deviceGetPcieThroughput` returns decimal KB/s while the
snapshot field and the UI label say KiB/s (`src/backend/nvidia.rs`,
`src/ui.rs`). Fix: convert to KiB/s, or relabel.

## 16. Headless sync mode walks `/proc` once per backend cursor

**Severity: negligible.** `--once`/`--json` on a mixed AMD+Intel box walks twice
per poll (one per cursor — the shared-walk dedup is lost in synchronous mode),
and the scanner worker thread stays alive through the one-shot run. A one-shot
pays a few extra ms. Fix if it ever matters: share one synchronous walk per
poll.

## 17. `first_dir` trusts sysfs readdir order

**Severity: low.** `src/backend/linux.rs` `first_dir` picks the first hwmon
child of a card; amdgpu/i915 register one per device in practice. Fix: if a card
ever exposes several, filter for the one whose `name` identifies the device.

## 18. Micro-optimisations declined as not worth the churn

From the 0.12.0 perf review's hardening list. All correct today; each is pure
waste at the margin, and none was worth the churn when the finding was closed.
Revisit individually if the relevant path shows up in a profile.

- **PCIe link maxes re-read every tick.** `src/backend/linux.rs` `pcie_link`
  does 4 sysfs reads per GPU per tick (call sites: `amd.rs`, `intel.rs`, nouveau
  in `nvidia.rs`) for values that are almost static: max gen/width never change,
  current gen/width change on negotiation only. Cache the maxes at scan; re-read
  the current pair on a slow schedule.
- **History front-drain is an O(cap) memmove per poll** once the ring is full
  (`src/app.rs` `poll_inner`, `drain(..overflow)` on four vecs). A `VecDeque`
  makes it O(1); single-digit MB/s at real widths, so only worth it if the cap
  grows with terminal width.
- **`draw_meter` pushes one `Span` per meter column per frame** (`src/ui.rs`) —
  ~2 × meter width × cards spans/frame on a wide terminal. Idiomatic ratatui;
  revisit only if frame rate becomes a goal.
- **`footer_hints()` and `driver_info()` rebuild static text every frame**
  (`src/ui.rs` header and footer). Trivial to cache; noise at any realistic
  frame rate.

---

## Settled by review

The v0.10.1 full-codebase review raised sixteen findings across correctness,
security, duplication and dead code. All of them shipped between 0.10.2 and
0.11.1 except one that was declined; each is described in `CHANGELOG.md` under
the release that carried it.

Re-verified against the tree at 0.11.1 before this list replaced the review
document — every fix is still in place, and the properties below still hold.
They are recorded so a later pass does not spend time re-deriving them.

- **Device identity is sound.** `device_keys` degrades a duplicate or absent
  `device_id` to a positional key, positional keys are never persisted, and
  `CompositeBackend` namespaces every child's ids by child index so two backends
  cannot mint the same key. The vacated-slot de-duplication is correct.
- **Process index rebasing is sound.** `CompositeBackend::processes` accumulates
  by `slots`, the high-water mark, rather than by the current poll's device
  count, and drops rows outside their own child's span. The three-child and
  middle-child-resize tests cover the off-by-one variants.
- **Mouse hit-testing cannot select an off-screen row.** `proc_visible` is
  bounded by both the drawn row count and `procs.len()`, and `proc_scroll` is
  clamped to `total - visible` before the table slice, so that slice is provably
  in range.
- **The numeric guards hold.** `gradient` handles empty and single-stop ramps,
  `parse_size` and the Apple VRAM total saturate, `le_int` rejects non-scalars,
  `windowed` handles short data, and `proc_pane_height` computes in `u32`. All
  carry tests.
- **Modal input is contained.** Control chords cannot reach the filter buffer,
  mouse events are ignored while a modal is up, and `Ctrl-C` still quits from
  filter mode.
- **The kill path holds up.** Pid identity is pinned by `(pid, start_time)`
  across the confirmation gap; pid 1 and gpur itself are refused; an absent
  executable refuses kernel threads and processes this user cannot read;
  `can_signal()` is re-checked inside `confirm_kill` rather than only when the
  dialog opens; mock and replay refuse outright, so a recording from a stranger
  is not a one-keystroke signal primitive; and `CompositeBackend::can_signal` is
  the AND of its children.
- **The PDH buffer handling is correct.** It allocates with the item type's
  alignment and sizes from the `PDH_MORE_DATA` byte count, and the `Vec`
  outlives the raw pointer derived from it.

### The 0.12.0 reviews

Both review documents from 2026-08-04 were folded into this file — their open
items became entries 9-18 above, and the documents were deleted; the full texts
are recoverable from git (`a285467` code review, `ad28ab8` perf review). Their
verdicts, so a later pass does not re-derive them:

- **The 0.12.0 code review found no open correctness findings.** Fourteen
  suspicious paths were traced and disproved (kill/pidfd identity,
  `parse_start_ticks`, braille dot orientation, `windowed` bounds, PDH sizing,
  `Instant` deltas, counter resets, layout arithmetic, the Intel
  attributed/video invariant, composite slot bookkeeping, sweep-state pruning,
  state-file atomicity, unknown-vs-zero rendering, LUID matching, mock safety,
  `--once`/replay framing), and the eight commits between the 0.12.0 review and
  its predecessor close the predecessor's eight findings one-to-one, each with a
  regression test.
- **The 0.12.0 perf review's three findings were fixed the same day**: `fe37c82`
  (paced sweep), `f5d76f0` (NVML static cache), `2d63c5e` (container-id cache).
  Its hardening items are entry 18; its coverage gaps map onto the hardware and
  profiling entries above.
- **Out of scope of both reviews:** `pkg/` templates, `assets/`, prose docs, and
  dependency internals beyond the nvml-wrapper/sysinfo spot-checks.

## Decisions taken deliberately

Recorded so they are not re-opened as findings.

- **Per-process figures may be one poll old, and that is the trade the worker
  thread buys.** `ProcScanner` publishes walks from another thread and the poll
  reads the newest finished one, so a walk that lands mid-tick is drawn on the
  following tick. Stale is not the same as wrong: a utilization is a cumulative
  counter's delta divided by the interval between the two readings, and both
  backends divide by `ProcSnapshot::at` — when the walk actually ran — rather
  than by the clock at attribution. Measuring against attribution time is the
  tempting simplification and it inflates: a walk delayed by a slow tick would
  report a card that idled through it as pegged, which
  `utilization_spans_the_two_walks_and_survives_a_poll_without_one` (in both
  `amd.rs` and `intel.rs`) pins at 100% against the correct 50%/75%.
- **The first poll of a session waits for the first walk.** There is no earlier
  reading to fall back on, and starting with an empty process table that fills
  in a tick later reads as "no processes" rather than as "not yet measured". The
  wait is bounded by `FIRST_SCAN_WAIT` so a worker that never publishes costs
  one late frame instead of a hang. Every later poll takes whatever is ready.
- **Where a walk outlasts the tick, process figures update at the walk's pace,
  not the tick's.** `SweepCursor` returns `None` on a poll with no new walk and
  the last figures are redrawn, so a `--tick-ms 50` session on a machine whose
  walk takes 70 ms refreshes the device gauges every 50 ms and the process rows
  every 70. That is the intended degradation rather than a bug to file: the
  alternative is the tick waiting on the walk, which is what this change
  removed. The utilizations stay honest across it because they are measured
  between the walks' own timestamps.
- **A poll that finds no new walk redraws the last figures rather than
  re-deriving them.** `SweepCursor` hands each walk to a backend once. Feeding
  the same walk twice would divide identical counters by a zero interval and
  report every process idle, so a walk running late would render as every card
  flickering to 0% — worst exactly when the machine is busy enough to delay it.
  The cached figures each backend keeps (`AmdBackend::media`, `IntelBuckets`)
  exist for that, not as an optimisation.
- **`--once` and `--json` keep the walk on the polling thread.** They poll twice
  a known sleep apart and report the delta, so both walks have to bracket that
  sleep; the newest-finished-walk rule would leave the sleep outside the
  interval being measured and answer about a few milliseconds of the wrong
  moment. `backend::sweep_on_poll_thread`, called from `main` when headless, is
  the switch. There is no UI to keep responsive in that mode, so the thing the
  worker exists to protect is not at stake.
- **A record field cannot gain a `null` on one side only.** `--json` and `--log`
  are written from `ProcRow` and read back into `GpuProcess`, two types that
  line up only by field name, and `#[serde(default)]` covers a missing key but
  not an explicit null. So widening a field to `Option` on the writing side
  without widening it on the reading side makes `--replay` reject whole records
  — silently skipping frames when only some carry the null, and refusing the
  file outright when all of them do. Both sides must move in the same release.
  This is what made the 0.11.0 process-metric change a minor rather than a
  patch.
- **An Intel GPU with no visible DRM clients reports `n/a`, not `0%`.** The fix
  for review finding C1 keys utilization off whether the fdinfo sweep attributed
  any client to the device, because "nobody is using it" and "I cannot see who
  is using it" are indistinguishable from unprivileged userspace — the sweep
  reads nothing for processes another user owns. On a desktop this never shows,
  since the compositor holds a client owned by the same user; on a headless box
  with a genuinely idle iGPU it does. A device whose clients _were_ read and
  summed to zero still reports an honest `0%`, so an idle GPU keeps its empty
  meter.
- **The Linux fdinfo sweeps report `Some(0)` per process, never unknown.**
  fdinfo names a memory region only when the client holds something in it, so a
  client with no `vram` or `gtt` region genuinely holds nothing there. That is a
  measurement; degrading it to `n/a` alongside the 0.11.0 change would have
  hidden real idle clients.
- **A pid that leaves the GPU but stays alive remains in the sysinfo cache**
  until it exits. The C5 fix sweeps departed pids with a second no-field
  refresh, and `remove_dead` drops the ones that have exited; nothing short of a
  `ProcessesToUpdate::All` refresh can evict a live process, and that refresh is
  the per-tick cost the narrow update set exists to avoid. The residual set is
  bounded by the machine's process table rather than by session length, which
  was the finding.
- **Block graphs lose resolution rather than direction under Mono.** Unicode has
  upper partial blocks only at ⅛ and ½, so the down-growing half of a block
  waveform is normally drawn with a complement trick — bar in the background,
  hole in the foreground — which needs two distinct colours. `ColorMode::Mono`
  has none, so that half now falls back to `▔`/`▀`/`█`, rounded to the nearest
  and rounding ties down. Three levels instead of eight is the price of a bar
  that points the way it grows; before, the complement was drawn in the default
  foreground and the whole half rendered inverted.
- **Sub-cell quantization stays hand-rolled.** Five sites across three functions
  turn a value into filled sub-units — `mini_spark`'s three glyph branches,
  `draw_waveform`'s `dots_for`, and `draw_waveform_cells` — and they quantize
  against different unit counts (4, 8, and a per-half row count) with different
  clamp floors (0 or 1, depending on whether the widget draws a baseline). A
  shared helper would take both as parameters and save only the arithmetic, so
  the review declined it. Worth revisiting only if a fourth caller appears,
  since this is the kind of arithmetic that drifts.
- **An unreadable graph sample is a glyph, not a colour.** The waveform marks it
  `·` in braille and block — a character in neither value ramp, and already this
  UI's mark for nothing-here, since `draw_meter` paints an empty non-ascii track
  with it — and the dim styling only reinforces that. Colour and `DIM` are both
  lost under `NO_COLOR`, under `TERM=dumb`, on a terminal that ignores `DIM`,
  and in any screenshot or copy-paste, so neither can carry the distinction on
  its own.
- **Ascii spells the same thing `_`, and that collides with `mini_spark`.**
  `--graphs ascii` is chosen by people whose font may not have `·` at all, which
  is the same reason they are not being handed braille. That leaves the baseline
  `_` meaning opposite things in two widgets: `mini_spark` is styled whole by
  its caller, so it cannot dim anything and spells unknown as a gap, leaving `_`
  for a measured 0; the waveform must not leave gaps — the trace has to stay
  continuous — so there `_` is unknown and a measured 0 takes the level-1 `.`.
  Each widget is unambiguous within itself, and the alternative was giving up
  the distinction in whichever widget lost the glyph, on exactly the terminals
  that can least afford to lose it.
- **`--once` keeps raw MiB** (`vram 40MiB/24560MiB`) while the TUI uses
  `human_bytes` (`14G`). The review called this drift, but the TUI is
  width-constrained and a one-shot diagnostic is not; precision is worth more
  there, and MiB matches what `nvidia-smi` and `nvtop` print. For the same
  reason `--once` spells an unreadable per-process figure `-` while the TUI
  spells it `N/A`: the columns on one whitespace-splittable line agree with each
  other, which matters more there than agreeing with the table.
- **NVIDIA `integrated` is derived from `BusType::Fpci`.** Tegra's on-SoC host
  interface is the only NVML signal available, and it could not be verified
  without a Jetson. Fail-safe: any error or older driver yields `false`, exactly
  the previous behaviour.
- **xe VRAM total comes from `physical_vram_size_bytes`**, which igt asserts is
  larger than usable VRAM because it includes reserved and stolen pages. It is
  the only figure xe publishes; overstating by the carve-out beats rendering a
  16 GB card as memory-less.
- **Intel `gtt_total_bytes` comes from `/proc/meminfo` `MemTotal`.** i915 and xe
  size their system-memory region at `totalram_pages()`, so total RAM is the
  region size rather than an invented ceiling.
- **`drm-total-*` stays unparsed.** It counts allocated-but-possibly-evicted
  pages; the memory column means resident, which is `drm-resident-*`.
- **AMD enc/dec live in a closure-captured map** rather than widening the shared
  `ClientSample`. `None` and `Some(0.0)` mean different things for a ring amdgpu
  never printed, and only one of three backends reads it.
- **The composite backend reports `name() == "multi"`** rather than a joined
  string. Widening the trait to `String` would touch every backend, and leaking
  a joined name would leak again on every re-detect. Vendor identity lives in
  `driver_info()`, and each card title carries its own device name.
- **`MIN_TICK_MS` is 50, not 100.** The two floors disagreed (CLI 50, `+` key
  100); unifying upward would have silently removed `--tick-ms 50`, a capability
  users already had.
- **The process command line is not cached** even though the container id is. A
  pid's cgroup path is ~static, so caching the container on (pid, start time) is
  safe; the command line re-derives each poll from sysinfo's already-cached
  cmdline — an in-memory join with no I/O — so an in-place `exec`, which keeps
  the pid and its start time, never leaves a stale COMMAND column. Cache it only
  if the join ever shows up in a profile.
- **`enforced_power_limit` is not cached at probe** with the other
  session-static NVML values. It changes when the user moves the power cap;
  caching it would show a stale limit until restart. One query per poll keeps
  the displayed limit live.
