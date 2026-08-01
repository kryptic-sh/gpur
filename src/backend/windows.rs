//! Windows vendor-generic backend: PDH "GPU Engine" / "GPU Adapter Memory"
//! performance counters (what Task Manager uses) + DXGI for adapter names and
//! VRAM totals. Covers AMD and Intel on Windows, where NVML doesn't apply.
//! It runs alongside the vendor backends rather than instead of them —
//! `detect()` passes the PCI vendors those already cover and the adapters from
//! those vendors are dropped here, so an NVIDIA dGPU comes from NVML while the
//! AMD or Intel iGPU beside it still shows up.
//! Temperature/fan/clocks are not exposed by PDH — those need ADLX (TODO).

use super::GpuBackend;

/// `claimed` lists PCI vendor ids another backend already reports devices for.
/// `None` when nothing is left to report, so an NVIDIA-only rig keeps NVML as
/// its sole backend instead of gaining an empty peer.
pub fn probe(claimed: &[u16]) -> Option<Box<dyn GpuBackend>> {
    #[cfg(windows)]
    if let Some(b) = win::probe(claimed) {
        return Some(b);
    }
    #[cfg(not(windows))]
    let _ = claimed;
    None
}

/// PDH instance-name parsing, the iGPU heuristic and the claimed-vendor filter.
/// Pure functions, kept out of the `#[cfg(windows)]` module below so they
/// compile and are testable on any host — a fixed-width LUID slice shipped
/// broken precisely because nothing off Windows could exercise it.
#[cfg_attr(not(windows), allow(dead_code))]
mod parse {
    /// "pid_1234_luid_0x..._0x..._phys_0_engtype_3d" -> luid key + engine type.
    pub fn luid_and_engtype(instance: &str) -> Option<(String, String)> {
        let luid = luid_prefix(instance)?;
        let eng = instance.split("engtype_").nth(1)?.to_string();
        Some((luid, eng))
    }

    /// "pid_1234_luid_..." -> 1234
    pub fn pid_prefix(instance: &str) -> Option<u32> {
        instance
            .strip_prefix("pid_")?
            .split('_')
            .next()?
            .parse()
            .ok()
    }

    /// Extract "luid_0x????????_0x????????" from anywhere in the instance name.
    /// Matched structurally over `_`-separated tokens: slicing a fixed width
    /// truncates the key on the slightest miscount, and a truncated key also
    /// collides adapters that differ only in the low bits of `LowPart`.
    pub fn luid_prefix(instance: &str) -> Option<String> {
        let tokens: Vec<&str> = instance.split('_').collect();
        tokens.windows(3).find_map(|w| {
            (w[0] == "luid" && is_hex32(w[1]) && is_hex32(w[2]))
                .then(|| format!("luid_{}_{}", w[1], w[2]))
        })
    }

    /// "0x" followed by exactly 8 hex digits — one half of a LUID.
    fn is_hex32(token: &str) -> bool {
        token
            .strip_prefix("0x")
            .is_some_and(|h| h.len() == 8 && h.bytes().all(|b| b.is_ascii_hexdigit()))
    }

    /// iGPUs carve from system RAM: a small fixed dedicated pool dwarfed by the
    /// shared one. Both halves matter — an absolute threshold alone files a
    /// 512 MiB RX 550 or a GT 710 as integrated (its VRAM total then becomes
    /// 16-32 GB of shared RAM), while dominance alone would too, since shared
    /// outweighs dedicated on any small discrete card. Ties break to discrete:
    /// reading a large APU carve-out as dedicated is the milder error.
    pub fn is_integrated(dedicated_bytes: u64, shared_bytes: u64) -> bool {
        dedicated_bytes < 512 * 1024 * 1024 && shared_bytes >= dedicated_bytes.saturating_mul(4)
    }

