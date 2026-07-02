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

#[cfg(windows)]
mod win {
    use crate::backend::{GpuBackend, GpuProcess, GpuSnapshot, ProcKind};
    use anyhow::Result;
    use std::collections::HashMap;
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
            // Busiest engine type per adapter = Task Manager's GPU %.
            let mut util_by_luid: HashMap<String, f64> = HashMap::new();
            for ((luid, _), v) in engine {
                let e = util_by_luid.entry(luid).or_default();
                *e = e.max(v);
            }
            // Same convention per process.
            let mut util_by_proc: HashMap<(u32, String), f64> = HashMap::new();
            for ((pid, luid, _), v) in proc_engine {
                let e = util_by_proc.entry((pid, luid)).or_default();
                *e = e.max(v);
            }

            let mem_by_luid = |c: Option<PDH_HCOUNTER>| -> HashMap<String, u64> {
                let mut m = HashMap::new();
                if let Some(c) = c {
                    for (inst, v) in read_array(c) {
                        if let Some(luid) = luid_prefix(&inst) {
                            *m.entry(luid).or_default() += v as u64;
                        }
                    }
                }
                m
            };
            let dedicated = mem_by_luid(self.dedicated);
            let shared = mem_by_luid(self.shared);

            // Per-process memory: (pid, luid) -> bytes.
            let proc_mem = |c: Option<PDH_HCOUNTER>| -> HashMap<(u32, String), u64> {
                let mut m = HashMap::new();
                if let Some(c) = c {
                    for (inst, v) in read_array(c) {
                        if let (Some(pid), Some(luid)) = (pid_prefix(&inst), luid_prefix(&inst)) {
                            *m.entry((pid, luid)).or_default() += v as u64;
                        }
                    }
                }
                m
            };
            let proc_ded = proc_mem(self.proc_dedicated);
            let proc_shr = proc_mem(self.proc_shared);

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
                };
                procs.insert(key, p);
            }
            self.last_procs = procs
                .into_values()
                .filter(|p| p.gpu_mem_bytes > 0 || p.gpu_util_pct.unwrap_or(0.0) > 0.0)
                .collect();

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
                        utilization_pct: util_by_luid
                            .get(&a.luid_key)
                            .copied()
                            .unwrap_or(0.0)
                            .clamp(0.0, 100.0),
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
            // iGPUs carve from system RAM: tiny dedicated pool, big shared pool.
            let integrated = desc.DedicatedVideoMemory < 1024 * 1024 * 1024;
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
        let mut buf = vec![0u8; size as usize];
        let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
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

    /// "pid_1234_luid_0x..._0x..._phys_0_engtype_3d" -> luid key + engine type.
    fn luid_and_engtype(instance: &str) -> Option<(String, String)> {
        let luid = luid_prefix(instance)?;
        let eng = instance.split("engtype_").nth(1)?.to_string();
        Some((luid, eng))
    }

    /// "pid_1234_luid_..." -> 1234
    fn pid_prefix(instance: &str) -> Option<u32> {
        instance
            .strip_prefix("pid_")?
            .split('_')
            .next()?
            .parse()
            .ok()
    }

    /// Extract "luid_0x????????_0x????????" from anywhere in the instance name.
    fn luid_prefix(instance: &str) -> Option<String> {
        let start = instance.find("luid_0x")?;
        let key = instance.get(start..start + 22)?;
        key.len().eq(&22).then(|| key.to_string())
    }
}
