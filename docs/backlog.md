# gpur backlog

Known gaps, carried over from the review passes on `v0.8.1` (`fad7fe9`).
Everything already fixed lives in `CHANGELOG.md`; only open work is listed here.

Ordered by what is worth doing first rather than strictly by severity — item 1
unblocks item 2 and cleans up three workarounds already in the tree.

---

## 1. Devices have no identity — `App` keys everything by position

**Severity: medium.** `src/app.rs:250` (`folded: Vec<usize>`), `:317`
(`HashSet<usize>`), and the `history` / `session` vectors zipped positionally in
`poll`.

Three fixes already in the tree work around this rather than solving it:

- `backend/apple.rs` sorts by IOKit registry entry id so the order is at least
  deterministic — but an eGPU hotplug whose id sorts before an existing device
  still shifts every later index.
- `backend/nvidia.rs` pushes a placeholder for a device that errors mid-poll,
  purely so indices do not shift.
- The composite backend holds each child's slot count as a high-water mark, for
  the same reason.

One bug remains outright: `folded` is persisted as bare indices, so adding or
removing a card between runs folds the wrong GPU on the next start.

**Fix:** put an opaque stable device id on `GpuSnapshot` — registry entry id on
macOS, PCI BDF on Linux, adapter LUID on Windows, NVML UUID — and key `history`,
`session`, `folded` and `selected` on it instead of on position. Every backend
already has the value in hand; each currently discards it. The three workarounds
above can then be simplified or dropped.

## 2. Windows shows one vendor on a mixed rig

**Severity: medium-low.** `src/backend/mod.rs` `detect()`,
`src/backend/windows.rs`.

Multi-vendor enumeration works on Linux and macOS: every backend that probes is
composed. PDH is deliberately still a _fallback_, taken only when nothing else
probed, because it is vendor-generic and would list every NVIDIA card a second
time alongside NVML's richer entries.

So a Windows box with an NVIDIA card beside an AMD or Intel iGPU shows only the
NVIDIA one.

**Fix:** filter PDH's adapter list by vendor id and exclude adapters another
backend already claimed. Doing that reliably needs the device identity from
item 1.

## 3. Mouse input has no test coverage at all

**Severity: low, but the largest untested surface.** No test in either suite
emits an SGR mouse sequence.

Unverified as a result: the process-pane hit test (which had an off-by-one that
selected invisible rows), `card_rects` hit-testing, wheel routing, and the
modal-dispatch guard that stops clicks moving the selection under an open
dialog. All four were touched during the review and rest on inspection alone.

**Fix:** the PTY harness can already write arbitrary bytes — `Tui::send` takes a
`&str`, so an SGR sequence like `\x1b[<0;40;27M` is a one-line addition.

## 4. The `/proc` sweep runs on the render thread

**Severity: low.** `src/backend/linux.rs` `sweep_clients`, called from
`App::poll` on the render loop.

Each pid's `fd` directory is now walked once rather than once per driver
(measured 4.2 ms against 7.8–9.9 ms over 588 pids), but the scan is still
synchronous, so `event::poll` cannot run during it and keypresses queue. Fine at
ordinary scale; on a 10k-process node at `--tick-ms 100` the sweep can exceed
the tick.

**Fix:** move the sweep to a worker thread handing snapshots to the UI over a
channel. Deliberately deferred — a structural change, not a bug fix.

## 5. Remaining test gaps

- **The kill path's signal branch.** Unit tests in `app.rs` cover every refusal
  and one successful signal against a spawned `sleep`, but no PTY test reaches
  it, because mock and replay now correctly refuse to signal. End-to-end
  coverage needs a backend stub the test binary can inject.
- **`--graphs block` / `ascii` rendering.** Only the invalid-config-value error
  path is covered (`tests/smoke.rs:223`). Both alternate renderers — where a
  colour-quantization bug lived — are unasserted.
- **Colour modes.** Nothing runs with `NO_COLOR=1` or without `COLORTERM`, so
  the quantizers are covered by their own unit tests but never by rendered
  output.
- `--man` and `--once`'s plain-text stdout are unasserted.
- **No CI job runs against real GPU hardware.** Inherent to hosted runners; the
  mitigation was fixture-testing every backend's pure parsers, which is why
  `windows.rs` and `apple.rs` now have unit tests that run on any host. Closing
  it properly needs a self-hosted runner.

## 6. NVIDIA temperature threshold unread

**Severity: low.** `Device::temperature_threshold()` is available in
`nvml-wrapper 0.12.1` (`device.rs:4048`) and would give the temperature meter a
per-card scale instead of a fixed one. Needs a new `GpuSnapshot` field and a
`draw_meter` scale change.

Junction temperature is genuinely unavailable rather than merely unimplemented:
the only temperature field ids in `nvml-wrapper-sys 0.9.1` are
`NVML_FI_DEV_MEMORY_TEMP` (wired up) and the four `*_TLIMIT` margins, which are
thresholds, not a hotspot reading.

## 7. Waveform history cannot represent "unknown"

**Severity: low.** `src/app.rs`. `utilization_pct` is `Option`, but an
unreadable sample is recorded as `0` in the history ring because a sparkline has
no glyph for absent. The meter above the graph carries the distinction; the
graph does not. Commented at the site.

## 8. Device naming differs per backend

**Severity: low, cosmetic.** `nvidia.rs` gives the marketing name
(`NVIDIA GeForce RTX 4090`), the Linux backends the pci.ids codename
(`Navi 31 [Radeon RX 7900 XT/...]`), Windows the DXGI description, Apple the SoC
brand. Fallbacks differ too: `NVIDIA GPU 0` (index) against
`AMD GPU 0x744c (card1)` (device id plus card).

**Fix:** settle on a convention — prefer the marketing name, fall back to the
codename, always suffix the card or index — and apply it uniformly.

## 9. `splash::build_path` truncates coordinates to `u8`

**Severity: low.** `src/splash.rs:24` — `path.push((r as u8, c as u8, ch))`.
`art.txt` is 5×33 so this is safe today, and the `u8` is imposed by
`hjkl_splash`'s API, but a banner wider than 255 columns would silently wrap and
scatter the cursor trail. Assert the art's dimensions rather than relying on it
staying small.

## 10. Residual YAGNI

- `src/ui.rs:603` `draw_meter` takes 8 arguments behind
  `#[allow(clippy::too_many_arguments)]` with two production call sites.
  Collapsing it into a params struct is a refactor, not a deletion.
- `src/theme.rs:89` `UiTheme::temp_ok` is `pub` but used only by `temp_style` at
  `:179` in the same file, unlike its peers `temp_warn` / `temp_crit`. Narrowing
  it alone would be asymmetric.
- `src/keys.rs:7` `enum Mode { Normal }` has one variant threaded through
  `Keymap<Action, Mode>`. Required by the `hjkl-keymap` API, so it cannot go,
  but no second mode is planned — filter and confirm bypass the keymap entirely.

---

## Decisions taken deliberately

Recorded so they are not re-opened as findings.

- **`--once` keeps raw MiB** (`vram 40MiB/24560MiB`) while the TUI uses
  `human_bytes` (`14G`). The review called this drift, but the TUI is
  width-constrained and a one-shot diagnostic is not; precision is worth more
  there, and MiB matches what `nvidia-smi` and `nvtop` print.
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
