# gpur backlog

Open work from the review passes, the multi-vendor detection audit and the
full-codebase review of v0.10.1, whose separate document was folded into this
one once every finding it raised had shipped. The 0.12.0 code and performance
reviews were folded in the same way: their still-open items are the entries
below, and their verdicts and decisions live in the settled-by-review and
decisions sections at the bottom. Closed items are removed rather than struck
through — what was fixed, and why, lives in `CHANGELOG.md`. Anything from a
closed item that is still a live constraint on future work is kept below, either
as a deliberate decision or in the settled-by-review list, rather than as a
task.

Roughly ordered by what is worth doing first. Nothing open here is a correctness
bug; the remainder is granularity, cost, coverage and polish. Every item that is
still open is open because it turns on a product or architecture call rather
than on effort.

No fabricated readings remain anywhere in the UI: the device gauges stopped
inventing a `0` in 0.10.2, the process table in 0.11.0, and the graph history —
the last holdout, because a waveform has no glyph for absent — in 0.11.1.

---

## Pending: the AUR publish (0.12.0 and 0.13.0)

**Everything else in both releases landed.** The GitHub release for 0.12.0 and
0.13.0 each carries all 25 assets, crates.io reports 0.13.0 as its max version,
and the Homebrew, Scoop and Alpine jobs succeeded on both tags. Only
`Publish AUR (gpur-bin)` failed on each — 0.12.0's run `30735988517` and
0.13.0's run `30873677163`, both at
`git clone ssh://aur@aur.archlinux.org/gpur-bin.git`:

> The AUR is down due to maintenance. We will be back soon.

Not a configuration fault. The same `git ls-remote` fails identically from a
developer machine with a working AUR account, while `ssh aur@aur.archlinux.org`
answers its greeter normally — the interactive shell is up and git-upload-pack
is not. Re-run attempts at the time all hit the same banner, and re-checks on
2026-08-03 and 2026-08-04 (the 0.13.0 tag run included) got the same one — so
this is an outage measured in days, not minutes.

**To finish:** once `git ls-remote ssh://aur@aur.archlinux.org/gpur-bin.git`
succeeds, `gh run rerun 30873677163 --failed` — the 0.13.0 run. Its job is
idempotent — it exits early with "nothing to push" if the PKGBUILD is already
current — and pushing the newest version supersedes the 0.12.0 state, which
never needs to land separately. Delete this section once it is green.

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

## 2. Remaining test gaps

- **Mouse kinds with no behaviour attached** — drag, middle and right button,
  and moves all fall to the `_ => None` arm. Untested because untested is what
  they are: there is nothing to assert yet.
- **The threaded sweep reaches a real card on both Linux backends.** AMD has
  `the_worker_thread_feeds_the_backend_on_real_hardware` in `amd.rs` and Intel
  gained its twin (same name, `intel.rs`) — each gives the backend a scanner
  wired the production way and asserts its own render node reaches a row. The
  other hardware tests pin the shared scanner to synchronous through `amd()` /
  `intel()`, deliberately — a test that opens a client and polls is asserting on
  that client, so the walk has to happen after the open. The Intel twin was
  written against the machine this backlog is written on but only runs where
  `GPUR_REQUIRE_INTEL` is set; it has not yet been run on real silicon.
- **One walk serving every vendor: the count half is pinned, the live half is
  not.** Both backends take `SweepCursor::default()` (`amd.rs`, `intel.rs`), the
  shared `ProcScanner`, so structurally an AMD + Intel machine takes one walk a
  tick rather than one per backend. `a_cursor_attributes_each_walk_once` shows
  two cursors over one scanner each receive the same walk, and
  `two_cursors_over_one_worker_share_each_walk` now pins the count: two cursors
  polling one worker-backed scanner cost one paced walk, not two. What still has
  no observation is the thing itself — two LIVE backends walking the shared
  scanner on an actual mixed-vendor box (this one has AMD cards only). Next time
  such a machine runs the suite with `GPUR_REQUIRE_AMD` and `GPUR_REQUIRE_INTEL`
  set, a poll of both backends asserting their process rows land together would
  close it.
- **`kill_dialog_opens_for_a_real_process_and_cancels` (tests/tui.rs) is flaky
  under parallel load.** It waits for the stub backend's `sleep 60` row and has
  timed out twice (2026-08-04) while the suite ran other tests in the same
  binary — the screen showed two rows whose COMMAND column did not name `sleep`,
  as if sysinfo enrichment of the stub's child row raced. It passes in isolation
  and in every other full run, and no change under test has been implicated.
  Uninvestigated. **Escalated 2026-08-06:** failed 3 of 3 full gate runs under
  default parallel load during the backlog work session (always as the last tui
  test), each time with the same signature (both rows' COMMAND showing the gpur
  binary path, no `sleep`), and passed every isolation and serial run (all 38
  tui tests serially, and 310/310 with `--test-threads=1`). Nothing under test
  was implicated. Still the documented workaround: run the tui binary serially
  if it bites again; a real fix needs the enrichment race investigated.
- **No before/after measurement of the responsiveness this bought.** The 4.2 ms
  over 588 pids in the old entry measured the walk itself, and the walk's code
  is unchanged — what moved is which thread runs it. That keystrokes no longer
  queue behind it follows from `App::poll` no longer walking, not from anything
  timed, and the pathological case that motivated the work (a 10k-process node
  at `--tick-ms 100`) has never been run.
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

## 5. Intel memory totals could come from the DRM query ioctl

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

## 6. Residual YAGNI

- `UiTheme::temp_ok` (`src/theme.rs`) is `pub` but used only by `temp_style` in
  the same file, unlike its peers `temp_warn` / `temp_crit`. Narrowing it alone
  would be asymmetric.
- `enum Mode { Normal }` (`src/keys.rs`) has one variant threaded through
  `Keymap<Action, Mode>`. Required by the `hjkl-keymap` API, so it cannot go,
  but no second mode is planned — filter and confirm bypass the keymap entirely.

## 7. Noted upstream, not gpur's to fix

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

## 9. Headless sync mode walks `/proc` once per backend cursor

**Severity: negligible.** `--once`/`--json` on a mixed AMD+Intel box walks twice
per poll (one per cursor — the shared-walk dedup is lost in synchronous mode),
and the scanner worker thread stays alive through the one-shot run. A one-shot
pays a few extra ms. Fix if it ever matters: share one synchronous walk per
poll.

## 11. Micro-optimisations declined as not worth the churn

From the 0.12.0 perf review's hardening list. All correct today; each is pure
waste at the margin, and none was worth the churn when the finding was closed.
Revisit individually if the relevant path shows up in a profile.

- **History front-drain is an O(cap) memmove per poll** once the ring is full
  (`src/app.rs` `poll_inner`, `drain(..overflow)` on four vecs). A `VecDeque`
  makes it O(1); single-digit MB/s at real widths, so only worth it if the cap
  grows with terminal width.
