//! Persistent scans and comparisons: `scan`, `snapshots`, `top`, `diff`.

use crate::config::Effective;
use diskdrift_core::browse::{self, DiffListing, Listing};
use diskdrift_core::diff::{self, DiffOptions, DiffReport};
use diskdrift_core::paths::components_under;
use diskdrift_core::record::Record;
use diskdrift_core::top::{self, TopOptions, TopReport};
use diskdrift_core::walk::{self, ExcludeRules, Progress, WalkError, WalkOptions};
use diskdrift_platform::ListerKind;
use diskdrift_storage::{Rebase, ScanOptions, SnapshotMeta, SpaceSample, Status, Store, StoreError, VolumeIdentity, meta};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// Bad input: exit code 2.
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{0}")]
    Runtime(String),
    /// The scanned disk matches a known one only doubtfully: the user says
    /// whether it is that disk (`ScanRequest::disk_choice`). Exit code 2.
    #[error("{0}")]
    DiskDoubt(String),
}

impl AppError {
    pub fn is_usage(&self) -> bool {
        matches!(
            self,
            AppError::Usage(_)
                | AppError::DiskDoubt(_)
                | AppError::Store(
                    StoreError::NotFound(_)
                        | StoreError::DuplicateName(_)
                        | StoreError::NoData { .. }
                        | StoreError::UnknownDisk(_)
                        | StoreError::DuplicateLabel { .. }
                        | StoreError::InvalidLabel { .. }
                        | StoreError::Invalid(_)
                )
        )
    }
}

pub type Result<T> = std::result::Result<T, AppError>;

fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// OS-reported space of the volume holding `path`.
pub fn space_sample(path: &Path) -> Option<SpaceSample> {
    let m = diskdrift_platform::statfs_path(path).ok()?;
    let cap = diskdrift_platform::volume_capacity(path);
    Some(SpaceSample {
        total_bytes: m.total_bytes,
        free_bytes: m.free_bytes,
        available_bytes: m.available_bytes,
        available_important_bytes: cap.available_important_bytes,
    })
}

/// Opens the configured store if it exists (read-only commands never create it).
pub fn open_store(settings: &Effective) -> Result<Option<Store>> {
    let p = &settings.storage_path.value;
    if !Store::exists(p) {
        return Ok(None);
    }
    Ok(Some(Store::open(p)?))
}

pub(crate) fn require_store(settings: &Effective) -> Result<Store> {
    open_store(settings)?.ok_or_else(|| {
        AppError::Usage(format!(
            "no snapshots yet in {} (run `diskdrift scan` first)",
            settings.storage_path.value.display()
        ))
    })
}

#[derive(Debug, Clone)]
pub struct ScanRequest {
    pub root: PathBuf,
    pub root_native: Option<diskdrift_core::native::NativeRoot>,
    pub name: Option<String>,
    /// Exclusion patterns (config plus command line).
    pub exclude: Vec<String>,
    pub threads: usize,
    pub stay_on_filesystem: bool,
    pub lister: ListerKind,
    /// Run at low CPU and I/O priority (the automatic scan at startup).
    pub background: bool,
    /// The answer when the disk matches a known one only doubtfully.
    pub disk_choice: Option<crate::disks::DiskChoice>,
    /// A careful scan (spec 3C): one thread at low priority, marked careful.
    pub careful: bool,
}

