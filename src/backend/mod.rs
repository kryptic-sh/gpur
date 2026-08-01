//! GPU telemetry backends. One trait, one impl per vendor/platform.

mod amd;
mod apple;
mod intel;
#[cfg(target_os = "linux")]
mod linux;
mod mock;
mod nvidia;
mod replay;
mod windows;

use anyhow::Result;

/// One sample of one GPU at one instant.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct GpuSnapshot {
    pub name: String,
    /// Opaque, stable identity for this device — NVML UUID, PCI BDF, IOKit
    /// registry entry id, adapter LUID. `App` keys graph history, session
    /// peaks and folding on it, so it must name the same physical GPU across
    /// polls, across hotplug, and across runs. `None` when the backend
    /// genuinely cannot identify a device: `App` then falls back to the
    /// device's position, which is honest about being positional instead of
    /// pretending an identity it doesn't have.
    pub device_id: Option<String>,
    /// Integrated (APU/iGPU) as opposed to a discrete card.
    pub integrated: bool,
    /// Core utilization, 0..=100. `None` means the backend cannot read it —
    /// never substitute 0, which renders as a confident "the GPU is idle".
    pub utilization_pct: Option<f64>,
    /// Memory-controller busy %, distinct from VRAM fill level.
    pub mem_util_pct: Option<f64>,
    /// Video engine busy % — unified (VCN/media) engines report here.
    pub video_util_pct: Option<f64>,
    /// Split encoder/decoder utilization where the vendor separates them.
    pub enc_util_pct: Option<f64>,
    pub dec_util_pct: Option<f64>,
    /// Active clock-throttle cause ("thermal", "power-limit", ...), when
    /// known or confidently derivable.
    pub throttle: Option<String>,
    /// VRAM fill level. `None` on the several devices whose driver publishes
    /// no figure at all (mainline i915, some APU configs, a PDH adapter with
    /// no matching counter instance) — `0/0` there is indistinguishable from
    /// a genuinely empty pool.
    pub vram_used_bytes: Option<u64>,
    pub vram_total_bytes: Option<u64>,
    pub temperature_c: Option<f64>,
    /// Hotspot / memory-junction temperatures where exposed (AMD temp2/3).
    pub temp_junction_c: Option<f64>,
    pub temp_mem_c: Option<f64>,
    pub power_w: Option<f64>,
    pub power_limit_w: Option<f64>,
    pub fan_pct: Option<f64>,
    pub fan_rpm: Option<u64>,
    pub clock_mhz: Option<u64>,
    pub mem_clock_mhz: Option<u64>,
    /// Current PCIe generation (1..=7).
    pub pcie_gen: Option<u8>,
    /// Current PCIe lane count.
    pub pcie_width: Option<u32>,
    /// Maximum supported PCIe generation/width, for downgrade detection.
    pub pcie_max_gen: Option<u8>,
    pub pcie_max_width: Option<u32>,
    /// PCIe throughput, KiB/s.
    pub pcie_rx_kbs: Option<u64>,
    pub pcie_tx_kbs: Option<u64>,
    /// GTT (system memory graphics pool) usage — matters for APUs.
    pub gtt_used_bytes: Option<u64>,
    pub gtt_total_bytes: Option<u64>,
    /// Core voltage, millivolts (AMD vddgfx).
    pub volt_mv: Option<u64>,
    /// DPM performance level when forced off "auto".
    pub perf_level: Option<String>,
}

/// One memory pool as a card reports it, plus where the bytes physically
/// live. `shared` is what the UI needs and no single field carries: an iGPU,
/// an APU and an Apple Silicon part all spend host RAM, but they publish it
/// through different fields of [`GpuSnapshot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemReadout {
    pub used: Option<u64>,
    pub total: Option<u64>,
    /// System RAM the GPU maps, rather than memory of the device's own.
    pub shared: bool,
}

impl MemReadout {
    /// Fill level, or `None` when either figure is unknown or the pool has no
    /// size. A meter drawn from a fabricated 0 here is the whole point of the
    /// `Option`s in [`GpuSnapshot`], so this refuses to invent one.
    pub fn pct(&self) -> Option<f64> {
        let (used, total) = (self.used?, self.total?);
        (total > 0).then(|| used as f64 / total as f64 * 100.0)
    }
}

impl GpuSnapshot {
    /// The pool the MEM meter and the memory graph describe: the memory this
    /// card actually spends.
    ///
    /// Which field carries it depends on the device, and there is no single
    /// convention across the backends because there is none across the
    /// platforms either:
    ///
    /// - A dGPU has real VRAM, and `gtt_*` is the *spill* pool beside it —
    ///   host RAM reached over PCIe, which is a signal in its own right.
    /// - An Intel iGPU has no local pool at all: `vram_*` is `None` and the
    ///   only memory it has is the system-backed one in `gtt_*`.
    /// - Apple Silicon and Windows' integrated adapters report their unified
    ///   memory *through* `vram_*` (IOKit's "In use system memory", DXGI's
    ///   `SharedSystemMemory`), because that is the only pool those APIs name.
    /// - An AMD APU has both: a small BIOS carve-out in `vram_*` that the
    ///   kernel really does account separately, and the rest in `gtt_*`.
    ///
    /// So the primary pool is the device-local one wherever a total for it
    /// exists, and the system-backed one otherwise — and `shared` records
    /// which of those it turned out to be, so nothing renders host RAM as a
    /// card's dedicated VRAM.
    pub fn mem_primary(&self) -> MemReadout {
        if self.has_device_pool() {
            return MemReadout {
                used: self.vram_used_bytes,
                total: self.vram_total_bytes,
                // Integrated *and* nothing in `gtt_*` means this figure is
                // the unified pool itself (Apple, Windows), not a carve-out
                // sitting beside one (AMD APU).
                shared: self.integrated && !self.has_system_pool(),
            };
        }
        MemReadout {
            used: self.gtt_used_bytes,
            total: self.gtt_total_bytes,
            // Whatever the device is, this pool is host RAM — but only if it
            // reported one. A card that published no memory figure at all
            // knows nothing about where its bytes live either, and "shared"
            // is a claim like any other.
            shared: self.has_system_pool(),
        }
    }