- **`draw_meter` pushes one `Span` per meter column per frame** (`src/ui.rs`) —
  ~2 × meter width × cards spans/frame on a wide terminal. Idiomatic ratatui;
  revisit only if frame rate becomes a goal.

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
  `CompositeBackend` namespaces every child's ids by backend name — the index
  only where two children share a name, which `detect()` cannot produce — so two
  backends cannot mint the same key, and the namespace survives a re-detect
  whose child set changed. The vacated-slot de-duplication is correct.
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
items became the entries above, and the documents were deleted; the full texts
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
  Its hardening items are the micro-optimisation entry above; its coverage gaps
  map onto the hardware and profiling entries above.
- **Out of scope of both reviews:** `pkg/` templates, `assets/`, prose docs, and
  dependency internals beyond the nvml-wrapper/sysinfo spot-checks.

### The 2026-08-04 code review

A full-codebase correctness review (all of `src/` plus `tests/tui.rs`, split
across two read-only passes over disjoint file sets; depth low, high-confidence
findings only). Its two findings were fixed the same day: the headless
`--once`/`--json` silent-failure path in `89a4492` (exit non-zero with the poll
error on stderr when every poll fails; smoke tests), and the scanner seq
regression in `b332275` (`next_seq()` minted under the lock, publish never
writes the counter back; stress test proven red on the old worker code). What
was traced and disproved, so a later pass does not re-derive it (these are the
sub-agents' verdicts, each claiming full-file reads; the two findings themselves
were re-traced by the orchestrator against the cited lines):

