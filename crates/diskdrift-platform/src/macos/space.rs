//! Volume capacity values from CoreFoundation URL resource properties.
//!
//! `statfs` only knows strictly-free blocks. macOS additionally reports capacity
//! "available for important usage" (what Finder shows), which includes purgeable
//! space the OS can reclaim on demand: local snapshots, evictable caches.

use super::cf::{CFStringRef, Url};
use crate::VolumeCapacity;
use std::path::Path;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFURLVolumeTotalCapacityKey: CFStringRef;
    static kCFURLVolumeAvailableCapacityKey: CFStringRef;
    static kCFURLVolumeAvailableCapacityForImportantUsageKey: CFStringRef;
    static kCFURLVolumeAvailableCapacityForOpportunisticUsageKey: CFStringRef;
    static kCFURLVolumeNameKey: CFStringRef;
    static kCFURLVolumeUUIDStringKey: CFStringRef;
}

/// Queries capacity values for the volume containing `path`.
pub fn volume_capacity(path: &Path) -> VolumeCapacity {
    let Some(url) = Url::new(path, true) else { return VolumeCapacity::default() };
    unsafe {
        VolumeCapacity {
            total_bytes: url.number(kCFURLVolumeTotalCapacityKey),
            available_bytes: url.number(kCFURLVolumeAvailableCapacityKey),
            available_important_bytes: url.number(kCFURLVolumeAvailableCapacityForImportantUsageKey),
            available_opportunistic_bytes: url.number(kCFURLVolumeAvailableCapacityForOpportunisticUsageKey),
            volume_name: url.string(kCFURLVolumeNameKey),
            volume_uuid: url.string(kCFURLVolumeUUIDStringKey),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_volume_has_capacity() {
        let c = volume_capacity(Path::new("/"));
        assert!(c.total_bytes.unwrap() > 0);
        assert!(c.available_bytes.is_some());
        assert!(c.volume_uuid.is_some());
        if let (Some(i), Some(a)) = (c.available_important_bytes, c.available_bytes) {
            assert!(i + (1 << 30) >= a, "important-usage capacity should not be far below free space");
        }
    }
}
