//! Careful mode (spec 3C): what a held-back volume is, and the careful scan
//! (read-only mount, one gentle pass, eject). The guard itself is
//! `diskdrift_platform::MountGuard`; the CLI and the TUI own it.

use crate::config::Effective;
use crate::disks::{DiskChoice, identity_of, sighting_of};
use crate::snapshots::{self, AppError, Result, ScanOutcome, ScanRequest, open_store};
use diskdrift_core::walk::Progress;
use diskdrift_platform::{DaError, HeldVolume, MountGuard};
use diskdrift_storage::disks::{Disk, Doubt, Observed, Registry};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

/// A held volume and what the store knows about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Held {
    pub volume: HeldVolume,
    /// The known disk it is (or doubtfully matches).
    pub disk: Option<Disk>,
    /// Matches a known disk only doubtfully: the scan asks first.
    pub doubt: Option<String>,
    /// Start of the newest usable scan of the known disk.
    pub last_scan: Option<String>,
}

impl Held {
    pub fn bsd(&self) -> &str {
        &self.volume.bsd_name
    }

    /// Label, else volume name, else the device name.
    pub fn name(&self) -> String {
        match &self.disk {
            Some(d) if self.doubt.is_none() => d.display_name().to_string(),
            _ => self.volume.info.volume_name.clone().unwrap_or_else(|| self.volume.bsd_name.clone()),
        }
    }
}

/// What the store knows about `volume`. Reads only: the registry is not changed.
pub fn describe(settings: &Effective, volume: HeldVolume) -> Result<Held> {
    let (mut reg, metas) = match open_store(settings)? {
        Some(store) => (store.registry()?, store.list()?),
        None => (Registry::default(), Vec::new()),
    };
    let at = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
    let sighting = sighting_of(&volume.info, &at);
    let (disk, doubt) = match reg.find(&identity_of(&volume.info)).map(|d| d.id.clone()) {
        None => (None, None),
        // Only a preview: the registry is written when the scan records the disk.
        Some(_) => match reg.observe(&sighting) {
            Observed::Known(id) => (reg.get(&id).cloned(), None),
            Observed::New(_) => (None, None),
            Observed::Ask { existing, doubt } => {
                let d = reg.get(&existing).cloned();
                let name = d.as_ref().map(|d| d.display_name().to_string()).unwrap_or(existing);
                let why = match doubt {
                    Doubt::Clone => format!("it has the identity of {name} but is on another device: a copy of it?"),
                    Doubt::Weak => format!("it reports no UUID; its filesystem, name and size match {name}"),
                };
                (d, Some(why))
            }
        },
    };
    let last_scan = disk.as_ref().and_then(|d| metas.iter().rev().find(|m| m.status.is_usable() && m.disk.as_deref() == Some(d.id.as_str()))).map(|m| m.started_at.clone());
    Ok(Held { volume, disk, doubt, last_scan })
}

/// The command a user can run (with administrator rights) to mount a volume
/// Disk Arbitration refused read-only, without repairing it.
pub fn direct_mount_hint(volume: &HeldVolume) -> String {
    let dev = format!("/dev/{}", volume.bsd_name);
    let dir = format!("/tmp/diskdrift-ro-{}", volume.bsd_name);
    let tool = match volume.info.fs_type.as_str() {
        "hfs" => "mount_hfs -j -o rdonly",
        "exfat" => "mount_exfat -o rdonly",
        "msdos" => "mount_msdos -o rdonly",
        "apfs" => "mount_apfs -o rdonly",
        "ntfs" => "mount_ntfs -o rdonly",
        other => return format!("mount it read-only with the mount tool of {other} (`-o rdonly`), then `diskdrift scan <folder> --careful`"),
    };
    format!("sudo mkdir -p {dir} && sudo {tool} {dev} {dir}, then `diskdrift scan {dir} --careful`; afterwards `sudo umount {dir}`")
}

/// Why a mount failed, in words, with what the user can do.
pub fn mount_failure(volume: &HeldVolume, e: &DaError) -> String {
    if e.needs_direct_mount() {
        format!(
            "macOS will not mount {} read-only without repairing it first (it was not unmounted cleanly, or its journal is not empty), \
             and careful mode never repairs: it stays unmounted and nothing was written. To read it anyway: {}",
            volume.info.volume_name.as_deref().unwrap_or(&volume.bsd_name),
            direct_mount_hint(volume)
        )
    } else {
        format!("could not mount it read-only: {e}; nothing was written")
    }
}

/// `None` when `path` is on a read-only mount, else the warning.
pub fn read_only_warning(path: &Path) -> Option<String> {
    match diskdrift_platform::statfs_path(path) {
        Ok(m) if m.read_only => None,
        Ok(m) => Some(format!("{} is mounted read-write: the system may write to it (careful mode mounts it read-only)", m.mount_point.display())),
        Err(e) => Some(format!("cannot tell whether {} is read-only: {e}", path.display())),
    }
}

/// What a careful scan did.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CarefulOutcome {
    pub mount_point: PathBuf,
    /// The scan (`Err`: why it failed or did not start).
    #[serde(serialize_with = "ser_result")]
    pub scan: std::result::Result<ScanOutcome, String>,
    /// Unmounted and ejected: safe to unplug.
    #[serde(serialize_with = "ser_result")]
    pub ejected: std::result::Result<(), String>,
}