- **The kill path, device-keyed state, record shape and rendering hold up.** The
  pidfd pinning, `(pid, start_time)` identity, `(pid, drm-client-id)` fdinfo
  dedup (sound against the kernel's file-scope global client-id counter), the
  composite's slot bookkeeping, the serde record round-trip, and the PTY suite's
  expectations all cleared.
- **The one known PTY flake is test infrastructure, not product code.**
  `kill_dialog_opens_for_a_real_process_and_cancels` (see the test-gap entry
  above) is the only red seen, under parallel load only.
- **Hardening notes (correct today, fragile by convention):** per-child poll
  errors are dropped when a sibling child survives (`backend/mod.rs`) — the
  "(unavailable)" placeholders remain but the reason text is unrecoverable; the
  fdinfo dedup depends on the kernel keeping `drm_client_id` process-global; the
  pidfd identity re-read precedes `pidfd_open` by a sub-microsecond window
  (standard pidfd practice, fail-closed); holding a digit key delivers `Repeat`
  events that toggle a fold on the selected card (visible flicker, no wrong end
  state, `main.rs:334` filters only `Release`); a `start_ticks`
  unreadable-at-open / readable-at-confirm transition refuses a same-process
  confirm (fail-closed); signal-exit drops unsaved UI state (documented,
  intentional).
- **Not reviewed:** `pkg/` templates, `assets/`, prose docs — the same exclusion
  as the 0.12.0 reviews. The cleared claims above were not independently
  re-traced line by line by the orchestrator; the two findings were.

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
- **The kill-dialog PTY tests use a separate stub backend, never a signalable
  mock.** `GPUR_STUB_BACKEND=1` (`src/backend/stub.rs`) reports one real local
  process so the dialog is reachable from the harness; making the mock
  signalable instead would weaken the guard that exists to stop exactly that —
  fabricated pids must never be signalable.

---

## Codebase review 2026-08-05

The sweep's correctness pass. The tree was clean at run time, so this covered
all of `src/` and `tests/` in full; depth low (high-confidence findings only).
One finding; everything else traced held. Verdict: safe to ship as-is.

### Finding — a TUI invocation with redirected stdout writes one spurious

`--log` record (and stalls up to 2 s) before failing the terminal check —
**shipped 2026-08-06** (`8de0e69`): the `is_terminal` check now runs before
`open_log` and the first `app.poll()`, so a session that is about to bail
creates no `--log` file, writes no record, and fails immediately. Smoke test
proven red on the old code.

### Cleared

- **Scanner seq "publish regression" in synchronous mode — disproved.** The
  interleave where the worker publishes a lower seq after the sync path
  published a higher one is harmless: in synchronous mode `latest()`
  (`linux.rs:375-378`) returns the fresh `scan_here()` walk directly and
  overwrites `latest` before returning, so the cursor is always handed the walk
  it just minted; in async mode the worker is the sole producer and seqs are
  strictly monotonic (minted under the lock, `linux.rs:411-415`), so
  `SweepCursor::next`'s equality check (`linux.rs:521-528`) suffices.
  `b332275`'s fix is complete for this too.
- **Kill-path guards** — `confirm_kill` re-checks `can_signal`, pid 1, self,
  `(pid, start_time)` via a fresh one-pid refresh, `exe()` refusal, and the
  pidfd fast path with `start_ticks` re-read (`app.rs:1222-1332`). All held;
  fail-closed everywhere.
- **Mouse hit-testing** — `main.rs:433-438` bounds `clicked` against
  `proc_scroll + proc_visible`, the table slice
  `procs[proc_scroll..proc_scroll +visible]` is provably in range
  (`ui.rs:1214`), border row excluded. Held.
- **PDH buffer handling** — `read_array` (`windows.rs:432-484`): item-aligned
  `Vec`, `div_ceil` sizing, bounded retry, `CStatus` filter. Held.
- **LUID matching** — `read_array` lowercases instance names (`windows.rs:476`)
  and DXGI keys are built `{:08x}` lowercase (`windows.rs:397-400`); both halves
  agree.
- **`windowed`/`gradient`/`proc_pane_height`/`cards_that_fit`** — padded-left
  `None`, empty/single-stop ramps, u32/u64 arithmetic — held, tests present.
- **Composite slot bookkeeping, vacated-slot id dedup, process rebasing by
  `slots` high-water mark** (`backend/mod.rs:388-415`, `427-443`) — held against
  the tri-vendor and middle-child-resize tests.
- **AMD/Intel counter-delta paths** — `ns_delta_util` divides by the walk's
  `at`, saturating subtractions, per-client delta maps pruned to `sweep.seen`,
  energy baseline dropped on unreadable counter (`intel.rs:315-334`). Held.
- **Mock/replay/stub signalability** — `can_signal` false for mock and replay,
  AND-composition in `CompositeBackend`, stub only via `GPUR_STUB_BACKEND`.
- **History cap/eviction/persisted folds** — cap =
  `max(history_len, history_need+8)`, `evict_absent_devices` bounded by
  `MAX_ABSENT_DEVICES`, positional keys never persisted. Held.

### Hardening (correct today, fragile — not defects)

- **`amd.rs:297-300`** — `temp1_crit` is read without the `> 0` filter its
  siblings `power1_max`/`power1_cap` use (`amd.rs:337-340`, `intel.rs:181-183`).
  A chip that ever published a 0 would make the "thermal" throttle label
  unconditional (any `t >= -3`). Add `.filter(|v| *v > 0)` for symmetry.
- **`main.rs:100` vs `104`** — the first `app.poll()` (up to `FIRST_SCAN_WAIT`
  on Linux) ran before the terminal was known to exist; the check moved ahead of
  `open_log` and the poll in `8de0e69`, so this half is closed with the finding
  above.
- **`nvidia.rs:258-265`** — a pid in both `running_compute_processes` and
  `running_graphics_processes` gets the graphics entry wholesale (kind _and_
  memory); if NVML ever reports different per-context memory, the compute figure
  is dropped. No concrete input demonstrated; the two lists' memory figures are
  expected to agree today.
- **`amd.rs:284`** — `vram.saturating_add(...)` saturates, but the sum of a
  client's two regions across clients in `sweep_clients` (`linux.rs:206`) is a
  plain `u64 +=`; only absurd multi-exabyte fdinfo totals could wrap.
  Parse-level saturation covers the realistic range.

### Coverage

Walked in full, line by line: `src/` (all files, incl. unit/hardware tests) plus
`tests/smoke.rs`, `tests/tui.rs`. Gaps, honestly named: the verification gate
was not run (read-only pass; the orchestrator runs it); the `#[cfg(windows)]`
PDH/DXGI and `#[cfg(target_os = "macos")]` IOKit modules and every NVML call are
not compiled or executed on this host — reviewed by reading only; the amd/intel
hardware-test modules skip without a GPU; dependency internals (nvml-wrapper,
hjkl-\* crates, sysinfo) were not inspected beyond the call sites.

## Codebase audit 2026-08-05

The sweep's security pass. Tree clean, so full-codebase; depth low. Worked from
the backlog first; every settled-by-review item was re-verified against the tree
rather than re-reported. **2 findings, both low, both on the `--replay`
untrusted-recording surface. Zero critical/high/medium.** The kill path, PDH
buffer handling, device identity and every kernel/vendor text parser hold up
exactly as the backlog records them. Overall risk: low.

### 1. LOW — Terminal escape injection through the `--once` headless path via a

crafted replay recording — **shipped 2026-08-06** (`9c97d57`): `snapshot()` now
strips control characters from recorded GPU names and commands with the same
`char::is_control()` filter ratatui applies; end-to-end smoke test proven red on
the old code.

### 2. LOW — Unbounded allocation on replay input — **shipped 2026-08-06**

(`dc927e6`): the replay reader now reads lines in bounded chunks and drops any
line past 8 MiB like a malformed one; unit test at a small cap plus an e2e test
at the real cap, proven red on the unbounded reader.

### Cleared

- **Kill path a signal primitive from a crafted recording?** No —
  `ReplayBackend::can_signal` is `false` (replay.rs:118-120), mock likewise
  (mock.rs:162-164), the composite is the AND of its children (mod.rs:472-474),
  `confirm_kill` re-checks `can_signal()` after taking the pending kill
  (app.rs:1237-1243), pid 1 and self are refused (app.rs:1244-1251), the
  single-pid refresh demands an identical `start_time` (app.rs:1261-1274),
  `exe()` must resolve (app.rs:1278-1283), and the Linux fast path pins identity
  with `pidfd_open` + a re-read of stat field 22 before `pidfd_send_signal`
  (app.rs:1291-1332). The failure re-detect re-opens the same `BackendSource`,
  so a replay can never be promoted to live hardware mid-session
  (app.rs:852-861).
- **TUI escape injection from replay/vendor strings?** Disproved — see finding
  1; ratatui-core 0.1.2 `set_stringn` filters `char::is_control()`
  (buffer.rs:351), covering ESC and the C1 range.
- **PDH buffer overrun?** Disproved — `Vec` sized from PDH's own `PDH_MORE_DATA`
  byte count with item alignment, bounded retry, `CStatus` filter
  (windows.rs:438-484).
- **Panic on malformed kernel/vendor text?** No — every sysfs/fdinfo/stat parse
  degrades via `.ok()?`/`unwrap_or`; all `unwrap`/`panic!`/`expect` in the two
  big backends are inside `#[cfg(test)]`; `parse_size` saturates
  (linux.rs:640-649), `le_int` rejects non-scalars (apple.rs:150-166), NVML's
  sentinel temp threshold is range-filtered (nvidia.rs:637-642).
- **`--log` leaking argv/command lines?** Deliberate and guarded — 0600 on unix
  (main.rs:157-166), records flock-serialized (app.rs:950-956).
- **`state.json` torn writes / cross-instance clobber?** No — temp file + atomic
  rename, pid-suffixed temp name, 0600 (app.rs:465-516).
- **Mock pids landing on live processes?** No — mock pids start at 1_000_000
  (above Linux pid_max) and the backend refuses to signal anyway.
- **fdinfo dedup / attribution confusion?** No — keyed on `(pid, drm-client-id)`
  with the kernel's process-global counter (linux.rs:147, 198),
  attribute-by-pdev+driver (linux.rs:542-547).
- **Arithmetic overflow in UI layout/history?** No — `u32`/`u64` widening and
  saturating ops throughout, each with tests.

### Hardening (correct today, fragile — not vulnerabilities)

- **`lock_log` is a blocking `flock` on the render thread** (app.rs:524-527). A
  peer `gpur --log` appender SIGSTOPped while holding the lock stalls this
  process's every poll until it resumes. Window is microseconds in practice; a
  non-blocking retry-with-status would keep the monitor alive.
- **`write_private` opens with `O_CREAT` (no `O_EXCL`) and follows symlinks**
  (app.rs:506-516). A pre-planted `state.json.<pid>.tmp` symlink in the cache
  dir would be truncated/overwritten. Unreachable in practice (cache dir is the
  user's own, 0700 by default, pid suffix unpredictable) but a hostile
  local-user-with-cache-write can race it; `O_EXCL`+retry is the standard
  hardening.
- **Replay frames with huge process lists** are sorted/refreshed per poll
  (app.rs:988-1054) — bounded by the file, self-chosen input; same class as
  finding 2.
- **Signal-teardown thread** calls `ratatui::restore()`/`restore_extras()` while
  the main thread may be mid-draw (main.rs:185-192) — a cosmetic terminal-state
  race at exit, best-effort by design.
- **`StubBackend` spawns `sleep 60`** (stub.rs:24); a SIGKILLed gpur leaks the
  child for up to 60 s. Test-hook only, behind `GPUR_STUB_BACKEND`.

### Coverage

Walked, line by line: `replay.rs`, `cli.rs`, `config.rs`, `keys.rs`, `main.rs`,
`app.rs` (full), `backend/mod.rs`, `linux.rs` (full), `nvidia.rs` (full),
`windows.rs` (full), `apple.rs` (full), `mock.rs`, `stub.rs`, `ui.rs` (full),
`theme.rs`, `splash.rs`. Attack surfaces mapped: replay parsing, kill path,
CLI/env (`--mock`/`--replay`/`--log`/`--once`/`--json`,
`GPUR_STUB_BACKEND`/`GPUR_MOCK_FAIL`, `NO_COLOR`/`TERM`), config + theme files,
key handling, sysfs/fdinfo/NVML/PDH/IOKit parsing, all `unsafe` blocks (flock,
pidfd, PDH, DXGI, IOKit, signal handler, sysctl). Classes walked: injection,
memory/resource, crypto (none present — no secrets, no RNG), authN/Z, data
integrity, error handling, concurrency.

Gaps, honestly named: (a) nothing behind
`#[cfg(windows)]`/`cfg(target_os = "macos")` was compiled on this Linux host —
PDH/DXGI and IOKit correctness rests on reading plus the cross-platform unit
tests, the backlog's own standing limitation; (b) `amd.rs`/`intel.rs` test
bodies were covered by grep for panic/unsafe patterns rather than full reads;
(c) hjkl-\* dependency internals were not audited beyond the project's own
spot-checks; (d) the CI gate was not run (read-only pass; the tree was left
clean).

**Summary:** 2 findings, both low, both on the `--replay` untrusted-recording
surface: terminal escape injection in the `--once` printer (verified end-to-end
— the TUI sanitizes, the headless path does not), and unbounded per-line
allocation in the replay reader. **Both shipped 2026-08-06** — `9c97d57`
(`char::is_control()` filter in the headless printers, mirroring the TUI) and
`dc927e6` (8 MiB per-line cap, oversized lines dropped). Everything else can
ship as-is.

## Codebase tidy 2026-08-05

The sweep's cleanup pass. Tree clean, so full-codebase; behavior-preserving
cleanups only. All files read in full; `cargo check --all-features --locked` is
clean, so nothing rustc flags as dead. Four cleanups survive verification; no
dead code in the tree. Nothing here blocks shipping.

### 1. `src/backend/linux.rs:784-801` — duplicated PCIe link-pair reader —

**shipped 2026-08-06** (`f0f7a7a`): `pcie_current_link` and `pcie_max_link` now
call one shared `link_pair(dev, speed_file, width_file)`; the doc comments keep
their distinct semantics.

### 2. `src/backend/amd.rs:897-998` + `src/backend/intel.rs:884-985` —

byte-identical hardware-test helpers — **shipped 2026-08-06** (`719184b`):
`open_render_node`, the `RENDER_NODES` mutex, `Held` and `hold_clients` moved
into `linux::testing` with thin device-typed wrappers at each call site; the
shared mutex now serializes both backends' render-node tests against each other,
which is safe and was already the per-module pattern. Net −24 lines.

### 3. `src/main.rs:115-124` — duplicated setup-failure teardown-and-bail

blocks — **shipped 2026-08-06** (`fb2ab32`): both post-`try_init` failure arms
now return `Err(fail_setup(e, "…"))`, putting the "restore before bail"
invariant in one place (`fail_setup` takes `&'static str` because anyhow's
context requires it).

### 4. `src/backend/mod.rs:352-356` — per-poll `name_counts` HashMap for a child

set that provably never changes

`poll()` rebuilds `name_counts` from `self.children` on every call, but a
composite's child set is fixed for its life (a re-detect builds a new composite
— the `driver: OnceLock` comment at 316-319 says exactly this). **Action:**
compute it once in `CompositeBackend::new` and store it as a field, mirroring
the `driver` OnceLock precedent. Behavior identical; the per-poll allocation
disappears. Low severity — a ≤4-entry HashMap per poll — but the same shape as
the `driver_info` caching deliberately done, so it reads as an oversight rather
than a decision. Optional: if it sits inside the spirit of the declined "pure
waste at the margin" category, leave it. **Left 2026-08-06** under that optional
note — a ≤4-entry HashMap per poll is squarely the declined "pure waste at the
margin" class, and the review itself offered leaving it.

### Dropped after verification

- `app.rs:942-944` — `write_log`'s early return when `--log` is off looks
  subsumed by the `let Some(w) = … else` at 946, but it is not: the first guard
  skips `self.record()`'s full process-table serialization on every poll.
  Removing it would regress per-poll cost. Intentional early-out.
- `amd.rs`/`intel.rs` `attribute` building `Vec<SweepDevice>` (also repeated in
  two hardware tests) — a shared helper needs accessor closures and makes the
  call sites uglier than the 5-line blocks; the amd/intel twin structure is a
  documented deliberate parallel.
- `nvidia.rs` `used_bytes`/`mem_bytes` split — documented deliberate
  (testability).
- `windows.rs` `Adapter::vendor_id` stored but only read at probe — it is read
  (by the filter closure); not dead.
- `smoke.rs`/`tui.rs`/`app.rs`-tests/`linux::testing` Sandbox copies — comments
  document the deliberate pattern (integration tests are another crate).

### Coverage

Read in full: `src/` (all files), `tests/smoke.rs`, `tests/tui.rs`,
`docs/backlog.md` (declined items noted). Not read (out of scope): `pkg/`,
`assets/`, prose docs. Verification: `cargo check --all-features --locked`
clean; `rg` confirms no rustc-detectable dead code and the cited call sites.

## Codebase perf 2026-08-05

The sweep's performance pass. Tree clean, so full-codebase. All of `src/` read;
the backlog's open items (headless double-walk, history front-drain,
`draw_meter` spans, command-line-not-cached, `enforced_power_limit`-not-cached,
paced sweep, NVML static cache, container cache) were verified in the code and
excluded. Verdict: no significant new hot-path problems — the previous perf pass
and the recorded decisions cover the real costs; findings 2 and 1 are the ones
worth taking in the next cleanup.

