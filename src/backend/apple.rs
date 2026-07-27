//! Apple backend via IOKit IOAccelerator PerformanceStatistics (macOS).
//! Covers Apple Silicon (AGXAccelerator) and Intel-Mac dGPUs/iGPUs, which all
//! publish an IOAccelerator service. Temperature/power need SMC/IOReport and
//! are left `None` for now.

use super::GpuBackend;

pub fn probe() -> Option<Box<dyn GpuBackend>> {
    #[cfg(target_os = "macos")]
    if let Some(b) = macos::probe() {
        return Some(b);
    }
    None
}

/// Snapshot shaping, the header driver line and device-tree blob decoding.
/// Pure functions, kept out of the `#[cfg(target_os = "macos")]` module below
/// so they compile and are testable on any host — everything behind that gate
/// is unreachable to CI on Linux.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod parse {
    use crate::backend::{GpuSnapshot, clamp_pct};

    /// One accelerator's registry properties, lifted out of CoreFoundation so
    /// the shaping below is plain Rust. `None` means "property absent", which
    /// is deliberately distinct from zero.
    #[derive(Debug, Default, Clone, PartialEq, Eq)]
    pub struct RawAccel {
        pub io_class: Option<String>,
        /// "Device Utilization %" / "GPU Activity(%)".
        pub util_pct: Option<i64>,
        /// "In use system memory" / "vramUsedBytes".
        pub vram_used_bytes: Option<i64>,
        /// "VRAM,totalMB" — discrete cards only.
        pub vram_total_mb: Option<i64>,
        pub core_count: Option<i64>,
        /// Kext bundle id of the driver that matched, when IOKit publishes it.
        pub bundle_id: Option<String>,
    }

    /// AGX = Apple Silicon: unified memory, GPU name derives from the SoC.
    pub fn is_agx(io_class: Option<&str>) -> bool {
        io_class.is_some_and(|c| c.starts_with("AGX"))
    }

    pub fn snapshot(a: &RawAccel, cpu_brand: Option<&str>, system_mem: u64) -> GpuSnapshot {
        let agx = is_agx(a.io_class.as_deref());
        let name = if agx {
            let soc = cpu_brand.unwrap_or("Apple GPU");
            match a.core_count {
                Some(n) if n > 0 => format!("{soc} ({n}-core GPU)"),
                _ => soc.to_string(),
            }
        } else {
            a.io_class.clone().unwrap_or_else(|| "GPU".to_string())
        };

        // Discrete cards report VRAM,totalMB; unified memory uses system RAM.
        let total = a
            .vram_total_mb
            .filter(|mb| *mb > 0)
            .map(|mb| (mb as u64).saturating_mul(1024 * 1024))
            .unwrap_or(system_mem);

        GpuSnapshot {
            name,
            integrated: agx,
            // The `unwrap_or(0)` pair below is forced by the snapshot fields
            // being non-Option: a missing property renders as a confident
            // "idle"/"empty" rather than "unknown". `RawAccel` keeps the
            // distinction so this collapses to a `?` once they become Option.
            utilization_pct: clamp_pct(a.util_pct.unwrap_or(0) as f64),
            vram_used_bytes: a.vram_used_bytes.unwrap_or(0).max(0) as u64,
            vram_total_bytes: total,
            ..Default::default()
        }
    }

    /// Header line. macOS publishes no per-GPU driver version — the GPU stack
    /// ships with the OS, so the OS product/build *is* the version — prefixed
    /// by the accelerators' kext bundle ids when IOKit names them. Every
    /// accelerator contributes, mirroring the intel backend's "i915+xe" line:
    /// an Intel Mac runs a different kext per GPU.
    pub fn driver_line(
        accels: &[RawAccel],
        product: Option<&str>,
        build: Option<&str>,
    ) -> Option<String> {
        let os = match (product, build) {
            (Some(p), Some(b)) => Some(format!("macOS {p} ({b})")),
            (Some(p), None) => Some(format!("macOS {p}")),
            (None, Some(b)) => Some(format!("macOS build {b}")),
            (None, None) => None,
        };
        // Order-preserving dedup: two accelerators sharing a kext must not
        // print it twice, and the order tracks the (stable) device order.
        let mut kexts: Vec<String> = Vec::new();
        for a in accels {
            let Some(k) = a
                .bundle_id
                .as_deref()
                .map(kext_short)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            if !kexts.contains(&k) {
                kexts.push(k);
            }
        }
        let kexts = (!kexts.is_empty()).then(|| kexts.join("+"));
        match (kexts, os) {
            (Some(k), Some(o)) => Some(format!("{k} · {o}")),
            (Some(k), None) => Some(k),
            (None, o) => o,
        }
    }

    /// "com.apple.kext.AMDRadeonX6000" -> "AMDRadeonX6000". Vendor prefixes
    /// are noise in a one-line header; third-party ids keep their own prefix
    /// since that is the only thing distinguishing them.
    fn kext_short(id: &str) -> String {
        id.trim()
            .trim_start_matches("com.apple.")
            .trim_start_matches("driver.")
            .trim_start_matches("kext.")
            .trim_start_matches("iokit.")
            .to_string()
    }

    /// Decode a device-tree `CFData` blob as a little-endian integer.
    /// Device-tree-backed properties (`gpu-core-count`) arrive as a raw blob,
    /// not a `CFNumber`; widths of 1/2/4/8 bytes all occur. Anything wider is
    /// not a scalar and is rejected rather than truncated.
    pub fn le_int(bytes: &[u8]) -> Option<i64> {
        if bytes.is_empty() || bytes.len() > 8 {
            return None;
        }
        let mut buf = [0u8; 8];
        buf[..bytes.len()].copy_from_slice(bytes);
        Some(i64::from_le_bytes(buf))
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::parse::{RawAccel, driver_line, le_int, snapshot};
    use crate::backend::{GpuBackend, GpuSnapshot};
    use anyhow::Result;
    use core_foundation::base::{CFType, TCFType, kCFAllocatorDefault};
    use core_foundation::data::CFData;
    use core_foundation::dictionary::{
        CFDictionary, CFDictionaryGetValueIfPresent, CFDictionaryRef, CFMutableDictionaryRef,
    };
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use io_kit_sys::{
        IOIteratorNext, IOObjectRelease, IORegistryEntryCreateCFProperties,
        IORegistryEntryGetRegistryEntryID, IOServiceGetMatchingServices, IOServiceMatching,
        kIOMasterPortDefault,
    };
    use std::ffi::c_void;

    pub fn probe() -> Option<Box<dyn GpuBackend>> {
        let accels = enumerate_accelerators();
        if accels.is_empty() {
            return None;
        }
        Some(Box::new(AppleBackend {
            driver: driver_line(
                &accels,
                sysctl_string("kern.osproductversion").as_deref(),
                sysctl_string("kern.osversion").as_deref(),
            ),
            cpu_brand: sysctl_string("machdep.cpu.brand_string"),
            total_mem: sysctl_u64("hw.memsize").unwrap_or(0),
        }))
    }

    struct AppleBackend {
        driver: Option<String>,
        cpu_brand: Option<String>,
        total_mem: u64,
    }

    impl GpuBackend for AppleBackend {
        fn name(&self) -> &'static str {
            "ioaccel"
        }

        fn poll(&mut self) -> Result<Vec<GpuSnapshot>> {
            // Re-enumerate per poll: cheap, and picks up eGPU hotplug. Index
            // order stays stable because enumerate_accelerators sorts.
            Ok(enumerate_accelerators()
                .iter()
                .map(|a| snapshot(a, self.cpu_brand.as_deref(), self.total_mem))
                .collect())
        }

        fn driver_info(&self) -> Option<String> {
            self.driver.clone()
        }
    }

    /// Properties of every IOAccelerator service, ordered by registry entry id.
    ///
    /// `IOServiceGetMatchingServices` documents no iteration order, but `poll`
    /// must return a stable index order: `App` keys history, session peaks,
    /// folding and selection by position alone, so a reshuffle would swap two
    /// GPUs' entire graphs with no visual cue. The registry entry id is a
    /// stable 64-bit key for the entry's lifetime, so sorting on it pins the
    /// order without giving up hotplug pickup.
    fn enumerate_accelerators() -> Vec<RawAccel> {
        let mut out: Vec<(u64, RawAccel)> = Vec::new();
        unsafe {
            let matching = IOServiceMatching(c"IOAccelerator".as_ptr());
            if matching.is_null() {
                return Vec::new();
            }
            let mut iter = 0;
            // Consumes `matching` regardless of outcome.
            if IOServiceGetMatchingServices(kIOMasterPortDefault, matching, &mut iter) != 0 {
                return Vec::new();
            }
            loop {
                let entry = IOIteratorNext(iter);
                if entry == 0 {
                    break;
                }
                let mut id: u64 = 0;
                // An entry with no id sorts last, still deterministically.
                let id = if IORegistryEntryGetRegistryEntryID(entry, &mut id) == 0 {
                    id
                } else {
                    u64::MAX
                };
                let mut props: CFMutableDictionaryRef = std::ptr::null_mut();
                let kr =
                    IORegistryEntryCreateCFProperties(entry, &mut props, kCFAllocatorDefault, 0);
                IOObjectRelease(entry);
                if kr == 0 && !props.is_null() {
                    // Create rule: the wrapper owns the dictionary.
                    let props = CFDictionary::wrap_under_create_rule(props as CFDictionaryRef);
                    out.push((id, read_accel(&props)));
                }
            }
            IOObjectRelease(iter);
        }
        out.sort_by_key(|(id, _)| *id);
        out.into_iter().map(|(_, a)| a).collect()
    }

    /// Lift the properties this backend cares about out of CoreFoundation, so
    /// nothing below the FFI boundary escapes into the shaping code.
    fn read_accel(props: &CFDictionary) -> RawAccel {
        let root = props.as_concrete_TypeRef();
        let stats = dict_get_dict(root, "PerformanceStatistics");
        let stat = |key: &str| {
            stats
                .as_ref()
                .and_then(|s| dict_get_i64(s.as_concrete_TypeRef(), key))
        };
        RawAccel {
            io_class: dict_get_string(root, "IOClass"),
            util_pct: stat("Device Utilization %").or_else(|| stat("GPU Activity(%)")),
            vram_used_bytes: stat("In use system memory").or_else(|| stat("vramUsedBytes")),
            vram_total_mb: dict_get_i64(root, "VRAM,totalMB"),
            core_count: dict_get_i64(root, "gpu-core-count"),
            bundle_id: dict_get_string(root, "CFBundleIdentifier"),
        }
    }

    /// Fetch `key` as a type-erased CFType. Every typed accessor below goes
    /// through `downcast` from here: IOKit property types vary across Apple
    /// Silicon, Intel-Mac AMD/NVIDIA drivers, eGPUs and paravirt accelerators,
    /// and reinterpreting e.g. a CFString as a CFNumber is UB inside
    /// CoreFoundation rather than a clean `None`.
    fn dict_get(dict: CFDictionaryRef, key: &str) -> Option<CFType> {
        let key = CFString::new(key);
        let mut value: *const c_void = std::ptr::null();
        let found = unsafe {
            CFDictionaryGetValueIfPresent(
                dict,
                key.as_concrete_TypeRef() as *const c_void,
                &mut value,
            )
        };
        (found != 0 && !value.is_null()).then(|| unsafe { CFType::wrap_under_get_rule(value as _) })
    }

    fn dict_get_dict(dict: CFDictionaryRef, key: &str) -> Option<CFDictionary> {
        dict_get(dict, key)?.downcast::<CFDictionary>()
    }

    /// Numeric property, accepting either a `CFNumber` or the raw
    /// little-endian `CFData` blob that device-tree-backed keys carry.
    fn dict_get_i64(dict: CFDictionaryRef, key: &str) -> Option<i64> {
        let v = dict_get(dict, key)?;
        if let Some(n) = v.downcast::<CFNumber>() {
            return n.to_i64();
        }
        le_int(v.downcast::<CFData>()?.bytes())
    }

    fn dict_get_string(dict: CFDictionaryRef, key: &str) -> Option<String> {
        Some(dict_get(dict, key)?.downcast::<CFString>()?.to_string())
    }

    fn sysctl_u64(name: &str) -> Option<u64> {
        let cname = std::ffi::CString::new(name).ok()?;
        let mut val: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        let rc = unsafe {
            libc::sysctlbyname(
                cname.as_ptr(),
                &mut val as *mut u64 as *mut c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0).then_some(val)
    }

    fn sysctl_string(name: &str) -> Option<String> {
        let cname = std::ffi::CString::new(name).ok()?;
        let mut len = 0usize;
        unsafe {
            if libc::sysctlbyname(
                cname.as_ptr(),
                std::ptr::null_mut(),
                &mut len,
                std::ptr::null_mut(),
                0,
            ) != 0
            {
                return None;
            }
            let mut buf = vec![0u8; len];
            if libc::sysctlbyname(
                cname.as_ptr(),
                buf.as_mut_ptr() as *mut c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            ) != 0
            {
                return None;
            }
            buf.truncate(len.saturating_sub(1)); // drop trailing NUL
            String::from_utf8(buf).ok().map(|s| s.trim().to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse::*;

    fn agx(cores: Option<i64>) -> RawAccel {
        RawAccel {
            io_class: Some("AGXAcceleratorG13X".to_string()),
            core_count: cores,
            ..Default::default()
        }
    }

    #[test]
    fn agx_detection_keys_off_the_io_class_prefix() {
        assert!(is_agx(Some("AGXAcceleratorG13X")));
        assert!(!is_agx(Some("AMDRadeonX6000")));
        assert!(!is_agx(Some("agxaccelerator"))); // IOClass is case-exact
        assert!(!is_agx(None));
    }

    #[test]
    fn apple_silicon_name_folds_in_soc_and_core_count() {
        let s = snapshot(&agx(Some(38)), Some("Apple M2 Max"), 64 << 30);
        assert_eq!(s.name, "Apple M2 Max (38-core GPU)");
        assert!(s.integrated);
        // Unified memory: no VRAM,totalMB, so system RAM is the pool.
        assert_eq!(s.vram_total_bytes, 64 << 30);

        // Missing core count or SoC name must not fabricate either.
        assert_eq!(
            snapshot(&agx(None), Some("Apple M1"), 0).name,
            "Apple M1".to_string()
        );
        assert_eq!(
            snapshot(&agx(Some(8)), None, 0).name,
            "Apple GPU (8-core GPU)"
        );
        assert_eq!(snapshot(&agx(None), None, 0).name, "Apple GPU");
        // A zero/garbage core count is worse than saying nothing.
        assert_eq!(
            snapshot(&agx(Some(0)), Some("Apple M1"), 0).name,
            "Apple M1"
        );
    }

    #[test]
    fn discrete_cards_use_io_class_and_their_own_vram() {
        let a = RawAccel {
            io_class: Some("AMDRadeonX6000".to_string()),
            vram_total_mb: Some(8192),
            vram_used_bytes: Some(1024 * 1024 * 1024),
            ..Default::default()
        };
        let s = snapshot(&a, Some("Intel Core i9"), 32 << 30);
        assert_eq!(s.name, "AMDRadeonX6000");
        assert!(!s.integrated);
        assert_eq!(s.vram_total_bytes, 8 << 30);
        assert_eq!(s.vram_used_bytes, 1 << 30);
        // Nameless accelerator still renders as something.
        assert_eq!(snapshot(&RawAccel::default(), None, 0).name, "GPU");
        // VRAM,totalMB of 0 is not a real pool; fall back to system RAM.
        let zero = RawAccel {
            vram_total_mb: Some(0),
            ..Default::default()
        };
        assert_eq!(snapshot(&zero, None, 16 << 30).vram_total_bytes, 16 << 30);
    }

    #[test]
    fn utilization_is_clamped_and_negatives_do_not_wrap() {
        let with = |util, used| RawAccel {
            util_pct: Some(util),
            vram_used_bytes: Some(used),
            ..Default::default()
        };
        assert_eq!(snapshot(&with(42, 0), None, 0).utilization_pct, 42.0);
        assert_eq!(snapshot(&with(140, 0), None, 0).utilization_pct, 100.0);
        assert_eq!(snapshot(&with(-5, 0), None, 0).utilization_pct, 0.0);
        // A negative byte count must not become 18 exabytes.
        assert_eq!(snapshot(&with(0, -1), None, 0).vram_used_bytes, 0);
        // Absent properties currently render as zero; see the note in snapshot.
        let s = snapshot(&RawAccel::default(), None, 0);
        assert_eq!(s.utilization_pct, 0.0);
        assert_eq!(s.vram_used_bytes, 0);
    }

    #[test]
    fn vram_total_does_not_overflow_on_a_bogus_mb_count() {
        let a = RawAccel {
            vram_total_mb: Some(i64::MAX),
            ..Default::default()
        };
        assert_eq!(snapshot(&a, None, 0).vram_total_bytes, u64::MAX);
    }

    fn kext(id: &str) -> RawAccel {
        RawAccel {
            bundle_id: Some(id.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn driver_line_degrades_one_field_at_a_time() {
        let amd = [kext("com.apple.kext.AMDRadeonX6000")];
        assert_eq!(
            driver_line(&amd, Some("14.5"), Some("23F79")),
            Some("AMDRadeonX6000 · macOS 14.5 (23F79)".to_string())
        );
        assert_eq!(
            driver_line(&amd, Some("15.0"), None),
            Some("AMDRadeonX6000 · macOS 15.0".to_string())
        );
        assert_eq!(
            driver_line(&amd, None, None),
            Some("AMDRadeonX6000".to_string())
        );
        // No bundle id: the OS line stands on its own.
        let bare = [RawAccel::default()];
        assert_eq!(
            driver_line(&bare, Some("14.5"), Some("23F79")),
            Some("macOS 14.5 (23F79)".to_string())
        );
        assert_eq!(
            driver_line(&bare, None, Some("23F79")),
            Some("macOS build 23F79".to_string())
        );
        // Third-party kexts keep their prefix — it is their only identity.
        assert_eq!(
            driver_line(&[kext("net.example.MyGPU")], None, None),
            Some("net.example.MyGPU".to_string())
        );
        // Nothing known beats an empty or misleading line.
        assert_eq!(driver_line(&[], None, None), None);
        assert_eq!(driver_line(&bare, None, None), None);
        assert_eq!(driver_line(&[kext("   ")], None, None), None);
    }

    #[test]
    fn driver_line_names_every_distinct_kext_once() {
        // Intel Mac: iGPU and dGPU run different drivers.
        let mixed = [
            kext("com.apple.driver.AppleIntelKBLGraphics"),
            kext("com.apple.kext.AMDRadeonX6000"),
        ];
        assert_eq!(
            driver_line(&mixed, Some("13.6"), None),
            Some("AppleIntelKBLGraphics+AMDRadeonX6000 · macOS 13.6".to_string())
        );
        // Two cards on the same driver print it once.
        let pair = [
            kext("com.apple.kext.AMDRadeonX6000"),
            kext("com.apple.kext.AMDRadeonX6000"),
        ];
        assert_eq!(
            driver_line(&pair, None, None),
            Some("AMDRadeonX6000".to_string())
        );
    }

    #[test]
    fn le_int_decodes_device_tree_blobs() {
        assert_eq!(le_int(&[10]), Some(10));
        assert_eq!(le_int(&[0x26, 0x00]), Some(38));
        assert_eq!(le_int(&[0x26, 0x00, 0x00, 0x00]), Some(38));
        assert_eq!(le_int(&[0xff; 8]), Some(-1));
        // Not a scalar: reject rather than silently truncate.
        assert_eq!(le_int(&[]), None);
        assert_eq!(le_int(&[0; 9]), None);
    }
}