fn ser_result<T: Serialize, S: serde::Serializer>(r: &std::result::Result<T, String>, s: S) -> std::result::Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    enum R<'a, T> {
        Ok(&'a T),
        Error(&'a str),
    }
    match r {
        Ok(v) => R::Ok(v).serialize(s),
        Err(e) => R::<T>::Error(e).serialize(s),
    }
}

/// Mounts `held` read-only, scans it with one thread, unmounts and ejects it
/// (also when the scan fails or is stopped). A mount that is refused is an
/// error and nothing else happens.
pub fn careful_scan(
    settings: &Effective,
    guard: &MountGuard,
    held: &Held,
    choice: Option<DiskChoice>,
    cancel: &AtomicBool,
    progress: Option<&Progress>,
) -> Result<CarefulOutcome> {
    if held.doubt.is_some() && choice.is_none() {
        return Err(AppError::DiskDoubt(format!("{}: {}", held.name(), held.doubt.as_deref().unwrap_or_default())));
    }
    let at = guard.mount_read_only(held.bsd(), false).map_err(|e| AppError::Runtime(mount_failure(&held.volume, &e)))?;
    let scan = match read_only_warning(&at) {
        // Something else mounted it read-write meanwhile: scanning would not be careful.
        Some(w) => Err(format!("{w}; DiskDrift did not scan it")),
        None => {
            let mut req = ScanRequest::from_settings(settings, Some(at.clone()), None, &[]);
            req.careful = true;
            req.disk_choice = choice;
            snapshots::scan(settings, &req, cancel, progress).map_err(|e| e.to_string())
        }
    };
    let ejected = guard.unmount_and_eject(held.bsd()).map_err(|e| format!("could not eject it: {e}; eject it in Finder before unplugging"));
    Ok(CarefulOutcome { mount_point: at, scan, ejected })
}

/// Mounts `held` read-only and visible in Finder (to copy a file off).
pub fn mount_for_finder(guard: &MountGuard, held: &Held) -> Result<PathBuf> {
    guard.mount_read_only(held.bsd(), true).map_err(|e| AppError::Runtime(mount_failure(&held.volume, &e)))
}

/// Ejects the disk holding `held` without mounting it.
pub fn eject(guard: &MountGuard, held: &Held) -> Result<()> {
    guard.unmount_and_eject(held.bsd()).map_err(|e| AppError::Runtime(format!("could not eject it: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Overrides, resolve_with};
    use diskdrift_core::testutil::TestDir;
    use diskdrift_platform::{DeviceInfo, DiskInfo};

    fn volume(fs: &str, uuid: Option<&str>) -> HeldVolume {
        HeldVolume {
            bsd_name: "disk9s1".into(),
            writable: Some(true),
            info: DiskInfo {
                mount_point: PathBuf::new(),
                fs_type: fs.into(),
                volume_name: Some("OLD".into()),
                volume_uuid: uuid.map(String::from),
                capacity_bytes: Some(500_000_000_000),
                bsd_name: Some("disk9s1".into()),
                device: DeviceInfo { bus: Some("USB".into()), serial: Some("SN1".into()), ..Default::default() },
            },
        }
    }

    #[test]
    fn a_held_volume_is_known_new_or_doubtful_and_nothing_is_written() {
        let t = TestDir::new("app-careful-describe");
        let db = t.path().join("store");
        let settings = resolve_with(&Overrides { db: Some(db.clone()), ..Default::default() }, t.path(), t.path()).unwrap();
        // No store: new, and none is created.
        assert_eq!(describe(&settings, volume("hfs", Some("U1"))).unwrap().disk, None);
        assert!(!db.exists());
        let store = diskdrift_storage::Store::create(&db).unwrap();
        let known = crate::disks::record_scan_disk(&store, &volume("hfs", Some("U1")).info, "2026-10-01T10:00:00+02:00", None).unwrap();
        let before = std::fs::read(db.join("disks.json")).unwrap();
        let h = describe(&settings, volume("hfs", Some("U1"))).unwrap();
        assert_eq!((h.disk.map(|d| d.id), h.doubt), (Some(known.disk.id.clone()), None));
        let mut copy = volume("hfs", Some("U1"));
        copy.info.device.serial = Some("SN2".into());
        let h = describe(&settings, copy).unwrap();
        assert!(h.doubt.as_deref().unwrap().contains("copy"), "{:?}", h.doubt);
        assert_eq!(describe(&settings, volume("exfat", Some("V2"))).unwrap().disk, None);
        assert_eq!(std::fs::read(db.join("disks.json")).unwrap(), before, "describing writes nothing");
    }

    #[test]
    fn a_dirty_volume_gets_the_direct_read_only_mount() {
        let e = DaError { status: 0xc05c, message: None };
        let m = mount_failure(&volume("exfat", Some("V")), &e);
        assert!(m.contains("never repairs") && m.contains("sudo mount_exfat -o rdonly /dev/disk9s1"), "{m}");
        assert!(direct_mount_hint(&volume("hfs", None)).contains("mount_hfs -j -o rdonly"));
        let other = mount_failure(&volume("hfs", None), &DaError { status: 0xf8da0002, message: Some("busy".into()) });
        assert!(other.contains("busy") && !other.contains("sudo"), "{other}");
    }

    #[test]
    fn the_read_only_check() {
        assert!(read_only_warning(Path::new("/System/Volumes/Data")).is_some(), "the data volume is read-write");
    }
}
