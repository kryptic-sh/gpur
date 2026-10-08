//! PDH status handling and wildcard buffers, isolated from adapter aggregation.
use anyhow::{Result, bail, ensure};
use windows::Win32::System::Performance::{
    PDH_CSTATUS_INVALID_DATA, PDH_CSTATUS_ITEM_NOT_VALIDATED, PDH_CSTATUS_NEW_DATA,
    PDH_CSTATUS_NO_INSTANCE, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE,
    PDH_HCOUNTER, PDH_HQUERY, PDH_INVALID_DATA, PDH_MORE_DATA, PDH_NO_DATA, PdhCollectQueryData,
    PdhGetFormattedCounterArrayW,
};

const MAX_ATTEMPTS: usize = 8;

fn available(status: u32, operation: &str) -> Result<bool> {
    match status {
        PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA => Ok(true),
        PDH_NO_DATA
        | PDH_INVALID_DATA
        | PDH_CSTATUS_INVALID_DATA
        | PDH_CSTATUS_NO_INSTANCE
        | PDH_CSTATUS_ITEM_NOT_VALIDATED => Ok(false),
        _ => bail!("{operation} failed with PDH status 0x{status:08x}"),
    }
}

pub(super) fn collect(query: PDH_HQUERY) -> Result<bool> {
    available(unsafe { PdhCollectQueryData(query) }, "PdhCollectQueryData")
}

// u64 storage is initialized, including the bytes used for names after the items.
// Its alignment must satisfy the native item type on every Windows target.
const _: () = assert!(align_of::<u64>() >= align_of::<PDH_FMT_COUNTERVALUE_ITEM_W>());

fn buffer(
    mut fetch: impl FnMut(&mut u32, &mut u32, Option<&mut [u64]>) -> u32,
) -> Result<(Vec<u64>, usize)> {
    for _ in 0..MAX_ATTEMPTS {
        let mut size = 0;
        let mut count = 0;
        let status = fetch(&mut size, &mut count, None);
        if status != PDH_MORE_DATA {
            available(status, "PdhGetFormattedCounterArrayW sizing")?;
            return Ok((Vec::new(), 0));
        }
        ensure!(
            size > 0,
            "PdhGetFormattedCounterArrayW returned PDH_MORE_DATA with an empty buffer"
        );
        let capacity_bytes = size as usize;
        let mut storage = vec![0u64; capacity_bytes.div_ceil(size_of::<u64>())];
        let status = fetch(&mut size, &mut count, Some(&mut storage));
        if status == PDH_MORE_DATA {
            continue;
        }
        if !available(status, "PdhGetFormattedCounterArrayW fill")? {
            return Ok((Vec::new(), 0));
        }
        ensure!(
            size as usize <= capacity_bytes
                && count as usize <= size as usize / size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>(),
            "PdhGetFormattedCounterArrayW returned an invalid buffer size/count"
        );
        return Ok((storage, count as usize));
    }
    bail!("PdhGetFormattedCounterArrayW exhausted retries with PDH status 0x{PDH_MORE_DATA:08x}")
}

pub(super) fn read_array(counter: PDH_HCOUNTER) -> Result<Vec<(String, f64)>> {
    let (storage, count) = buffer(|size, count, storage| unsafe {
        PdhGetFormattedCounterArrayW(
            counter,
            PDH_FMT_DOUBLE,
            size,
            count,
            storage.map(|s| s.as_mut_ptr().cast()),
        )
    })?;
    decode(&storage, count)
}