impl ScanRequest {
    pub fn from_settings(settings: &Effective, root: Option<PathBuf>, name: Option<String>, extra_exclude: &[String]) -> Self {
        let mut exclude = settings.exclude.value.clone();
        exclude.extend(extra_exclude.iter().cloned());
        ScanRequest {
            root_native: root.is_none().then(|| settings.default_root_native.value.clone()),
            root: root.unwrap_or_else(|| settings.default_root.value.clone()),
            name,
            exclude,
            threads: settings.threads.value,
            stay_on_filesystem: settings.stay_on_filesystem.value,
            lister: ListerKind::Auto,
            background: false,
            disk_choice: None,
            careful: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanOutcome {
    pub snapshot: SnapshotMeta,
    pub cancelled: bool,
    pub store_path: PathBuf,
    pub store_bytes: u64,
    /// What happened to the previous snapshot of the same root.
    pub rebase: Rebase,
    /// The previous snapshot could not be compacted (it stays full and valid).
    pub rebase_error: Option<String>,
    /// The disk the scan is recorded against (`None` when the OS could not
    /// describe the volume; the store then derives it from the metadata).
    pub disk: Option<crate::disks::ScanDisk>,
}

fn resolve_root(root: &Path, exact: Option<&diskdrift_core::native::NativeRoot>) -> Result<PathBuf> {
    let root = exact.map(|r| diskdrift_platform::path_from_native(&r.path).map_err(|e| AppError::Usage(format!("scan root {}: {e}", r.path.display())))).transpose()?.unwrap_or_else(|| root.to_path_buf());
    let abs = std::path::absolute(&root).map_err(|e| AppError::Usage(format!("{}: {e}", root.display())))?;
    let canon = abs.canonicalize().map_err(|e| AppError::Usage(format!("scan root {}: {e}", abs.display())))?;
    if !canon.is_dir() {
        return Err(AppError::Usage(format!("scan root {} is not a directory", canon.display())));
    }
    Ok(canon)
}

/// Default snapshot name: the local start time.
pub fn default_name() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H-%M-%S").to_string()
}

/// Whether `name` is the start time `default_name` gives: it says nothing the
/// start time does not.
pub fn is_default_name(name: &str) -> bool {
    chrono::NaiveDateTime::parse_from_str(name, "%Y-%m-%dT%H-%M-%S").is_ok()
}

/// "scan #4 of 2026-10-21 13:13 (25 min ago)": where reused sizes come from.
pub fn scan_label(m: &SnapshotMeta, now: i64) -> String {
    let started = chrono::DateTime::parse_from_rfc3339(&m.started_at).map(|t| t.timestamp()).unwrap_or(0);
    let ago = match now - started {
        s if s < 3600 => format!("{} min ago", (s / 60).max(1)),
        s => crate::analyze::ago(s),
    };
    format!("scan #{} of {} ({ago})", m.id, m.started_at.get(..16).unwrap_or(&m.started_at).replace('T', " "))
}

/// Below this much available space no automatic scan starts (it writes to the
/// store on the same disk).
pub const AUTO_SCAN_MIN_FREE: u64 = 2_000_000_000;

/// Whether the TUI should rescan in the background at start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoScan {
    Start {
        root: PathBuf,
        name: String,
        /// Entries of the previous scan of the root, for a progress estimate.
        expected_entries: Option<u64>,
    },
    /// Not started, and why.
    Skip(String),
}

/// The rule for the automatic scan: enabled, the user has scanned this root
/// before (the first scan is always explicit), the latest scan is older than
/// the configured age and the disk is not nearly full.
pub fn auto_scan(settings: &Effective, list: &SnapshotList, now: i64) -> AutoScan {
    let root = settings.default_root.value.canonicalize().unwrap_or_else(|_| settings.default_root.value.clone());
    if !settings.auto_scan.value {
        return AutoScan::Skip("automatic scan is off (Settings)".into());
    }
    let Some(latest) = list.snapshots.iter().rev().map(|r| &r.meta).find(|m| m.status.is_usable() && Path::new(&m.root) == root) else {
        return AutoScan::Skip(format!("no earlier scan of {}: the first one is up to you (S)", root.display()));
    };
    let started = chrono::DateTime::parse_from_rfc3339(&latest.started_at).map(|t| t.timestamp()).unwrap_or(0);
    let age = now - started;
    let max = settings.auto_max_age_hours.value as i64 * 3600;
    if age < max {
        return AutoScan::Skip(format!(
            "latest scan of {} is {} old; the next automatic one after {} h",
            root.display(),
            crate::analyze::age(age).replace("just now", "under a minute"),
            settings.auto_max_age_hours.value
        ));
    }
    if let Some(s) = space_sample(&root) {
        let avail = s.available_important_bytes.unwrap_or(s.available_bytes);
        if avail < AUTO_SCAN_MIN_FREE {
            return AutoScan::Skip(format!("only {} available: no automatic scan", diskdrift_core::units::format_size(avail, settings.size_units.value)));
        }
    }
    AutoScan::Start {
        root,
        name: format!("auto-{}", default_name()),
        expected_entries: latest.counts.as_ref().map(|c| c.entries),
    }
}

/// Runs a full metadata scan and stores it as a snapshot.
pub fn scan(settings: &Effective, req: &ScanRequest, cancel: &AtomicBool, progress: Option<&Progress>) -> Result<ScanOutcome> {
    let root = resolve_root(&req.root, req.root_native.as_ref())?;
    let store_path = std::path::absolute(&settings.storage_path.value).map_err(|e| AppError::Runtime(e.to_string()))?;
    let store = Store::create(&store_path)?;
    let lock = store.lock()?;
    let name = req.name.clone().unwrap_or_else(default_name);

    let layout = diskdrift_platform::scan_layout(&root);
    let mount = diskdrift_platform::statfs_path(&root).map_err(|e| AppError::Runtime(format!("{}: {e}", root.display())))?;
    let cap = diskdrift_platform::volume_capacity(&root);
    let started_at = now_rfc3339();
    let disk_info = diskdrift_platform::disk_info(&root).ok();
    let disk = match &disk_info {
        Some(info) => Some(crate::disks::record_scan_disk(&store, info, &started_at, req.disk_choice)?),
        None => None,
    };
    let lister = diskdrift_platform::lister(req.lister);
    // Careful: one thread spares a weak disk; low priority like a background scan.
    let gentle = req.background || req.careful;
    let mut opts = WalkOptions {
        threads: if req.careful { 1 } else { req.threads },
        stay_on_filesystem: req.stay_on_filesystem,
        same_filesystem_devs: layout.same_filesystem_devs.clone(),
        alias_paths: layout.alias_paths.clone(),
        exclude: ExcludeRules::from_patterns(&req.exclude),
        on_thread_start: gentle.then_some(diskdrift_platform::lower_thread_priority as fn()),
        ..WalkOptions::default()
    };
    if gentle {
        diskdrift_platform::lower_thread_priority();
    }
    // Never scan our own store: it changes while we write it.
    if let Ok(canon_store) = store_path.canonicalize()
        && canon_store.starts_with(&root) {
            opts.alias_paths.insert(canon_store);
        }
    let mut template = SnapshotMeta {
        format_version: meta::META_FORMAT_VERSION,
        id: 0,
        name,
        root: root.to_string_lossy().into_owned(),
        disk: disk.as_ref().map(|d| d.disk.id.clone()),
        root_in_volume: None,
        status: Status::InProgress,
        started_at,
        finished_at: None,
        volume: VolumeIdentity {
            // Disk Arbitration's UUID: stable for FAT volumes (ADR 0002, 2.2).
            uuid: disk_info.as_ref().and_then(|i| i.volume_uuid.clone()).or_else(|| cap.volume_uuid.clone()),
            name: cap.volume_name.clone(),
            fs_type: mount.fs_type.clone(),
            mount_point: mount.mount_point.to_string_lossy().into_owned(),
            volume_group: layout.volume_group,
        },
        options: ScanOptions {
            stay_on_filesystem: req.stay_on_filesystem,
            exclude: opts.exclude.patterns(),
            lister: lister.name().to_string(),
            threads: opts.effective_threads(),
            careful: req.careful,
        },
        space_before: space_sample(&root),
        space_after: None,
        storage: Default::default(),
        counts: None,
        error: None,
        diskdrift_version: crate::VERSION.to_string(),
    };
    template.root_in_volume = Some(template.derived_root_in_volume());
    // Same device order as the previous comparable scan keeps the delta small.
    if let Some(prev) = store.list()?.iter().rev().find(|m| m.status.is_usable() && m.incompatibility(&template).is_none()) {
        opts.known_devices = prev.counts.as_ref().map(|c| c.devices.clone()).unwrap_or_default();
    }
    let mut writer = store.begin(lock, template, None)?;

    let mut store_error: Option<StoreError> = None;
    let mut sink = |rec: &Record, _: &Path| -> std::io::Result<()> {
        writer.push(rec).map_err(|e| {
            let msg = e.to_string();
            store_error = Some(e);
            std::io::Error::other(msg)
        })
    };
    let result = walk::walk(&root, lister.as_ref(), &opts, &mut sink, cancel, progress);
    let finished = now_rfc3339();
    let snapshot = match result {
        Err(WalkError::Root { path, source }) => {
            let m = writer.abort(Status::Failed, Some(format!("{path}: {source}")), None, finished);
            return Err(AppError::Runtime(m.error.unwrap_or_default()));
        }
        Err(WalkError::Sink(e)) => {
            let err = store_error.take().map(AppError::Store).unwrap_or_else(|| AppError::Runtime(e.to_string()));
            writer.abort(Status::Failed, Some(err.to_string()), None, finished);
            return Err(err);
        }
        Ok(report) if report.cancelled => {
            let m = writer.abort(Status::Cancelled, None, Some(report), finished);
            return Ok(ScanOutcome {
                snapshot: m,
                cancelled: true,
                store_bytes: store.size_on_disk(),
                store_path,
                rebase: Rebase::Nothing,
                rebase_error: None,
                disk,
            });
        }
        Ok(report) => writer.commit_and_rebase(report, space_sample(&root), finished)?,
    };
    let (snapshot, rebase) = snapshot;
    let (rebase, rebase_error) = match rebase {
        Ok(r) => (r, None),
        Err(e) => (Rebase::Nothing, Some(e)),
    };
    Ok(ScanOutcome { snapshot, cancelled: false, store_bytes: store.size_on_disk(), store_path, rebase, rebase_error, disk })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotRow {
    #[serde(flatten)]
    pub meta: SnapshotMeta,
    /// Allocated bytes of this snapshot's own files.
    pub size_on_disk: u64,
    /// The disk's label, else its volume name, else its short ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotList {
    pub store_path: PathBuf,
    pub store_exists: bool,
    pub store_bytes: u64,
    pub snapshots: Vec<SnapshotRow>,
    /// Disks with snapshots in the store (all of them, also when filtered).
    pub disk_count: usize,
    /// Only this disk's snapshots are listed (`snapshots --disk`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk: Option<crate::storage::Disk>,
}

pub fn list(settings: &Effective) -> Result<SnapshotList> {
    list_disk(settings, None)
}

/// The snapshots, all or those of the disk `disk` names.
pub fn list_disk(settings: &Effective, disk: Option<&str>) -> Result<SnapshotList> {
    let store_path = settings.storage_path.value.clone();
    let Some(store) = open_store(settings)? else {
        if let Some(d) = disk {
            return Err(AppError::Usage(format!("no disk '{d}': there are no snapshots yet in {}", store_path.display())));
        }
        return Ok(SnapshotList { store_path, store_exists: false, store_bytes: 0, snapshots: Vec::new(), disk_count: 0, disk: None });
    };
    let reg = store.registry()?;
    let filter = disk.map(|d| crate::disks::resolve(&store, d)).transpose()?;
    let metas = store.list()?;
    let mut ids: Vec<&str> = metas.iter().filter_map(|m| m.disk.as_deref()).collect();
    ids.sort_unstable();
    ids.dedup();
    let disk_count = ids.len();
    let snapshots = metas
        .into_iter()
        .filter(|m| filter.as_ref().is_none_or(|d| m.disk.as_deref() == Some(d.id.as_str())))
        .map(|m| SnapshotRow { size_on_disk: store.snapshot_size(&m), disk_name: crate::disks::disk_name(&reg, &m), meta: m })
        .collect();
    Ok(SnapshotList { store_bytes: store.size_on_disk(), store_path, store_exists: true, snapshots, disk_count, disk: filter })
}

/// Removes a snapshot from the store (DiskDrift's own data only).
/// Returns the removed snapshot, bytes freed and snapshots rewritten to keep them readable.
pub fn remove(settings: &Effective, reference: &str) -> Result<(SnapshotMeta, u64, Vec<u64>)> {
    let store = require_store(settings)?;
    let m = store.resolve(reference)?;
    let lock = store.lock()?;
    let (freed, folded) = store.remove(&lock, m.id)?;
    Ok((m, freed, folded))
}

/// Integrity check of a snapshot (and its delta base).
pub fn verify(settings: &Effective, reference: &str) -> Result<(SnapshotMeta, u64)> {
    let store = require_store(settings)?;
    let m = store.resolve(reference)?;
    let n = store.verify(m.id)?;
    Ok((m, n))
}

fn prefix_for(meta: &SnapshotMeta, path: Option<&Path>) -> Result<Option<Vec<Vec<u8>>>> {
    let Some(p) = path else { return Ok(None) };
    let abs = std::path::absolute(p).map_err(|e| AppError::Usage(e.to_string()))?;
    let comps = components_under(meta.root.as_bytes(), &path_bytes(&abs)).ok_or_else(|| {
        AppError::Usage(format!("{} is not inside the scan root {} of snapshot {}", abs.display(), meta.root, meta.id))
    })?;
    Ok(Some(comps))
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TopResult {
    pub snapshot: SnapshotMeta,
    pub path: Option<PathBuf>,
    /// Directories are the deepest large ones only (see `TopOptions::deepest`).
    pub deepest: bool,
    #[serde(flatten)]
    pub report: TopReport,
}

pub fn top(settings: &Effective, reference: &str, path: Option<&Path>, mut opts: TopOptions) -> Result<TopResult> {
    let store = require_store(settings)?;
    let m = store.resolve(reference)?;
    // The algorithms filter by prefix themselves and need the ancestors' records
    // for path context, so they always get the whole stream.
    let stream = store.records(m.id)?;
    opts.prefix = prefix_for(&m, path)?;
    let report = top::top(stream, &opts).map_err(|e| AppError::Store(StoreError::Corrupt(e.to_string())))?;
    Ok(TopResult { snapshot: m, path: path.map(Path::to_path_buf), deepest: opts.deepest, report })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowseResult {
    pub snapshot: SnapshotMeta,
    pub path: PathBuf,
    #[serde(flatten)]
    pub listing: Listing,
}

/// One directory level of a snapshot: its children, largest first.
/// `path` defaults to the scan root.
pub fn browse(settings: &Effective, reference: &str, path: Option<&Path>, limit: usize) -> Result<BrowseResult> {
    let store = require_store(settings)?;
    let m = store.resolve(reference)?;
    let path = path.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(&m.root));
    let dir = prefix_for(&m, Some(&path))?.unwrap_or_default();
    let listing = browse::children(store.records(m.id)?, &dir, limit).map_err(|e| AppError::Store(StoreError::Corrupt(e.to_string())))?;
    if listing.dir.is_none() {
        return Err(AppError::Usage(format!("{} is not in snapshot {}", path.display(), m.id)));
    }
    Ok(BrowseResult { snapshot: m, path, listing })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffBrowseResult {
    pub old: SnapshotMeta,
    pub new: SnapshotMeta,
    pub path: PathBuf,
    #[serde(flatten)]
    pub listing: DiffListing,
}

/// One directory level of a comparison: the children that changed, largest change first.
pub fn diff_browse(settings: &Effective, a: &str, b: &str, path: Option<&Path>, limit: usize) -> Result<DiffBrowseResult> {
    let store = require_store(settings)?;
    let (old, new) = (store.resolve(a)?, store.resolve(b)?);
    for m in [&old, &new] {
        if !m.status.is_usable() {
            return Err(AppError::Store(StoreError::NoData { id: m.id, status: m.status }));
        }
    }
    if old.root != new.root && old.incompatibility(&new).is_some() {
        return Err(AppError::Usage(format!("snapshots {} and {} have different roots", old.id, new.id)));
    }
    let path = path.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(&new.root));
    let dir = prefix_for(&new, Some(&path))?.unwrap_or_default();
    // Paths are shown under the newer scan's root (the disk may have moved).
    let listing = browse::diff_children(store.records_as(old.id, &new.root)?, store.records(new.id)?, &dir, limit)
        .map_err(|e| AppError::Store(StoreError::Corrupt(e.to_string())))?;
    if listing.dir.is_none() {
        return Err(AppError::Usage(format!("{} is in neither snapshot", path.display())));
    }
    Ok(DiffBrowseResult { old, new, path, listing })
}

/// OS-level space change between the end of two scans.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpaceChange {
    /// Positive: free space decreased (consumed).
    pub free_consumed_bytes: i128,
    pub available_consumed_bytes: Option<i128>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffResult {
    pub old: SnapshotMeta,
    pub new: SnapshotMeta,
    pub path: Option<PathBuf>,
    /// `None` when the snapshots are equivalent; otherwise why they are not.
    pub non_equivalent: Option<String>,
    pub space: Option<SpaceChange>,
    /// Space the OS reports consumed minus the tracked allocated growth. A
    /// diagnostic, not proof of missing files.
    pub unreconciled_bytes: Option<i128>,
    pub notes: Vec<String>,
    #[serde(flatten)]
    pub report: DiffReport,
}

/// Whether `old` can be compared with `new`: both hold data, `old` is the
/// earlier scan, and both cover the same root, volume and scan options.
pub fn comparable(old: &SnapshotMeta, new: &SnapshotMeta) -> bool {
    old.id < new.id && old.status.is_usable() && new.status.is_usable() && old.incompatibility(new).is_none()
}

/// The comparison to show first, as indices into `metas` (oldest first): the
/// newest scan against the previous one comparable with it, falling back to
/// older scans when the newest has nothing to compare with.
pub fn latest_pair(metas: &[&SnapshotMeta]) -> Option<(usize, usize)> {
    (0..metas.len()).rev().find_map(|new| (0..new).rev().find(|&old| comparable(metas[old], metas[new])).map(|old| (old, new)))
}

/// The newest scan before `new` that it can be compared with (`metas` oldest first).
pub fn previous_comparable<'a>(metas: &[&'a SnapshotMeta], new: &SnapshotMeta) -> Option<&'a SnapshotMeta> {
    metas.iter().rev().find(|m| comparable(m, new)).copied()
}

