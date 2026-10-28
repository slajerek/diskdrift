//! Disks: what the store knows about each scanned disk (ADR 0002), the
//! `disks` command and the disk a scan is recorded against.

use crate::config::Effective;
use crate::snapshots::{AppError, Result, open_store};
use diskdrift_platform::DiskInfo;
use diskdrift_storage::disks::{DeviceFacts, Disk, Doubt, Identity, Observed, Registry, Sighting, Strength, VolumeFacts};
use diskdrift_storage::{SnapshotMeta, Store};
use serde::Serialize;
use std::path::PathBuf;

/// The identity of the volume `info` describes (ADR 0002, 2.2): its UUID, the
/// UUID macOS derives from a FAT/exFAT serial, or a weak one.
pub fn identity_of(info: &DiskInfo) -> Identity {
    match &info.volume_uuid {
        Some(u) if matches!(info.fs_type.as_str(), "msdos" | "exfat") => Identity::new(Strength::Serial, u.clone()),
        Some(u) => Identity::new(Strength::Uuid, u.clone()),
        None => Identity::weak(&info.fs_type, info.volume_name.as_deref(), info.capacity_bytes),
    }
}

/// A sighting of the disk `info` describes, at `at` (RFC 3339).
pub fn sighting_of(info: &DiskInfo, at: &str) -> Sighting {
    let d = &info.device;
    let device = DeviceFacts {
        model: d.model.clone(),
        vendor: d.vendor.clone(),
        bus: d.bus.clone(),
        serial: d.serial.clone(),
        media_bytes: d.media_bytes,
        internal: d.internal,
    };
    let any = device != DeviceFacts::default();
    Sighting {
        identity: identity_of(info),
        volume: VolumeFacts { name: info.volume_name.clone(), fs_type: info.fs_type.clone(), capacity_bytes: info.capacity_bytes },
        device: any.then_some(device),
        mount_point: info.mount_point.to_string_lossy().into_owned(),
        at: at.into(),
    }
}

/// What to do when a scanned disk matches a known one only doubtfully (a
/// probable copy, or a weak identity). The user decides every time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskChoice {
    /// It is the known disk.
    Same,
    /// It is another disk.
    New,
}

/// The disk a scan is recorded against.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanDisk {
    pub disk: Disk,
    /// Seen for the first time.
    pub new: bool,
}

/// Records the disk `info` describes in the store's registry and returns it.
/// A doubtful match is a usage error that says what to choose, unless `choice`
/// already says it.
pub fn record_scan_disk(store: &Store, info: &DiskInfo, at: &str, choice: Option<DiskChoice>) -> Result<ScanDisk> {
    let s = sighting_of(info, at);
    store
        .update_registry(|reg| {
            let (id, new) = match reg.observe(&s) {
                Observed::Known(id) => (id, false),
                Observed::New(id) => (id, true),
                Observed::Ask { existing, doubt } => match choice {
                    Some(DiskChoice::Same) => {
                        reg.touch(&existing, &s)?;
                        (existing, false)
                    }
                    Some(DiskChoice::New) => (reg.add(&Sighting { identity: split_identity(&s, doubt), ..s.clone() }), true),
                    None => return Ok(Err(doubt_message(reg, &existing, doubt, &s))),
                },
            };
            Ok(Ok(ScanDisk { disk: reg.get(&id).cloned().expect("recorded"), new }))
        })?
        .map_err(AppError::DiskDoubt)
}

/// A separate identity for a disk the user says is not the known one.
fn split_identity(s: &Sighting, doubt: Doubt) -> Identity {
    let serial = s.device.as_ref().and_then(|d| d.serial.clone()).unwrap_or_default();
    match doubt {
        Doubt::Clone => Identity::new(s.identity.strength, format!("{}+{serial}", s.identity.source)),
        Doubt::Weak => Identity::new(Strength::Weak, format!("{}#{}", s.identity.source, s.at)),
    }
}

