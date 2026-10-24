//! `diskdrift info`: environment facts.

use crate::config::Effective;
use diskdrift_diagnostics::access::{self, AccessCheck};
use diskdrift_diagnostics::apfs::{self, ApfsContainer};
use serde::Serialize;
use std::ffi::CString;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemInfo {
    pub os: String,
    pub os_version: Option<String>,
    pub model: Option<String>,
    pub cpus: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InfoReport {
    pub version: String,
    pub system: SystemInfo,
    pub containers: Vec<ApfsContainer>,
    pub apfs_error: Option<String>,
    pub local_snapshots: usize,
    pub access: AccessCheck,
    pub settings: Effective,
    pub store_exists: bool,
}

fn sysctl_string(name: &str) -> Option<String> {
    let cname = CString::new(name).ok()?;
    let mut len: libc::size_t = 0;
    unsafe {
        if libc::sysctlbyname(cname.as_ptr(), std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) != 0 || len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len];
        if libc::sysctlbyname(cname.as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) != 0 {
            return None;
        }
        buf.truncate(len);
        while buf.last() == Some(&0) {
            buf.pop();
        }
        Some(String::from_utf8_lossy(&buf).into_owned())
    }
}

pub fn system_info() -> SystemInfo {
    SystemInfo {
        os: std::env::consts::OS.into(),
        os_version: sysctl_string("kern.osproductversion"),
        model: sysctl_string("hw.model"),
        cpus: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
    }
}

pub fn info(settings: &Effective) -> InfoReport {
    let mounts = diskdrift_platform::mounts().unwrap_or_default();
    let topology = apfs::topology(&mounts);
    let (containers, apfs_error) = match topology {
        Ok(c) => (c, None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    };
    let (snapshots, _) = apfs::local_snapshots(&containers);
    InfoReport {
        version: crate::VERSION.into(),
        system: system_info(),
        local_snapshots: snapshots.len(),
        containers,
        apfs_error,
        access: access::full_disk_access(),
        store_exists: settings.storage_path.value.exists(),
        settings: settings.clone(),
    }
}
