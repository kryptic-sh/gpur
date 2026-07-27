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

impl GpuSnapshot {
    /// Fill level, or `None` when either figure is unknown or the pool has no
    /// size. A meter drawn from a fabricated 0 here is the whole point of the
    /// `Option`s above, so this refuses to invent one.
    pub fn vram_pct(&self) -> Option<f64> {
        let (used, total) = (self.vram_used_bytes?, self.vram_total_bytes?);
        (total > 0).then(|| used as f64 / total as f64 * 100.0)
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
    pub gpu_mem_bytes: u64,
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

/// One child of a [`CompositeBackend`], plus the bookkeeping that keeps its
/// devices on the same indices for the life of the session.
struct Child {
    backend: Box<dyn GpuBackend>,
    /// Slots this child owns in the concatenated snapshot vec — a high-water
    /// mark, never shrunk. `App` keys history, session peaks and folded state
    /// positionally, so a child that fails or returns short for one tick must
    /// not slide every later child's devices onto another card's graphs.
    /// Growth (a hotplugged device) does shift the children after it, once —
    /// nothing positional can avoid that without device identity.
    slots: usize,
    /// Device names from the last poll that saw each slot, to label the
    /// placeholders that hold the slots open.
    names: Vec<String>,
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
        for c in &mut self.children {
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
            c.slots = c.slots.max(snaps.len());
            c.names.resize(c.slots, String::new());
            for (slot, snap) in c.names.iter_mut().zip(&snaps) {
                snap.name.clone_into(slot);
            }
            for i in snaps.len()..c.slots {
                let label = match c.names.get(i) {
                    Some(n) if !n.is_empty() => n.clone(),
                    _ => format!("{} GPU {i}", c.backend.name()),
                };
                snaps.push(GpuSnapshot {
                    name: format!("{label} (unavailable)"),
                    ..Default::default()
                });
            }
            out.append(&mut snaps);
        }
        if !errors.is_empty() && errors.len() == self.children.len() {
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
    // devices — so every one that probes adds cards no other one reports. The
    // probes that find nothing are a failed `Nvml::init` and two readdirs of
    // /sys/class/drm, which is why probing all of them is affordable.
    let mut found: Vec<Box<dyn GpuBackend>> = [
        nvidia::probe(),
        amd::probe(),
        intel::probe(),
        apple::probe(),
    ]
    .into_iter()
    .flatten()
    .collect();
    // PDH is vendor-generic (Task Manager's counters): it enumerates every
    // adapter, including ones a vendor backend above already claimed, so it
    // stays a fallback rather than a peer — otherwise an NVIDIA rig on Windows
    // would list each card twice.
    if found.is_empty()
        && let Some(b) = windows::probe()
    {
        found.push(b);
    }
    compose(found)
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
            }
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
