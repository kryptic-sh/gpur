# gpur audit — open items

The original audit ran over `v0.8.1` (`fad7fe9`) in four passes: two independent
reviewers on correctness and on parity/DRY/YAGNI, then two more that
adversarially verified those findings and swept the UI, app and test code the
first round under-covered.

Of 40 findings plus the DRY, YAGNI and docs tables, everything actionable has
been fixed across 17 commits — see `CHANGELOG.md` under `[Unreleased]` for the
user-visible list. **This file now holds only what is still open**, plus the
decisions taken deliberately, so they are not re-raised.

Tests over the branch: 28 → 118. `cargo clippy --all-targets -- -D warnings`,
`cargo fmt --all`, `cargo test`, and `cargo check` for both
`x86_64-pc-windows-msvc` and `x86_64-apple-darwin` are all clean.

---

## Open

### 1. MEDIUM — devices have no identity; `App` still keys everything by position

`src/app.rs:250` (`folded: Vec<usize>`), `:317` (`HashSet<usize>`), and the
`history` / `session` vectors zipped positionally in `poll`.

Several fixes on this branch worked around this rather than solving it:

- `backend/apple.rs` now sorts by IOKit registry entry id, so the order is at
  least deterministic — but an eGPU hotplug whose id sorts before an existing
  device still shifts every later index.
- `backend/nvidia.rs` pushes a placeholder for a device that errors mid-poll,
  and the composite backend holds each child's slots open as a high-water mark,
  both purely to stop indices shifting.
- `folded` is persisted as bare indices, so adding or removing a card between
  runs folds the wrong GPU on the next start.

The real fix is one change that closes all of it: put an opaque stable device id
on `GpuSnapshot` — registry entry id on macOS, PCI BDF on Linux, adapter LUID on
Windows, NVML UUID — and key `history`, `session`, `folded` and `selected` on
that instead of on position. Every backend already has the value in hand; each
currently discards it.

Until then, growth (hotplug, a card returning after a driver reload) can still
reattach one GPU's graph history and session peaks to another.

### 2. MEDIUM-LOW — Windows shows one vendor on a mixed rig

`src/backend/mod.rs` `detect()`, `src/backend/windows.rs`.

Finding 6 is fixed for Linux and macOS: every backend that probes is now
composed. PDH is deliberately still a _fallback_, taken only when nothing else
probed, because it is vendor-generic and would list every NVIDIA card a second
time alongside NVML's richer entries.

So a Windows box with an NVIDIA card beside an AMD or Intel iGPU shows only the
NVIDIA one. Fixing it means filtering PDH's adapter list by vendor id and
excluding adapters another backend already claimed, which needs the device
identity from item 1 to do reliably.

### 3. LOW — NVIDIA temperature threshold unread, so the temp meter has no real scale

`src/backend/nvidia.rs`. `Device::temperature_threshold()` is available in
`nvml-wrapper 0.12.1` (`device.rs:4048`) and would give the temperature meter a
per-card scale instead of a fixed one. Skipped because it needs a new
`GpuSnapshot` field and a `draw_meter` scale change, and the backend pass that
found it did not own those files.

Junction temperature remains genuinely unavailable: the only temperature field
ids in `nvml-wrapper-sys 0.9.1` are `NVML_FI_DEV_MEMORY_TEMP` (now wired up) and
the four `*_TLIMIT` margins, which are thresholds rather than a hotspot reading.

### 4. LOW — the `/proc` sweep still runs on the render thread

`src/backend/linux.rs` `sweep_clients`, called from `App::poll` on the render
loop. Each pid's `fd` directory is now walked once rather than once per driver
(measured 4.2 ms against 7.8–9.9 ms over 588 pids), but the scan is still
synchronous, so `event::poll` cannot run during it and keypresses queue.

Fine at this host's scale; on a 10k-process node at `--tick-ms 100` the sweep
can exceed the tick. The structural fix is a worker thread handing snapshots to
the UI over a channel — deliberately out of scope for a bug-fix pass.

### 5. LOW — waveform history cannot represent "unknown"

`src/app.rs`. Now that `utilization_pct` is `Option`, an unreadable sample is
recorded as `0` in the history ring, because a sparkline has no glyph for
absent. The meter above the graph carries the distinction correctly; the graph
does not. Commented at the site.

### 6. LOW — device naming still differs per backend

`nvidia.rs` gives the marketing name (`NVIDIA GeForce RTX 4090`), the Linux
backends give the pci.ids codename (`Navi 31 [Radeon RX 7900 XT/...]`), Windows
gives the DXGI description, Apple the SoC brand. Fallbacks differ too:
`NVIDIA GPU 0` (index) against `AMD GPU 0x744c (card1)` (device id plus card).

Cosmetic, but the same column reads differently depending on the hardware. Worth
settling on a convention — prefer the marketing name, fall back to the codename,
always suffix the card or index — and applying it uniformly.

### 7. LOW — `splash::build_path` truncates coordinates to `u8`

`src/splash.rs:24` — `path.push((r as u8, c as u8, ch))`. `art.txt` is 5×33 so
this is safe today, and the `u8` is imposed by `hjkl_splash`'s API, but a banner
wider than 255 columns would silently wrap and scatter the cursor trail. Assert
the art's dimensions rather than relying on it staying small.

### 8. LOW — residual YAGNI

- `src/ui.rs` `draw_meter` takes 8 arguments behind
  `#[allow(clippy::too_many_arguments)]` with two production call sites.
  Collapsing it into a params struct is a refactor, not a deletion.
- `src/theme.rs` `UiTheme::temp_ok` is `pub` but used only by `temp_style` in
  the same file, unlike its peers `temp_warn` / `temp_crit`. Narrowing it alone
  would be asymmetric.
- `src/keys.rs` `enum Mode { Normal }` has one variant threaded through
  `Keymap<Action, Mode>`. Required by the `hjkl-keymap` API, so it cannot go,
  but no second mode is planned — filter and confirm bypass the keymap entirely.

### 9. Test gaps

The suite is hermetic now, the four vacuous assertions are pinned to behavior,
and coverage was added for sort ordering, pause, tick keys, card overflow
scrolling, the `GPUR_MOCK_FAIL` degradation and re-detect paths, and the `n/a`
rendering. Still uncovered:

- **Mouse input — nothing at all.** No test emits an SGR sequence, so the
  process-pane hit test (fixed on this branch), `card_rects` hit-testing and
  wheel routing are all unverified.
- **The kill path's signal branch.** Unit tests in `app.rs` cover every refusal
  and one successful signal against a spawned `sleep`, but no PTY test reaches
  it, because mock and replay now correctly refuse to signal. Exercising it end
  to end needs a backend stub the test binary can inject.
- `--graphs block` / `ascii` rendering, `NO_COLOR` output, `--replay` in TUI
  mode, `--man`, and `--once`'s plain-text stdout are all unasserted.
- **No CI job runs against real GPU hardware.** Inherent to hosted runners — the
  mitigation was fixture-testing every backend's pure parsers, which is why
  `windows.rs` and `apple.rs` now have unit tests that run on any host. Closing
  it properly needs a self-hosted runner.

---

## Decisions taken deliberately

Recorded so they are not re-opened as findings.

- **`--once` keeps raw MiB** (`vram 40MiB/24560MiB`) while the TUI uses
  `human_bytes` (`14G`). The audit called this drift, but the TUI is
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