fn doubt_message(reg: &Registry, existing: &str, doubt: Doubt, s: &Sighting) -> String {
    let known = reg.get(existing).map(|d| format!("{} ({})", d.display_name(), d.id)).unwrap_or_else(|| existing.into());
    match doubt {
        Doubt::Clone => format!(
            "{} at {} has the identity of {known}, but is on another device (serial {}): it looks like a copy of it.",
            s.volume.name.as_deref().unwrap_or("This volume"),
            s.mount_point,
            s.device.as_ref().and_then(|d| d.serial.as_deref()).unwrap_or("?")
        ),
        Doubt::Weak => format!(
            "{} at {} reports no UUID, so it is recognised only by its filesystem, name and size, which match {known}.",
            s.volume.name.as_deref().unwrap_or("This volume"),
            s.mount_point
        ),
    }
}

/// A volume mounted now that counts as a disk: browsable (Finder shows it),
/// which leaves out the system's helper volumes and `nobrowse` mounts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Volume {
    pub identity: Identity,
    pub mount_point: PathBuf,
    pub name: Option<String>,
    pub fs_type: String,
    pub capacity_bytes: Option<u64>,
}

/// The mount points that count as disks (cheap: no Disk Arbitration call).
pub fn disk_mount_points() -> Vec<PathBuf> {
    diskdrift_platform::mounts()
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.browsable && !m.mount_point.starts_with("/System/Volumes"))
        .map(|m| m.mount_point)
        .collect()
}

/// The volumes mounted now that count as disks, each described once.
pub fn volumes() -> Vec<Volume> {
    let mut out: Vec<Volume> = Vec::new();
    for mp in disk_mount_points() {
        if let Ok(info) = diskdrift_platform::disk_info(&mp) {
            let identity = identity_of(&info);
            if !out.iter().any(|v| v.identity == identity) {
                out.push(Volume { identity, mount_point: info.mount_point, name: info.volume_name, fs_type: info.fs_type, capacity_bytes: info.capacity_bytes });
            }
        }
    }
    out
}

/// One row of `diskdrift disks`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskRow {
    #[serde(flatten)]
    pub disk: Disk,
    /// In the registry; `false`: a connected volume the store does not know
    /// yet (its ID is the one it would get).
    pub known: bool,
    /// Mounted now, and where.
    pub connected: bool,
    pub mount_point: Option<PathBuf>,
    pub snapshots: usize,
    /// Start of the newest snapshot that holds data.
    pub last_scan: Option<String>,
    /// Allocated bytes that snapshot tracked (what the disk held then; for a
    /// disk that is not connected, its size as far as DiskDrift knows).
    pub last_size: Option<u64>,
    /// The folder that scan covered (`last_size` is of this folder, not of
    /// the whole disk unless it is the volume's root).
    pub last_root: Option<String>,
    /// Allocated bytes of this disk's snapshot files in the store.
    pub store_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskList {
    pub store_path: PathBuf,
    pub store_exists: bool,
    pub disks: Vec<DiskRow>,
}

/// Every disk the store knows, newest scan first, then the connected volumes
/// it does not know yet. Reads only; without a `disks.json` the registry is
/// derived from the snapshots.
pub fn list(settings: &Effective) -> Result<DiskList> {
    list_with(settings, &volumes())
}

/// [`list`] with the volumes mounted now given (the TUI has them already).
pub fn list_with(settings: &Effective, mounted: &[Volume]) -> Result<DiskList> {
    let store_path = settings.storage_path.value.clone();
    let store = open_store(settings)?;
    let reg = match &store {
        Some(s) => s.registry()?,
        None => Registry::default(),
    };
    let metas = match &store {
        Some(s) => s.list()?,
        None => Vec::new(),
    };
    let mut disks: Vec<DiskRow> = reg
        .disks
        .iter()
        .map(|d| {
            let mine: Vec<&SnapshotMeta> = metas.iter().filter(|m| m.disk.as_deref() == Some(d.id.as_str())).collect();
            let at = mounted.iter().find(|v| v.identity == d.identity).map(|v| v.mount_point.clone());
            DiskRow {
                connected: at.is_some(),
                known: true,
                mount_point: at,
                snapshots: mine.len(),
                last_scan: mine.iter().rev().find(|m| m.status.is_usable()).map(|m| m.started_at.clone()),
                last_size: mine.iter().rev().find(|m| m.status.is_usable()).and_then(|m| m.counts.as_ref()).map(|c| c.allocated_bytes),
                last_root: mine.iter().rev().find(|m| m.status.is_usable()).map(|m| m.root.clone()),
                store_bytes: store.as_ref().map(|s| mine.iter().map(|m| s.snapshot_size(m)).sum()).unwrap_or(0),
                disk: d.clone(),
            }
        })
        .collect();
    disks.sort_by(|a, b| b.last_scan.cmp(&a.last_scan).then_with(|| a.disk.id.cmp(&b.disk.id)));
    for v in mounted.iter().filter(|v| reg.find(&v.identity).is_none()) {
        let now = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
        disks.push(DiskRow {
            disk: Disk {
                id: reg.short_id_for(&v.identity),
                identity: v.identity.clone(),
                label: None,
                description: None,
                sets: Vec::new(),
                volume: VolumeFacts { name: v.name.clone(), fs_type: v.fs_type.clone(), capacity_bytes: v.capacity_bytes },
                device: None,
                first_seen: now.clone(),
                last_seen: now,
                last_mount_point: Some(v.mount_point.to_string_lossy().into_owned()),
            },
            known: false,
            connected: true,
            mount_point: Some(v.mount_point.clone()),
            snapshots: 0,
            last_scan: None,
            last_size: None,
            last_root: None,
            store_bytes: 0,
        });
    }
    Ok(DiskList { store_path, store_exists: store.is_some(), disks })
}