    /// Whether `vram_*` carries anything at all. Either half is enough: a
    /// backend that read one figure and not the other measured *something*,
    /// and falling through to the system pool would drop it on the floor.
    fn has_device_pool(&self) -> bool {
        self.vram_used_bytes.is_some() || self.vram_total_bytes.is_some()
    }

    fn has_system_pool(&self) -> bool {
        self.gtt_used_bytes.is_some() || self.gtt_total_bytes.is_some()
    }

    /// The second pool, shown beside the meter rather than buried in the
    /// footer, for the cards that have two. `None` when the card has one pool
    /// or none — the primary already covers it.
    pub fn mem_secondary(&self) -> Option<MemReadout> {
        (self.has_device_pool() && self.has_system_pool()).then_some(MemReadout {
            used: self.gtt_used_bytes,
            total: self.gtt_total_bytes,
            // On a dGPU this is GTT proper: host RAM the card maps across
            // PCIe, and a rising figure means the working set spilled off the
            // card. On an APU nothing spilled anywhere — both pools are RAM.
            shared: self.integrated,
        })
    }

    /// Fill level of the pool the meter shows.
    pub fn mem_pct(&self) -> Option<f64> {
        self.mem_primary().pct()
    }
}

/// Collapse throttle-reason fragments into a `+`-joined label, or None when
/// nothing throttled. Shared by the nvidia and amdgpu backends.
pub fn join_throttle(parts: &[&str]) -> Option<String> {
    (!parts.is_empty()).then(|| parts.join("+"))
}

/// Clamp a raw percentage into 0..=100. Backends derive these from counter
/// ratios that can under/overshoot; this is the single guard.
pub fn clamp_pct(v: f64) -> f64 {
    v.clamp(0.0, 100.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ProcKind {
    Graphics,
    #[default]
    Compute,
}

impl ProcKind {
    pub fn label(&self) -> &'static str {
        match self {
            ProcKind::Graphics => "Graphic",
            ProcKind::Compute => "Compute",
        }
    }
}

/// One process currently using one GPU.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct GpuProcess {
    pub pid: u32,
    /// Index into the snapshot vec returned by `poll`.
    pub gpu_index: usize,
    pub kind: ProcKind,
    /// GPU utilization attributable to this process, when the backend knows.
    pub gpu_util_pct: Option<f64>,
    /// GPU memory this process holds. `None` when the backend cannot account
    /// for it at all — NVML answers `Unavailable` for every process under
    /// WDDM, i.e. on ordinary consumer Windows, and PDH publishes no memory
    /// instance for some processes it does report engine time for. A `0` there
    /// would be a claim the process holds nothing, which is exactly what is
    /// not known; `Some(0)` stays reserved for a pool that was read and found
    /// empty.
    pub gpu_mem_bytes: Option<u64>,
    /// Pre-enriched host data. Live backends leave these None (sysinfo fills
    /// them in); the replay backend supplies the RECORDED values so playback
    /// doesn't resolve foreign pids against this host.
    pub user: Option<String>,
    pub command: Option<String>,
    pub cpu_pct: Option<f32>,
    pub host_mem_bytes: Option<u64>,
    /// Container runtime + short id, recorded rather than re-resolved: the
    /// replaying host's `/proc` knows nothing about a foreign pid.
    pub container: Option<String>,
}

/// A source of GPU telemetry. Implementations poll all devices they can see.
pub trait GpuBackend {
    /// Human-readable backend name ("nvml", "amdgpu", "metal", "mock").
    fn name(&self) -> &'static str;
    /// Sample every visible GPU. Index order must be stable across calls.
    fn poll(&mut self) -> Result<Vec<GpuSnapshot>>;
    /// Processes using the GPUs, sampled after `poll`. Backends without
    /// per-process visibility return nothing.
    fn processes(&mut self) -> Vec<GpuProcess> {
        Vec::new()
    }
    /// Driver / kernel version line for the header, when known.
    fn driver_info(&self) -> Option<String> {
        None
    }
    /// Whether the pids from `processes` name processes on THIS machine.
    /// Fabricated (mock) and foreign (replay) pids do not, so the kill path
    /// must refuse them — a recording from a stranger is otherwise a
    /// one-keystroke signal primitive against whoever opens it.
    fn can_signal(&self) -> bool {
        true
    }
}

const NO_BACKEND: &str = "no supported GPU backend found (run with --mock to demo the UI)";

/// PCI vendor ids, as DXGI's `AdapterDesc1.VendorId` and lspci report them.
/// Each vendor backend covers exactly one, which is what lets a vendor-generic
/// backend be told which devices are already accounted for.
const PCI_VENDOR_NVIDIA: u16 = 0x10de;
const PCI_VENDOR_AMD: u16 = 0x1002;
const PCI_VENDOR_INTEL: u16 = 0x8086;

/// One child of a [`CompositeBackend`], plus the bookkeeping that keeps its
/// devices on the same indices for the life of the session.
struct Child {
    backend: Box<dyn GpuBackend>,
    /// Slots this child owns in the concatenated snapshot vec — a high-water
    /// mark, never shrunk. Purely for display continuity now that
    /// [`GpuSnapshot::device_id`] carries identity: a child failing for one
    /// tick would otherwise pull every later card up the screen and push it
    /// back down on the next tick. Per-device state follows the id, so a
    /// shifted index no longer misattributes anything.
    slots: usize,
    /// Device names from the last poll that saw each slot, to label the
    /// placeholders that hold the slots open.
    names: Vec<String>,
    /// Namespaced device id last seen in each slot. A placeholder inherits
    /// it: the card is the same GPU, briefly unreadable, so its graphs and
    /// session peaks must carry on rather than restart.
    ids: Vec<Option<String>>,
}

/// Every backend that reported devices, polled as one. Mixed-vendor rigs — an
/// NVIDIA dGPU beside an AMD APU, an Intel iGPU beside an AMD dGPU — are the
/// common laptop and workstation case, and stopping at the first probe hid
/// whichever vendor came later in the chain.
struct CompositeBackend {
    children: Vec<Child>,
}