fn decode(storage: &[u64], count: usize) -> Result<Vec<(String, f64)>> {
    ensure!(
        count <= std::mem::size_of_val(storage) / size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>(),
        "PDH item count exceeds buffer"
    );
    let mut result = Vec::new();
    for index in 0..count {
        // The successful native call initializes the items; buffer checked their
        // extent and the backing allocation has the required alignment.
        let item = unsafe {
            &*storage
                .as_ptr()
                .cast::<PDH_FMT_COUNTERVALUE_ITEM_W>()
                .add(index)
        };
        if !matches!(
            item.FmtValue.CStatus,
            PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA
        ) {
            continue;
        }
        let value = unsafe { item.FmtValue.Anonymous.doubleValue };
        if !value.is_finite() || value < 0.0 {
            continue;
        }
        // Validate the name against the owned buffer rather than trusting an
        // unbounded NUL scan through the returned pointer.
        let offset = (item.szName.0 as usize).checked_sub(storage.as_ptr() as usize);
        let bytes = std::mem::size_of_val(storage);
        let offset = offset
            .filter(|o| {
                *o >= count * size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>()
                    && *o < bytes
                    && o.is_multiple_of(size_of::<u16>())
            })
            .ok_or_else(|| anyhow::anyhow!("PDH counter name is outside its buffer"))?;
        let name = unsafe {
            std::slice::from_raw_parts(
                storage.as_ptr().cast::<u16>().add(offset / 2),
                (bytes - offset) / 2,
            )
        };
        let end = name
            .iter()
            .position(|&c| c == 0)
            .ok_or_else(|| anyhow::anyhow!("PDH counter name is not NUL terminated"))?;
        result.push((String::from_utf16(&name[..end])?.to_lowercase(), value));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Performance::{
        PDH_CALC_NEGATIVE_DENOMINATOR, PDH_CALC_NEGATIVE_TIMEBASE, PDH_CALC_NEGATIVE_VALUE,
        PDH_INVALID_HANDLE,
    };

    fn samples(statuses: &[u32]) -> Vec<u64> {
        let names_offset = statuses.len() * size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        let mut storage = vec![0u64; (names_offset + statuses.len() * 4).div_ceil(8)];
        for (index, &status) in statuses.iter().enumerate() {
            // Names and items occupy disjoint, aligned ranges of the allocation.
            unsafe {
                let base = storage.as_mut_ptr();
                let name = base
                    .cast::<u8>()
                    .add(names_offset + index * 4)
                    .cast::<u16>();
                name.write(u16::from(b'A') + index as u16);
                name.add(1).write(0);
                let item = base.cast::<PDH_FMT_COUNTERVALUE_ITEM_W>().add(index);
                (*item).szName = windows::core::PWSTR(name);
                (*item).FmtValue.CStatus = status;
                (*item).FmtValue.Anonymous.doubleValue = index as f64 + 1.0;
            }
        }
        storage
    }

    #[test]
    fn calculation_errors_do_not_discard_valid_samples() {
        let statuses = [
            PDH_CSTATUS_VALID_DATA,
            PDH_CALC_NEGATIVE_DENOMINATOR,
            PDH_CSTATUS_NEW_DATA,
            PDH_CALC_NEGATIVE_TIMEBASE,
            PDH_CALC_NEGATIVE_VALUE,
            u32::MAX,
        ];
        let storage = samples(&statuses);
        assert_eq!(
            decode(&storage, statuses.len()).unwrap(),
            vec![("a".into(), 1.0), ("c".into(), 3.0)]
        );
        for status in &statuses[3..5] {
            assert!(available(*status, "operation").is_err());
        }
    }

    #[test]
    fn non_null_names_must_be_aligned_and_inside_the_name_region() {
        let mut storage = samples(&[PDH_CSTATUS_VALID_DATA]);
        let bytes = std::mem::size_of_val(storage.as_slice());
        for offset in [
            0,
            size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>() - 2,
            size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>() + 1,
            bytes,
            bytes + 2,
        ] {
            unsafe {
                let base = storage.as_mut_ptr();
                (*base.cast::<PDH_FMT_COUNTERVALUE_ITEM_W>()).szName =
                    windows::core::PWSTR(base.cast::<u8>().wrapping_add(offset).cast());
            }
            assert!(
                decode(&storage, 1)
                    .unwrap_err()
                    .to_string()
                    .contains("outside its buffer"),
                "offset {offset}"
            );
        }
    }

    #[test]
    fn names_must_terminate_inside_the_buffer() {
        let mut storage = samples(&[PDH_CSTATUS_VALID_DATA]);
        let start = size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>() / size_of::<u16>();
        let end = std::mem::size_of_val(storage.as_slice()) / size_of::<u16>();
        unsafe {
            let base = storage.as_mut_ptr().cast::<u16>();
            for index in start..end {
                base.add(index).write(u16::from(b'A'));
            }
        }
        assert!(
            decode(&storage, 1)
                .unwrap_err()
                .to_string()
                .contains("not NUL terminated")
        );
    }

    #[test]
    fn fill_size_and_count_must_fit_the_reported_extent() {
        let item_size = size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>() as u32;
        for (size_after_fill, count_after_fill) in [
            (129, 1),
            (128, 128 / item_size + 1),
            (item_size - 1, 1),
            (0, 1),
        ] {
            let error = buffer(|size, count, storage| {
                if storage.is_none() {
                    *size = 128;
                    PDH_MORE_DATA
                } else {
                    *size = size_after_fill;
                    *count = count_after_fill;
                    PDH_CSTATUS_VALID_DATA
                }
            })
            .unwrap_err();
            assert!(error.to_string().contains("invalid buffer size/count"));
        }
    }

    #[test]
    fn decoded_values_accept_new_and_valid_but_not_invalid() {
        let mut storage = vec![0u64; 16];
        let name_offset = size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        // Construct the native layout in its aligned allocation, with the name
        // after the item, exactly as PDH's successful fill provides it.
        unsafe {
            let name = storage
                .as_mut_ptr()
                .cast::<u8>()
                .add(name_offset)
                .cast::<u16>();
            name.write(u16::from(b'A'));
            name.add(1).write(0);
            let item = storage.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
            (*item).szName = windows::core::PWSTR(name);
            (*item).FmtValue.Anonymous.doubleValue = 37.5;
        }
        for status in [
            PDH_CSTATUS_VALID_DATA,
            PDH_CSTATUS_NEW_DATA,
            PDH_CSTATUS_INVALID_DATA,
        ] {
            unsafe {
                (*storage.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>())
                    .FmtValue
                    .CStatus = status;
            }
            let values = decode(&storage, 1).unwrap();
            if status == PDH_CSTATUS_INVALID_DATA {
                assert!(values.is_empty());
            } else {
                assert_eq!(values, vec![("a".into(), 37.5)]);
            }
        }
        assert!(decode(&storage, usize::MAX).is_err());
        unsafe {
            let item = storage.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
            (*item).FmtValue.CStatus = PDH_CSTATUS_VALID_DATA;
            (*item).szName = windows::core::PWSTR::null();
        }
        assert!(
            decode(&storage, 1)
                .unwrap_err()
                .to_string()
                .contains("outside its buffer")
        );
    }

    #[test]
    fn valid_and_new_are_readings_but_transient_data_is_unknown() {
        assert!(available(PDH_CSTATUS_VALID_DATA, "test").unwrap());
        assert!(available(PDH_CSTATUS_NEW_DATA, "test").unwrap());
        for status in [
            PDH_CSTATUS_INVALID_DATA,
            PDH_CSTATUS_NO_INSTANCE,
            PDH_NO_DATA,
            PDH_INVALID_DATA,
            PDH_CSTATUS_ITEM_NOT_VALIDATED,
        ] {
            assert!(!available(status, "test").unwrap());
        }
        assert!(
            available(PDH_INVALID_HANDLE, "test")
                .unwrap_err()
                .to_string()
                .contains("0xc0000bbc")
        );
    }

    #[test]
    fn growth_retries_and_retains_successful_buffer() {
        let mut calls = 0;
        let (data, count) = buffer(|size, count, data| {
            calls += 1;
            if let Some(data) = data {
                if calls == 2 {
                    return PDH_MORE_DATA;
                }
                data[0] = 123;
                *count = 1;
                0
            } else {
                *size = 128;
                PDH_MORE_DATA
            }
        })
        .unwrap();
        assert_eq!(calls, 4);
        assert_eq!(count, 1);
        assert_eq!(data[0], 123);
    }

    #[test]
    fn endless_growth_and_fill_errors_are_errors() {
        let mut calls = 0;
        let error = buffer(|size, _, _| {
            calls += 1;
            *size = 128;
            PDH_MORE_DATA
        })
        .unwrap_err();
        assert_eq!(calls, MAX_ATTEMPTS * 2);
        assert!(error.to_string().contains("0x800007d2"));
        let error = buffer(|size, _, data| {
            *size = 128;
            if data.is_none() {
                PDH_MORE_DATA
            } else {
                PDH_INVALID_HANDLE
            }
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("fill failed with PDH status 0xc0000bbc")
        );
    }
}
