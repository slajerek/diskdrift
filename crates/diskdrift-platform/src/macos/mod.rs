//! macOS adapters.

pub mod bulk;
pub(crate) mod cf;
pub(crate) mod disk;
pub mod guard;
pub mod space;

use crate::{MountInfo, ScanLayout};
use std::ffi::CStr;
use std::io;
use std::path::{Path, PathBuf};

pub const DATA_VOLUME_MOUNT: &str = "/System/Volumes/Data";
const FIRMLINKS_FILE: &str = "/usr/share/firmlinks";

fn cstr_field(field: &[libc::c_char]) -> String {
    unsafe { CStr::from_ptr(field.as_ptr()) }.to_string_lossy().into_owned()
}

unsafe extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
}

/// `QOS_CLASS_UTILITY` from `<sys/qos.h>`: long-running work the user is not
/// waiting on; the scheduler gives it lower CPU and I/O priority.
const QOS_CLASS_UTILITY: u32 = 0x11;

/// Lowers the calling thread to the utility QoS class (CPU and disk I/O).
pub fn lower_thread_priority() {
    // SAFETY: plain syscall wrapper with constant arguments; failure only
    // leaves the priority unchanged.
    unsafe {
        pthread_set_qos_class_self_np(QOS_CLASS_UTILITY, 0);
    }
}

/// All mounted filesystems (`getfsstat` with MNT_NOWAIT: never blocks on stale
/// network mounts).
pub fn mounts() -> io::Result<Vec<MountInfo>> {
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf: Vec<libc::statfs> = Vec::with_capacity(count as usize + 8);
    let size = (buf.capacity() * std::mem::size_of::<libc::statfs>()) as libc::c_int;
    let n = unsafe { libc::getfsstat(buf.as_mut_ptr(), size, libc::MNT_NOWAIT) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe { buf.set_len(n as usize) };
    Ok(buf.iter().map(mount_from_statfs).collect())
}

fn mount_from_statfs(s: &libc::statfs) -> MountInfo {
    let bsize = s.f_bsize as u64;
    let mount_point = PathBuf::from(cstr_field(&s.f_mntonname));
    let local = s.f_flags & libc::MNT_LOCAL as u32 != 0;
    let dev = if local {
        std::fs::symlink_metadata(&mount_point)
            .map(|m| std::os::unix::fs::MetadataExt::dev(&m))
            .unwrap_or(0)
    } else {
        0
    };
    MountInfo {
        mount_point,
        device: cstr_field(&s.f_mntfromname),
        fs_type: cstr_field(&s.f_fstypename),
        total_bytes: s.f_blocks.saturating_mul(bsize),
        free_bytes: s.f_bfree.saturating_mul(bsize),
        available_bytes: s.f_bavail.saturating_mul(bsize),
        read_only: s.f_flags & libc::MNT_RDONLY as u32 != 0,
        local,
        browsable: s.f_flags & libc::MNT_DONTBROWSE as u32 == 0,
        dev,
    }
}

/// `statfs` of the filesystem containing `path`.
pub fn statfs_path(path: &Path) -> io::Result<MountInfo> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(mount_from_statfs(&s))
}

/// Firmlinks from `/usr/share/firmlinks`: (path seen from `/`, path relative to the
/// Data volume root).
pub fn firmlinks() -> Vec<(PathBuf, PathBuf)> {
    let Ok(text) = std::fs::read_to_string(FIRMLINKS_FILE) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let (from, to) = line.split_once('\t')?;
            Some((PathBuf::from(from.trim()), PathBuf::from(to.trim())))
        })
        .collect()
}

/// Scan layout for the macOS System+Data volume group (spec section 5).
///
/// When scanning `/`, the System and Data volumes count as one filesystem;
/// firmlinked directories are reached from `/` and their targets inside
/// `/System/Volumes/Data` are omitted, so nothing is counted twice, while the
/// Data-volume content that has no firmlink (e.g. `.Spotlight-V100`) is still
/// scanned under `/System/Volumes/Data`.
///
/// The group is detected from the mount table, not from device numbers: since
/// macOS 11 `stat` reports the same `st_dev` for both volumes, so a device check
/// alone would neither separate them nor prevent counting `/Users` twice.
pub fn scan_layout(root: &Path) -> ScanLayout {
    let mut layout = ScanLayout::default();
    let data = Path::new(DATA_VOLUME_MOUNT);
    let data_mounted = mounts()
        .map(|ms| ms.iter().any(|m| m.mount_point == data && m.fs_type == "apfs"))
        .unwrap_or(false);
    if !data_mounted {
        return layout; // pre-Catalina layout: a single volume
    }
    let dev_of = |p: &Path| {
        std::fs::symlink_metadata(p).ok().map(|m| std::os::unix::fs::MetadataExt::dev(&m))
    };
    let (Some(system_dev), Some(data_dev), Some(root_dev)) =
        (dev_of(Path::new("/")), dev_of(data), dev_of(root))
    else {
        return layout;
    };
    if root_dev == system_dev || root_dev == data_dev {
        layout.same_filesystem_devs = vec![system_dev];
        if data_dev != system_dev {
            layout.same_filesystem_devs.push(data_dev);
        }
        layout.volume_group = true;
    }
    if root == Path::new("/") {
        layout.alias_paths = firmlinks().into_iter().map(|(_, rel)| data.join(rel)).collect();
    }
    layout
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_root_mount() {
        let m = mounts().unwrap();
        let root = m.iter().find(|m| m.mount_point == Path::new("/")).unwrap();
        assert_eq!(root.fs_type, "apfs");
        assert!(root.total_bytes > 0);
    }

    #[test]
    fn volume_group_layout_for_root() {
        let l = scan_layout(Path::new("/"));
        if Path::new(DATA_VOLUME_MOUNT).exists() {
            assert!(l.volume_group);
            assert!(l.alias_paths.contains(&PathBuf::from("/System/Volumes/Data/Users")));
        }
        let home = scan_layout(Path::new(env!("CARGO_MANIFEST_DIR")));
        assert!(home.alias_paths.is_empty(), "aliases only apply when scanning /");
    }
}