    /// GPU memory to charge one `(pid, luid)` row, from that key's lookups in
    /// the two `GPU Process Memory` counters. `None` means PDH published no
    /// memory instance for the process at all.
    ///
    /// Which counter is "the relevant" one follows the adapter: an integrated
    /// adapter's client allocations live in the shared pool, a discrete one's
    /// in dedicated, and that is the pool the row reports.
    ///
    /// Three cases, and the middle one is the reason this is a function:
    /// - the relevant counter carries the key: a real reading, `0` included.
    /// - neither counter carries it: PDH accounted for none of this process's
    ///   memory — the key reached here from the engine-utilization map alone —
    ///   so the figure is unknown. `0` would assert an empty pool.
    /// - only the other counter carries it: PDH *did* account for the process,
    ///   and instances only exist where there is something to report, so the
    ///   silence in the pool being asked about is a genuine nothing.
    pub fn proc_mem_bytes(
        integrated: bool,
        dedicated: Option<u64>,
        shared: Option<u64>,
    ) -> Option<u64> {
        let (relevant, other) = if integrated {
            (shared, dedicated)
        } else {
            (dedicated, shared)
        };
        relevant.or_else(|| other.map(|_| 0))
    }

    /// Drop the adapters a vendor-specific backend already reports, keeping
    /// DXGI's order for the rest.
    ///
    /// Matching is by PCI vendor id, not per device, because the two sides
    /// share no identifier: `DXGI_ADAPTER_DESC1` carries `VendorId`/`DeviceId`
    /// and an adapter LUID but no bus/device/function, while NVML identifies a
    /// device by UUID or PCI BDF. "Is this exact adapter that exact NVML
    /// device" is therefore unanswerable without dragging in SetupAPI or WMI to
    /// map LUID to BDF.
    ///
    /// Failure mode of the coarser match: a card that DXGI enumerates but its
    /// vendor's backend does not report — an NVIDIA card NVML refuses while
    /// NVML still initializes for another — vanishes entirely rather than
    /// appearing as a PDH row. That is the pre-existing blind spot (PDH used to
    /// be skipped wholesale whenever NVML probed), now narrowed to one vendor,
    /// and it fails toward hiding a card rather than listing the same GPU twice
    /// under two backends, which is the worse and more confusing bug.
    ///
    /// `vendor_of` widens to `u32` because that is DXGI's field width; the ids
    /// themselves are 16-bit, and comparing at full width keeps a nonsense
    /// high half from matching a real vendor.
    pub fn retain_unclaimed<T>(
        adapters: Vec<T>,
        claimed: &[u16],
        vendor_of: impl Fn(&T) -> u32,
    ) -> Vec<T> {
        adapters
            .into_iter()
            .filter(|a| {
                let vendor = vendor_of(a);
                !claimed.iter().any(|&c| u32::from(c) == vendor)
            })
            .collect()
    }
}

