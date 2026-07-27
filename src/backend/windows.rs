//! Windows vendor-generic backend: PDH "GPU Engine" / "GPU Adapter Memory"
//! performance counters (what Task Manager uses) + DXGI for adapter names and
//! VRAM totals. Covers AMD and Intel on Windows, where NVML doesn't apply;
//! detect() tries NVML first, so NVIDIA rigs get the richer backend.
//! Temperature/fan/clocks are not exposed by PDH — those need ADLX (TODO).

use super::GpuBackend;

pub fn probe() -> Option<Box<dyn GpuBackend>> {
    #[cfg(windows)]
    if let Some(b) = win::probe() {
        return Some(b);
    }
    None
}

/// PDH instance-name parsing and the iGPU heuristic. Pure functions, kept out
/// of the `#[cfg(windows)]` module below so they compile and are testable on
/// any host — a fixed-width LUID slice shipped broken precisely because nothing
/// off Windows could exercise it.
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
}

#[cfg(windows)]
mod win {
    use super::parse::{is_integrated, luid_and_engtype, luid_prefix, pid_prefix};
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

    pub fn probe() -> Option<Box<dyn GpuBackend>> {
        let adapters = enum_adapters();
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

    // PDH handles are plain opaque values owned by this struct.
    unsafe impl Send for PdhBackend {}

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
                let mem = if integrated {
                    proc_shr.get(&key).copied().unwrap_or(0)
                } else {
                    proc_ded.get(&key).copied().unwrap_or(0)
                };
                let kind = if proc_graphics.get(&key).copied().unwrap_or(false) {
                    ProcKind::Graphics
                } else {
                    ProcKind::Compute
                };
                let p = GpuProcess {
                    pid: key.0,
                    gpu_index,
                    kind,
                    gpu_util_pct: util_by_proc.get(&key).copied(),
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
                    let used = if a.integrated {
                        shared.get(&a.luid_key).copied().unwrap_or(0)
                    } else {
                        dedicated.get(&a.luid_key).copied().unwrap_or(0)
                    };
                    GpuSnapshot {
                        name: a.name.clone(),
                        integrated: a.integrated,
                        utilization_pct: clamp_pct(
                            util_by_luid.get(&a.luid_key).copied().unwrap_or(0.0),
                        ),
                        enc_util_pct: enc_by_luid.get(&a.luid_key).copied().map(clamp_pct),
                        dec_util_pct: dec_by_luid.get(&a.luid_key).copied().map(clamp_pct),
                        vram_used_bytes: used,
                        vram_total_bytes: a.vram_total,
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
}