impl CompositeBackend {
    fn new(backends: Vec<Box<dyn GpuBackend>>) -> Self {
        Self {
            children: backends
                .into_iter()
                .map(|backend| Child {
                    backend,
                    slots: 0,
                    names: Vec::new(),
                    ids: Vec::new(),
                })
                .collect(),
        }
    }
}

impl GpuBackend for CompositeBackend {
    /// Fixed: the child set is only known at runtime and this returns
    /// `&'static str`. The vendors show up in `driver_info`, which the header
    /// prints right after the name, and in each card's device name.
    fn name(&self) -> &'static str {
        "multi"
    }

    fn poll(&mut self) -> Result<Vec<GpuSnapshot>> {
        let mut out = Vec::new();
        let mut errors = Vec::new();
        for (ci, c) in self.children.iter_mut().enumerate() {
            let mut snaps = match c.backend.poll() {
                Ok(s) => s,
                // One vendor's driver going away must not take the others'
                // cards off screen with it, so a failed child degrades to
                // placeholders instead of aborting the whole poll.
                Err(e) => {
                    errors.push(format!("{}: {e:#}", c.backend.name()));
                    Vec::new()
                }
            };
            // Namespace every child's ids. Nothing in the trait stops two
            // children minting the same string, and two devices sharing one
            // key would share one set of graphs. The child index is fixed for
            // the session (and re-detect rebuilds the same probe order), so
            // it also survives a re-detect.
            for s in &mut snaps {
                if let Some(id) = s.device_id.take() {
                    s.device_id = Some(format!("{}#{ci}:{id}", c.backend.name()));
                }
            }
            c.slots = c.slots.max(snaps.len());
            c.names.resize(c.slots, String::new());
            c.ids.resize(c.slots, None);
            for (i, snap) in snaps.iter().enumerate() {
                snap.name.clone_into(&mut c.names[i]);
                c.ids[i].clone_from(&snap.device_id);
            }
            for i in snaps.len()..c.slots {
                // A device that came back at a lower slot is already live at
                // its new index, so the slot it vacated must stop claiming it.
                // Two rows in one poll carrying one `device_id` share one set
                // of graphs and one session peak, and `--json` would name the
                // same GPU twice — which `--replay` then reads back.
                if c.ids[i].is_some() && c.ids[..snaps.len()].contains(&c.ids[i]) {
                    c.ids[i] = None;
                    c.names[i].clear();
                }
                let label = match c.names.get(i) {
                    Some(n) if !n.is_empty() => n.clone(),
                    _ => format!("{} GPU {i}", c.backend.name()),
                };
                snaps.push(GpuSnapshot {
                    name: format!("{label} (unavailable)"),
                    device_id: c.ids.get(i).cloned().flatten(),
                    ..Default::default()
                });
            }
            out.append(&mut snaps);
        }
        // A partial failure keeps the surviving vendors' cards on screen. An
        // empty result must not: every child failing, or the one child that
        // answered having nothing yet to hold slots open, would otherwise
        // render as a blank device list with no error to explain it.
        if !errors.is_empty() && (out.is_empty() || errors.len() == self.children.len()) {
            anyhow::bail!("{}", errors.join("; "));
        }
        Ok(out)
    }

    fn processes(&mut self) -> Vec<GpuProcess> {
        let mut out = Vec::new();
        let mut base = 0;
        for c in &mut self.children {
            for mut p in c.backend.processes() {
                // A row outside its own child's span would be drawn against
                // another vendor's card. Dropping it loses one row; keeping it
                // misattributes it.
                if p.gpu_index >= c.slots {
                    continue;
                }
                p.gpu_index += base;
                out.push(p);
            }
            base += c.slots;
        }
        out
    }

    /// Names each child, since `name()` cannot. The vendor backends' own lines
    /// already lead with their module name ("amdgpu · kernel 7.1"), so only
    /// prefix the ones that don't.
    fn driver_info(&self) -> Option<String> {
        let parts: Vec<String> = self
            .children
            .iter()
            .map(|c| {
                let name = c.backend.name();
                match c.backend.driver_info() {
                    Some(d) if d.contains(name) => d,
                    Some(d) => format!("{name} {d}"),
                    None => name.to_string(),
                }
            })
            .collect();
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    /// One un-signalable child disables the kill path for every row: the
    /// process table is a single list and a fabricated pid in it is exactly
    /// the hazard `can_signal` exists to stop.
    fn can_signal(&self) -> bool {
        self.children.iter().all(|c| c.backend.can_signal())
    }
}

/// Wrap only what needs wrapping: the single-vendor machine — the common case
/// — gets its backend back untouched, so it pays nothing for this.
fn compose(mut found: Vec<Box<dyn GpuBackend>>) -> Result<Box<dyn GpuBackend>> {
    if found.len() <= 1 {
        return found.pop().ok_or_else(|| anyhow::anyhow!(NO_BACKEND));
    }
    Ok(Box::new(CompositeBackend::new(found)))
}

/// Where a session's telemetry comes from: this machine, fabricated cards, or
/// a recording. One value rather than a `--mock` and a `--replay` field side by
/// side, because the two flags are alternatives — clap already rejects them
/// together — and two parallel `Option`s can spell a fourth state that
/// [`detect`] has to pick a winner for.
///
/// [`crate::app::App`] keeps one of these for the life of the session and
/// re-detects through it, which is what makes a re-detect a repeat of the
/// detection that started the session rather than a partial reconstruction of
/// it. See the failure path in `App::poll_inner` for why that matters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendSource {
    /// Whatever hardware this machine has.
    Live,
    /// `--mock N`: N fabricated cards, no hardware involved.
    Mock(usize),
    /// `--replay FILE`: frames recorded on some other machine.
    Replay(std::path::PathBuf),
}

