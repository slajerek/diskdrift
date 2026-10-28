//! Platform adapters: directory listing, mounted volumes, capacity values.
//!
//! macOS is the implemented target. Other Unix systems get a portable fallback
//! (std listing, `statvfs` without volume-group handling).

use diskdrift_core::walk::DirLister;
use diskdrift_core::native::{ForeignEncoding, NativePath, NativeRoot, RootKind};
use std::ffi::OsStr;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub fn native_path(value: &OsStr) -> NativePath {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        NativePath::unix(value.as_bytes().to_vec())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        NativePath::windows(value.encode_wide().collect())
    }
}

pub fn native_root(value: &OsStr) -> Result<NativeRoot, diskdrift_core::native::NativeError> {
    let path = native_path(value);
    #[cfg(unix)]
    { NativeRoot::new(path, RootKind::Posix, false) }
    #[cfg(windows)]
    {
        let units = match &path { NativePath::Windows(v) => v, NativePath::Unix(_) => unreachable!() };
        let text = String::from_utf16_lossy(units);
        let (kind, verbatim) = if text.starts_with(r"\\?\Volume{") { (RootKind::VolumeGuid, true) }
        else if text.starts_with(r"\\?\UNC\") { (RootKind::Unc, true) }
        else if text.starts_with(r"\\?\") { (RootKind::Drive, true) }
        else if text.starts_with(r"\\") { (RootKind::Unc, false) }
        else { (RootKind::Drive, false) };
        NativeRoot::new(path, kind, verbatim)
    }
}

/// The platform's default volume root for a full scan.
pub fn system_volume_root() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/")
    }
    #[cfg(target_os = "linux")]
    {
        PathBuf::from("/")
    }
    #[cfg(target_os = "windows")]
    {
        let system_directory = std::env::var_os("SystemRoot")
            .or_else(|| std::env::var_os("WINDIR"))
            .unwrap_or_else(|| std::ffi::OsString::from(r"C:\Windows"));

        Path::new(&system_directory)
            .components()
            .next()
            .map(|prefix| {
                let mut root = PathBuf::from(prefix.as_os_str());
                root.push(r"\");
                root
            })
            .unwrap_or_else(|| PathBuf::from(r"C:\"))
    }
}

#[cfg(test)]
mod root_tests {
    use super::system_volume_root;
    use std::path::Path;
    #[cfg(target_os = "windows")]
    use std::path::PathBuf;

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_system_volume_root_is_filesystem_root() {
        assert_eq!(system_volume_root(), Path::new("/"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_system_volume_root_is_filesystem_root() {
        assert_eq!(system_volume_root(), Path::new("/"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_system_volume_root_is_system_directory_volume_root() {
        let system_directory = std::env::var_os("SystemRoot")
            .or_else(|| std::env::var_os("WINDIR"))
            .expect("Windows must provide the system directory");
        let expected = Path::new(&system_directory)
            .components()
            .next()
            .expect("system directory must have a volume prefix")
            .as_os_str()
            .to_owned();
        let mut expected = PathBuf::from(expected);
        expected.push("\\");

        assert_eq!(system_volume_root(), expected);
    }
}

pub fn path_from_native(value: &NativePath) -> Result<PathBuf, ForeignEncoding> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        match value {
            NativePath::Unix(bytes) => Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes.clone()))),
            NativePath::Windows(_) => Err(ForeignEncoding::WindowsOnUnix),
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        match value {
            NativePath::Unix(_) => Err(ForeignEncoding::UnixOnWindows),
            NativePath::Windows(units) => Ok(PathBuf::from(std::ffi::OsString::from_wide(units))),
        }
    }
}

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
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
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
        #[cfg(unix)]
        ListerKind::ReadDir => Box::new(diskdrift_core::walk::StdLister),
        #[cfg(target_os = "macos")]
        ListerKind::Auto | ListerKind::Bulk => Box::new(macos::bulk::BulkLister),
        #[cfg(all(unix, not(target_os = "macos")))]
        ListerKind::Auto | ListerKind::Bulk => Box::new(diskdrift_core::walk::StdLister),
        #[cfg(not(unix))]
        ListerKind::Auto | ListerKind::Bulk | ListerKind::ReadDir => Box::new(diskdrift_core::walk::StdLister),
    }
}

pub fn mounts() -> std::io::Result<Vec<MountInfo>> {
    #[cfg(target_os = "macos")]
    {
        macos::mounts()
    }
    #[cfg(not(target_os = "macos"))]
    {
        #[cfg(target_os = "linux")]
        {
            let text = std::fs::read_to_string("/proc/self/mountinfo")?;
            let mut out = Vec::new();
            for line in text.lines() {
                let Some((left, right)) = line.split_once(" - ") else { continue };
                let fields: Vec<_> = left.split_whitespace().collect();
                let Some(raw_mount) = fields.get(4) else { continue };
                let mount = raw_mount.replace(r"\040", " ").replace(r"\011", "\t").replace(r"\012", "\n").replace(r"\134", "\\");
                let mount_point = PathBuf::from(mount);
                if out.iter().any(|m: &MountInfo| m.mount_point == mount_point) {
                    continue;
                }
                let mut info = statfs_path(&mount_point)?;
                let details: Vec<_> = right.split_whitespace().collect();
                info.mount_point = mount_point;
                info.fs_type = details.first().copied().unwrap_or("unknown").into();
                info.device = details.get(1).copied().unwrap_or_default().into();
                out.push(info);
            }
            Ok(out)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "mount listing is not implemented on this target"))
        }
    }
}

pub fn statfs_path(path: &Path) -> std::io::Result<MountInfo> {
    #[cfg(target_os = "macos")]
    {
        macos::statfs_path(path)
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))?;
        let mut stats = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
        if unsafe { libc::statvfs(c_path.as_ptr(), stats.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let stats = unsafe { stats.assume_init() };
        let bytes = |blocks: libc::c_ulong| blocks.saturating_mul(stats.f_frsize);
        use std::os::unix::fs::MetadataExt;
        let dev = std::fs::metadata(path).map(|m| m.dev()).unwrap_or(0);
        let mut mount_point = PathBuf::from("/");
        let mut device = String::new();
        let mut fs_type = "unknown".to_string();
        if let Ok(mounts) = std::fs::read_to_string("/proc/self/mountinfo") {
            for line in mounts.lines() {
                let Some((left, right)) = line.split_once(" - ") else { continue };
                let fields: Vec<_> = left.split_whitespace().collect();
                let Some(raw_mount) = fields.get(4) else { continue };
                let mount = raw_mount.replace(r"\040", " ").replace(r"\011", "\t").replace(r"\012", "\n").replace(r"\134", "\\");
                let candidate = Path::new(&mount);
                if path.starts_with(candidate) && candidate.as_os_str().len() > mount_point.as_os_str().len() {
                    mount_point = candidate.to_path_buf();
                    let details: Vec<_> = right.split_whitespace().collect();
                    fs_type = details.first().copied().unwrap_or("unknown").into();
                    device = details.get(1).copied().unwrap_or_default().into();
                }
            }
        }
        Ok(MountInfo {
            mount_point,
            device,
            fs_type,
            total_bytes: bytes(stats.f_blocks),
            free_bytes: bytes(stats.f_bfree),
            available_bytes: bytes(stats.f_bavail),
            read_only: (stats.f_flag & libc::ST_RDONLY) != 0,
            local: true,
            browsable: true,
            dev,
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        std::fs::metadata(path)?;
        Ok(MountInfo {
            mount_point: path.to_path_buf(),
            device: String::new(),
            fs_type: "unknown".into(),
            total_bytes: 0,
            free_bytes: 0,
            available_bytes: 0,
            read_only: false,
            local: true,
            browsable: true,
            dev: 0,
        })
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
