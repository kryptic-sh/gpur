#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Engine {
    pub luid: String,
    pub physical: u32,
    pub index: u32,
}

/// Only process instances contribute: adapter totals, if supplied by a provider,
/// must not be added to the same engine's process samples.
pub fn engine_instance(instance: &str) -> Option<(u32, Engine, String)> {
    let pid = pid_prefix(instance)?;
    let luid = luid_prefix(instance)?;
    let (_, tail) = instance.split_once("_phys_")?;
    let (physical, tail) = tail.split_once("_eng_")?;
    let (index, kind) = tail.split_once("_engtype_")?;
    if kind.is_empty() {
        return None;
    }
    Some((
        pid,
        Engine {
            luid,
            physical: physical.parse().ok()?,
            index: index.parse().ok()?,
        },
        kind.to_string(),
    ))
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

/// Select the adapter's memory pool. An instance in only the other pool proves
/// the process was accounted for, with no bytes in the selected pool. Neither
/// instance means the reading is unknown.
pub fn proc_mem_bytes(
    integrated: bool,
    dedicated: Option<u64>,
    shared: Option<u64>,
) -> Option<u64> {
    if integrated {
        shared.or(dedicated.map(|_| 0))
    } else {
        dedicated.or(shared.map(|_| 0))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn engine_instance_preserves_identity() {
        assert_eq!(
            engine_instance(ENGINE),
            Some((
                1234,
                Engine {
                    luid: "luid_0x00000000_0x0000c4cf".into(),
                    physical: 0,
                    index: 0
                },
                "3d".into()
            ))
        );
        assert_eq!(engine_instance(ADAPTER_MEM), None);
        assert_eq!(engine_instance("pid_5_phys_0_engtype_3d"), None);
        assert_eq!(engine_instance(&ENGINE.replace("_eng_0", "_eng_bad")), None);
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