### 1. `rebuild_proc_view` clones every row and re-sorts the whole table every

poll — **shipped 2026-08-06** (`1cdca59`): `App.procs` is now `Vec<usize>`
indices into `all_procs`; the filter and per-tick sort run over indices, and
`draw_processes` dereferences the view at draw time. The per-row String clones
disappear; sort order, filter semantics and cursor-keeping are byte-identical,
pinned by a new unit test over a non-identity index mapping.

### 2. Linux vendor backends call `driver_info()` per frame — a fresh `uname()`

syscall plus ~5 allocations per frame for a string constant for the backend's
life — **shipped 2026-08-06** (`ab86308`): each sysfs backend (amd, intel,
nouveau) caches its joined driver line in a `OnceLock`, the NVML side
pre-formats `"driver {d}"` at probe, and `MergedNvidiaBackend` caches its join —
mirroring `CompositeBackend::driver`. Identical string, zero behavior change.

### 3. Active process filter re-lowercases every row's command/user/container

every poll

**`src/app.rs:1111-1123`** — with a non-empty filter, the closure calls
`p.command.to_lowercase()`, `p.user.to_lowercase()`, `p.pid.to_string()` and
`p.container…to_lowercase()` per row — full-length String allocations per tick
against text that is usually byte-identical (the command line is re-joined from
sysinfo's cached cmdline per poll by decision). Empty filter short-circuits
(`app.rs:1116`), so filtered sessions only. At 500 rows ~2-3k allocations per
tick; 10k rows ~30-40k/tick. **Fix:** cache a pre-lowercased search key per row
identity (keyed on the same `(pid, start_time)` the container cache uses,
`app.rs:634`), or store `command_lower`/`user_lower` fields on `ProcRow`
computed when the row is built, so the per-poll filter pass only compares.
Either keeps the "fresh after exec" property for the visible column. **Left
2026-08-06:** both proposed shapes are defective on inspection — a
`command_lower` field on `ProcRow` saves nothing, because the rows are rebuilt
from scratch every poll (the lowercase would be recomputed anyway); a
`(pid, start_time)`-keyed cache reintroduces exactly the exec-staleness the
recorded "command line is not cached" decision exists to prevent (an in-place
exec keeps both keys, so the filter key would go stale while the visible column
stays fresh). Below the review's own top-wins line; revisit only if a filtered
session shows up in a profile.

### 4. NVML re-queries `num_fans()` every poll though the fan count is a fixed

device property — **shipped 2026-08-06** (`9bd30cf`): the count is read once at
probe per device and stored beside the other probe-cached vecs; only the per-fan
speeds stay live per poll.

### 5. Windows PDH re-parses and re-lowercases every counter instance name on

every poll

**`src/backend/windows.rs:432-484`** — `read_array` reads the counter into a
fresh `Vec<PDH_FMT_COUNTERVALUE_ITEM_W>` and calls
`item.szName.to_string()? .to_lowercase()` per instance, five times per poll
(engine counter at `windows.rs:249`, plus the two adapter-memory and two
process-memory counters, `:284-290`). Each name is then parsed again —
`luid_prefix` splits into a `Vec<&str>` and `format!`s a new key (`:52-58`),
`luid_and_engtype` and `pid_prefix` re-split and re-format (`:32-46`). On a busy
box the `GPU Engine(*)` wildcard alone has hundreds of instances, so a few
thousand small allocations per poll — with no live-value reason for the names to
be re-parsed. **Fix:** parse the LUID/pid/engine once per instance into a
borrowed view and derive the keys in one pass; drop the per-instance
`to_lowercase` unless a mixed-case instance is actually observed. **Unverifiable
here** — Windows-only; the instance count and parse cost cannot be profiled on
this machine, only the shape of the per-poll work established by reading.

### Coverage

Traced (frequency and size established): the full per-tick path — `run` loop
(`main.rs:327-467`, one `terminal.draw` per tick plus per input event, one
`poll` per tick), `poll_inner` (`app.rs:824-916`: `device_keys`, per-device
history push + O(cap) drain [backlog item 11], `refresh_processes`,
`write_log`), `refresh_processes` (`app.rs:963-1061`), `rebuild_proc_view`; the
per-frame draw (`ui.rs`: header, `draw_gpus` O(n) stack math, per-card meters
[backlog item 11], waveform O(cols×rows) with O(1) `windowed`, process-table
visible-slice cells); the per-poll backend reads — amd (~22 sysfs
reads/device/poll, sweep attribution per 200 ms walk), intel (similar shape),
nvidia (~17 NVML round trips/device/poll + 3 process queries), windows PDH,
apple IOKit re-enumeration, mock/stub/replay. The /proc walk cost itself is the
code's own measured 4.2 ms/588 pids, paced by `MIN_WALK_INTERVAL` and
worker-threaded — settled by the recorded design, not re-litigated.

Traced and deliberately excluded (recorded in the backlog or settled by
decision): history front-drain, `draw_meter` spans, command-line-not-cached,
`enforced_power_limit`-not-cached, paced sweep, NVML static cache, container
cache, headless double-walk, the per-tick `evict_departed_processes`/`proc_text`
sweeps.

Not settled without profiling: NVML round-trip latency per call, actual PDH
instance counts on a busy Windows box, per-frame render time, and the walk's
cost at the 10k-process pathological scale. All findings are stated at their
traceable frequencies; none claims a measured microsecond figure.

## Codebase review 2026-08-06

The sweep's correctness pass, second run over the same tree. Tree clean, so this
covered all of `src/` and `tests/` in full again; depth low (high-confidence
findings only). **The standing 08-05 finding — the spurious `--log` record
written by `app.poll()` at `src/main.rs:100` before the `is_terminal` guard at
`:104` — was re-verified against the current tree and still holds exactly as
recorded** (see the 08-05 section above): `poll()` → `poll_inner(true)`
(`app.rs:813-815`, `:824`), a successful poll appends via `write_log`
(`app.rs:913-915`, `:940-961`). Not re-reported.

Two new findings, both low, neither a regression of anything fixed since the
last pass. Everything else traced held.

### Finding 1 — Windows: the GPU% gauge sums per-process and adapter-aggregate

PDH engine instances, so it reads roughly double the real utilization

**Severity: low (Windows only, one gauge, clamped at 100).**
`src/backend/windows.rs:253` —
`*engine.entry((luid.clone(), eng.clone())).or_default() += v;` sums **every**
`GPU Engine` instance that parses to a `(luid, engtype)` pair, pid-scoped and
adapter-scoped alike. The WDDM counter publishes both forms per engine —
`pid_<pid>_luid_…_engtype_<type>` for each process and a
`luid_…_phys_…_eng_…_engtype_<type>` aggregate — and the aggregate already
contains everything the pid instances sum to. `util_by_luid` then takes the max
over engines of the inflated sum (`:274-275`, clamped at `:355`), so a GPU
genuinely 40% busy renders ~80% and anything past ~50% renders a confident 100%.
The per-process column is unaffected — it is built only from pid instances
(`:254-260`).

The comment at `:243-244` says the map is "summed % **across processes**"; the
implementation sums all instances instead, so intent and code disagree even
before the counter shape is consulted.

```
Repro: on a Windows box, one engine whose adapter-aggregate instance reads 50%
       beside pid instances summing to 50%:
       engine[(luid, "3d")] = 50 + 50 = 100  ->  adapter gauge shows 100%
Expect: the adapter gauge reads ~50% (the aggregate, or the pid sum — not both)
Actual: the sum of both, ~100% (clamped)
```

Unverifiable on this host — the `#[cfg(windows)]` module is not compiled here,
and the claim rests on the instance shape of `\GPU Engine(*)`, which the unit
tests never exercise (their fixtures are pid-scoped only, `windows.rs:491`). A
Windows box running the binary against a known load settles it in minutes.
**Fix, if confirmed:** skip instances without a `pid_` prefix in the `engine`
loop (the aggregate then comes out of the pid sum, matching the comment), or
take the adapter gauge from the non-pid instances alone. **Deferred 2026-08-06:
** the fix lives on the `#[cfg(windows)]` poll path, which is neither compiled
nor executable on a Linux host — shipping it would be unverified platform code
that CI's other runners would have to catch. Needs a Windows box running the
binary against a known load to confirm the premise and validate the change.

### Finding 2 — headless `--once`/`--json` silently report the priming poll when

only the final poll fails — **shipped 2026-08-06** (`ce64036`): `snapshot()` now
bails whenever `poll_error` is set after the final poll, not only when `gpus` is
also empty; smoke test with `GPUR_MOCK_FAIL=2` proven red on the old code.

### Cleared

Suspects raised and disproved this pass, so the next review does not re-tread
them:

- **Process-pane click/scroll bounds after a filter shrinks the table** — a
  click can land past the new row count, but the next `draw_processes` re-clamps
  `proc_sel` and `proc_scroll` before slicing (`ui.rs:1132-1138`), and
  `visible == 0` yields the empty slice `[x..x]`, never an out-of-range panic.
  Held.
- **Sort tie-break not following the arrow** — `rebuild_proc_view`'s
  `then(a.pid.cmp(&b.pid))` (`app.rs:1150`) orders equal-key rows by pid
  ascending in both directions. Deterministic and consistent with the record
  sort (`app.rs:1049-1054`); a display nicety, not a correctness defect.
- **Ctrl-C in the kill-confirm dialog cancels rather than quits** — the modal
  contract is "anything else cancels" (`main.rs:372-378`); deliberate and
  covered by the modal-input containment tests. Held.
- **Mock pids (1_000_000..1_000_047) colliding with a real high-pid process** —
  mock rows pre-supply the host columns and `can_signal()` is false
  (`mock.rs:162-164`), so nothing can act on a collided pid. Held.
- **First-poll `FIRST_SCAN_WAIT` (2 s) blocking the terminal check** — that is
  the standing finding's second half (08-05 hardening item 2), re-verified, not
  new.
- **Restored folds for never-present devices** — persist as "gone" entries
  capped by `MAX_FOLDS_PERSISTED` without ever affecting the UI; bounded and
  harmless.
- **Composite slots / device keys / process rebasing**
  (`backend/mod.rs:388-443`), **seq pacing and `SweepCursor`**
  (`linux.rs:375-404`, `411-415`, `521-528`), **kill-path guards**
  (`app.rs:1222-1367`), **PDH buffer sizing and LUID matching**
  (`windows.rs:432-484`, `397-400`), **AMD/Intel counter-delta paths** — all
  re-read against the 08-05 cleared list and held.
- **Arithmetic bounds** — `stacked_height` / `cards_that_fit` /
  `proc_pane_height` in u32, `parse_size` saturation, `gradient` degenerate
  ramps, `windowed` padding, `le_int` sign extension — held with their tests.
- **Production `expect`s** — only `keys.rs:172,176` on the run path, both over
  compile-time constant chords; unreachable without a static table edit.

### Hardening (correct today, fragile — not defects)

- **`keys.rs:168-179`** — `default_keymap` panics via
  `.expect("static chord parses")` if a future `BINDS`/`DIGITS` edit ever
  introduces an unparseable chord; today's table is compile-time constant, so
  the panic cannot fire. The only live panicking path in `src/`; a
  fail-fast-at-startup is arguably the right behaviour anyway.
- **`windows.rs:253`** — even if finding 1's premise fails (no adapter-
  aggregate engine instances), the `engine` map's documented contract ("summed %
  across processes") is enforced by nothing; a future counter-shape change
  silently changes the gauge. A `pid_`-prefix filter makes the code match its
  comment in either world.