impl BackendSource {
    /// Read the choice off the parsed CLI.
    pub fn from_cli(mock: Option<usize>, replay: Option<std::path::PathBuf>) -> Self {
        match (mock, replay) {
            // Both at once is unreachable through the CLI. If it ever became
            // reachable, replay is the right winner: it is the source whose
            // pids must never be signalable.
            (_, Some(path)) => Self::Replay(path),
            (Some(n), None) => Self::Mock(n),
            (None, None) => Self::Live,
        }
    }

    /// Build the backend this source describes. Startup and the failure
    /// re-detect both come through here, so they cannot drift apart.
    pub fn detect(&self) -> Result<Box<dyn GpuBackend>> {
        match self {
            Self::Live => detect(None, None),
            Self::Mock(n) => detect(Some(*n), None),
            Self::Replay(path) => detect(None, Some(path)),
        }
    }
}

/// Every backend that reports usable devices on this machine, as one backend.
pub fn detect(
    mock: Option<usize>,
    replay: Option<&std::path::Path>,
) -> Result<Box<dyn GpuBackend>> {
    if let Some(path) = replay {
        return replay::load(path);
    }
    if let Some(n) = mock {
        // The CLI range-validates `--mock`; this clamp only guards internal
        // callers (re-detect) from a count the mock backend can't render.
        return Ok(Box::new(mock::MockBackend::new(n.clamp(1, 16))));
    }
    // The vendor backends are disjoint — each claims only its own PCI vendor's
    // devices — so every one that probes adds cards no other one reports, and
    // names the vendor it has covered. The probes that find nothing are a
    // failed `Nvml::init` and two readdirs of /sys/class/drm, which is why
    // probing all of them is affordable.
    //
    // Apple claims no PCI vendor: it is macOS-only and the only backend that
    // consumes these claims is Windows-only, so the two never meet and naming
    // a vendor here would assert a fact nothing checks.
    let found: Vec<(Box<dyn GpuBackend>, &'static [u16])> = [
        (nvidia::probe(), &[PCI_VENDOR_NVIDIA][..]),
        (amd::probe(), &[PCI_VENDOR_AMD][..]),
        (intel::probe(), &[PCI_VENDOR_INTEL][..]),
        (apple::probe(), &[][..]),
    ]
    .into_iter()
    .filter_map(|(backend, vendors)| backend.map(|b| (b, vendors)))
    .collect();
    compose_with_generic(found, windows::probe)
}

