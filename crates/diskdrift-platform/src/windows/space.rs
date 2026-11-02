//! Windows capacity and allocation provenance.

use crate::{MeasurementQuality, VolumeCapacity};
use diskdrift_core::record::TimestampResolution;
use std::io;
use std::path::Path;
use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

pub fn capacity(path: &Path) -> io::Result<VolumeCapacity> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let (mut available_to_caller, mut total, mut total_free) = (0u64, 0u64, 0u64);
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available_to_caller,
            &mut total,
            &mut total_free,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(capacity_from_values(available_to_caller, total, total_free))
}

fn capacity_from_values(available_to_caller: u64, total: u64, total_free: u64) -> VolumeCapacity {
    VolumeCapacity {
        total_bytes: Some(total),
        free_bytes: Some(total_free),
        available_bytes: Some(available_to_caller),
        // Windows exposes one caller-available value, not APFS-style reclaimability.
        available_important_bytes: None,
        available_opportunistic_bytes: None,
        domain: Some("volume".into()),
        scope: Some("caller".into()),
        observed_at: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        quality: MeasurementQuality::Observed,
        provenance: Some(
            "GetDiskFreeSpaceExW: lpFreeBytesAvailableToCaller and lpTotalNumberOfFreeBytes".into(),
        ),
        timestamp_resolution: TimestampResolution::Seconds,
        ..VolumeCapacity::default()
    }
}

pub fn volume_capacity(path: &Path) -> io::Result<VolumeCapacity> {
    capacity(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_caller_available_and_total_free_to_distinct_fields() {
        let sample = capacity_from_values(11, 100, 20);
        assert_eq!(sample.total_bytes, Some(100));
        assert_eq!(sample.free_bytes, Some(20));
        assert_eq!(sample.available_bytes, Some(11));
        assert_eq!(sample.quality, MeasurementQuality::Observed);
        assert!(
            sample
                .provenance
                .as_deref()
                .is_some_and(|value| value.contains("lpFreeBytesAvailableToCaller"))
        );
    }
}