### Coverage

Walked in full, line by line: `src/` (all files, incl. unit and hardware-test
modules) plus `tests/smoke.rs` and `tests/tui.rs`. Gaps, honestly named: the
verification gate was not run (read-only pass per this sweep's constraints; the
orchestrator runs it); nothing behind `#[cfg(windows)]` (the PDH/DXGI `win`
module — finding 1 rests on reading it, not executing it) or
`cfg(target_os = "macos")` (the IOKit `macos` module) is compiled on this Linux
host, and every NVML call is unexecuted — reviewed by reading only; the
amd/intel hardware-test modules skip without a GPU; dependency internals
(hjkl-\*, ratatui, sysinfo, nvml-wrapper, portable-pty) were not inspected
beyond the call sites; `pkg/`, `assets/` and prose docs were out of scope, as in
the 08-05 pass.

## Codebase audit 2026-08-06

The sweep's security pass, run over the same tree the 08-05 audit and the 08-06
review covered; `git status` shows only this backlog modified, and the
orchestrator verified the source tree is unchanged since 08-05 (only
`.github/workflows/ci.yml` moved). Tree clean, so full-codebase; depth low
(high-confidence findings only). Worked from the backlog: the two standing 08-05
audit findings were re-verified against the current tree and hold exactly as
recorded — terminal escape injection through `--once` via a crafted replay
recording (`src/main.rs:249-299`, the sink fed verbatim from the recording at
`src/app.rs:1010-1014`) and unbounded per-line allocation in the replay reader
(`src/backend/replay.rs:37`, `:54`) — so they are not re-reported here (see the
08-05 section above). The two 08-06 review findings (Windows PDH adapter-gauge
double-counting, `src/backend/windows.rs:253`; headless `--once` priming-poll
silent success, `src/main.rs:227-231`) were re-verified and also hold as
recorded. **Zero new findings at depth low** — the Cleared list below is the
trail.

