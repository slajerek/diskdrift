//! Platform adapters: directory listing, mounted volumes, capacity values.
//!
//! macOS is the implemented target. Other Unix systems get a portable fallback
//! (std listing, `statvfs` without volume-group handling).

use diskdrift_core::walk::DirLister;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
pub mod macos;

/// One mounted filesystem.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MountInfo {
    pub mount_point: PathBuf,
    /// Device or source (`/dev/disk3s5`, `map auto_home`, ...).
    pub device: String,
    pub fs_type: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// Free bytes available to unprivileged users.
    pub available_bytes: u64,
    pub read_only: bool,
    pub local: bool,
    /// Not flagged "nobrowse" (Finder shows it).
    pub browsable: bool,
    /// `st_dev` of the mount point; 0 if not determined (non-local mounts).
    #[serde(skip)]
    pub dev: u64,
}

/// Capacity values for the volume containing a path. Every field is optional: the
/// OS may not provide a value for a given volume.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeCapacity {
    pub total_bytes: Option<u64>,
    /// Strictly free space.
    pub available_bytes: Option<u64>,
    /// Space available for important (user-initiated) writes, including purgeable.
    pub available_important_bytes: Option<u64>,
    /// Space available for opportunistic (background) writes.
    pub available_opportunistic_bytes: Option<u64>,
    pub volume_name: Option<String>,
    pub volume_uuid: Option<String>,
}

impl VolumeCapacity {
    /// Purgeable estimate: important-usage available minus strictly free.
    pub fn purgeable_estimate(&self) -> Option<u64> {
        match (self.available_important_bytes, self.available_bytes) {
            (Some(i), Some(a)) => Some(i.saturating_sub(a)),
            _ => None,
        }
    }
}

/// Capacity values for the volume containing `path` (all `None` where unsupported).
pub fn volume_capacity(path: &Path) -> VolumeCapacity {
    #[cfg(target_os = "macos")]
    {
        macos::space::volume_capacity(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        VolumeCapacity::default()
    }
}

/// The physical device a volume is on, as far as the OS reports it. Every field
/// is optional: disk images, network volumes and some adapters report little.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub model: Option<String>,
    pub vendor: Option<String>,
    /// Bus or protocol ("USB", "PCI-Express", "Disk Image", ...).
    pub bus: Option<String>,
    pub serial: Option<String>,
    /// Size of the whole medium in bytes.
    pub media_bytes: Option<u64>,
    pub internal: Option<bool>,
    pub removable: Option<bool>,
    pub ejectable: Option<bool>,
    /// BSD name of the whole disk (`disk4`).
    pub whole_disk: Option<String>,
}

/// What disk the volume holding a path is: the facts a disk identity is built
/// from (ADR 0002). Read-only.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskInfo {
    pub mount_point: PathBuf,
    pub fs_type: String,
    pub volume_name: Option<String>,
    pub volume_uuid: Option<String>,
    /// Capacity of the volume (for APFS: of its container).
    pub capacity_bytes: Option<u64>,
    /// BSD name of the volume (`disk4s1`).
    pub bsd_name: Option<String>,
    pub device: DeviceInfo,
}

/// The disk holding `path`: mount point and filesystem from `statfs`, name and
/// capacity from the volume, UUID and device facts from Disk Arbitration and
/// IOKit (macOS). Nothing is mounted or changed.
///
/// The UUID is Disk Arbitration's: for FAT volumes the one CoreFoundation
/// reports changed between two mounts of the same volume (2026-10-22, disk
/// images), while Disk Arbitration's, derived from the volume serial, did not.
/// For `/` it describes the Data volume instead: the system volume `/` is a
/// sealed snapshot whose UUID changes with every macOS update, and the macOS
/// system disk is one disk (ADR 0002).
pub fn disk_info(path: &Path) -> std::io::Result<DiskInfo> {
    let mount = statfs_path(path)?;
    let cap = volume_capacity(&mount.mount_point);
    let mut info = DiskInfo {
        fs_type: mount.fs_type.clone(),
        volume_name: cap.volume_name,
        volume_uuid: cap.volume_uuid,
        capacity_bytes: cap.total_bytes.or(Some(mount.total_bytes).filter(|&b| b > 0)),
        mount_point: mount.mount_point,
        ..DiskInfo::default()
    };
    #[cfg(target_os = "macos")]
    {
        let data = Path::new("/System/Volumes/Data");
        let described = if info.mount_point == Path::new("/") && data.is_dir() { data } else { info.mount_point.as_path() };
        if let Some(da) = macos::disk::describe(described) {
            info.volume_uuid = da.volume_uuid.or(info.volume_uuid);
            info.bsd_name = da.bsd_name;
            info.device = da.device;
        }
    }
    Ok(info)
}

/// A volume whose automatic mount careful mode refused (spec 3C): not
/// mounted, nothing written. `info.mount_point` is empty.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeldVolume {
    /// BSD name of the volume (`disk4s1`).
    pub bsd_name: String,
    /// The medium accepts writes (a write-protected card or a write blocker: `false`).
    pub writable: Option<bool>,
    pub info: DiskInfo,
}

