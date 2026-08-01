# gpur backlog

Open work from the review passes, the multi-vendor detection audit and the
full-codebase review in `docs/code-review.md`. Closed items are removed rather
than struck through — what was fixed, and why, lives in `CHANGELOG.md`, and the
review document tracks each finding's fate. Anything from a closed item that is
still a live constraint on future work has been moved down into "Decisions taken
deliberately" instead of being kept as a task.

Roughly ordered by what is worth doing first. Nothing open here is a correctness
bug; the remainder is granularity, cost, coverage and polish. Every item that is
still open is open because it turns on a product or architecture call rather
than on effort.

No fabricated readings remain anywhere in the UI: the device gauges stopped
inventing a `0` in 0.10.2, the process table in 0.11.0, and the graph history —
the last holdout, because a waveform has no glyph for absent — in 0.11.1.

---

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

## 2. The `/proc` sweep runs on the render thread

**Severity: low.** `src/backend/linux.rs` `sweep_clients`, called from
`App::poll` on the render loop.

Each pid's `fd` directory is now walked once rather than once per driver
(measured 4.2 ms against 7.8–9.9 ms over 588 pids), but the scan is still
synchronous, so `event::poll` cannot run during it and keypresses queue. Fine at
ordinary scale; on a 10k-process node at `--tick-ms 100` the sweep can exceed
the tick.

**And it multiplies per vendor.** Each Linux backend runs its own full `/proc`
walk, so an AMD + Intel + nouveau box stats every fd of every process three
times per tick, discarding the clients that belong to the other backends each
time. Deduplicating means one sweep shared across children, which is a `mod.rs`
design change rather than a per-backend fix.

**Fix:** one shared sweep per poll, on a worker thread handing snapshots to the
UI over a channel. Deliberately deferred — structural, not a bug fix.

## 3. An NVIDIA card is still invisible when NVML is broken rather than absent

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

## 4. Remaining test gaps

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
- **No CI job runs against real GPU hardware.** Inherent to hosted runners; the
  mitigation was fixture-testing every backend's pure parsers, which is why
  `windows.rs` and `apple.rs` have unit tests that run on any host. Closing it
  properly needs a self-hosted runner.

  This is the gap behind every "unverifiable without hardware" note in this
  file, and it is load-bearing: the WDDM per-process-memory bug fixed in 0.11.0
  was diagnosed from NVIDIA's documentation, not from an observation, and
  nothing in CI could have caught it or can confirm the fix.

## 5. NVIDIA temperature threshold unread

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

## 6. Device naming differs per backend

**Severity: low, cosmetic.** `nvidia.rs` gives the marketing name
(`NVIDIA GeForce RTX 4090`), the Linux backends the pci.ids codename
(`Navi 31 [Radeon RX 7900 XT/...]`), Windows the DXGI description, Apple the SoC
brand. Fallbacks differ too: `NVIDIA GPU 0` (index) against
`AMD GPU 0x744c (card1)` (device id plus card).

**Fix:** settle on a convention — prefer the marketing name, fall back to the
codename, always suffix the card or index — and apply it uniformly.

## 7. Residual YAGNI

- `src/theme.rs:89` `UiTheme::temp_ok` is `pub` but used only by `temp_style` at
  `:173` in the same file, unlike its peers `temp_warn` / `temp_crit`. Narrowing
  it alone would be asymmetric.
- `src/keys.rs:7` `enum Mode { Normal }` has one variant threaded through
  `Keymap<Action, Mode>`. Required by the `hjkl-keymap` API, so it cannot go,
  but no second mode is planned — filter and confirm bypass the keymap entirely.

---

## Decisions taken deliberately

Recorded so they are not re-opened as findings.

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
