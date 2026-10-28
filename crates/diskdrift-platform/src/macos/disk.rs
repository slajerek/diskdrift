//! What disk a mounted volume is on: Disk Arbitration's description of the
//! volume and of its whole disk, plus the device serial number from the IOKit
//! registry when the device reports one. Read-only: nothing is mounted,
//! claimed or changed.

use super::cf::{self, CFAllocatorRef, CFDictionaryRef, CFStringRef, CFTypeRef, CFURLRef, Owned, Url};
use crate::DeviceInfo;
use std::ffi::{CStr, CString, c_char};
use std::path::Path;

pub(crate) type DASessionRef = CFTypeRef;
pub(crate) type DADiskRef = CFTypeRef;
type IoObject = u32;

#[link(name = "DiskArbitration", kind = "framework")]
unsafe extern "C" {
    pub(crate) fn DASessionCreate(allocator: CFAllocatorRef) -> DASessionRef;
    fn DADiskCreateFromVolumePath(allocator: CFAllocatorRef, session: DASessionRef, path: CFURLRef) -> DADiskRef;
    pub(crate) fn DADiskCopyDescription(disk: DADiskRef) -> CFDictionaryRef;
    pub(crate) fn DADiskCopyWholeDisk(disk: DADiskRef) -> DADiskRef;
    fn DADiskGetBSDName(disk: DADiskRef) -> *const c_char;

    pub(crate) static kDADiskDescriptionVolumeUUIDKey: CFStringRef;
    static kDADiskDescriptionMediaSizeKey: CFStringRef;
    static kDADiskDescriptionMediaRemovableKey: CFStringRef;
    static kDADiskDescriptionMediaEjectableKey: CFStringRef;
    static kDADiskDescriptionDeviceInternalKey: CFStringRef;
    static kDADiskDescriptionDeviceModelKey: CFStringRef;
    static kDADiskDescriptionDeviceVendorKey: CFStringRef;
    static kDADiskDescriptionDeviceProtocolKey: CFStringRef;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOBSDNameMatching(main_port: u32, options: u32, bsd_name: *const c_char) -> CFDictionaryRef;
    fn IOServiceGetMatchingService(main_port: u32, matching: CFDictionaryRef) -> IoObject;
    fn IORegistryEntrySearchCFProperty(entry: IoObject, plane: *const c_char, key: CFStringRef, allocator: CFAllocatorRef, options: u32) -> CFTypeRef;
    fn IOObjectRelease(object: IoObject) -> i32;
}

const K_IO_REGISTRY_ITERATE_RECURSIVELY: u32 = 1;
const K_IO_REGISTRY_ITERATE_PARENTS: u32 = 2;

/// Disk Arbitration's view of the volume mounted at `mount_point`: its BSD
/// name, the volume UUID it reports, and the device.
pub struct DaVolume {
    pub bsd_name: Option<String>,
    pub volume_uuid: Option<String>,
    pub device: DeviceInfo,
}

pub fn describe(mount_point: &Path) -> Option<DaVolume> {
    let url = Url::new(mount_point, true)?;
    unsafe {
        let session = Owned::new(DASessionCreate(std::ptr::null()))?;
        let disk = Owned::new(DADiskCreateFromVolumePath(std::ptr::null(), session.0, url.0.0))?;
        let desc = Owned::new(DADiskCopyDescription(disk.0))?;
        Some(DaVolume {
            bsd_name: bsd_name(disk.0),
            volume_uuid: cf::string_of(cf::dict_get(desc.0, kDADiskDescriptionVolumeUUIDKey)),
            device: device_of(disk.0, desc.0),
        })
    }
}

/// The device facts of a Disk Arbitration disk object (`desc`: its description).
pub(crate) fn device_of(disk: DADiskRef, desc: CFDictionaryRef) -> DeviceInfo {
    unsafe {
        let whole = Owned::new(DADiskCopyWholeDisk(disk));
        let whole_desc = whole.as_ref().and_then(|w| Owned::new(DADiskCopyDescription(w.0)));
        // Device keys are on the volume; the media size and serial belong to the whole disk.
        let dev = |key: CFStringRef| cf::dict_get(desc, key);
        let whole_or_vol = |key: CFStringRef| {
            let v = whole_desc.as_ref().map(|d| cf::dict_get(d.0, key)).unwrap_or(std::ptr::null());
            if v.is_null() { dev(key) } else { v }
        };
        let whole_bsd = whole.as_ref().and_then(|w| bsd_name(w.0));
        DeviceInfo {
            model: cf::string_of(whole_or_vol(kDADiskDescriptionDeviceModelKey)),
            vendor: cf::string_of(whole_or_vol(kDADiskDescriptionDeviceVendorKey)),
            bus: cf::string_of(whole_or_vol(kDADiskDescriptionDeviceProtocolKey)),
            serial: whole_bsd.as_deref().or(bsd_name(disk).as_deref()).and_then(serial_number),
            media_bytes: cf::number_of(whole_or_vol(kDADiskDescriptionMediaSizeKey)),
            internal: cf::bool_of(whole_or_vol(kDADiskDescriptionDeviceInternalKey)),
            removable: cf::bool_of(whole_or_vol(kDADiskDescriptionMediaRemovableKey)),
            ejectable: cf::bool_of(whole_or_vol(kDADiskDescriptionMediaEjectableKey)),
            whole_disk: whole_bsd,
        }
    }
}

pub(crate) fn bsd_name(disk: DADiskRef) -> Option<String> {
    let p = unsafe { DADiskGetBSDName(disk) };
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// The serial number of the device behind `bsd` (`disk4`), searching the IOKit
/// registry upwards; `None` when the device does not report one.
fn serial_number(bsd: &str) -> Option<String> {
    let name = CString::new(bsd).ok()?;
    unsafe {
        let matching = IOBSDNameMatching(0, 0, name.as_ptr());
        if matching.is_null() {
            return None;
        }
        // IOServiceGetMatchingService consumes `matching`.
        let entry = IOServiceGetMatchingService(0, matching);
        if entry == 0 {
            return None;
        }
        let plane = c"IOService";
        let opts = K_IO_REGISTRY_ITERATE_RECURSIVELY | K_IO_REGISTRY_ITERATE_PARENTS;
        let search = |key: &str| Owned::new(IORegistryEntrySearchCFProperty(entry, plane.as_ptr(), cf::cfstr(key).0, std::ptr::null(), opts));
        let mut found = None;
        for key in ["USB Serial Number", "kUSBSerialNumberString", "Serial Number"] {
            if let Some(s) = search(key).and_then(|v| cf::string_of(v.0)) {
                found = Some(s);
                break;
            }
        }
        if found.is_none() {
            // SATA and NVMe devices keep it in a dictionary.
            found = search("Device Characteristics").and_then(|d| cf::string_of(cf::dict_get(d.0, cf::cfstr("Serial Number").0)));
        }
        IOObjectRelease(entry);
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_data_volume_is_described() {
        let v = describe(Path::new("/System/Volumes/Data")).expect("Disk Arbitration knows the Data volume");
        assert!(v.bsd_name.as_deref().is_some_and(|b| b.starts_with("disk")));
        assert!(v.volume_uuid.is_some());
        assert_eq!(v.device.internal, Some(true));
        assert!(v.device.media_bytes.unwrap_or(0) > 0);
    }
}