/// The newest scan with data of `root` (`metas` oldest first), for an estimate
/// of how long scanning it again takes.
pub fn last_scan_of<'a>(metas: &[&'a SnapshotMeta], root: &Path) -> Option<&'a SnapshotMeta> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    metas.iter().rev().find(|m| m.status.is_usable() && Path::new(&m.root) == root).copied()
}

/// A scan root as typed in the TUI: `~` expanded, a full path, an existing
/// folder. The error says what to type instead.
pub fn check_root(input: &str, home: &Path) -> std::result::Result<PathBuf, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Type the folder to scan: / for the whole disk, ~ for your home folder.".into());
    }
    let path = match input.strip_prefix('~') {
        Some("") => home.to_path_buf(),
        Some(rest) if rest.starts_with('/') => home.join(rest.trim_start_matches('/')),
        _ => PathBuf::from(input),
    };
    if !path.is_absolute() {
        return Err(format!("{input} is not a full path: start with / or ~ (for example ~/Library)."));
    }
    match std::fs::metadata(&path) {
        Ok(m) if m.is_dir() => Ok(path),
        Ok(_) => Err(format!("{input} is a file; type the folder that holds it.")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!("{input} does not exist.")),
        Err(e) => Err(format!("{input}: {e}")),
    }
}

pub fn diff(
    settings: &Effective,
    a: &str,
    b: &str,
    path: Option<&Path>,
    allow_incompatible: bool,
    mut opts: DiffOptions,
) -> Result<DiffResult> {
    let store = require_store(settings)?;
    let old = store.resolve(a)?;
    let new = store.resolve(b)?;
    for m in [&old, &new] {
        if !m.status.is_usable() {
            return Err(AppError::Store(StoreError::NoData { id: m.id, status: m.status }));
        }
    }
    let non_equivalent = old.incompatibility(&new);
    if let Some(why) = &non_equivalent
        && !allow_incompatible {
            return Err(AppError::Usage(format!(
                "snapshots {} and {} are not comparable: {why} (use --allow-incompatible to compare anyway)",
                old.id, new.id
            )));
        }
    let prefix = prefix_for(&new, path)?;
    // One lineage at two mount points compares under the newer root; scans that
    // are not comparable at all (--allow-incompatible) are compared as they are.
    let sa = if non_equivalent.is_none() { store.records_as(old.id, &new.root)? } else { store.records(old.id)? };
    let sb = store.records(new.id)?;
    opts.prefix = prefix.clone();
    let report = diff::diff(sa, sb, opts).map_err(|e| AppError::Store(StoreError::Corrupt(e.to_string())))?;

    let space = match (&old.space_after, &new.space_after) {
        (Some(x), Some(y)) => Some(SpaceChange {
            free_consumed_bytes: x.free_bytes as i128 - y.free_bytes as i128,
            available_consumed_bytes: match (x.available_important_bytes, y.available_important_bytes) {
                (Some(p), Some(q)) => Some(p as i128 - q as i128),
                _ => None,
            },
        }),
        _ => None,
    };
    let unreconciled_bytes = match (&space, &prefix, &non_equivalent) {
        (Some(s), None, None) => Some(s.free_consumed_bytes - report.summary.net_allocated_bytes),
        _ => None,
    };
    let mut notes = Vec::new();
    for m in [&old, &new] {
        if m.status == Status::Partial {
            let c = m.counts.as_ref();
            notes.push(format!(
                "Snapshot {} is PARTIAL: {} unreadable directories, {} unreadable entries; their contents are not compared.",
                m.id,
                c.map(|c| c.inaccessible_dirs).unwrap_or(0),
                c.map(|c| c.stat_errors).unwrap_or(0)
            ));
        }
    }
    let s = &report.summary;
    if s.newly_visible.count + s.no_longer_visible.count > 0 {
        notes.push(format!(
            "{} entries became visible and {} stopped being visible because access changed; they are excluded from growth.",
            s.newly_visible.count, s.no_longer_visible.count
        ));
    }
    if unreconciled_bytes.is_some() && new.root != "/" && new.root != new.volume.mount_point {
        notes.push(format!(
            "The scans cover {} only: the OS space change includes everything else on the volume, so the unreconciled difference mostly reflects activity outside the scanned tree.",
            new.root
        ));
    }
    notes.push("Scans are not atomic: files changing while a scan runs are captured at different moments.".into());
    if old.volume.fs_type == "apfs" {
        notes.push(
            "On APFS the sum of allocated sizes is not exclusive physical usage: clones share blocks and local snapshots keep deleted data.".into(),
        );
    }
    if prefix.is_some() {
        notes.push("Restricted to a path: OS space change and unreconciled difference are not attributable and are omitted.".into());
    }
    if let Some(why) = &non_equivalent {
        notes.push(format!("NON-EQUIVALENT comparison ({why}): differences may reflect scan settings, not real changes."));
    }
    Ok(DiffResult { old, new, path: path.map(Path::to_path_buf), non_equivalent, space, unreconciled_bytes, notes, report })
}