### Findings

None. Every candidate raised this pass died at a guard, a parse or a caller
during tracing; the Cleared list names each one and the step that killed it.

### Cleared

Suspects walked with fresh eyes this pass (re-verifying several of the earlier
passes' cleared items, plus new angles on the surfaces the task named), each
disproved by tracing, not by assertion:

- **PDH `read_array` item count vs buffer capacity** (`windows.rs:432-484`) —
  the second `PdhGetFormattedCounterArrayW` call writes `count` items into
  `size` bytes, and success means they fit; `Vec::with_capacity(n)` with
  `n = size.div_ceil(sizeof(Item))` covers the first sizing, so the unsafe
  `items.add(i)` for `i < count` stays inside the allocation; growth between
  sizing and fill returns `PDH_MORE_DATA` and retries, bounded at 8. Held.
- **Kill-path TOCTOU in the `kill_with` fallback** (`app.rs:1333-1366`) — the
  seconds-resolution `start_time` re-check (fresh one-pid refresh) immediately
  precedes the signal; on Linux the pidfd fast path (field-22 re-read at pin
  time, `app.rs:1293-1331`) is attempted first and only its refusal reaches the
  fallback, matching the 08-04 review's cleared "standard pidfd practice,
  fail-closed" item. A same-second reuse inside the fallback's microsecond
  window is the platform's best available, not a regression.
- **Replay process rows vs frame size** — `gpu_index >= frame` rows are dropped
  (`replay.rs:98-104`), the composite drops out-of-span rows
  (`backend/mod.rs:435-437`), and the TUI prints `gpu_index` as text only, never
  indexing with it, so a crafted recording cannot point a row at another card.
  Held.
- **`parse_fdinfo` engine/cycles pairing** (`linux.rs:587-626`) —
  `drm-engine-capacity-*` is skipped, `drm-total-cycles-*` and `drm-cycles-*`
  fill distinct fields of one entry in either line order, a failing
  `drm-client-id` parse drops the whole client (never a fabricated id), and
  `parse_size` saturates. Held.
- **Worker/sync walk timestamp monotonicity** — a synchronous `scan_here` and
  the worker both stamp `Instant::now()` at walk end; in sync mode the worker
  runs exactly one walk (the initial `wanted: true`), started at scanner init
  during probe and finished before any headless poll, so `ns_delta_util`'s
  `duration_since` never sees a regressed `at`. Held.
- **Scanner concurrency** (`linux.rs:293-454`) — seq is minted under the lock by
  both producers and publish never writes the counter back; a poisoned mutex is
  recovered via `into_inner`; the `synchronous` flag is set once before any poll
  reads it. Held.
- **`--json` sink from a crafted recording** — `serde_json::to_string_pretty`
  escapes control characters, so the same payload that reaches `--once` raw
  arrives at `--json` escaped; that asymmetry is exactly the 08-05 finding's
  scope, and the json half stays safe. Held.
- **Env-var hooks** — `GPUR_STUB_BACKEND` is checked after mock/replay
  (`backend/mod.rs:535-549`), so neither can be shadowed, and it spawns only
  `sleep 60`, killed on drop; `GPUR_MOCK_FAIL` parses a u64 and a non-numeric
  value disables the hook; `NO_COLOR`/`TERM`/`COLORTERM` are string-compared;
  XDG vars feed hjkl-config. Held.
- **No crypto, no RNG, no secret handling** — nothing exists in these classes to
  audit; the only `Command::new` in production is the stub's `sleep 60` (already
  a recorded hardening item).