/// What the careful-mode guard reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardEvent {
    /// The system tried to mount it; careful mode refused.
    Held(Box<HeldVolume>),
    /// A held disk, or one DiskDrift mounted, is gone (unplugged or ejected).
    Gone(String),
}

/// A refusal from Disk Arbitration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{} (0x{status:08x})", message.as_deref().unwrap_or("refused"))]
pub struct DaError {
    pub status: u32,
    pub message: Option<String>,
}

impl DaError {
    /// The volume is dirty (not unmounted cleanly) or its journal is not
    /// empty: Disk Arbitration will not mount it read-only without repairing
    /// it first, and careful mode never repairs (research careful-scan.md 4).
    pub fn needs_direct_mount(&self) -> bool {
        // EDIRTY from a dirty exFAT/FAT volume; EPERM for HFS+ with a non-empty journal.
        matches!(self.status, 0xc05c | 0xc001)
    }
}

#[cfg(target_os = "macos")]
pub use macos::guard::MountGuard;

/// Careful mode is macOS only (Disk Arbitration).
#[cfg(not(target_os = "macos"))]
pub struct MountGuard;

#[cfg(not(target_os = "macos"))]
impl MountGuard {
    pub fn start(_events: Box<dyn Fn(GuardEvent) + Send>) -> std::io::Result<MountGuard> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "careful mode needs macOS"))
    }
    pub fn mount_read_only(&self, _bsd: &str, _browse: bool) -> Result<PathBuf, DaError> {
        Err(DaError { status: 0, message: Some("careful mode needs macOS".into()) })
    }
    pub fn unmount_and_eject(&self, _bsd: &str) -> Result<(), DaError> {
        Err(DaError { status: 0, message: Some("careful mode needs macOS".into()) })
    }
}

/// Platform-specific adjustments to a scan.
#[derive(Debug, Clone, Default)]
pub struct ScanLayout {
    pub same_filesystem_devs: Vec<u64>,
    pub alias_paths: HashSet<PathBuf>,
    /// The root is part of the macOS System+Data volume group.
    pub volume_group: bool,
}

/// Selectable directory listing backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListerKind {
    /// Fastest available on this platform.
    Auto,
    /// Portable `read_dir` + `lstat`.
    ReadDir,
    /// macOS `getattrlistbulk`.
    Bulk,
}

/// Lowers the calling thread's CPU and I/O priority (background work). A no-op
/// where unsupported.
pub fn lower_thread_priority() {
    #[cfg(target_os = "macos")]
    macos::lower_thread_priority();
}

pub fn lister(kind: ListerKind) -> Box<dyn DirLister> {
    match kind {
        ListerKind::ReadDir => Box::new(diskdrift_core::walk::StdLister),
        #[cfg(target_os = "macos")]
        ListerKind::Auto | ListerKind::Bulk => Box::new(macos::bulk::BulkLister),
        #[cfg(not(target_os = "macos"))]
        ListerKind::Auto | ListerKind::Bulk => Box::new(diskdrift_core::walk::StdLister),
    }
}

pub fn mounts() -> std::io::Result<Vec<MountInfo>> {
    #[cfg(target_os = "macos")]
    {
        macos::mounts()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "mount listing is implemented for macOS only"))
    }
}

pub fn statfs_path(path: &Path) -> std::io::Result<MountInfo> {
    #[cfg(target_os = "macos")]
    {
        macos::statfs_path(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "statfs is implemented for macOS only"))
    }
}

pub fn scan_layout(root: &Path) -> ScanLayout {
    #[cfg(target_os = "macos")]
    {
        macos::scan_layout(root)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = root;
        ScanLayout::default()
    }
}

/// Mount containing `path`: the mount with the longest matching mount point.
pub fn mount_for<'a>(mounts: &'a [MountInfo], path: &Path) -> Option<&'a MountInfo> {
    mounts
        .iter()
        .filter(|m| path.starts_with(&m.mount_point))
        .max_by_key(|m| m.mount_point.as_os_str().len())
}

#[cfg(all(test, target_os = "macos"))]
mod disk_tests {
    use super::*;

    #[test]
    fn disk_info_of_the_home_folder() {
        let home = std::env::home_dir().unwrap();
        let i = disk_info(&home).unwrap();
        assert_eq!(i.mount_point, Path::new("/System/Volumes/Data"));
        assert_eq!(i.fs_type, "apfs");
        assert!(i.volume_uuid.is_some() && i.bsd_name.is_some());
        assert_eq!(i.device.internal, Some(true));
        assert!(i.capacity_bytes.unwrap() > 0);
    }

    #[test]
    fn the_system_disk_is_one_disk() {
        let root = disk_info(Path::new("/")).unwrap();
        let home = disk_info(&std::env::home_dir().unwrap()).unwrap();
        assert_eq!(root.mount_point, Path::new("/"));
        assert_eq!(root.volume_uuid, home.volume_uuid, "/ is identified by the Data volume, as the home folder is");
        assert_eq!(root.bsd_name, home.bsd_name);
    }
}