fn require_store(settings: &Effective) -> Result<Store> {
    open_store(settings)?.ok_or_else(|| AppError::Usage(format!("no disks yet in {} (run `diskdrift scan` first)", settings.storage_path.value.display())))
}

/// The disk `reference` names (short ID, label or ID prefix).
pub fn resolve(store: &Store, reference: &str) -> Result<Disk> {
    store
        .registry()?
        .resolve(reference)
        .cloned()
        .ok_or_else(|| AppError::Usage(format!("no disk '{reference}' (see `diskdrift disks`)")))
}

/// `reference` as the mount point of a connected volume: the volume's disk,
/// recorded in the registry if it is new (this creates the store, an explicit
/// user action). `None` when `reference` is not a mount point.
fn register_mounted(settings: &Effective, reference: &str) -> Result<Option<(Store, String)>> {
    if !reference.starts_with('/') {
        return Ok(None);
    }
    let path = std::path::Path::new(reference);
    let Ok(info) = diskdrift_platform::disk_info(path) else { return Ok(None) };
    if info.mount_point != path {
        return Err(AppError::Usage(format!("{reference} is not a mount point; {} is", info.mount_point.display())));
    }
    let store_path = std::path::absolute(&settings.storage_path.value).map_err(|e| AppError::Runtime(e.to_string()))?;
    let store = Store::create(&store_path)?;
    let now = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
    let s = sighting_of(&info, &now);
    let id = store.update_registry(|reg| Ok(reg.record(&s)))?;
    Ok(Some((store, id)))
}

