//! Full Disk Access detection.

use serde::Serialize;
#[cfg(unix)]
use std::ffi::CString;

/// Protected by TCC; readable only with Full Disk Access (even for root).
#[cfg(unix)]
const PROBE: &str = "/Library/Application Support/com.apple.TCC/TCC.db";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Access {
    Granted,
    NotGranted,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessCheck {
    pub full_disk_access: Access,
    pub detail: String,
}

/// Probes Full Disk Access by opening a TCC-protected file read-only (the file is
/// opened and closed, never read or written).
#[cfg(unix)]
pub fn full_disk_access() -> AccessCheck {
    let path = CString::new(PROBE).unwrap();
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd >= 0 {
        unsafe { libc::close(fd) };
        return AccessCheck {
            full_disk_access: Access::Granted,
            detail: "this process can read TCC-protected locations".into(),
        };
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EPERM) | Some(libc::EACCES) => AccessCheck {
            full_disk_access: Access::NotGranted,
            detail: "protected locations (Mail, Messages, Safari, iOS backups, Trash) cannot be measured; \
                     grant Full Disk Access to your terminal in System Settings > Privacy & Security"
                .into(),
        },
        _ => AccessCheck { full_disk_access: Access::Unknown, detail: format!("probe failed: {err}") },
    }
}

#[cfg(not(unix))]
pub fn full_disk_access() -> AccessCheck {
    AccessCheck {
        full_disk_access: Access::Unknown,
        detail: "Full Disk Access probing is not supported on this platform".into(),
    }
}