- **Mock pids and signalability** — `PID_BASE` (1_000_000) sits above Linux
  pid_max; mock and replay return `can_signal() == false`; the composite ANDs
  its children; `confirm_kill` re-checks `can_signal()` after a failed-poll
  backend swap, and the failure re-detect re-opens the same `BackendSource`.
  Held.
- **state.json write path** (`app.rs:465-516`) — temp file + atomic rename,
  pid-suffixed temp name, 0600; the pre-existing `O_CREAT`-without-`O_EXCL`
  symlink note remains the recorded hardening item, unchanged. Held.
- **`--replay` re-serialization** — a recording replayed under `--log`/`--json`
  is re-encoded into the same record shape; no new sink appears (log file 0600,
  json escaped). Held.

### Hardening (correct today, fragile — not vulnerabilities)

- **`open_log` runs before the `is_terminal` check** (`main.rs:73-76` vs
  `:104`): closed with the 08-05 finding by `8de0e69` — the check moved ahead of
  both `open_log` and `app.poll()`, so no file is created and no record lands
  for a session that never ran.
- **`history_len` from `config.toml` is not clamped** — **shipped 2026-08-06**
  (`1abae14`): `App::new` now caps it at `MAX_HISTORY_LEN` (100_000) the way
  `tick_ms` is clamped at startup, so a typo'd config value cannot grow the four
  history vectors without bound; unit test proven red with the clamp removed.

### Coverage

Walked in full, line by line: `src/` (all files, incl. unit and hardware-test
modules) plus `tests/smoke.rs` and `tests/tui.rs`. Gaps, honestly named — the
same standing ones as the two earlier passes: the verification gate was not run
(read-only pass per this sweep's constraints; the orchestrator runs it); nothing
behind `#[cfg(windows)]` (PDH/DXGI) or `#[cfg(target_os = "macos")]` (IOKit) is
compiled or executed on this Linux host — reviewed by reading only; every NVML
call is unexecuted; the amd/intel hardware-test modules skip without a GPU;
dependency internals (hjkl-\*, ratatui, sysinfo, nvml-wrapper, portable-pty,
signal-hook) were not inspected beyond the call sites; `pkg/`, `assets/` and
prose docs were out of scope, as before.

**Summary:** 0 new findings at depth low. The four standing low findings were
re-verified against the tree at the time of the pass and all held: two from the
08-05 audit (replay→`--once` escape injection; unbounded replay line allocation)
and two from the 08-06 review (PDH GPU% double-count; headless priming-poll
silent success). **Three of the four shipped 2026-08-06** — `9c97d57`,
`dc927e6`, `ce64036` — leaving only the Windows-only PDH GPU% double-count,
deferred pending a Windows box (see its finding above). Overall risk: low —
unchanged since 08-05.

## Codebase tidy 2026-08-06

The sweep's cleanup pass, second run over the same tree. Tree clean (only
`docs/backlog.md` modified), so full-codebase; behavior-preserving cleanups
only. All of `src/` and `tests/` read in full again;
`cargo check --all-features --locked` is clean, so nothing rustc flags as dead.
The four standing 08-05 cleanups were re-verified against the current tree and
all hold exactly as recorded (see the 08-05 section above): the duplicated PCIe
link-pair reader (`linux.rs:784-801`), the byte-identical amd/intel
hardware-test helpers (`amd.rs:897-998` / `intel.rs:884-985`), the duplicated
setup-failure teardown blocks (`main.rs:115-124`), and the per-poll
`name_counts` HashMap (`backend/mod.rs:352-356`). Not re-reported.

Four new cleanups survive verification, all small; nothing here blocks shipping.

### 1. `src/backend/nvidia.rs:149-232` — the cached-at-probe-with-live-fallback

pattern repeated five times in `poll` — **shipped 2026-08-06** (`6e8db99`): a
`cached(&slot, i)` helper now serves the two plain cache reads and all five
cached-plus-fallback sites, fallback queries intact.

### 2. `src/backend/linux.rs` (new helpers) + `amd.rs:333`, `intel.rs:179`,

`nvidia.rs:434` — hwmon temperature and power-limit readers duplicated across
the three Linux backends — **shipped 2026-08-06** (`571ef55`):
`linux::hwmon_temp_c` and `linux::hwmon_power_limit_w` sit beside `fan_pct`;
amd's `power1_cap` reader stays bespoke (it reads the right attribute for
amdgpu).

### 3. `src/main.rs:240-243` + `:284-287`, `src/ui.rs:1116-1119` — the