/// An edit of one disk's entry.
pub enum Edit<'a> {
    Label(Option<&'a str>),
    Describe(Option<&'a str>),
    AddToSet(&'a str),
    RemoveFromSet(&'a str),
}

/// Applies `edit` to disk `reference` and returns the entry as it is now.
pub fn edit(settings: &Effective, reference: &str, edit: Edit<'_>) -> Result<Disk> {
    let (store, id) = match register_mounted(settings, reference)? {
        Some(r) => r,
        None => {
            let store = require_store(settings)?;
            let id = resolve(&store, reference)?.id;
            (store, id)
        }
    };
    Ok(store.update_registry(|reg| {
        match edit {
            Edit::Label(l) => reg.set_label(&id, l)?,
            Edit::Describe(t) => reg.set_description(&id, t)?,
            Edit::AddToSet(s) => reg.add_to_set(&id, s)?,
            Edit::RemoveFromSet(s) => reg.remove_from_set(&id, s)?,
        }
        Ok(reg.get(&id).cloned().expect("exists"))
    })?)
}

/// What forgetting a disk removes (or removed).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Forget {
    pub disk: Disk,
    pub snapshots: Vec<u64>,
    /// Allocated bytes of those snapshots' files (before) or freed (after).
    pub bytes: u64,
    pub done: bool,
}

/// Forgets disk `reference`: removes its snapshots (oldest first, so nothing
/// needs refolding), then its entry. With `confirmed` false it only says what
/// would be removed.
pub fn forget(settings: &Effective, reference: &str, confirmed: bool) -> Result<Forget> {
    let store = require_store(settings)?;
    let disk = resolve(&store, reference)?;
    let mine: Vec<SnapshotMeta> = store.list()?.into_iter().filter(|m| m.disk.as_deref() == Some(disk.id.as_str())).collect();
    let ids: Vec<u64> = mine.iter().map(|m| m.id).collect();
    if !confirmed {
        let bytes = mine.iter().map(|m| store.snapshot_size(m)).sum();
        return Ok(Forget { disk, snapshots: ids, bytes, done: false });
    }
    let lock = store.lock()?;
    let mut freed = 0;
    for id in &ids {
        freed += store.remove(&lock, *id)?.0;
    }
    store.update_registry(|reg| reg.forget(&disk.id).map(|_| ()))?;
    Ok(Forget { disk, snapshots: ids, bytes: freed, done: true })
}

/// Joins disk `from`'s history to disk `into` (one disk recognised as two).
pub fn merge(settings: &Effective, from: &str, into: &str) -> Result<(Disk, Vec<u64>)> {
    let store = require_store(settings)?;
    let (a, b) = (resolve(&store, from)?, resolve(&store, into)?);
    let lock = store.lock()?;
    let moved = store.merge_disks(&lock, &a.id, &b.id)?;
    Ok((resolve(&store, &b.id)?, moved))
}

/// The disk ID of `m`, shown with the disk's name when the registry has it.
pub fn disk_name(reg: &Registry, m: &SnapshotMeta) -> Option<String> {
    let id = m.disk.as_deref()?;
    Some(reg.get(id).map(|d| d.display_name().to_string()).unwrap_or_else(|| id.to_string()))
}

/// What the TUI does when a disk is connected (spec 3A, item 11a).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ConnectAction {
    /// A known disk whose last scan is old: rescan `root` in the background.
    Rescan { disk: Disk, root: PathBuf, last_scan: String },
    /// A known disk scanned recently enough: nothing to do.
    Recent { disk: Disk, last_scan: String },
    /// A known disk, but rescanning on connect is off (Settings).
    Off { disk: Disk, last_scan: Option<String> },
    /// Not in the store, or never scanned: the first scan is never automatic.
    New { disk: Option<Disk> },
    /// It matches a known disk only doubtfully: a scan asks first.
    Doubt { message: String },
}

impl ConnectAction {
    /// The name to show for the disk, if it is known.
    pub fn disk(&self) -> Option<&Disk> {
        match self {
            ConnectAction::Rescan { disk, .. } | ConnectAction::Recent { disk, .. } | ConnectAction::Off { disk, .. } => Some(disk),
            ConnectAction::New { disk } => disk.as_ref(),
            ConnectAction::Doubt { .. } => None,
        }
    }
}

/// Decides what to do about `volume`, just connected, at Unix time `now`.
/// Reads only.
pub fn on_connect(settings: &Effective, volume: &Volume, now: i64) -> Result<ConnectAction> {
    let Some(store) = open_store(settings)? else { return Ok(ConnectAction::New { disk: None }) };
    let reg = store.registry()?;
    let Some(known) = reg.find(&volume.identity).cloned() else { return Ok(ConnectAction::New { disk: None }) };
    // The same questions a scan would ask (a copy, a weak identity), without recording anything.
    if let Ok(info) = diskdrift_platform::disk_info(&volume.mount_point) {
        let at = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
        let s = sighting_of(&info, &at);
        if let Observed::Ask { existing, doubt } = reg.clone().observe(&s) {
            return Ok(ConnectAction::Doubt { message: doubt_message(&reg, &existing, doubt, &s) });
        }
    }
    let last = store.list()?.into_iter().rev().find(|m| m.disk.as_deref() == Some(known.id.as_str()) && m.status.is_usable());
    let Some(last) = last else { return Ok(ConnectAction::New { disk: Some(known) }) };
    if !settings.scan_on_connect.value {
        return Ok(ConnectAction::Off { disk: known, last_scan: Some(last.started_at) });
    }
    let started = chrono::DateTime::parse_from_rfc3339(&last.started_at).map(|t| t.timestamp()).unwrap_or(0);
    if now - started < settings.auto_max_age_hours.value as i64 * 3600 {
        return Ok(ConnectAction::Recent { disk: known, last_scan: last.started_at });
    }
    // The folder last scanned on it, if it is still there (the mount point may have changed).
    let root = last
        .root_in_volume
        .as_deref()
        .map(|r| if r == "/" { volume.mount_point.clone() } else { volume.mount_point.join(r.trim_start_matches('/')) })
        .filter(|p| p.is_dir() && !last.volume.volume_group)
        .or_else(|| Some(PathBuf::from(&last.root)).filter(|p| p.is_dir()))
        .unwrap_or_else(|| volume.mount_point.clone());
    Ok(ConnectAction::Rescan { disk: known, root, last_scan: last.started_at })
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskdrift_core::testutil::TestDir;
    use diskdrift_platform::DeviceInfo;

    fn info(fs: &str, uuid: Option<&str>, serial: Option<&str>) -> DiskInfo {
        DiskInfo {
            mount_point: "/Volumes/STICK".into(),
            fs_type: fs.into(),
            volume_name: Some("STICK".into()),
            volume_uuid: uuid.map(String::from),
            capacity_bytes: Some(64_000_000),
            bsd_name: Some("disk9s1".into()),
            device: DeviceInfo { serial: serial.map(String::from), bus: Some("USB".into()), ..Default::default() },
        }
    }

    #[test]
    fn identities_follow_the_filesystem() {
        assert_eq!(identity_of(&info("apfs", Some("U"), None)), Identity::new(Strength::Uuid, "U"));
        assert_eq!(identity_of(&info("msdos", Some("V3"), None)).strength, Strength::Serial);
        assert_eq!(identity_of(&info("exfat", Some("V3"), None)).strength, Strength::Serial);
        assert_eq!(identity_of(&info("ntfs", None, None)), Identity::weak("ntfs", Some("STICK"), Some(64_000_000)));
        let s = sighting_of(&info("hfs", Some("U"), Some("SN1")), "t");
        assert_eq!(s.device.unwrap().serial.as_deref(), Some("SN1"));
        assert!(sighting_of(&DiskInfo { device: DeviceInfo::default(), ..info("hfs", Some("U"), None) }, "t").device.is_none());
    }

    #[test]
    fn a_probable_copy_is_asked_about_and_the_answer_is_kept() {
        let t = TestDir::new("app-disk-clone");
        let store = Store::create(&t.path().join("store")).unwrap();
        let first = record_scan_disk(&store, &info("hfs", Some("U"), Some("SN1")), "2026-10-22T10:00:00+02:00", None).unwrap();
        assert!(first.new);
        let again = record_scan_disk(&store, &info("hfs", Some("U"), Some("SN1")), "2026-10-22T11:00:00+02:00", None).unwrap();
        assert_eq!((again.new, again.disk.id.as_str()), (false, first.disk.id.as_str()));

        let copy = info("hfs", Some("U"), Some("SN2"));
        let err = record_scan_disk(&store, &copy, "2026-10-22T12:00:00+02:00", None).unwrap_err();
        assert!(matches!(&err, AppError::DiskDoubt(m) if m.contains("looks like a copy")) && err.is_usage(), "{err}");
        assert_eq!(store.registry().unwrap().disks.len(), 1, "nothing recorded before the answer");
        let same = record_scan_disk(&store, &copy, "2026-10-22T12:00:00+02:00", Some(DiskChoice::Same)).unwrap();
        assert_eq!(same.disk.id, first.disk.id);
        // The copy now carries SN2, so the original disk is the one asked about next.
        let split = record_scan_disk(&store, &info("hfs", Some("U"), Some("SN1")), "2026-10-22T13:00:00+02:00", Some(DiskChoice::New)).unwrap();
        assert!(split.new);
        assert_ne!(split.disk.id, first.disk.id);
        assert_eq!(split.disk.identity.source, "U+SN1");
        assert_eq!(store.registry().unwrap().disks.len(), 2);
    }

    #[test]
    fn a_weak_match_is_asked_about() {
        let t = TestDir::new("app-disk-weak");
        let store = Store::create(&t.path().join("store")).unwrap();
        let ntfs = info("ntfs", None, None);
        assert!(record_scan_disk(&store, &ntfs, "t1", None).unwrap().new);
        let err = record_scan_disk(&store, &ntfs, "t2", None).unwrap_err();
        assert!(err.to_string().contains("recognised only by its filesystem"), "{err}");
        assert!(!record_scan_disk(&store, &ntfs, "t2", Some(DiskChoice::Same)).unwrap().new);
    }

    fn scanned_store(name: &str) -> (TestDir, Effective, PathBuf) {
        let t = TestDir::new(name);
        let tree = t.path().join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("f"), b"1").unwrap();
        std::fs::write(t.path().join("config.json"), r#"{"schemaVersion":1}"#).unwrap();
        let o = crate::config::Overrides { config: Some(t.path().join("config.json")), db: Some(t.path().join("store")), ..Default::default() };
        let s = crate::config::resolve_with(&o, t.path(), std::path::Path::new("/")).unwrap();
        let req = crate::snapshots::ScanRequest::from_settings(&s, Some(tree.clone()), None, &[]);
        crate::snapshots::scan(&s, &req, &std::sync::atomic::AtomicBool::new(false), None).unwrap();
        (t, s, tree)
    }

    fn volume_of(path: &std::path::Path) -> Volume {
        let info = diskdrift_platform::disk_info(path).unwrap();
        Volume { identity: identity_of(&info), mount_point: info.mount_point, name: info.volume_name, fs_type: info.fs_type, capacity_bytes: info.capacity_bytes }
    }

    #[test]
    fn a_connected_disk_is_rescanned_only_when_known_old_and_allowed() {
        let (_t, mut s, tree) = scanned_store("app-on-connect");
        let v = volume_of(&tree);
        let now = chrono::Local::now().timestamp();
        assert!(matches!(on_connect(&s, &v, now).unwrap(), ConnectAction::Recent { .. }), "just scanned");
        let later = now + 7 * 3600;
        match on_connect(&s, &v, later).unwrap() {
            ConnectAction::Rescan { root, disk, .. } => {
                assert_eq!(root, tree.canonicalize().unwrap(), "the folder last scanned on it");
                assert_eq!(disk.identity, v.identity);
            }
            other => panic!("{other:?}"),
        }
        s.scan_on_connect.value = false;
        assert!(matches!(on_connect(&s, &v, later).unwrap(), ConnectAction::Off { .. }));
        let stranger = Volume { identity: Identity::new(Strength::Uuid, "NEVER-SEEN"), ..v.clone() };
        assert!(matches!(on_connect(&s, &stranger, later).unwrap(), ConnectAction::New { disk: None }));
    }

    #[test]
    fn the_disk_list_shows_new_volumes_and_a_mount_point_can_be_labelled() {
        let (_t, s, tree) = scanned_store("app-new-volumes");
        let known = volume_of(&tree);
        let stick = Volume {
            identity: Identity::new(Strength::Serial, "STICK-UUID"),
            mount_point: "/Volumes/STICK".into(),
            name: Some("STICK".into()),
            fs_type: "exfat".into(),
            capacity_bytes: Some(64_000_000_000),
        };
        let l = list_with(&s, &[known.clone(), stick.clone()]).unwrap();
        assert_eq!(l.disks.len(), 2);
        assert!(l.disks[0].known && l.disks[0].connected);
        let new = &l.disks[1];
        assert!(!new.known && new.connected && new.snapshots == 0);
        assert_eq!(new.disk.id, stick.identity.short_id(8), "the ID it would get");
        let text = crate::text::disk_list(&l, crate::core::units::SizeUnits::Decimal, 0);
        assert!(text.contains("new, not scanned") && text.contains("never scans a disk on its own"), "{text}");

        // A mount point is a reference: the system volume here (a fake one cannot be described).
        let mp = known.mount_point.to_string_lossy().into_owned();
        let d = edit(&s, &mp, Edit::Label(Some("systemowy"))).unwrap();
        assert_eq!(d.label.as_deref(), Some("systemowy"));
        assert_eq!(d.id, l.disks[0].disk.id, "the known disk, not a new entry");
        let not_a_mount = tree.to_string_lossy().into_owned();
        assert!(edit(&s, &not_a_mount, Edit::Label(Some("x"))).unwrap_err().to_string().contains("is not a mount point"));
    }
}