#[cfg(test)]
mod pair_tests {
    use super::*;

    fn meta(id: u64, root: &str, status: Status) -> SnapshotMeta {
        SnapshotMeta {
            format_version: 1,
            id,
            name: format!("s{id}"),
            root: root.into(),
            disk: None,
            root_in_volume: None,
            status,
            started_at: "2026-10-21T10:00:00+02:00".into(),
            finished_at: None,
            volume: Default::default(),
            options: Default::default(),
            space_before: None,
            space_after: None,
            storage: Default::default(),
            counts: None,
            error: None,
            diskdrift_version: String::new(),
        }
    }

    #[test]
    fn default_names_are_recognised() {
        assert!(is_default_name(&default_name()));
        assert!(!is_default_name("baseline"));
        assert!(!is_default_name("auto-2026-10-21T13-13-41"));
    }

    #[test]
    fn previous_comparable_and_last_scan_of_a_root() {
        let root_a = std::env::temp_dir().join("diskdrift-pair-a");
        let root_b = std::env::temp_dir().join("diskdrift-pair-b");
        let root_c = std::env::temp_dir().join("diskdrift-pair-c");
        let root_a = root_a.to_string_lossy().into_owned();
        let root_b = root_b.to_string_lossy().into_owned();
        let root_c = root_c.to_string_lossy().into_owned();
        let all = [meta(1, &root_a, Status::Completed), meta(2, &root_b, Status::Completed), meta(3, &root_a, Status::Cancelled), meta(4, &root_a, Status::Partial)];
        let metas: Vec<&SnapshotMeta> = all.iter().collect();
        assert_eq!(previous_comparable(&metas, &all[3]).map(|m| m.id), Some(1));
        assert_eq!(previous_comparable(&metas, &all[1]).map(|m| m.id), None);
        assert_eq!(last_scan_of(&metas[..3], Path::new(&root_a)).map(|m| m.id), Some(1), "#3 holds no data");
        assert_eq!(last_scan_of(&metas, Path::new(&root_c)).map(|m| m.id), None);
    }

