use super::parse::{is_integrated, luid_prefix, pid_prefix, proc_mem_bytes, retain_unclaimed};
use super::pdh::{collect, read_array};
use crate::backend::{GpuBackend, GpuProcess, GpuSnapshot, ProcKind, clamp_pct};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::hash::Hash;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1,
};
use windows::Win32::System::Performance::{
    PDH_HCOUNTER, PDH_HQUERY, PdhAddEnglishCounterW, PdhCloseQuery, PdhOpenQueryW,
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
    if collect(query).is_err() {
        unsafe { PdhCloseQuery(query) };
        return None;
    }
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
        self.last_procs.clear();
        let collected = collect(self.query)?;

        let super::aggregate::Utilization {
            util_by_luid,
            enc_by_luid,
            dec_by_luid,
            util_by_proc,
            proc_graphics,
        } = super::aggregate::aggregate(
            self.util
                .filter(|_| collected)
                .map(read_array)
                .transpose()
                .context("GPU Engine Utilization Percentage")?
                .unwrap_or_default(),
        );

        let dedicated = sum_by(self.dedicated.filter(|_| collected), luid_prefix)
            .context("GPU Adapter Memory Dedicated Usage")?;
        let shared = sum_by(self.shared.filter(|_| collected), luid_prefix)
            .context("GPU Adapter Memory Shared Usage")?;

        // Per-process memory: (pid, luid) -> bytes.
        let proc_key = |inst: &str| Some((pid_prefix(inst)?, luid_prefix(inst)?));
        let proc_ded = sum_by(self.proc_dedicated.filter(|_| collected), proc_key)
            .context("GPU Process Memory Dedicated Usage")?;
        let proc_shr = sum_by(self.proc_shared.filter(|_| collected), proc_key)
            .context("GPU Process Memory Shared Usage")?;

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
) -> Result<HashMap<K, u64>> {
    let mut m = HashMap::new();
    if let Some(c) = counter {
        for (inst, v) in read_array(c)? {
            if let Some(k) = key(&inst) {
                *m.entry(k).or_default() += v as u64;
            }
        }
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Performance::PDH_INVALID_HANDLE;

    #[test]
    fn invalid_counter_is_an_error() {
        let error = read_array(PDH_HCOUNTER::default()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&format!("0x{PDH_INVALID_HANDLE:08x}"))
        );
    }

    #[test]
    fn invalid_query_poll_is_an_error() {
        let mut backend = PdhBackend {
            query: PDH_HQUERY::default(),
            util: None,
            dedicated: None,
            shared: None,
            proc_dedicated: None,
            proc_shared: None,
            adapters: Vec::new(),
            last_procs: Vec::new(),
        };
        let error = backend.poll().unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&format!("0x{PDH_INVALID_HANDLE:08x}"))
        );
    }
    #[test]
    #[ignore = "requires live Windows GPU adapters and PDH counters"]
    fn live_hardware_identity_and_memory() {
        let adapters = enum_adapters();
        assert!(!adapters.is_empty(), "DXGI must enumerate actual hardware");
        let mut backend = probe(&[]).expect("PDH must initialize on real hardware");
        std::thread::sleep(std::time::Duration::from_secs(1));
        let snapshots = backend.poll().expect("live PDH collection");
        assert_eq!(snapshots.len(), adapters.len());
        for (snapshot, adapter) in snapshots.iter().zip(&adapters) {
            assert_eq!(
                snapshot.device_id.as_deref(),
                Some(adapter.luid_key.as_str())
            );
            assert_eq!(snapshot.name, adapter.name);
            assert_eq!(snapshot.vram_total_bytes, Some(adapter.vram_total));
            assert!(adapter.vram_total > 0);
            let used = snapshot
                .vram_used_bytes
                .expect("adapter memory must have a reading");
            assert!(
                used <= adapter.vram_total,
                "memory reading exceeds selected pool"
            );
        }
        assert!(
            snapshots
                .iter()
                .any(|s| s.vram_used_bytes.is_some_and(|v| v > 0)),
            "live desktop GPU must have a nonempty memory allocation"
        );
    }
}