bytes→"{}MiB" formatter triplicated — **shipped 2026-08-06** (`0c3c659`): one
`pub(crate) ui::mib` serves the TUI table and both `--once` closures; the
`n/a`/`-`/`N/A` fallbacks stay at their documented homes.

### 4. `src/splash.rs:104-105` — `render` re-scans the art every splash frame

when the same numbers are already compile-time constants — **shipped
2026-08-06** (`b39bf7f`): `render` reads `ART_BOUNDS` (both fit u8 per the const
asserts); `art_dims` stays alive under `#[cfg(test)]` for the drift test.

### Dropped after verification

- `cli.rs:89-100` — the five non-nushell `generate` arms differ only in the
  `Shell` variant. Table-ifying needs a variant→`Shell` mapping to live
  somewhere anyway, and the match is the idiomatic form; net same line count.
  Held.
- `app.rs:942-946` — `write_log`'s second guard
  (`let Some(w) = self.log.as_mut() else { return }`) is provably always-Some
  after the `is_none` guard — only `record()` runs between, taking `&self` — so
  its else-arm is unreachable. But 08-05 already walked this exact code and kept
  the first guard for the `record()` cost; collapsing to one guard is a
  one-dead-branch diff with no behavior change. Held.
- `ui.rs:334` `pct_or_na` vs `:1109` `proc_pct` — same shape as finding 3
  (`{v:>3.0}%` formatter, different fallbacks), but the `n/a` vs `N/A` split is
  explicitly documented house style (:1107-1108) and the shared part is one
  `format!`. Held.
- `amd.rs:871` / `intel.rs:840` — identical `const GAP` in the two hardware-test
  modules, plus the two `backend()`/test constructor literals. Test-only, and
  the amd/intel twin structure is the documented deliberate parallel the 08-05
  dropped list already cites. Held.
- `windows.rs:476-479` — `.then(|| …).flatten()` inside the `filter_map`
  closure; a guard would read marginally better, but the module is Windows-only
  and uncompiled on this host, so nothing here verifies the change. Held.
- `intel.rs:141-143` — the `powers` Vec pre-collection is a borrow-split
  workaround (`power_w` takes `&mut self` while the device map borrows `&self`),
  not avoidable duplication. Held.

### Coverage

Read in full: `src/` (all files, incl. unit and hardware-test modules) plus
`tests/smoke.rs` and `tests/tui.rs`. Verification:
`cargo check --all-features --locked` clean; `rg` confirms the cited call sites
and no rustc-detectable dead code. Gaps, as in the earlier passes: nothing
behind `#[cfg(windows)]` (PDH/DXGI) or `cfg(target_os = "macos")` (IOKit) is
compiled on this Linux host; the amd/intel hardware-test modules skip without a
GPU; the verification gate was not run (per this sweep's constraints; the
orchestrator runs it).

## Codebase perf 2026-08-06

The sweep's performance pass, second run over the same tree. Tree clean (only
`docs/backlog.md` modified), so full-codebase: all of `src/` and `tests/` read
in full again, no builds or profiling runs (figures come from reading, as in
08-05). **The five standing 08-05 findings were re-verified against the current
tree and all hold exactly as recorded** — `rebuild_proc_view`'s per-poll clone
and re-sort (`app.rs:1060`, `:1124`, `:1128-1151`), the per-frame
`driver_info()` uname (`ui.rs:41`, `amd.rs:182-184`, `intel.rs:210-212`,
`linux.rs:819-821`, `:833-836`), the filter's per-row re-lowercasing
(`app.rs:1111-1123`), NVML's per-poll `num_fans()` (`nvidia.rs:653-666`), and
the Windows PDH per-poll instance re-parse (`windows.rs:432-484`, five calls per
poll at `:249`/`:284-290`) — so they are not re-reported here; see the 08-05
section above. The 08-05 Coverage exclusions and the "Decisions taken
deliberately" section were re-read against the code and still hold (confirmed in
Coverage below). Verdict: no significant new hot-path problems — one new small
finding survives verification; the top wins remain 08-05 findings 1 and 2.

### 1. Every process row re-resolves its user name per poll — a linear scan

over the whole user list plus a fresh String, for a mapping that is
session-constant — **shipped 2026-08-06** (`2872099`): resolved names are now
cached per uid (misses included, since `Users` is a startup snapshot and can
never resolve later) in a `HashMap<sysinfo::Uid, Option<String>>` — keyed on
`Uid` because Windows wraps a `Sid`, not a `u32`. After the first row per uid,
each row is one hash lookup.

### Coverage

Traced (frequency and size established, as in 08-05): the full per-tick path —
run loop (`main.rs:327-467`, one `terminal.draw` per tick plus per input event,
one `poll` per tick), `poll_inner` (`app.rs:824-916`), `refresh_processes`
(`app.rs:963-1061`, incl. the per-row user/command/container resolution — the
user half is finding 1, the command half is the settled decision), the per-frame
draw (`ui.rs`: header, `draw_gpus`, per-card meters [settled `draw_meter`
spans], waveform O(cols×rows) with O(1) `windowed`, process-table visible-slice
cells), and the per-poll backend reads — amd ~22 sysfs reads/device/poll, intel
(similar shape, plus the per-walk `xe_state` `cycles` clone at
`intel.rs:247-255`, small: a handful of (String, counter) pairs per client),
nvidia ~17 NVML round trips/device/poll + 3 process queries, windows PDH (the
per-poll `keys` clone and `procs` map assembly at `windows.rs:299-335` were
traced and sit inside finding 5's umbrella, not separate), apple IOKit
re-enumeration, mock/stub/replay. The /proc walk cost itself remains the code's
own measured 4.2 ms/588 pids, worker-threaded and paced by `MIN_WALK_INTERVAL` —
settled by the recorded design.

Traced and deliberately excluded, re-confirmed against the code: the 08-05
Coverage list (history front-drain, `draw_meter` spans, command-line-not-cached,
`enforced_power_limit`-not-cached, paced sweep, NVML static cache, container
cache, headless double-walk, the per-tick `evict_departed_processes`/`proc_text`
sweeps) and every "Decisions taken deliberately" entry still holds as written.
Also cross-referenced rather than re-reported: the splash's per-frame
`art_dims()` re-scan (`splash.rs:104-105`) — the 08-06 tidy pass flagged it as
cleanup #4, and I agree with that framing: it runs for ~25 frames once at
startup over a ~40-line constant string, which is not a hot path by this pass's
criteria (no frequency to speak of, negligible size).

Not settled without profiling (unchanged from 08-05): NVML round-trip latency
per call, actual PDH instance counts on a busy Windows box, per-frame render
time, the walk's cost at the 10k-process pathological scale, and — for finding 1
— the real per-box user count that `get_user_by_id` scans. All findings are
stated at their traceable frequencies; none claims a measured microsecond
figure.