#[cfg(windows)]
mod win {
    use super::parse::{
        is_integrated, luid_and_engtype, luid_prefix, pid_prefix, proc_mem_bytes, retain_unclaimed,
    };
    use crate::backend::{GpuBackend, GpuProcess, GpuSnapshot, ProcKind, clamp_pct};
    use anyhow::Result;
    use std::collections::HashMap;
    use std::hash::Hash;
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1,
    };
    use windows::Win32::System::Performance::{
        PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
        PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
        PdhOpenQueryW,
    };
    use windows::core::{PCWSTR, w};

    const MICROSOFT_BASIC_RENDER: u32 = 0x1414;

    pub fn probe(claimed: &[u16]) -> Option<Box<dyn GpuBackend>> {
        // Filtered once, here, and never again: `poll` maps adapters onto
        // snapshot indices positionally and its contract is that the order is
        // stable across calls, so recomputing the set per poll would shift the
        // device list under the user whenever a vendor backend blinked.
        let adapters = retain_unclaimed(enum_adapters(), claimed, |a| a.vendor_id);
        if adapters.is_empty() {
            return None;
        }
        let mut query: PDH_HQUERY = Default::default();
        unsafe {
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }
        }
        let add = |path: PCWSTR| -> Option<PDH_HCOUNTER> {
            let mut c: PDH_HCOUNTER = Default::default();
            (unsafe { PdhAddEnglishCounterW(query, path, 0, &mut c) } == 0).then_some(c)
        };
        let util = add(w!(r"\GPU Engine(*)\Utilization Percentage"));
        let dedicated = add(w!(r"\GPU Adapter Memory(*)\Dedicated Usage"));
        let shared = add(w!(r"\GPU Adapter Memory(*)\Shared Usage"));
        let proc_dedicated = add(w!(r"\GPU Process Memory(*)\Dedicated Usage"));
        let proc_shared = add(w!(r"\GPU Process Memory(*)\Shared Usage"));
        if util.is_none() && dedicated.is_none() {
            unsafe { PdhCloseQuery(query) };
            return None;
        }
        // Prime: rate counters need two collections before the first read.
        unsafe { PdhCollectQueryData(query) };
        Some(Box::new(PdhBackend {
            query,
            util,
            dedicated,
            shared,
            proc_dedicated,
            proc_shared,
            adapters,
            last_procs: Vec::new(),
        }))
    }

    struct Adapter {
        /// "luid_0x00000000_0x0000c4cf" — lowercase key matched against
        /// counter instance names.
        luid_key: String,
        name: String,
        /// DXGI's `VendorId` — the PCI vendor id, used once at probe to drop
        /// adapters another backend already claims.
        vendor_id: u32,
        vram_total: u64,
        integrated: bool,
    }

    struct PdhBackend {
        query: PDH_HQUERY,
        util: Option<PDH_HCOUNTER>,
        dedicated: Option<PDH_HCOUNTER>,
        shared: Option<PDH_HCOUNTER>,
        proc_dedicated: Option<PDH_HCOUNTER>,
        proc_shared: Option<PDH_HCOUNTER>,
        adapters: Vec<Adapter>,
        /// Built during poll (same PDH collection), served by processes().
        last_procs: Vec<GpuProcess>,
    }

    impl Drop for PdhBackend {
        fn drop(&mut self) {
            unsafe { PdhCloseQuery(self.query) };
        }
    }

    impl GpuBackend for PdhBackend {
        fn name(&self) -> &'static str {
            "pdh"
        }

        fn poll(&mut self) -> Result<Vec<GpuSnapshot>> {
            unsafe { PdhCollectQueryData(self.query) };

            // (luid, engtype) -> summed % across processes, and
            // (pid, luid, engtype) -> % for the process table.
            let mut engine: HashMap<(String, String), f64> = HashMap::new();
            let mut proc_engine: HashMap<(u32, String, String), f64> = HashMap::new();
            let mut proc_graphics: HashMap<(u32, String), bool> = HashMap::new();
            if let Some(c) = self.util {
                for (inst, v) in read_array(c) {
                    let Some((luid, eng)) = luid_and_engtype(&inst) else {
                        continue;
                    };
                    *engine.entry((luid.clone(), eng.clone())).or_default() += v;
                    if let Some(pid) = pid_prefix(&inst) {
                        *proc_engine
                            .entry((pid, luid.clone(), eng.clone()))
                            .or_default() += v;
                        let g = proc_graphics.entry((pid, luid)).or_default();
                        *g |= eng.contains("3d") || eng.contains("graphics");
                    }
                }
            }
            // Busiest engine type per adapter = Task Manager's GPU %; the
            // video engine types feed the enc/dec readouts.
            let mut util_by_luid: HashMap<String, f64> = HashMap::new();
            let mut enc_by_luid: HashMap<String, f64> = HashMap::new();
            let mut dec_by_luid: HashMap<String, f64> = HashMap::new();
            for ((luid, eng), v) in engine {
                if eng.contains("videoencode") {
                    *enc_by_luid.entry(luid.clone()).or_default() += v;
                } else if eng.contains("videodecode") {
                    *dec_by_luid.entry(luid.clone()).or_default() += v;
                }
                let e = util_by_luid.entry(luid).or_default();
                *e = e.max(v);
            }
            // Same convention per process.
            let mut util_by_proc: HashMap<(u32, String), f64> = HashMap::new();
            for ((pid, luid, _), v) in proc_engine {
                let e = util_by_proc.entry((pid, luid)).or_default();
                *e = e.max(v);
            }

            let dedicated = sum_by(self.dedicated, luid_prefix);
            let shared = sum_by(self.shared, luid_prefix);

            // Per-process memory: (pid, luid) -> bytes.
            let proc_key = |inst: &str| Some((pid_prefix(inst)?, luid_prefix(inst)?));
            let proc_ded = sum_by(self.proc_dedicated, proc_key);
            let proc_shr = sum_by(self.proc_shared, proc_key);

            let luid_to_gpu: HashMap<&str, (usize, bool)> = self
                .adapters
                .iter()
                .enumerate()
                .map(|(i, a)| (a.luid_key.as_str(), (i, a.integrated)))
                .collect();
            let mut procs: HashMap<(u32, String), GpuProcess> = HashMap::new();
            let keys: Vec<(u32, String)> = util_by_proc
                .keys()
                .chain(proc_ded.keys())
                .chain(proc_shr.keys())
                .cloned()
                .collect();
            for key in keys {
                if procs.contains_key(&key) {
                    continue;
                }
                let Some(&(gpu_index, integrated)) = luid_to_gpu.get(key.1.as_str()) else {
                    continue;
                };
                let mem = proc_mem_bytes(
                    integrated,
                    proc_ded.get(&key).copied(),
                    proc_shr.get(&key).copied(),
                );
                let kind = if proc_graphics.get(&key).copied().unwrap_or(false) {
                    ProcKind::Graphics
                } else {
                    ProcKind::Compute
                };
                let p = GpuProcess {
                    pid: key.0,
                    gpu_index,
                    kind,
                    gpu_util_pct: util_by_proc.get(&key).copied().map(clamp_pct),
                    gpu_mem_bytes: mem,
                    ..Default::default()
                };
                procs.insert(key, p);
            }
            // No activity filter: every other backend lists any process holding
            // a context, and filtering here made idle rows blink in and out on
            // Windows alone. The sort sinks zero rows to the bottom anyway.
            self.last_procs = procs.into_values().collect();

            Ok(self
                .adapters
                .iter()
                .map(|a| {
                    // No counter instance matched this adapter's LUID: PDH
                    // published nothing for it, which is not the same as the
                    // adapter sitting idle with an empty pool.
                    let used = if a.integrated {
                        shared.get(&a.luid_key).copied()
                    } else {
                        dedicated.get(&a.luid_key).copied()
                    };
                    GpuSnapshot {
                        name: a.name.clone(),
                        // The adapter LUID is the identity Windows itself
                        // uses to name an adapter across processes.
                        device_id: Some(a.luid_key.clone()),
                        integrated: a.integrated,
                        utilization_pct: util_by_luid.get(&a.luid_key).copied().map(clamp_pct),
                        enc_util_pct: enc_by_luid.get(&a.luid_key).copied().map(clamp_pct),
                        dec_util_pct: dec_by_luid.get(&a.luid_key).copied().map(clamp_pct),
                        vram_used_bytes: used,
                        // DXGI always reports a total for a real adapter.
                        vram_total_bytes: Some(a.vram_total),
                        ..Default::default()
                    }
                })
                .collect())
        }

        fn processes(&mut self) -> Vec<GpuProcess> {
            self.last_procs.clone()
        }
    }

    fn enum_adapters() -> Vec<Adapter> {
        let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for i in 0.. {
            let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
                break;
            };
            let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
                continue;
            };
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0
                || desc.VendorId == MICROSOFT_BASIC_RENDER
            {
                continue;
            }
            let name = String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .to_string();
            let integrated = is_integrated(
                desc.DedicatedVideoMemory as u64,
                desc.SharedSystemMemory as u64,
            );
            out.push(Adapter {
                luid_key: format!(
                    "luid_0x{:08x}_0x{:08x}",
                    desc.AdapterLuid.HighPart as u32, desc.AdapterLuid.LowPart
                ),
                name,
                vendor_id: desc.VendorId,
                vram_total: if integrated {
                    desc.SharedSystemMemory as u64
                } else {
                    desc.DedicatedVideoMemory as u64
                },
                integrated,
            });
        }
        out
    }

    /// Sum a wildcard counter's values into buckets keyed off the instance
    /// name; instances the key function rejects are skipped.
    fn sum_by<K: Eq + Hash>(
        counter: Option<PDH_HCOUNTER>,
        key: impl Fn(&str) -> Option<K>,
    ) -> HashMap<K, u64> {
        let mut m = HashMap::new();
        if let Some(c) = counter {
            for (inst, v) in read_array(c) {
                if let Some(k) = key(&inst) {
                    *m.entry(k).or_default() += v as u64;
                }
            }
        }
        m
    }

    /// Read a wildcard counter into (instance_name, value) pairs.
    fn read_array(counter: PDH_HCOUNTER) -> Vec<(String, f64)> {
        let mut size = 0u32;
        let mut count = 0u32;
        let status = unsafe {
            PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, None)
        };
        if status != PDH_MORE_DATA || size == 0 {
            return Vec::new();
        }
        // PDH sizes the buffer in bytes but writes an array of items, so the
        // allocation must carry the item's alignment — a `Vec<u8>` only
        // promises 1. Capacity stays uninitialized; PDH fills it.
        let n = (size as usize).div_ceil(size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>());
        let mut buf: Vec<PDH_FMT_COUNTERVALUE_ITEM_W> = Vec::with_capacity(n);
        let items = buf.as_mut_ptr();
        let status = unsafe {
            PdhGetFormattedCounterArrayW(
                counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                Some(items),
            )
        };
        if status != 0 {
            return Vec::new();
        }
        (0..count as usize)
            .filter_map(|i| unsafe {
                let item = &*items.add(i);
                let name = item.szName.to_string().ok()?.to_lowercase();
                Some((name, item.FmtValue.Anonymous.doubleValue))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::parse::*;

    const ENGINE: &str = "pid_1234_luid_0x00000000_0x0000c4cf_phys_0_eng_0_engtype_3d";
    const ADAPTER_MEM: &str = "luid_0x00000000_0x0000c4cf_phys_0";

    #[test]
    fn luid_prefix_reads_full_26_char_key() {
        let key = Some("luid_0x00000000_0x0000c4cf".to_string());
        assert_eq!(luid_prefix(ENGINE), key);
        assert_eq!(luid_prefix(ADAPTER_MEM), key);
        assert_eq!(luid_prefix("luid_0x00000000_0x0000c4cf"), key);
        // Adapters differing only in the low bits must stay distinct.
        assert_ne!(
            luid_prefix("luid_0x00000000_0x0000c4cf"),
            luid_prefix("luid_0x00000000_0x0000c4d0")
        );
        assert_eq!(
            luid_prefix("pid_9_luid_0x0000abcd_0xffffffff_phys_0"),
            Some("luid_0x0000abcd_0xffffffff".to_string())
        );
    }

    #[test]
    fn luid_prefix_rejects_malformed_keys() {
        assert_eq!(luid_prefix("pid_1234_phys_0_engtype_3d"), None);
        assert_eq!(luid_prefix(""), None);
        // Truncated halves.
        assert_eq!(luid_prefix("luid_0x00000000_0x0000"), None);
        assert_eq!(luid_prefix("luid_0x0000_0x0000c4cf"), None);
        // Missing "0x", non-hex digits, over-long half.
        assert_eq!(luid_prefix("luid_00000000_0x0000c4cf"), None);
        assert_eq!(luid_prefix("luid_0x00000000_0xzzzzc4cf"), None);
        assert_eq!(luid_prefix("luid_0x00000000_0x0000c4cff"), None);
        // "luid" must be a whole token, not a suffix.
        assert_eq!(luid_prefix("xluid_0x00000000_0x0000c4cf"), None);
    }

    #[test]
    fn luid_and_engtype_splits_instance() {
        assert_eq!(
            luid_and_engtype(ENGINE),
            Some(("luid_0x00000000_0x0000c4cf".to_string(), "3d".to_string()))
        );
        assert_eq!(
            luid_and_engtype("pid_5_luid_0x00000000_0x0000c4cf_phys_0_eng_1_engtype_videodecode"),
            Some((
                "luid_0x00000000_0x0000c4cf".to_string(),
                "videodecode".to_string()
            ))
        );
        // No engine type, or no LUID -> no pairing.
        assert_eq!(luid_and_engtype(ADAPTER_MEM), None);
        assert_eq!(luid_and_engtype("pid_5_phys_0_engtype_3d"), None);
    }

    #[test]
    fn pid_prefix_reads_leading_pid() {
        assert_eq!(pid_prefix(ENGINE), Some(1234));
        assert_eq!(pid_prefix("pid_0_luid_0x00000000_0x0000c4cf"), Some(0));
        // Adapter-scoped instances carry no pid.
        assert_eq!(pid_prefix(ADAPTER_MEM), None);
        assert_eq!(pid_prefix("pid_abc_luid_0x00000000_0x0000c4cf"), None);
        assert_eq!(pid_prefix("pid_99999999999_luid_0x0_0x0"), None);
    }

    #[test]
    fn integrated_heuristic_separates_small_discrete_cards() {
        const GIB: u64 = 1024 * 1024 * 1024;
        const MIB: u64 = 1024 * 1024;
        // iGPUs: token carve-out against a large shared pool.
        assert!(is_integrated(128 * MIB, 16 * GIB));
        assert!(is_integrated(0, 8 * GIB));
        // Small discrete cards — the 1 GiB threshold used to misfile these.
        assert!(!is_integrated(512 * MIB, 16 * GIB)); // 512 MB RX 550
        assert!(!is_integrated(2 * GIB, 16 * GIB)); // GT 710
        assert!(!is_integrated(8 * GIB, 16 * GIB));
        // Small dedicated pool that shared does not dominate: discrete.
        assert!(!is_integrated(256 * MIB, 256 * MIB));
    }

    /// A process PDH lists under "GPU Engine" but under neither memory
    /// counter: nothing published a figure, so there is no figure to show.
    /// `0` would claim the process holds nothing on the card.
    #[test]
    fn a_process_with_no_memory_instance_reads_unknown() {
        assert_eq!(proc_mem_bytes(false, None, None), None);
        assert_eq!(proc_mem_bytes(true, None, None), None);
        assert_ne!(proc_mem_bytes(false, None, None), Some(0));
    }

    /// The pool the adapter class selects is the one reported, and whatever it
    /// says is a reading — a published zero included.
    #[test]
    fn the_relevant_counter_is_the_reading_zero_included() {
        // Discrete: dedicated.
        assert_eq!(proc_mem_bytes(false, Some(512), Some(64)), Some(512));
        assert_eq!(proc_mem_bytes(false, Some(0), None), Some(0));
        // Integrated: shared.
        assert_eq!(proc_mem_bytes(true, Some(512), Some(64)), Some(64));
        assert_eq!(proc_mem_bytes(true, None, Some(0)), Some(0));
    }

    /// PDH accounted for the process, just not in the pool being asked about.
    /// Instances exist where there is something to report, so an empty one is
    /// a genuine nothing rather than an unknown.
    #[test]
    fn the_other_counter_alone_still_proves_the_process_was_accounted_for() {
        assert_eq!(proc_mem_bytes(false, None, Some(4096)), Some(0));
        assert_eq!(proc_mem_bytes(true, Some(4096), None), Some(0));
    }

    const NVIDIA: u16 = 0x10de;
    const AMD: u16 = 0x1002;
    const INTEL: u16 = 0x8086;

    /// (name, DXGI VendorId) standing in for the `Adapter` the cfg-gated
    /// module builds, which cannot be constructed off Windows.
    fn rig() -> Vec<(&'static str, u32)> {
        vec![
            ("NVIDIA GeForce RTX 4090", 0x10de),
            ("AMD Radeon 780M", 0x1002),
            ("Intel UHD Graphics 770", 0x8086),
        ]
    }

    fn kept(claimed: &[u16]) -> Vec<&'static str> {
        retain_unclaimed(rig(), claimed, |a| a.1)
            .into_iter()
            .map(|a| a.0)
            .collect()
    }

    #[test]
    fn unclaimed_adapters_survive_in_dxgi_order() {
        // Nothing else probed — the AMD/Intel-only Windows box. Unchanged.
        assert_eq!(
            kept(&[]),
            [
                "NVIDIA GeForce RTX 4090",
                "AMD Radeon 780M",
                "Intel UHD Graphics 770"
            ]
        );
        // NVML probed: its card is NVML's, the rest stay in enumeration order.
        assert_eq!(
            kept(&[NVIDIA]),
            ["AMD Radeon 780M", "Intel UHD Graphics 770"]
        );
        assert_eq!(kept(&[NVIDIA, INTEL]), ["AMD Radeon 780M"]);
        // Repeated claims are idempotent, not double-counted.
        assert_eq!(kept(&[NVIDIA, NVIDIA]), kept(&[NVIDIA]));
    }

    /// The filter is per vendor id, not one-per-vendor: the tri-vendor box
    /// with a second card from an unclaimed vendor — an Intel Arc beside the
    /// Intel iGPU, two AMD cards — must contribute every one of them, and a
    /// claimed vendor must lose every one of its own.
    #[test]
    fn several_adapters_of_one_vendor_are_all_kept_or_all_dropped() {
        let rig = || {
            vec![
                ("RTX 4090", 0x10de_u32),
                ("RTX 3060", 0x10de),
                ("Intel UHD 770", 0x8086),
                ("Intel Arc A770", 0x8086),
            ]
        };
        let kept = |claimed: &[u16]| -> Vec<&'static str> {
            retain_unclaimed(rig(), claimed, |a| a.1)
                .into_iter()
                .map(|a| a.0)
                .collect()
        };
        // NVML covers both its cards; both Intel adapters are PDH's.
        assert_eq!(kept(&[NVIDIA]), ["Intel UHD 770", "Intel Arc A770"]);
        assert_eq!(kept(&[INTEL]), ["RTX 4090", "RTX 3060"]);
        assert!(kept(&[NVIDIA, INTEL]).is_empty());
    }

    /// Every adapter claimed leaves nothing to report, which is what makes
    /// `probe` return None and an NVIDIA-only rig keep NVML alone.
    #[test]
    fn a_fully_claimed_rig_keeps_no_adapters() {
        assert!(kept(&[NVIDIA, AMD, INTEL]).is_empty());
        assert!(retain_unclaimed(Vec::new(), &[], |a: &(&str, u32)| a.1).is_empty());
    }

    #[test]
    fn vendor_ids_are_compared_at_dxgi_width() {
        // Truncating VendorId to u16 would read this as an NVIDIA claim and
        // silently drop the adapter.
        let odd = vec![("weird adapter", 0x0001_10de_u32)];
        assert_eq!(retain_unclaimed(odd, &[NVIDIA], |a| a.1).len(), 1);
    }
}