/// Compose the vendor backends that probed with a vendor-generic one, told
/// which PCI vendors are already covered so it only contributes what is left.
///
/// Windows PDH (Task Manager's counters) enumerates every adapter DXGI can see,
/// NVIDIA cards included, so composing it blind would list an NVIDIA card twice
/// — once from NVML with clocks, power and temperature, once from PDH with
/// utilization alone. Excluding it wholesale instead, as this used to, hid the
/// AMD or Intel iGPU sitting beside an NVIDIA dGPU: the common Windows laptop.
/// Filtering it by claimed vendor is what lets both show up exactly once.
///
/// The generic backend answers `None` when nothing survives its filter, so a
/// single-vendor machine is not wrapped in a one-child composite.
fn compose_with_generic(
    found: Vec<(Box<dyn GpuBackend>, &'static [u16])>,
    generic: impl FnOnce(&[u16]) -> Option<Box<dyn GpuBackend>>,
) -> Result<Box<dyn GpuBackend>> {
    let claimed: Vec<u16> = found.iter().flat_map(|(_, v)| v.iter().copied()).collect();
    let mut backends: Vec<Box<dyn GpuBackend>> = found.into_iter().map(|(b, _)| b).collect();
    backends.extend(generic(&claimed));
    compose(backends)
}

#[cfg(test)]
mod mem_pool_tests {
    use super::*;

    /// A dGPU: VRAM is the pool it spends, and GTT is host RAM it spilled
    /// into — a separate story, and never the meter's subject.
    #[test]
    fn a_discrete_card_meters_its_own_vram_and_keeps_gtt_beside_it() {
        let g = GpuSnapshot {
            vram_used_bytes: Some(8 << 30),
            vram_total_bytes: Some(24 << 30),
            gtt_used_bytes: Some(3 << 30),
            gtt_total_bytes: Some(16 << 30),
            ..Default::default()
        };
        let m = g.mem_primary();
        assert_eq!((m.used, m.total), (Some(8 << 30), Some(24 << 30)));
        assert!(!m.shared, "a card's own VRAM is not system RAM");
        let s = g.mem_secondary().expect("GTT is a pool of its own here");
        assert_eq!((s.used, s.total), (Some(3 << 30), Some(16 << 30)));
        assert!(!s.shared, "GTT on a dGPU is mapped host RAM, not unified");
        assert_eq!(g.mem_pct(), Some(1.0 / 3.0 * 100.0));
    }

    /// An Intel iGPU: no local pool exists at all, so the system-backed one
    /// is the only thing there is to meter. This is the case that used to
    /// render as `MEM n/a` beside a card that was demonstrably using memory.
    #[test]
    fn an_igpu_with_no_local_pool_meters_the_system_one() {
        let g = GpuSnapshot {
            integrated: true,
            gtt_used_bytes: Some(734 << 20),
            gtt_total_bytes: Some(15 << 30),
            ..Default::default()
        };
        let m = g.mem_primary();
        assert_eq!((m.used, m.total), (Some(734 << 20), Some(15 << 30)));
        assert!(m.shared);
        assert_eq!(g.mem_secondary(), None, "one pool, so nothing beside it");
        assert!(
            g.mem_pct().is_some(),
            "the memory graph must not stay blank"
        );
    }

    /// Apple Silicon and Windows' integrated adapters publish unified memory
    /// through the VRAM fields, because IOKit and DXGI name no other pool.
    /// The bytes are still system RAM and must say so.
    #[test]
    fn unified_memory_arriving_through_the_vram_fields_is_still_shared() {
        let g = GpuSnapshot {
            integrated: true,
            vram_used_bytes: Some(12 << 30),
            vram_total_bytes: Some(32 << 30),
            ..Default::default()
        };
        let m = g.mem_primary();
        assert_eq!((m.used, m.total), (Some(12 << 30), Some(32 << 30)));
        assert!(m.shared, "32 GB of dedicated VRAM is what this is not");
        assert_eq!(g.mem_secondary(), None);
    }

    /// An AMD APU has both: a BIOS carve-out the kernel accounts separately,
    /// and the rest of the system pool. The carve-out is the card's own, so
    /// it keeps the meter unmarked — but the pool beside it is RAM, not a
    /// spill across PCIe, so it is not called gtt either.
    #[test]
    fn an_apu_meters_its_carve_out_and_calls_the_rest_shared() {
        let g = GpuSnapshot {
            integrated: true,
            vram_used_bytes: Some(412 << 20),
            vram_total_bytes: Some(512 << 20),
            gtt_used_bytes: Some(3 << 30),
            gtt_total_bytes: Some(16 << 30),
            ..Default::default()
        };
        let m = g.mem_primary();
        assert_eq!((m.used, m.total), (Some(412 << 20), Some(512 << 20)));
        assert!(!m.shared, "the carve-out is reserved for the GPU alone");
        let s = g
            .mem_secondary()
            .expect("the system pool is the other half");
        assert!(s.shared, "nothing spilled anywhere on an APU");
    }

    /// A card that published nothing keeps saying nothing: no pool is
    /// invented from the absence of the other one.
    #[test]
    fn a_card_with_no_memory_figures_at_all_reports_none() {
        let g = GpuSnapshot::default();
        let m = g.mem_primary();
        assert_eq!((m.used, m.total), (None, None));
        assert_eq!(g.mem_secondary(), None);
        assert_eq!(g.mem_pct(), None);
    }

    /// Half a reading is a reading. A backend that got the usage but not the
    /// total must not have it dropped by the fall-through to the system pool.
    #[test]
    fn one_known_half_still_selects_the_pool_it_came_from() {
        let g = GpuSnapshot {
            vram_used_bytes: Some(2 << 30),
            gtt_used_bytes: Some(9 << 30),
            ..Default::default()
        };
        let m = g.mem_primary();
        assert_eq!((m.used, m.total), (Some(2 << 30), None));
        assert_eq!(m.pct(), None, "no total, so no fill level to draw");
        assert!(g.mem_secondary().is_some());
    }

    /// An empty pool that was actually read stays a reading, at every layer.
    #[test]
    fn a_measured_empty_pool_is_not_an_absent_one() {
        let g = GpuSnapshot {
            vram_used_bytes: Some(0),
            vram_total_bytes: Some(8 << 30),
            ..Default::default()
        };
        assert_eq!(g.mem_pct(), Some(0.0));
        // A pool with no size cannot yield a percentage of anything.
        assert_eq!(
            MemReadout {
                used: Some(0),
                total: Some(0),
                shared: false
            }
            .pct(),
            None
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted child: one entry per poll, `None` meaning that poll fails.
    /// The last entry repeats once the script runs out.
    struct Stub {
        name: &'static str,
        ticks: Vec<Option<Vec<&'static str>>>,
        tick: usize,
        /// (pid, child-local gpu_index)
        procs: Vec<(u32, usize)>,
        driver: Option<&'static str>,
        signal: bool,
        /// Whether devices carry an id (the device name doubles as one).
        ids: bool,
    }

    impl Stub {
        fn new(name: &'static str, ticks: Vec<Option<Vec<&'static str>>>) -> Self {
            Self {
                name,
                ticks,
                tick: 0,
                procs: Vec::new(),
                driver: None,
                signal: true,
                ids: true,
            }
        }
        /// A backend that cannot identify its devices.
        fn no_ids(mut self) -> Self {
            self.ids = false;
            self
        }
        fn procs(mut self, procs: &[(u32, usize)]) -> Self {
            self.procs = procs.to_vec();
            self
        }
        fn driver(mut self, driver: &'static str) -> Self {
            self.driver = Some(driver);
            self
        }
        fn no_signal(mut self) -> Self {
            self.signal = false;
            self
        }
    }

    impl GpuBackend for Stub {
        fn name(&self) -> &'static str {
            self.name
        }
        fn poll(&mut self) -> Result<Vec<GpuSnapshot>> {
            let i = self.tick.min(self.ticks.len() - 1);
            self.tick += 1;
            match &self.ticks[i] {
                Some(names) => Ok(names
                    .iter()
                    .map(|n| GpuSnapshot {
                        name: (*n).to_string(),
                        device_id: self.ids.then(|| (*n).to_string()),
                        ..Default::default()
                    })
                    .collect()),
                None => anyhow::bail!("{} is down", self.name),
            }
        }
        fn processes(&mut self) -> Vec<GpuProcess> {
            self.procs
                .iter()
                .map(|&(pid, gpu_index)| GpuProcess {
                    pid,
                    gpu_index,
                    ..Default::default()
                })
                .collect()
        }
        fn driver_info(&self) -> Option<String> {
            self.driver.map(str::to_string)
        }
        fn can_signal(&self) -> bool {
            self.signal
        }
    }

    fn names(snaps: &[GpuSnapshot]) -> Vec<&str> {
        snaps.iter().map(|s| s.name.as_str()).collect()
    }

    fn ids(snaps: &[GpuSnapshot]) -> Vec<Option<&str>> {
        snaps.iter().map(|s| s.device_id.as_deref()).collect()
    }

    #[test]
    fn one_backend_is_not_wrapped() {
        let b = compose(vec![Box::new(Stub::new("solo", vec![Some(vec!["a"])]))]).unwrap();
        assert_eq!(b.name(), "solo");
        assert!(compose(Vec::new()).is_err());
    }

    #[test]
    fn children_concatenate_in_probe_order() {
        let mut b = CompositeBackend::new(vec![
            Box::new(Stub::new("nv", vec![Some(vec!["4090", "4080"])])),
            Box::new(Stub::new("amd", vec![Some(vec!["APU"])])),
        ]);
        assert_eq!(names(&b.poll().unwrap()), ["4090", "4080", "APU"]);
        assert_eq!(b.name(), "multi");
    }

    #[test]
    fn process_indices_rebase_onto_the_child_offset() {
        let mut b = CompositeBackend::new(vec![
            Box::new(Stub::new("nv", vec![Some(vec!["4090", "4080"])]).procs(&[(1, 0), (2, 1)])),
            Box::new(Stub::new("amd", vec![Some(vec!["APU"])]).procs(&[(3, 0)])),
        ]);
        b.poll().unwrap();
        let procs: Vec<(u32, usize)> = b.processes().iter().map(|p| (p.pid, p.gpu_index)).collect();
        assert_eq!(procs, [(1, 0), (2, 1), (3, 2)]);
    }

    #[test]
    fn a_failing_child_holds_its_slots_open() {
        let mut b = CompositeBackend::new(vec![
            Box::new(Stub::new("nv", vec![Some(vec!["4090", "4080"]), None]).procs(&[(1, 0)])),
            Box::new(Stub::new("amd", vec![Some(vec!["APU"])]).procs(&[(3, 0)])),
        ]);
        b.poll().unwrap();
        let snaps = b.poll().unwrap();
        assert_eq!(
            names(&snaps),
            ["4090 (unavailable)", "4080 (unavailable)", "APU"]
        );
        // The surviving child's card and its process rows stay on index 2.
        let procs: Vec<(u32, usize)> = b.processes().iter().map(|p| (p.pid, p.gpu_index)).collect();
        assert_eq!(procs, [(1, 0), (3, 2)]);
    }

    #[test]
    fn a_short_child_holds_its_slots_open() {
        let mut b = CompositeBackend::new(vec![
            Box::new(Stub::new(
                "nv",
                vec![Some(vec!["4090", "4080"]), Some(vec!["4090"])],
            )),
            Box::new(Stub::new("amd", vec![Some(vec!["APU"])])),
        ]);
        b.poll().unwrap();
        assert_eq!(
            names(&b.poll().unwrap()),
            ["4090", "4080 (unavailable)", "APU"]
        );
    }

    /// Two children can mint the same id string — nothing in the trait stops
    /// them — and two devices sharing a key would share one set of graphs.
    #[test]
    fn device_ids_are_namespaced_per_child() {
        let mut b = CompositeBackend::new(vec![
            Box::new(Stub::new("nv", vec![Some(vec!["gpu0"])])),
            Box::new(Stub::new("amd", vec![Some(vec!["gpu0"])])),
        ]);
        assert_eq!(
            ids(&b.poll().unwrap()),
            [Some("nv#0:gpu0"), Some("amd#1:gpu0")]
        );

        // Even two children reporting the same backend name stay apart: the
        // child index is part of the namespace.
        let mut same = CompositeBackend::new(vec![
            Box::new(Stub::new("nv", vec![Some(vec!["gpu0"])])),
            Box::new(Stub::new("nv", vec![Some(vec!["gpu0"])])),
        ]);
        assert_eq!(
            ids(&same.poll().unwrap()),
            [Some("nv#0:gpu0"), Some("nv#1:gpu0")]
        );
    }

    /// A child that cannot identify its devices must not be handed a made-up
    /// id here — `App` falls back to position, and it has to know that.
    #[test]
    fn a_child_without_ids_reports_none() {
        let mut b = CompositeBackend::new(vec![
            Box::new(Stub::new("nv", vec![Some(vec!["4090"])])),
            Box::new(Stub::new("pdh", vec![Some(vec!["iGPU"])]).no_ids()),
        ]);
        assert_eq!(ids(&b.poll().unwrap()), [Some("nv#0:4090"), None]);
    }

    /// The placeholder holding a vanished device's slot is that same device,
    /// briefly unreadable — so its graphs and peaks must carry on, which
    /// means it keeps the id.
    #[test]
    fn a_placeholder_keeps_the_id_of_the_slot_it_holds() {
        let mut b = CompositeBackend::new(vec![
            Box::new(Stub::new(
                "nv",
                vec![Some(vec!["4090", "4080"]), Some(vec!["4090"])],
            )),
            Box::new(Stub::new("amd", vec![Some(vec!["APU"]), None])),
        ]);
        b.poll().unwrap();
        assert_eq!(
            ids(&b.poll().unwrap()),
            [Some("nv#0:4090"), Some("nv#0:4080"), Some("amd#1:APU")]
        );
    }

    #[test]
    fn a_process_outside_its_child_span_is_dropped() {
        let mut b = CompositeBackend::new(vec![
            Box::new(Stub::new("nv", vec![Some(vec!["4090"])]).procs(&[(1, 0), (2, 7)])),
            Box::new(Stub::new("amd", vec![Some(vec!["APU"])]).procs(&[(3, 0)])),
        ]);
        b.poll().unwrap();
        let procs: Vec<(u32, usize)> = b.processes().iter().map(|p| (p.pid, p.gpu_index)).collect();
        assert_eq!(procs, [(1, 0), (3, 1)]);
    }

    /// The tri-vendor rig, which is the whole reason this type exists: three
    /// children of three different sizes, every card listed once, and a
    /// process on the LAST child's SECOND device landing on the global index
    /// its card actually occupies. Offsets that used the poll's device count
    /// instead of the slot count, or that forgot to accumulate, both survive
    /// two children and fall over here.
    #[test]
    fn three_children_concatenate_and_rebase_processes() {
        let mut b = CompositeBackend::new(vec![
            Box::new(Stub::new("nvml", vec![Some(vec!["4090", "4080"])]).procs(&[(1, 0), (2, 1)])),
            Box::new(Stub::new("amdgpu", vec![Some(vec!["780M"])]).procs(&[(3, 0)])),
            Box::new(Stub::new("intel", vec![Some(vec!["UHD", "Arc"])]).procs(&[(4, 0), (5, 1)])),
        ]);
        assert_eq!(
            names(&b.poll().unwrap()),
            ["4090", "4080", "780M", "UHD", "Arc"]
        );
        assert_eq!(
            ids(&b.poll().unwrap()),
            [
                Some("nvml#0:4090"),
                Some("nvml#0:4080"),
                Some("amdgpu#1:780M"),
                Some("intel#2:UHD"),
                Some("intel#2:Arc"),
            ]
        );
        let procs: Vec<(u32, usize)> = b.processes().iter().map(|p| (p.pid, p.gpu_index)).collect();
        assert_eq!(procs, [(1, 0), (2, 1), (3, 2), (4, 3), (5, 4)]);
    }

    /// A genuine hotplug in the middle child. Slots are a high-water mark, so
    /// shrinking holds the later children still and growing pushes them down
    /// by exactly one — and the process rebasing has to track that on the same
    /// poll, or the last child's rows land on the vanished card.
    #[test]
    fn a_middle_child_changing_size_moves_the_children_after_it() {
        let mut b = CompositeBackend::new(vec![
            Box::new(Stub::new("nvml", vec![Some(vec!["4090"])]).procs(&[(1, 0)])),
            Box::new(Stub::new(
                "amdgpu",
                vec![
                    Some(vec!["W7900", "W7800"]),
                    Some(vec!["W7900"]),
                    Some(vec!["W7900", "W7800", "W7700"]),
                ],
            )),
            Box::new(Stub::new("intel", vec![Some(vec!["Arc"])]).procs(&[(9, 0)])),
        ]);
        let last = |b: &mut CompositeBackend| {
            b.processes()
                .iter()
                .map(|p| (p.pid, p.gpu_index))
                .collect::<Vec<_>>()
        };

        assert_eq!(names(&b.poll().unwrap()), ["4090", "W7900", "W7800", "Arc"]);
        assert_eq!(last(&mut b), [(1, 0), (9, 3)]);

        // Shrunk: the placeholder holds slot 2, so Arc does not move and keeps
        // both its id and its process row.
        let snaps = b.poll().unwrap();
        assert_eq!(
            names(&snaps),
            ["4090", "W7900", "W7800 (unavailable)", "Arc"]
        );
        assert_eq!(ids(&snaps)[2], Some("amdgpu#1:W7800"));
        assert_eq!(last(&mut b), [(1, 0), (9, 3)]);

        // Grown past its high-water mark: Arc moves down one, and its process
        // row moves with it rather than staying on the new AMD card.
        let snaps = b.poll().unwrap();
        assert_eq!(names(&snaps), ["4090", "W7900", "W7800", "W7700", "Arc"]);
        assert_eq!(ids(&snaps)[4], Some("intel#2:Arc"));
        assert_eq!(last(&mut b), [(1, 0), (9, 4)]);
    }

    /// A child that reports nothing at all owns no slots, so it must not
    /// consume an index — the children after it would each be off by one —
    /// and its own process rows have nowhere to go.
    #[test]
    fn a_child_with_no_devices_takes_no_slots() {
        let mut b = CompositeBackend::new(vec![
            Box::new(Stub::new("nvml", vec![Some(vec!["4090"])]).procs(&[(1, 0)])),
            Box::new(Stub::new("empty", vec![Some(Vec::new())]).procs(&[(7, 0)])),
            Box::new(Stub::new("amdgpu", vec![Some(vec!["780M"])]).procs(&[(3, 0)])),
        ]);
        assert_eq!(names(&b.poll().unwrap()), ["4090", "780M"]);
        let procs: Vec<(u32, usize)> = b.processes().iter().map(|p| (p.pid, p.gpu_index)).collect();
        assert_eq!(procs, [(1, 0), (3, 1)]);
    }

    /// A device that reappears at a lower slot within its child is live at its
    /// new index; the slot it vacated must stop claiming it. Inheriting it
    /// blind put the same `device_id` on two rows of one poll — one set of
    /// graphs for two cards, and a `--json` record naming the same GPU twice.
    #[test]
    fn a_vacated_slot_does_not_duplicate_a_live_device_id() {
        let mut b = CompositeBackend::new(vec![
            Box::new(Stub::new("nv", vec![Some(vec!["A", "B"]), Some(vec!["B"])])),
            Box::new(Stub::new("amd", vec![Some(vec!["APU"])])),
        ]);
        b.poll().unwrap();
        let snaps = b.poll().unwrap();
        assert_eq!(ids(&snaps), [Some("nv#0:B"), None, Some("amd#1:APU")]);
        // And the placeholder stops printing the name of a card that is on
        // screen one row above it.
        assert_eq!(names(&snaps), ["B", "nv GPU 1 (unavailable)", "APU"]);
    }

    #[test]
    fn every_child_failing_is_a_poll_error() {
        let mut b = CompositeBackend::new(vec![
            Box::new(Stub::new("nv", vec![None])),
            Box::new(Stub::new("amd", vec![None])),
        ]);
        let err = format!("{:#}", b.poll().unwrap_err());
        assert!(
            err.contains("nv is down") && err.contains("amd is down"),
            "{err}"
        );
    }

    /// Nothing came back and something failed: the error has to surface, or
    /// the header reads "no error" over an empty device list.
    #[test]
    fn an_empty_result_surfaces_a_partial_failure() {
        let mut b = CompositeBackend::new(vec![
            Box::new(Stub::new("nv", vec![None])),
            Box::new(Stub::new("amd", vec![Some(Vec::new())])),
        ]);
        let err = format!("{:#}", b.poll().unwrap_err());
        assert!(err.contains("nv is down"), "{err}");
    }

    #[test]
    fn can_signal_is_the_and_of_the_children() {
        let live = || Box::new(Stub::new("nv", vec![Some(vec!["4090"])]));
        assert!(CompositeBackend::new(vec![live(), live()]).can_signal());
        assert!(
            !CompositeBackend::new(vec![
                live(),
                Box::new(Stub::new("mock", vec![Some(vec!["fake"])]).no_signal()),
            ])
            .can_signal()
        );
        // Three children: one un-signalable in the middle still disables the
        // kill path, since the process table is one list.
        assert!(CompositeBackend::new(vec![live(), live(), live()]).can_signal());
        assert!(
            !CompositeBackend::new(vec![
                live(),
                Box::new(Stub::new("replay", vec![Some(vec!["rec"])]).no_signal()),
                live(),
            ])
            .can_signal()
        );
    }

    /// Stands in for `windows::probe`: a vendor-generic backend that sees
    /// every adapter on the rig and reports the ones no vendor backend
    /// claimed, or nothing at all when that leaves it empty.
    fn generic(
        adapters: &[(&'static str, u16)],
    ) -> impl FnOnce(&[u16]) -> Option<Box<dyn GpuBackend>> {
        let adapters = adapters.to_vec();
        move |claimed| {
            let kept: Vec<&'static str> = adapters
                .iter()
                .filter(|(_, vendor)| !claimed.contains(vendor))
                .map(|&(name, _)| name)
                .collect();
            (!kept.is_empty())
                .then(|| Box::new(Stub::new("pdh", vec![Some(kept)])) as Box<dyn GpuBackend>)
        }
    }

    fn vendor(
        name: &'static str,
        vendors: &'static [u16],
    ) -> (Box<dyn GpuBackend>, &'static [u16]) {
        (
            Box::new(Stub::new(name, vec![Some(vec!["card"])])) as Box<dyn GpuBackend>,
            vendors,
        )
    }

    /// The mixed Windows rig: NVML covers the NVIDIA card, PDH contributes the
    /// AMD iGPU NVML cannot see, and neither card is listed twice.
    #[test]
    fn a_generic_backend_contributes_only_unclaimed_vendors() {
        let nv = (
            Box::new(Stub::new("nvml", vec![Some(vec!["4090"])])) as Box<dyn GpuBackend>,
            &[PCI_VENDOR_NVIDIA][..],
        );
        let mut b = compose_with_generic(
            vec![nv],
            generic(&[("4090", PCI_VENDOR_NVIDIA), ("780M", PCI_VENDOR_AMD)]),
        )
        .unwrap();
        assert_eq!(b.name(), "multi");
        assert_eq!(names(&b.poll().unwrap()), ["4090", "780M"]);
    }

    /// Nothing overlapping: every adapter the generic backend sees is new, and
    /// claims from several vendor backends accumulate rather than the last one
    /// winning.
    #[test]
    fn a_generic_backend_without_overlap_contributes_everything() {
        let mut b = compose_with_generic(
            vec![
                vendor("nvml", &[PCI_VENDOR_NVIDIA]),
                vendor("amdgpu", &[PCI_VENDOR_AMD]),
            ],
            generic(&[("UHD", PCI_VENDOR_INTEL), ("Arc", PCI_VENDOR_INTEL)]),
        )
        .unwrap();
        assert_eq!(names(&b.poll().unwrap()), ["card", "card", "UHD", "Arc"]);

        // And with both of those vendors claimed it would have kept neither.
        let mut both = compose_with_generic(
            vec![
                vendor("nvml", &[PCI_VENDOR_NVIDIA]),
                vendor("amdgpu", &[PCI_VENDOR_AMD]),
            ],
            generic(&[("4090", PCI_VENDOR_NVIDIA), ("780M", PCI_VENDOR_AMD)]),
        )
        .unwrap();
        assert_eq!(names(&both.poll().unwrap()), ["card", "card"]);
    }

    /// The AMD/Intel-only Windows box: nothing else probes, so the generic
    /// backend filters nothing and is handed back unwrapped, as before.
    #[test]
    fn a_generic_backend_alone_is_unfiltered_and_unwrapped() {
        let mut b = compose_with_generic(
            Vec::new(),
            generic(&[("780M", PCI_VENDOR_AMD), ("UHD", PCI_VENDOR_INTEL)]),
        )
        .unwrap();
        assert_eq!(b.name(), "pdh");
        assert_eq!(names(&b.poll().unwrap()), ["780M", "UHD"]);
    }

    /// The NVIDIA-only Windows box: the generic backend has nothing left, so
    /// NVML stays the whole backend rather than gaining an empty peer.
    #[test]
    fn a_fully_claimed_generic_backend_drops_out() {
        let b = compose_with_generic(
            vec![vendor("nvml", &[PCI_VENDOR_NVIDIA])],
            generic(&[("4090", PCI_VENDOR_NVIDIA)]),
        )
        .unwrap();
        assert_eq!(b.name(), "nvml");
    }

    /// The Windows tri-vendor box, end to end. Only NVML probes there — the
    /// AMD and Intel backends are inner-gated to Linux and return `None` — so
    /// only NVIDIA is claimed and PDH must contribute BOTH the AMD and the
    /// Intel adapter. A claim keyed off the platform rather than off the probe
    /// actually returning a backend would silence one or both of them.
    #[test]
    fn a_vendor_that_did_not_probe_claims_nothing() {
        let mut b = compose_with_generic(
            vec![vendor("nvml", &[PCI_VENDOR_NVIDIA])],
            generic(&[
                ("4090", PCI_VENDOR_NVIDIA),
                ("780M", PCI_VENDOR_AMD),
                ("UHD", PCI_VENDOR_INTEL),
            ]),
        )
        .unwrap();
        assert_eq!(b.name(), "multi");
        // NVML's card once, from NVML; the other two vendors from PDH.
        assert_eq!(names(&b.poll().unwrap()), ["card", "780M", "UHD"]);
        assert_eq!(
            b.driver_info().as_deref(),
            Some("nvml · pdh"),
            "a child must not drop out of the header"
        );
    }

    #[test]
    fn nothing_probing_at_all_is_still_an_error() {
        assert!(compose_with_generic(Vec::new(), generic(&[])).is_err());
    }

    #[test]
    fn driver_info_joins_and_names_the_children() {
        let b = CompositeBackend::new(vec![
            Box::new(Stub::new("nvml", vec![Some(vec!["4090"])]).driver("driver 550.1")),
            Box::new(Stub::new("amdgpu", vec![Some(vec!["APU"])]).driver("amdgpu · kernel 7.1")),
            Box::new(Stub::new("intel", vec![Some(vec!["iGPU"])])),
        ]);
        assert_eq!(
            b.driver_info().as_deref(),
            Some("nvml driver 550.1 · amdgpu · kernel 7.1 · intel")
        );
    }
}