    #[test]
    fn check_root_expands_home_and_says_what_is_wrong() {
        let t = diskdrift_core::testutil::TestDir::new("check_root");
        let dir = t.path().to_path_buf();
        t.dir("Library");
        t.file("file", 1);
        assert_eq!(check_root("~", &dir).unwrap(), dir);
        assert_eq!(check_root(" ~/Library ", &dir).unwrap(), dir.join("Library"));
        assert_eq!(check_root(&dir.display().to_string(), &dir).unwrap(), dir);
        assert!(check_root("Library", &dir).unwrap_err().contains("not a full path"));
        assert!(check_root("~/missing", &dir).unwrap_err().contains("does not exist"));
        assert!(check_root("~/file", &dir).unwrap_err().contains("is a file"));
        assert!(check_root("", &dir).unwrap_err().contains("/ for the whole disk"));
    }

    #[test]
    fn latest_pair_skips_other_roots_and_scans_without_data() {
        let all = [meta(1, "/a", Status::Completed), meta(2, "/b", Status::Completed), meta(3, "/a", Status::Cancelled), meta(4, "/a", Status::Completed)];
        let metas: Vec<&SnapshotMeta> = all.iter().collect();
        assert_eq!(latest_pair(&metas), Some((0, 3)), "#4 against #1: #3 holds no data, #2 is another root");
        assert!(!comparable(metas[3], metas[0]), "the older scan comes first");
        assert!(!comparable(metas[0], metas[1]));
        // The newest scan (#5, /c) has no partner: fall back to the newest pair there is.
        let all = [meta(1, "/a", Status::Completed), meta(2, "/a", Status::Completed), meta(5, "/c", Status::Completed)];
        let metas: Vec<&SnapshotMeta> = all.iter().collect();
        assert_eq!(latest_pair(&metas), Some((0, 1)));
        assert_eq!(latest_pair(&metas[1..]), None);
    }
}
