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

#[cfg(target_os = "macos")]
mod macos {
    use crate::backend::{GpuBackend, GpuSnapshot};
    use anyhow::Result;
    use core_foundation::base::{CFRelease, CFType, TCFType, kCFAllocatorDefault};
    use core_foundation::dictionary::{
        CFDictionaryGetValueIfPresent, CFDictionaryRef, CFMutableDictionaryRef,
    };
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use io_kit_sys::{
        IOIteratorNext, IOObjectRelease, IORegistryEntryCreateCFProperties,
        IOServiceGetMatchingServices, IOServiceMatching, kIOMasterPortDefault,
    };
    use std::ffi::c_void;

    pub fn probe() -> Option<Box<dyn GpuBackend>> {
        let accels = enumerate_accelerators();
        if accels.is_empty() {
            return None;
        }
        Some(Box::new(AppleBackend {
            cpu_brand: sysctl_string("machdep.cpu.brand_string"),
            total_mem: sysctl_u64("hw.memsize").unwrap_or(0),
        }))
    }

    struct AppleBackend {
        cpu_brand: Option<String>,
        total_mem: u64,
    }

    impl GpuBackend for AppleBackend {
        fn name(&self) -> &'static str {
            "ioaccel"
        }

        fn poll(&mut self) -> Result<Vec<GpuSnapshot>> {
            // Re-enumerate per poll: cheap, and picks up eGPU hotplug.
            Ok(enumerate_accelerators()
                .into_iter()
                .map(|props| self.sample(&props))
                .collect())
        }
    }

    /// Root property dictionaries of every IOAccelerator service, ownership
    /// transferred to the wrapper (create rule).
    fn enumerate_accelerators() -> Vec<Props> {
        let mut out = Vec::new();
        unsafe {
            let matching = IOServiceMatching(c"IOAccelerator".as_ptr());
            if matching.is_null() {
                return out;
            }
            let mut iter = 0;
            // Consumes `matching` regardless of outcome.
            if IOServiceGetMatchingServices(kIOMasterPortDefault, matching, &mut iter) != 0 {
                return out;
            }
            loop {
                let entry = IOIteratorNext(iter);
                if entry == 0 {
                    break;
                }
                let mut props: CFMutableDictionaryRef = std::ptr::null_mut();
                let kr =
                    IORegistryEntryCreateCFProperties(entry, &mut props, kCFAllocatorDefault, 0);
                IOObjectRelease(entry);
                if kr == 0 && !props.is_null() {
                    out.push(Props(props as CFDictionaryRef));
                }
            }
            IOObjectRelease(iter);
        }
        out
    }

    /// Owned CFDictionary of a registry entry's properties.
    struct Props(CFDictionaryRef);

    impl Drop for Props {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0 as *const c_void) };
        }
    }

    impl AppleBackend {
        fn sample(&self, props: &Props) -> GpuSnapshot {
            let stats = dict_get_dict(props.0, "PerformanceStatistics");

            let util = stats
                .and_then(|s| dict_get_i64(s, "Device Utilization %"))
                .or_else(|| stats.and_then(|s| dict_get_i64(s, "GPU Activity(%)")))
                .unwrap_or(0);
            let used = stats
                .and_then(|s| dict_get_i64(s, "In use system memory"))
                .or_else(|| stats.and_then(|s| dict_get_i64(s, "vramUsedBytes")))
                .unwrap_or(0) as u64;

            // AGX = Apple Silicon: unified memory, GPU name derives from the SoC.
            let agx = dict_get_string(props.0, "IOClass").is_some_and(|c| c.starts_with("AGX"));
            let cores = dict_get_i64(props.0, "gpu-core-count");

            let name = if agx {
                let soc = self
                    .cpu_brand
                    .clone()
                    .unwrap_or_else(|| "Apple GPU".to_string());
                match cores {
                    Some(n) => format!("{soc} ({n}-core GPU)"),
                    None => soc,
                }
            } else {
                dict_get_string(props.0, "IOClass").unwrap_or_else(|| "GPU".to_string())
            };

            // Discrete cards report VRAM,totalMB; unified memory uses system RAM.
            let total = dict_get_i64(props.0, "VRAM,totalMB")
                .map(|mb| mb as u64 * 1024 * 1024)
                .unwrap_or(self.total_mem);

            GpuSnapshot {
                name,
                integrated: agx,
                utilization_pct: util as f64,
                vram_used_bytes: used,
                vram_total_bytes: total,
                ..Default::default()
            }
        }
    }

    fn dict_get_raw(dict: CFDictionaryRef, key: &str) -> Option<*const c_void> {
        let key = CFString::new(key);
        let mut value: *const c_void = std::ptr::null();
        let found = unsafe {
            CFDictionaryGetValueIfPresent(
                dict,
                key.as_concrete_TypeRef() as *const c_void,
                &mut value,
            )
        };
        (found != 0 && !value.is_null()).then_some(value)
    }

    fn dict_get_dict(dict: CFDictionaryRef, key: &str) -> Option<CFDictionaryRef> {
        dict_get_raw(dict, key).map(|v| v as CFDictionaryRef)
    }

    fn dict_get_i64(dict: CFDictionaryRef, key: &str) -> Option<i64> {
        let v = dict_get_raw(dict, key)?;
        let n = unsafe { CFNumber::wrap_under_get_rule(v as _) };
        n.to_i64()
    }

    fn dict_get_string(dict: CFDictionaryRef, key: &str) -> Option<String> {
        let v = dict_get_raw(dict, key)?;
        let t = unsafe { CFType::wrap_under_get_rule(v as _) };
        t.downcast::<CFString>().map(|s| s.to_string())
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
