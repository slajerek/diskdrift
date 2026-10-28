//! The snapshot store: a directory of immutable per-snapshot files.
//!
//! ```text
//! <store>/store.json              format marker
//! <store>/lock                    flock'ed by the single writer (holds its pid)
//! <store>/snapshots/<stem>.json             metadata, rewritten atomically
//! <store>/snapshots/<stem>.dds              full checkpoint, immutable
//! <store>/snapshots/<stem>.r<base-stem>.dds reverse delta against snapshot <base-stem>
//! <store>/snapshots/<stem>.dds.tmp          data of a scan in progress
//! <store>/disks.json, disks.lock            the disk registry (`disks.rs`)
//! ```
//! `<stem>` is the disk's short ID and the scan start, `<disk>-YYMMDD-HHMMSS` in
//! local time (`-2`, `-3`, ... appended if taken). Version 1 stores named files
//! after the zero-padded id, version 2 stores after the time alone; they are
//! migrated when the writer lock is first taken, and read as they are before.
//! Data files appear only by atomic rename after fsync, so a crash never leaves a
//! snapshot that looks complete. A snapshot whose metadata still says IN_PROGRESS
//! while no writer holds the lock was interrupted and is reported as FAILED.
//!
//! History layout (ADR 0001): the newest snapshot of a scan root is a full
//! checkpoint; when a newer one is committed, the previous one is rewritten as a
//! reverse delta against it. Every step (new file, metadata switch, removal of the
//! replaced file) leaves a readable store; files no metadata refers to are
//! leftovers of an interruption and are removed when the writer lock is taken.

use crate::codec::{OpEncoderBlocks, RecordEncoder};
use crate::container::{DataFile, DataWriter, FileKind};
use crate::disks::{Registry, Sighting};
use crate::meta::{SnapshotMeta, SpaceSample, Status};
use diskdrift_core::delta::{Apply, DeltaBuilder, DeltaOp, PushDelta};
use diskdrift_core::diff::{RecordStream, with_root_name};
use diskdrift_core::paths::{PathStack, compare_paths};
use diskdrift_core::record::Record;
use diskdrift_core::walk::WalkReport;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub const STORE_FORMAT: &str = "diskdrift-snapshots";
/// 2: files named after the scan start time instead of the id.
/// 3: the disk ID in front of the time; snapshots record their disk (ADR 0002).
pub const STORE_VERSION: u32 = 3;

/// Longest delta chain: an older snapshot stays a full keyframe rather than make
/// any snapshot need more than this many reconstruction steps.
pub const MAX_CHAIN: usize = 30;
/// A reverse delta larger than this fraction of the full snapshot is not worth it.
pub const MAX_REVERSE_RATIO: f64 = 0.5;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("not enough disk space to write the snapshot store at {path}: {source}")]
    DiskFull { path: PathBuf, source: io::Error },
    #[error("{0} is not a DiskDrift snapshot store")]
    NotAStore(PathBuf),
    #[error("store format version {found} is newer than this build supports ({STORE_VERSION})")]
    UnsupportedVersion { found: u32 },
    #[error("another scan is writing to this store (pid {pid}); wait for it to finish")]
    Locked { pid: String },
    #[error("snapshot '{0}' not found")]
    NotFound(String),
    #[error("snapshot {id} has no readable data (status {status})")]
    NoData { id: u64, status: Status },
    #[error("a snapshot named '{0}' already exists")]
    DuplicateName(String),
    #[error("snapshot {id} is the base of snapshot(s) {dependents:?}; remove those first")]
    HasDependents { id: u64, dependents: Vec<u64> },
    #[error("corrupt snapshot data: {0}")]
    Corrupt(String),
    #[error("disk registry format version {found} is newer than this build supports ({})", crate::disks::DISKS_VERSION)]
    UnsupportedDisksVersion { found: u32 },
    #[error("disk '{0}' not found")]
    UnknownDisk(String),
    #[error("the label '{label}' is already used by disk {disk}")]
    DuplicateLabel { label: String, disk: String },
    #[error("invalid label '{label}': {why}")]
    InvalidLabel { label: String, why: String },
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

fn is_disk_full(e: &io::Error) -> bool {
    #[cfg(unix)]
    let unix_full = matches!(e.raw_os_error(), Some(libc::ENOSPC) | Some(libc::EDQUOT) | Some(libc::EFBIG));
    #[cfg(not(unix))]
    let unix_full = false;
    unix_full
        || e.kind() == io::ErrorKind::StorageFull
}

fn io_err(path: &Path, source: io::Error) -> StoreError {
    if source.kind() == io::ErrorKind::InvalidData {
        return StoreError::Corrupt(source.to_string());
    }
    if is_disk_full(&source) {
        return StoreError::DiskFull { path: path.to_path_buf(), source };
    }
    StoreError::Io { path: path.to_path_buf(), source }
}

trait Ctx<T> {
    fn at(self, path: &Path) -> Result<T>;
}

impl<T> Ctx<T> for io::Result<T> {
    fn at(self, path: &Path) -> Result<T> {
        self.map_err(|e| io_err(path, e))
    }
}

/// Delta ops as stored, in canonical path order.
pub type OpStream = Box<dyn Iterator<Item = io::Result<DeltaOp>>>;

pub struct Store {
    root: PathBuf,
}

/// Exclusive writer lock (released when dropped or when the process dies).
pub struct WriteLock {
    _file: File,
    #[cfg(windows)]
    path: PathBuf,
}

#[cfg(windows)]
impl Drop for WriteLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(not(windows))]
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(windows)]
pub(crate) fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    sync_dir(path.parent().unwrap_or(Path::new("/")))
}

/// `stream` (a scan of `from`) with its root renamed to `to`; unchanged when the
/// roots are the same, so the exact root bytes are kept.
fn rooted_at(stream: RecordStream<'static>, from: &str, to: &str) -> RecordStream<'static> {
    if from == to { stream } else { with_root_name(stream, to.as_bytes().to_vec()) }
}

/// File-name stem of a snapshot (the zero-padded id in version 1 stores).
fn stem_of(m: &SnapshotMeta) -> String {
    m.storage.stem.clone().unwrap_or_else(|| format!("{:08}", m.id))
}

/// The snapshot is stored as version 3 names it: it has a disk, and its stem
/// starts with it.
fn is_current(m: &SnapshotMeta) -> bool {
    match (&m.disk, &m.storage.stem) {
        (Some(d), Some(s)) => s.strip_prefix(d.as_str()).is_some_and(|rest| rest.starts_with('-')),
        _ => false,
    }
}

/// Simulated crashes for the migration tests: the N-th step fails.
#[cfg(test)]
pub(crate) mod crash {
    use std::cell::Cell;
    thread_local! {
        pub static AFTER: Cell<Option<usize>> = const { Cell::new(None) };
        static DONE: Cell<usize> = const { Cell::new(0) };
    }
    pub fn arm(n: Option<usize>) {
        AFTER.set(n);
        DONE.set(0);
    }
    pub fn step() -> super::Result<()> {
        DONE.set(DONE.get() + 1);
        if AFTER.get() == Some(DONE.get()) {
            return Err(super::StoreError::Corrupt("simulated crash".into()));
        }
        Ok(())
    }
}

fn migration_step() -> Result<()> {
    #[cfg(test)]
    crash::step()?;
    Ok(())
}

/// `YYMMDD-HHMMSS` from an RFC 3339 time (`2026-10-21T04:03:12+02:00` →
/// `261021-040312`), keeping the local time it was written in.
fn time_stem(rfc3339: &str) -> Option<String> {
    let head = rfc3339.get(..19)?;
    let d: String = head.chars().filter(char::is_ascii_digit).collect();
    (d.len() == 14).then(|| format!("{}-{}", &d[2..8], &d[8..]))
}

fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    { a == b }
}

fn allocated(path: &Path) -> u64 {
    #[cfg(unix)]
    { std::fs::metadata(path).map(|m| { use std::os::unix::fs::MetadataExt; m.blocks() * 512 }).unwrap_or(0) }
    #[cfg(not(unix))]
    { std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) }
}

impl Store {
    pub fn exists(path: &Path) -> bool {
        path.join("store.json").is_file()
    }

    /// Opens an existing store (never creates anything).
    pub fn open(path: &Path) -> Result<Store> {
        let marker = path.join("store.json");
        let text = match std::fs::read_to_string(&marker) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(StoreError::NotAStore(path.to_path_buf())),
            Err(e) => return Err(io_err(&marker, e)),
        };
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|_| StoreError::NotAStore(path.to_path_buf()))?;
        if v["format"] != STORE_FORMAT {
            return Err(StoreError::NotAStore(path.to_path_buf()));
        }
        let found = v["version"].as_u64().unwrap_or(0) as u32;
        if found > STORE_VERSION {
            return Err(StoreError::UnsupportedVersion { found });
        }
        Ok(Store { root: path.to_path_buf() })
    }

    /// Opens the store, creating it (user-only permissions) if needed.
    pub fn create(path: &Path) -> Result<Store> {
        if Self::exists(path) {
            return Self::open(path);
        }
        if path.exists() && std::fs::read_dir(path).at(path)?.next().is_some() {
            return Err(StoreError::NotAStore(path.to_path_buf()));
        }
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(path.join("snapshots")).at(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).at(path)?;
        }
        let marker = serde_json::json!({ "format": STORE_FORMAT, "version": STORE_VERSION });
        write_atomic(&path.join("store.json"), marker.to_string().as_bytes()).at(path)?;
        Ok(Store { root: path.to_path_buf() })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    fn snap_dir(&self) -> PathBuf {
        self.root.join("snapshots")
    }

    fn meta_path(&self, m: &SnapshotMeta) -> PathBuf {
        self.snap_dir().join(format!("{}.json", stem_of(m)))
    }

    /// `name` (`<stem>.<rest>`), or `<stem>-N.<rest>` if that file already exists.
    fn unused_name(&self, name: String) -> String {
        if !self.snap_dir().join(&name).exists() {
            return name;
        }
        let (stem, rest) = name.split_once('.').unwrap_or((&name, ""));
        (2..)
            .map(|n| format!("{stem}-{n}.{rest}"))
            .find(|c| !self.snap_dir().join(c).exists())
            .expect("unbounded")
    }

    /// A stem for snapshot `m`, unique among `metas` (and, with `check_files`,
    /// among the metadata files in the directory).
    fn new_stem(&self, m: &SnapshotMeta, metas: &[SnapshotMeta], check_files: bool) -> String {
        let time = time_stem(&m.started_at).unwrap_or_else(|| format!("{:08}", m.id));
        let base = match &m.disk {
            Some(d) => format!("{d}-{time}"),
            None => time,
        };
        let taken = |s: &str| {
            metas.iter().any(|x| x.id != m.id && x.storage.stem.as_deref() == Some(s))
                || (check_files && self.snap_dir().join(format!("{s}.json")).exists())
        };
        if !taken(&base) {
            return base;
        }
        (2..).map(|n| format!("{base}-{n}")).find(|s| !taken(s)).expect("unbounded")
    }

    /// Brings the store to the current version: records every snapshot's disk
    /// in the registry and in its metadata, and renames its files to
    /// `<disk>-<time>` (version 1 and 2 stores; ADR 0002, 2.8). Order: the
    /// registry first, then per snapshot a hard link of the data to its new
    /// name, the new metadata, and the removal of the old names; the version
    /// marker last. An interruption at any step leaves a store that reads the
    /// same, and the next run finishes the job.
    fn migrate(&self) -> Result<()> {
        let entries = self.read_entries()?;
        let mut primary: Vec<SnapshotMeta> = Vec::new();
        let mut leftovers = Vec::new();
        for (path, m) in &entries {
            if primary.last().is_some_and(|p| p.id == m.id) {
                leftovers.push((path.clone(), m.clone()));
            } else {
                primary.push(m.clone());
            }
        }
        if leftovers.is_empty() && primary.iter().all(is_current) {
            return self.write_version();
        }
        let dir = self.snap_dir();
        // 1. The registry, in scan order, so "first seen" and "last seen" are right.
        let disks: Vec<Option<String>> = Registry::update(&self.root, |reg| {
            Ok(primary
                .iter()
                .map(|m| {
                    (!is_current(m)).then(|| match &m.disk {
                        // Re-pointed by `merge_disks`: keep it, only the names change.
                        Some(d) if reg.get(d).is_some() => d.clone(),
                        _ => reg.record(&Sighting::of_snapshot(m)),
                    })
                })
                .collect())
        })?;
        migration_step()?;
        // The target metadata: disk, folder within it, and the new stem.
        let mut target = primary.clone();
        for (t, disk) in target.iter_mut().zip(&disks) {
            if let Some(d) = disk {
                t.root_in_volume = Some(t.derived_root_in_volume());
                t.disk = Some(d.clone());
                t.storage.stem = None;
            }
        }
        for i in 0..target.len() {
            if target[i].storage.stem.is_none() {
                let stem = self.new_stem(&target[i], &target, false);
                target[i].storage.stem = Some(stem);
            }
        }
        // 2. Each snapshot.
        for (i, old) in primary.iter().enumerate() {
            if disks[i].is_none() {
                continue;
            }
            let mut m = target[i].clone();
            if let Some(old_data) = &old.storage.data_file {
                let name = match m.storage.base.and_then(|b| target.iter().find(|x| x.id == b)) {
                    Some(base) => format!("{}.r{}.dds", stem_of(&m), stem_of(base)),
                    None => format!("{}.dds", stem_of(&m)),
                };
                let (from, to) = (dir.join(old_data), dir.join(&name));
                if from.exists() && !same_file(&from, &to) {
                    // A file under the new name that is not this data is a leftover of
                    // an interrupted run (nothing refers to it yet).
                    let _ = std::fs::remove_file(&to);
                    std::fs::hard_link(&from, &to).at(&to)?;
                }
                m.storage.data_file = Some(name);
                migration_step()?;
            }
            self.write_meta(&m)?;
            migration_step()?;
            let old_meta = self.meta_path(old);
            if old_meta != self.meta_path(&m) {
                std::fs::remove_file(&old_meta).at(&old_meta)?;
            }
            migration_step()?;
            if let Some(old_data) = &old.storage.data_file
                && Some(old_data) != m.storage.data_file.as_ref()
            {
                let _ = std::fs::remove_file(dir.join(old_data));
            }
        }
        // Metadata files of snapshots an interrupted run had already migrated.
        for (path, _) in leftovers {
            std::fs::remove_file(&path).at(&path)?;
        }
        sync_dir(&dir).at(&dir)?;
        self.write_version()
    }

    /// Lock the registry, apply `f`, save. When `disks.json` does not exist yet,
    /// `f` starts from the registry derived from the metadata, so the disks it
    /// changes keep the IDs the snapshots already show.
    pub fn update_registry<T>(&self, f: impl FnOnce(&mut Registry) -> Result<T>) -> Result<T> {
        let lock = crate::disks::lock(&self.root)?;
        let mut reg = self.registry()?;
        let out = f(&mut reg)?;
        reg.save(&self.root, &lock)?;
        Ok(out)
    }

    /// Joins the history of disk `from` to disk `into` (ADR 0002: one disk
    /// recognised as two, for example a FAT stick whose old scans have only a
    /// weak identity). The snapshots of `from` are re-pointed and renamed,
    /// `into` keeps its label (or takes `from`'s), gets both sets, and `from`
    /// leaves the registry. Returns the snapshots moved.
    pub fn merge_disks(&self, _lock: &WriteLock, from: &str, into: &str) -> Result<Vec<u64>> {
        if from == into {
            return Err(StoreError::Invalid(format!("disk {from} cannot be merged into itself")));
        }
        let reg = self.registry()?;
        for id in [from, into] {
            if reg.get(id).is_none() {
                return Err(StoreError::UnknownDisk(id.into()));
            }
        }
        let mut moved = Vec::new();
        for mut m in self.read_metas()?.into_iter().filter(|m| m.disk.as_deref() == Some(from)) {
            m.disk = Some(into.to_string());
            self.write_meta(&m)?;
            moved.push(m.id);
        }
        self.update_registry(|reg| {
            let gone = reg.forget(from)?;
            let d = reg.disks.iter_mut().find(|d| d.id == into).ok_or_else(|| StoreError::UnknownDisk(into.into()))?;
            if gone.first_seen < d.first_seen {
                d.first_seen = gone.first_seen.clone();
            }
            for s in gone.sets {
                if !d.sets.iter().any(|x| x.eq_ignore_ascii_case(&s)) {
                    d.sets.push(s);
                }
            }
            if d.description.is_none() {
                d.description = gone.description;
            }
            if d.label.is_none() {
                d.label = gone.label;
            }
            Ok(())
        })?;
        self.migrate()?;
        Ok(moved)
    }

    /// The disk registry. When `disks.json` does not exist yet (a version 2
    /// store, or no scan has written it), it is derived in memory from the
    /// snapshots' metadata; nothing is written.
    pub fn registry(&self) -> Result<Registry> {
        if crate::disks::disks_path(&self.root).exists() {
            return Registry::load(&self.root);
        }
        let mut reg = Registry::default();
        for m in self.read_entries()?.iter().map(|(_, m)| m) {
            reg.record(&Sighting::of_snapshot(m));
        }
        Ok(reg)
    }

    fn write_version(&self) -> Result<()> {
        let marker = self.root.join("store.json");
        let text = std::fs::read_to_string(&marker).at(&marker)?;
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|_| StoreError::NotAStore(self.root.clone()))?;
        if v["version"].as_u64() == Some(STORE_VERSION as u64) {
            return Ok(());
        }
        let marker_json = serde_json::json!({ "format": STORE_FORMAT, "version": STORE_VERSION });
        write_atomic(&marker, marker_json.to_string().as_bytes()).at(&marker)
    }

    fn try_lock(&self) -> Result<Option<WriteLock>> {
        let p = self.root.join("lock");
        #[cfg(windows)]
        let mut f = match std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
            Err(e) => return Err(StoreError::Io { path: p, source: e }),
        };
        #[cfg(not(windows))]
        let mut f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&p).at(&p)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Ok(None);
            }
        }
        f.set_len(0).at(&p)?;
        let _ = write!(f, "{}", std::process::id());
        Ok(Some(WriteLock {
            _file: f,
            #[cfg(windows)]
            path: p,
        }))
    }

    /// True while another process holds the writer lock.
    pub fn is_busy(&self) -> bool {
        matches!(self.try_lock(), Ok(None))
    }

    /// Takes the writer lock and repairs the state left by an interrupted writer.
    pub fn lock(&self) -> Result<WriteLock> {
        let Some(lock) = self.try_lock()? else {
            let pid = std::fs::read_to_string(self.root.join("lock")).unwrap_or_default();
            return Err(StoreError::Locked { pid: pid.trim().to_string() });
        };
        // Recovery: interrupted scans become FAILED, their temporary data is removed.
        for m in self.read_metas()? {
            if m.status == Status::InProgress {
                let mut m = m;
                m.status = Status::Failed;
                m.error.get_or_insert_with(|| "interrupted (process ended before the scan finished)".into());
                self.write_meta(&m)?;
            }
        }
        self.migrate()?;
        // Leftovers: temporary files and data files no snapshot refers to (an
        // interrupted rebase or removal).
        let referenced: std::collections::HashSet<String> =
            self.read_metas()?.into_iter().filter_map(|m| m.storage.data_file).collect();
        let dir = self.snap_dir();
        for e in std::fs::read_dir(&dir).at(&dir)?.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            let stale_data = p.extension().is_some_and(|x| x == "dds") && !referenced.contains(&name);
            if p.extension().is_some_and(|x| x == "tmp") || stale_data {
                let _ = std::fs::remove_file(&p);
            }
        }
        Ok(lock)
    }

    /// Every metadata file with its path, by id (duplicates of one id, left by an
    /// interrupted migration, are all included: migrated one first).
    fn read_entries(&self) -> Result<Vec<(PathBuf, SnapshotMeta)>> {
        let dir = self.snap_dir();
        let mut out = Vec::new();
        for e in std::fs::read_dir(&dir).at(&dir)? {
            let p = e.at(&dir)?.path();
            if p.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let text = std::fs::read_to_string(&p).at(&p)?;
            let m: SnapshotMeta = serde_json::from_str(&text)
                .map_err(|e| StoreError::Corrupt(format!("{}: {e}", p.display())))?;
            out.push((p, m));
        }
        // Of the duplicates an interrupted migration leaves, the most migrated first.
        out.sort_by_key(|(_, m)| (m.id, !is_current(m), m.storage.stem.is_none()));
        Ok(out)
    }

    /// One metadata document per snapshot, by id. Snapshots of an unmigrated
    /// store get their disk and folder within it in memory (ADR 0002, 2.8).
    fn read_metas(&self) -> Result<Vec<SnapshotMeta>> {
        let mut out: Vec<SnapshotMeta> = self.read_entries()?.into_iter().map(|(_, m)| m).collect();
        out.dedup_by_key(|m| m.id);
        if out.iter().any(|m| m.disk.is_none()) {
            let mut reg = Registry::load(&self.root).unwrap_or_default();
            for m in out.iter_mut().filter(|m| m.disk.is_none()) {
                m.disk = Some(reg.record(&Sighting::of_snapshot(m)));
                m.root_in_volume = Some(m.derived_root_in_volume());
            }
        }
        Ok(out)
    }

    fn write_meta(&self, m: &SnapshotMeta) -> Result<()> {
        let p = self.meta_path(m);
        let text = serde_json::to_vec_pretty(m).expect("serializable");
        write_atomic(&p, &text).at(&p)
    }

    /// All snapshots, oldest first. Interrupted scans are reported as FAILED.
    pub fn list(&self) -> Result<Vec<SnapshotMeta>> {
        let mut v = self.read_metas()?;
        if v.iter().any(|m| m.status == Status::InProgress) && !self.is_busy() {
            for m in v.iter_mut().filter(|m| m.status == Status::InProgress) {
                m.status = Status::Failed;
                m.error.get_or_insert_with(|| "interrupted".into());
            }
        }
        Ok(v)
    }

    pub fn get(&self, id: u64) -> Result<SnapshotMeta> {
        self.list()?.into_iter().find(|m| m.id == id).ok_or_else(|| StoreError::NotFound(id.to_string()))
    }

    /// Resolves a snapshot reference: numeric id, name, or `latest` (most recent
    /// COMPLETED or PARTIAL snapshot). `latest~1` is the one before it. With a
    /// disk in front (`backupy-2014:latest`, `d1a2b3c4:latest~1`), only that
    /// disk's snapshots count.
    pub fn resolve(&self, reference: &str) -> Result<SnapshotMeta> {
        let all = self.list()?;
        if let Some((disk, rest)) = reference.split_once(':') {
            let reg = self.registry()?;
            let d = reg.resolve(disk).ok_or_else(|| StoreError::UnknownDisk(disk.into()))?;
            let mine: Vec<SnapshotMeta> = all.into_iter().filter(|m| m.disk.as_deref() == Some(d.id.as_str())).collect();
            return Self::resolve_in(&mine, if rest.is_empty() { "latest" } else { rest }, reference);
        }
        Self::resolve_in(&all, reference, reference)
    }

    fn resolve_in(all: &[SnapshotMeta], reference: &str, shown: &str) -> Result<SnapshotMeta> {
        let usable: Vec<&SnapshotMeta> = all.iter().filter(|m| m.status.is_usable()).collect();
        if let Some(rest) = reference.strip_prefix("latest") {
            let back: usize = match rest.strip_prefix('~') {
                Some(n) => n.parse().map_err(|_| StoreError::NotFound(shown.into()))?,
                None if rest.is_empty() => 0,
                None => return Err(StoreError::NotFound(shown.into())),
            };
            return usable
                .iter()
                .rev()
                .nth(back)
                .map(|m| (*m).clone())
                .ok_or_else(|| StoreError::NotFound(shown.into()));
        }
        if let Some(m) = all.iter().find(|m| m.name == reference) {
            return Ok(m.clone());
        }
        if let Ok(id) = reference.parse::<u64>()
            && let Some(m) = all.iter().find(|m| m.id == id) {
                return Ok(m.clone());
            }
        Err(StoreError::NotFound(shown.into()))
    }

    fn data_file(&self, m: &SnapshotMeta) -> Result<DataFile> {
        let name = m.storage.data_file.as_ref().filter(|_| m.status.is_usable());
        let Some(name) = name else {
            return Err(StoreError::NoData { id: m.id, status: m.status });
        };
        let p = self.snap_dir().join(name);
        DataFile::open(&p).at(&p)
    }

    /// Streams the records of a snapshot, reconstructing deltas.
    pub fn records(&self, id: u64) -> Result<RecordStream<'static>> {
        let all = self.list()?;
        self.records_in(&all, id, 0)
    }

    /// The records of snapshot `id` as if it had been scanned at `root`: for
    /// comparing scans of one disk mounted at different places.
    pub fn records_as(&self, id: u64, root: &str) -> Result<RecordStream<'static>> {
        let all = self.list()?;
        let m = all.iter().find(|m| m.id == id).ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        Ok(rooted_at(self.records_in(&all, id, 0)?, &m.root, root))
    }

    fn records_in(&self, all: &[SnapshotMeta], id: u64, depth: usize) -> Result<RecordStream<'static>> {
        let m = all.iter().find(|m| m.id == id).ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        if depth > 64 {
            return Err(StoreError::Corrupt(format!("delta chain of snapshot {id} is too long or cyclic")));
        }
        let df = self.data_file(m)?;
        let path = df.path.clone();
        match m.storage.base {
            None => Ok(Box::new(df.records_from(0).at(&path)?)),
            Some(base) => {
                // Ops are always built against the base renamed to this snapshot's root.
                let base_root = all.iter().find(|b| b.id == base).map(|b| b.root.as_str()).unwrap_or(&m.root);
                let base_stream = rooted_at(self.records_in(all, base, depth + 1)?, base_root, &m.root);
                Ok(Box::new(Apply::new(base_stream, Box::new(df.ops().at(&path)?))))
            }
        }
    }

    /// The stored delta of snapshot `id`, without reconstructing anything: its
    /// base and the ops that turn the base (renamed to this snapshot's root)
    /// into it. `None` for a full snapshot. For history search: an `Add` is a
    /// path this snapshot has and its base does not.
    pub fn delta_ops(&self, id: u64) -> Result<Option<(u64, OpStream)>> {
        let m = self.get(id)?;
        let Some(base) = m.storage.base else {
            return Ok(None);
        };
        let df = self.data_file(&m)?;
        let path = df.path.clone();
        Ok(Some((base, Box::new(df.ops().at(&path)?))))
    }

    /// Streams the records at or below `prefix` (components including the root).
    /// Full snapshots seek with the block index; deltas are filtered.
    pub fn records_under(&self, id: u64, prefix: &[Vec<u8>]) -> Result<RecordStream<'static>> {
        let all = self.list()?;
        let m = all.iter().find(|m| m.id == id).ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        if m.storage.base.is_some() {
            return Ok(Box::new(PrefixFilter::new(self.records_in(&all, id, 0)?, prefix.to_vec(), &[])));
        }
        let df = self.data_file(m)?;
        let path = df.path.clone();
        let start = df
            .index
            .partition_point(|e| compare_paths(&e.first_path, prefix) != std::cmp::Ordering::Greater)
            .saturating_sub(1);
        let ctx = if df.index.is_empty() { Vec::new() } else { df.block_context(start).at(&path)? };
        let stream = df.records_from(start).at(&path)?;
        Ok(Box::new(PrefixFilter::new(Box::new(stream), prefix.to_vec(), &ctx)))
    }

    /// Reads every block of a snapshot (and its base) and checks checksums and
    /// structure. Returns the number of records.
    pub fn verify(&self, id: u64) -> Result<u64> {
        let all = self.list()?;
        let m = all.iter().find(|m| m.id == id).ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        let df = self.data_file(m)?;
        df.verify().at(&df.path)?;
        let mut n = 0;
        for r in self.records_in(&all, id, 0)? {
            r.map_err(|e| StoreError::Corrupt(e.to_string()))?;
            n += 1;
        }
        Ok(n)
    }

    /// Allocated bytes of the whole store.
    pub fn size_on_disk(&self) -> u64 {
        let mut total = allocated(&self.root.join("store.json"));
        if let Ok(rd) = std::fs::read_dir(self.snap_dir()) {
            total += rd.flatten().map(|e| allocated(&e.path())).sum::<u64>();
        }
        total
    }

    /// Allocated bytes of one snapshot's own files (not its base).
    pub fn snapshot_size(&self, m: &SnapshotMeta) -> u64 {
        let data = m.storage.data_file.as_ref().map(|n| allocated(&self.snap_dir().join(n))).unwrap_or(0);
        data + allocated(&self.meta_path(m))
    }

    #[doc(hidden)]
    pub fn rewrite_for_test(&self, d: &SnapshotMeta, base: Option<&SnapshotMeta>) {
        self.rewrite(d, base).unwrap();
    }

    #[doc(hidden)]
    pub fn chains_for_test(all: &[SnapshotMeta]) -> std::collections::HashMap<u64, (usize, u64)> {
        Self::chains(all)
    }

    /// Reconstruction steps needed for each snapshot (0 for full ones) and the
    /// full snapshot its chain ends at.
    fn chains(all: &[SnapshotMeta]) -> std::collections::HashMap<u64, (usize, u64)> {
        let by_id: std::collections::HashMap<u64, &SnapshotMeta> = all.iter().map(|m| (m.id, m)).collect();
        let mut out = std::collections::HashMap::new();
        for m in all.iter().filter(|m| m.status.is_usable()) {
            let (mut cur, mut depth) = (m, 0usize);
            while let Some(b) = cur.storage.base {
                match by_id.get(&b) {
                    Some(next) if depth < 1000 => {
                        cur = next;
                        depth += 1;
                    }
                    _ => break,
                }
            }
            out.insert(m.id, (depth, cur.id));
        }
        out
    }

    /// Writes ops turning `base`'s records into `target`'s, as `dest`. Returns
    /// (items, blocks, records of `target`).
    fn write_delta_file(&self, base: &SnapshotMeta, target: &SnapshotMeta, dest: &Path) -> Result<(u64, u64, u64)> {
        let tmp = dest.with_extension("dds.tmp");
        let base_stream = self.records_as(base.id, &target.root)?;
        let records = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let seen = records.clone();
        let target_stream: RecordStream<'static> = Box::new(self.records(target.id)?.inspect(move |_| seen.set(seen.get() + 1)));
        let mut data = DataWriter::create(tmp.clone(), FileKind::Ops).at(&tmp)?;
        let mut ops = OpEncoderBlocks::new();
        let r = (|| -> io::Result<(u64, u64, u64)> {
            {
                let mut b = DeltaBuilder::new(|op: &DeltaOp| {
                    if let Some(block) = ops.push(op)? {
                        data.block(block)?;
                    }
                    Ok(())
                });
                diskdrift_core::diff::merge(base_stream, target_stream, &mut b)?;
            }
            if let Some(block) = ops.finish()? {
                data.block(block)?;
            }
            let blocks = data.blocks();
            Ok((data.commit(dest)?, blocks, records.get()))
        })();
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        r.at(dest)
    }

    /// Writes `id`'s records as a full data file `dest`. Returns (items, blocks).
    fn write_full_file(&self, id: u64, dest: &Path) -> Result<(u64, u64)> {
        let tmp = dest.with_extension("dds.tmp");
        let stream = self.records(id)?;
        let mut data = DataWriter::create(tmp.clone(), FileKind::Records).at(&tmp)?;
        let mut enc = RecordEncoder::new();
        let r = (|| -> io::Result<(u64, u64)> {
            for rec in stream {
                if let Some(b) = enc.push(&rec?)? {
                    data.block(b)?;
                }
            }
            if let Some(b) = enc.finish()? {
                data.block(b)?;
            }
            let blocks = data.blocks();
            Ok((data.commit(dest)?, blocks))
        })();
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        r.at(dest)
    }

    /// Points `m` at a new data file and deletes the one it replaces.
    fn switch_data(&self, mut m: SnapshotMeta, base: Option<u64>, name: String, items: u64, blocks: u64) -> Result<SnapshotMeta> {
        let old = m.storage.data_file.replace(name.clone());
        m.storage.base = base;
        m.storage.items = items;
        m.storage.blocks = blocks;
        m.storage.bytes_on_disk = allocated(&self.snap_dir().join(&name));
        self.write_meta(&m)?;
        if let Some(old) = old.filter(|o| *o != name) {
            let p = self.snap_dir().join(old);
            let _ = std::fs::remove_file(&p);
            sync_dir(&self.snap_dir()).at(&self.snap_dir())?;
        }
        Ok(m)
    }

    /// After `newest` was committed as a full snapshot: rewrite the previous
    /// comparable full snapshot as a reverse delta against it, unless that would
    /// make a delta chain longer than [`MAX_CHAIN`] or save too little.
    pub fn rebase_previous(&self, _lock: &WriteLock, newest: &SnapshotMeta) -> Result<Rebase> {
        let all = self.list()?;
        let Some(prev) = all
            .iter()
            .rev()
            .find(|m| m.id < newest.id && m.status.is_usable() && m.incompatibility(newest).is_none())
        else {
            return Ok(Rebase::Nothing);
        };
        if prev.storage.base.is_some() {
            return Ok(Rebase::Nothing);
        }
        let chains = Self::chains(&all);
        let deepest = chains.values().filter(|(_, end)| *end == prev.id).map(|(d, _)| *d).max().unwrap_or(0);
        if deepest + 1 > MAX_CHAIN {
            return Ok(Rebase::KeptFull { id: prev.id, reason: format!("keyframe (chains limited to {MAX_CHAIN} steps)") });
        }
        let name = self.unused_name(format!("{}.r{}.dds", stem_of(prev), stem_of(newest)));
        let dest = self.snap_dir().join(&name);
        let (items, blocks, _) = self.write_delta_file(newest, prev, &dest)?;
        let len = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let prev_file = self.snap_dir().join(prev.storage.data_file.as_deref().unwrap_or(""));
        let (delta_len, full_len) = (len(&dest), len(&prev_file));
        // Below 1 MB the space hardly matters and one consistent layout is simpler.
        if full_len >= 1 << 20 && delta_len as f64 > full_len as f64 * MAX_REVERSE_RATIO {
            let _ = std::fs::remove_file(&dest);
            return Ok(Rebase::KeptFull { id: prev.id, reason: "changes too large for a delta to pay off".into() });
        }
        let freed = allocated(&prev_file).saturating_sub(allocated(&dest));
        self.switch_data(prev.clone(), Some(newest.id), name, items, blocks)?;
        Ok(Rebase::Converted { id: prev.id, freed_bytes: freed })
    }

    fn next_id(&self) -> Result<u64> {
        Ok(self.read_metas()?.iter().map(|m| m.id).max().unwrap_or(0) + 1)
    }

    /// Starts a snapshot. `template` supplies name, root, volume, options and
    /// `space_before`, and its disk when the caller knows it; id, status and
    /// storage are filled in here. Without a disk, the disk is the one the
    /// volume in `template` belongs to, recorded in the registry. With a
    /// `base`, the data is written as a delta against it.
    pub fn begin(&self, lock: WriteLock, mut template: SnapshotMeta, base: Option<&SnapshotMeta>) -> Result<SnapshotWriter> {
        let metas = self.read_metas()?;
        if metas.iter().any(|m| m.name == template.name) {
            return Err(StoreError::DuplicateName(template.name));
        }
        template.id = self.next_id()?;
        template.status = Status::InProgress;
        template.storage = Default::default();
        template.storage.base = base.map(|b| b.id);
        if template.disk.is_none() {
            let s = Sighting::of_snapshot(&template);
            template.disk = Some(Registry::update(&self.root, |reg| Ok(reg.record(&s)))?);
        }
        template.root_in_volume = Some(template.derived_root_in_volume());
        let stem = self.new_stem(&template, &metas, true);
        let data_name = self.unused_name(match base {
            Some(b) => format!("{stem}.r{}.dds", stem_of(b)),
            None => format!("{stem}.dds"),
        });
        template.storage.stem = Some(stem);
        let tmp = self.snap_dir().join(format!("{data_name}.tmp"));
        self.write_meta(&template)?;
        let kind = if base.is_some() { FileKind::Ops } else { FileKind::Records };
        let data = match DataWriter::create(tmp.clone(), kind) {
            Ok(d) => d,
            Err(e) => {
                let err = io_err(&tmp, e);
                template.status = Status::Failed;
                template.error = Some(err.to_string());
                let _ = self.write_meta(&template);
                return Err(err);
            }
        };
        let encoder = match base {
            None => Encoder::Full(RecordEncoder::new()),
            Some(b) => Encoder::Delta { push: PushDelta::new(self.records_as(b.id, &template.root)?), ops: OpEncoderBlocks::new() },
        };
        Ok(SnapshotWriter {
            store: Store { root: self.root.clone() },
            lock: Some(std::sync::Arc::new(lock)),
            meta: template,
            data: Some(data),
            encoder,
            data_name,
            tmp,
            records: 0,
        })
    }

    /// Removes a snapshot. Snapshots stored as deltas against it are first
    /// rewritten against its own base (or as full snapshots), so no history is lost.
    /// Returns the bytes freed and the ids of the rewritten snapshots.
    pub fn remove(&self, lock: &WriteLock, id: u64) -> Result<(u64, Vec<u64>)> {
        let r = self.remove_many(lock, &[id], &AtomicBool::new(false))?;
        Ok((r.freed_bytes, r.rewritten))
    }

    /// What removing `ids` would do, without changing anything.
    pub fn plan_removal(&self, ids: &[u64]) -> Result<RemovalPlan> {
        let all = self.list()?;
        Self::plan_in(&all, ids, |m| self.snapshot_size(m))
    }

    fn plan_in(all: &[SnapshotMeta], ids: &[u64], size: impl Fn(&SnapshotMeta) -> u64) -> Result<RemovalPlan> {
        let by_id: std::collections::HashMap<u64, &SnapshotMeta> = all.iter().map(|m| (m.id, m)).collect();
        let gone: std::collections::BTreeSet<u64> = ids.iter().copied().collect();
        if let Some(id) = gone.iter().find(|id| !by_id.contains_key(id)) {
            return Err(StoreError::NotFound(id.to_string()));
        }
        // A kept snapshot whose base goes is rewritten once, against the first
        // kept snapshot down its chain (or as a full one): never once per removal.
        let mut rewrites = Vec::new();
        for m in all.iter().filter(|m| !gone.contains(&m.id) && m.status.is_usable()) {
            let (mut base, mut steps) = (m.storage.base, 0);
            while let Some(b) = base.filter(|b| gone.contains(b)) {
                base = by_id.get(&b).and_then(|x| x.storage.base);
                steps += 1;
                if steps > 1000 {
                    return Err(StoreError::Corrupt(format!("delta chain of snapshot {} is cyclic", m.id)));
                }
            }
            if base != m.storage.base {
                rewrites.push((m.id, base));
            }
        }
        let bytes = gone.iter().map(|id| size(by_id[id])).sum();
        Ok(RemovalPlan { removed: gone.into_iter().collect(), rewrites, bytes })
    }

    /// Removes the snapshots `ids` (see [`Store::plan_removal`]). Kept snapshots
    /// that depended on removed ones are rewritten first, each once; then the
    /// removed ones go, those nothing depends on first, metadata before data,
    /// so a crash at any point leaves every remaining snapshot readable (a data
    /// file left behind is removed when the lock is next taken). `cancel` is
    /// checked between rewrites: a stop leaves everything in place.
    pub fn remove_many(&self, _lock: &WriteLock, ids: &[u64], cancel: &AtomicBool) -> Result<Removal> {
        let before = self.size_on_disk();
        let all = self.list()?;
        let plan = Self::plan_in(&all, ids, |_| 0)?;
        let find = |id: u64| all.iter().find(|m| m.id == id).expect("planned from this list");
        let mut rewritten = Vec::new();
        for &(id, base) in &plan.rewrites {
            if cancel.load(Ordering::Relaxed) {
                return Ok(Removal { removed: Vec::new(), rewritten, freed_bytes: before.saturating_sub(self.size_on_disk()), cancelled: true });
            }
            let d = self.get(id)?;
            self.rewrite(&d, base.map(find))?;
            rewritten.push(id);
        }
        // Leaves first: a removed snapshot is deleted once no remaining one points at it.
        let mut left: Vec<SnapshotMeta> = plan.removed.iter().map(|&id| find(id).clone()).collect();
        while !left.is_empty() {
            let i = left.iter().position(|m| !left.iter().any(|x| x.storage.base == Some(m.id))).unwrap_or(0);
            let m = left.remove(i);
            let p = self.meta_path(&m);
            std::fs::remove_file(&p).at(&p)?;
            sync_dir(&self.snap_dir()).at(&self.snap_dir())?;
            if let Some(name) = &m.storage.data_file {
                let p = self.snap_dir().join(name);
                match std::fs::remove_file(&p) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(io_err(&p, e)),
                }
            }
        }
        sync_dir(&self.snap_dir()).at(&self.snap_dir())?;
        Ok(Removal { removed: plan.removed, rewritten, freed_bytes: before.saturating_sub(self.size_on_disk()), cancelled: false })
    }

    /// Rewrites `d` as a delta against `base`, or as a full snapshot. Returns
    /// the number of records `d` has.
    fn rewrite(&self, d: &SnapshotMeta, base: Option<&SnapshotMeta>) -> Result<u64> {
        let name = self.unused_name(match base {
            Some(b) => format!("{}.r{}.dds", stem_of(d), stem_of(b)),
            None => format!("{}.dds", stem_of(d)),
        });
        let dest = self.snap_dir().join(&name);
        let (items, blocks, records) = match base {
            Some(b) => self.write_delta_file(b, d, &dest)?,
            None => {
                let (items, blocks) = self.write_full_file(d.id, &dest)?;
                (items, blocks, items)
            }
        };
        self.switch_data(d.clone(), base.map(|b| b.id), name, items, blocks)?;
        Ok(records)
    }

    /// The layout the usable snapshots would have if they had been written one
    /// by one in order (see [`Store::rebase_previous`]): for each, the base it
    /// would be a delta against, or `None` for full. Records do not change
    /// with the layout, only how they are stored.
    fn written_layout(all: &[SnapshotMeta]) -> std::collections::BTreeMap<u64, Option<u64>> {
        let usable: Vec<&SnapshotMeta> = all.iter().filter(|m| m.status.is_usable()).collect();
        let mut target: std::collections::BTreeMap<u64, Option<u64>> = usable.iter().map(|m| (m.id, None)).collect();
        for (i, s) in usable.iter().enumerate() {
            let Some(prev) = usable[..i].iter().rev().find(|p| p.incompatibility(s).is_none()) else { continue };
            if target[&prev.id].is_some() {
                continue;
            }
            // Deepest chain ending at prev: what turning prev into a delta adds a step to.
            let depth_to = |mut id: u64| {
                let mut n = 0;
                while id != prev.id {
                    match target.get(&id).copied().flatten() {
                        Some(b) => {
                            id = b;
                            n += 1;
                        }
                        None => return None,
                    }
                }
                Some(n)
            };
            let deepest = target.keys().filter_map(|&id| depth_to(id)).max().unwrap_or(0);
            if deepest < MAX_CHAIN {
                target.insert(prev.id, Some(s.id));
            }
        }
        target
    }

    /// How many snapshots [`Store::compact`] would rewrite (reads metadata only).
    pub fn uncompacted(&self) -> Result<usize> {
        let all = self.list()?;
        let target = Self::written_layout(&all);
        Ok(all.iter().filter(|m| target.get(&m.id).is_some_and(|b| *b != m.storage.base)).count())
    }

    /// Rewrites every snapshot whose storage differs from what writing the
    /// scans one by one would have produced (after removals, a delta may span
    /// a long gap or a snapshot may have been made full), newest first, then
    /// checks the checksums of every data file and re-reads each rewritten
    /// snapshot. Every step leaves a readable store; `cancel` stops between
    /// snapshots. `progress` gets (done, total) as it goes.
    pub fn compact(&self, _lock: &WriteLock, cancel: &AtomicBool, progress: &mut dyn FnMut(CompactStep)) -> Result<Compaction> {
        let before = self.size_on_disk();
        let all = self.list()?;
        let target = Self::written_layout(&all);
        let todo: Vec<(u64, Option<u64>)> = target
            .iter()
            .rev()
            .filter(|(id, base)| all.iter().find(|m| m.id == **id).is_some_and(|m| m.storage.base != **base))
            .map(|(id, base)| (*id, *base))
            .collect();
        let mut out = Compaction { checked: 0, rewritten: Vec::new(), left: Vec::new(), bytes_before: before, bytes_after: before, cancelled: false };
        let mut counts = Vec::new();
        for (done, &(id, base)) in todo.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                out.cancelled = true;
                break;
            }
            progress(CompactStep::Rewriting { id, done, total: todo.len() });
            let m = self.get(id)?;
            let b = match base {
                Some(b) => Some(self.get(b)?),
                None => None,
            };
            let records = match &b {
                // A delta only when it pays off, as at write time; otherwise the
                // snapshot stays as it is (a newer base cannot form a cycle).
                Some(b) => match self.delta_if_smaller(&m, b)? {
                    Some(n) => n,
                    None if m.storage.base.is_none_or(|old| old > id) => {
                        out.left.push(id);
                        continue;
                    }
                    None => self.rewrite(&m, None)?,
                },
                None => self.rewrite(&m, None)?,
            };
            counts.push((id, records));
            out.rewritten.push(id);
        }
        if !out.cancelled {
            let metas: Vec<SnapshotMeta> = self.list()?.into_iter().filter(|m| m.status.is_usable()).collect();
            for (done, m) in metas.iter().enumerate() {
                if cancel.load(Ordering::Relaxed) {
                    out.cancelled = true;
                    break;
                }
                progress(CompactStep::Checking { id: m.id, done, total: metas.len() });
                let df = self.data_file(m)?;
                df.verify().map_err(|e| StoreError::Corrupt(format!("snapshot {}: {e}", m.id)))?;
                out.checked += 1;
            }
            for &(id, records) in &counts {
                let n = self.verify(id)?;
                if n != records {
                    return Err(StoreError::Corrupt(format!("snapshot {id} reads back {n} records after compaction, {records} before")));
                }
            }
        }
        out.bytes_after = self.size_on_disk();
        Ok(out)
    }

    /// Rewrites full or delta snapshot `m` as a delta against `base` unless the
    /// delta is larger than [`MAX_REVERSE_RATIO`] of a full copy (estimated
    /// from the full snapshot `base`'s chain ends at, scaled by entries).
    /// `Some(records)` when it was rewritten.
    fn delta_if_smaller(&self, m: &SnapshotMeta, base: &SnapshotMeta) -> Result<Option<u64>> {
        let name = self.unused_name(format!("{}.r{}.dds", stem_of(m), stem_of(base)));
        let dest = self.snap_dir().join(&name);
        let (items, blocks, records) = self.write_delta_file(base, m, &dest)?;
        let len = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let full_len = match m.storage.base {
            None => len(&self.snap_dir().join(m.storage.data_file.as_deref().unwrap_or(""))),
            Some(_) => {
                let all = self.list()?;
                let end = Self::chains(&all).get(&base.id).map(|(_, end)| *end).unwrap_or(base.id);
                let k = self.get(end)?;
                let entries = |x: &SnapshotMeta| x.counts.as_ref().map(|c| c.entries).filter(|n| *n > 0);
                let k_len = len(&self.snap_dir().join(k.storage.data_file.as_deref().unwrap_or("")));
                match (entries(m), entries(&k)) {
                    (Some(a), Some(b)) => (k_len as f64 * a as f64 / b as f64) as u64,
                    _ => k_len,
                }
            }
        };
        let delta_len = len(&dest);
        if full_len >= 1 << 20 && delta_len as f64 > full_len as f64 * MAX_REVERSE_RATIO {
            let _ = std::fs::remove_file(&dest);
            return Ok(None);
        }
        self.switch_data(m.clone(), Some(base.id), name, items, blocks)?;
        Ok(Some(records))
    }
}

/// What removing a set of snapshots does (see [`Store::plan_removal`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovalPlan {
    /// The snapshots removed, by id.
    pub removed: Vec<u64>,
    /// Kept snapshots rewritten so they stay readable: (id, new base; `None` = full).
    pub rewrites: Vec<(u64, Option<u64>)>,
    /// Allocated bytes of the removed snapshots' files. The rewritten ones can
    /// take some of it back.
    pub bytes: u64,
}

/// What [`Store::remove_many`] did.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Removal {
    pub removed: Vec<u64>,
    pub rewritten: Vec<u64>,
    /// How much smaller the store is, measured.
    pub freed_bytes: u64,
    /// Stopped before anything was removed (rewrites done so far are kept: they are valid).
    pub cancelled: bool,
}

/// Where compaction is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactStep {
    Rewriting { id: u64, done: usize, total: usize },
    Checking { id: u64, done: usize, total: usize },
}

/// What [`Store::compact`] did.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Compaction {
    /// Snapshots whose data files passed the checksum check.
    pub checked: usize,
    pub rewritten: Vec<u64>,
    /// Left as they were: a delta against the next scan would not pay off.
    pub left: Vec<u64>,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub cancelled: bool,
}

/// What happened to the previous snapshot after a new one was committed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Rebase {
    /// No earlier comparable full snapshot.
    Nothing,
    /// Rewritten as a reverse delta against the new snapshot.
    Converted { id: u64, freed_bytes: u64 },
    /// Left as a full snapshot.
    KeptFull { id: u64, reason: String },
}

enum Encoder {
    Full(RecordEncoder),
    Delta { push: PushDelta<'static>, ops: OpEncoderBlocks },
}

/// Writes one snapshot. Dropping it without `commit` aborts it as FAILED.
pub struct SnapshotWriter {
    store: Store,
    lock: Option<std::sync::Arc<WriteLock>>,
    meta: SnapshotMeta,
    data: Option<DataWriter>,
    encoder: Encoder,
    data_name: String,
    tmp: PathBuf,
    records: u64,
}

impl SnapshotWriter {
    /// A second handle on the writer lock, so it outlives `commit`.
    fn take_lock(&self) -> Option<std::sync::Arc<WriteLock>> {
        self.lock.clone()
    }

    pub fn id(&self) -> u64 {
        self.meta.id
    }

    pub fn meta(&self) -> &SnapshotMeta {
        &self.meta
    }

    pub fn is_delta(&self) -> bool {
        matches!(self.encoder, Encoder::Delta { .. })
    }

    pub fn push(&mut self, rec: &Record) -> Result<()> {
        let data = self.data.as_mut().expect("writer is open");
        let tmp = &self.tmp;
        self.records += 1;
        match &mut self.encoder {
            Encoder::Full(enc) => {
                if let Some(b) = enc.push(rec).at(tmp)? {
                    data.block(b).at(tmp)?;
                }
            }
            Encoder::Delta { push, ops } => {
                let mut sink = |op: &DeltaOp| -> io::Result<()> {
                    if let Some(b) = ops.push(op)? {
                        data.block(b)?;
                    }
                    Ok(())
                };
                push.push(rec, &mut sink).at(tmp)?;
            }
        }
        Ok(())
    }

    fn finish_data(&mut self) -> Result<u64> {
        let mut data = self.data.take().expect("writer is open");
        let tmp = self.tmp.clone();
        let r = (|| -> io::Result<u64> {
            match &mut self.encoder {
                Encoder::Full(enc) => {
                    if let Some(b) = enc.finish()? {
                        data.block(b)?;
                    }
                }
                Encoder::Delta { push, ops } => {
                    let mut sink = |op: &DeltaOp| -> io::Result<()> {
                        if let Some(b) = ops.push(op)? {
                            data.block(b)?;
                        }
                        Ok(())
                    };
                    push.finish(&mut sink)?;
                    if let Some(b) = ops.finish()? {
                        data.block(b)?;
                    }
                }
            }
            let blocks = data.blocks();
            let items = data.commit(&self.store.snap_dir().join(&self.data_name))?;
            self.meta.storage.blocks = blocks;
            Ok(items)
        })();
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        r.at(&tmp)
    }

    /// Finalizes the snapshot as COMPLETED or PARTIAL.
    pub fn commit(mut self, counts: WalkReport, space_after: Option<SpaceSample>, finished_at: String) -> Result<SnapshotMeta> {
        match self.finish_data() {
            Ok(items) => {
                self.meta.storage.items = items;
                self.meta.storage.data_file = Some(self.data_name.clone());
                self.meta.storage.bytes_on_disk = allocated(&self.store.snap_dir().join(&self.data_name));
                self.meta.status = if counts.is_partial() { Status::Partial } else { Status::Completed };
                self.meta.counts = Some(counts);
                self.meta.space_after = space_after;
                self.meta.finished_at = Some(finished_at);
                let meta = self.meta.clone();
                self.store.write_meta(&meta)?;
                self.data = None;
                Ok(meta)
            }
            Err(e) => {
                self.meta.status = Status::Failed;
                self.meta.error = Some(e.to_string());
                self.meta.counts = Some(counts);
                self.meta.finished_at = Some(finished_at);
                let _ = self.store.write_meta(&self.meta);
                Err(e)
            }
        }
    }

    /// Commits, then rewrites the previous comparable snapshot as a reverse delta
    /// (see [`Store::rebase_previous`]) while the writer lock is still held. A
    /// failed rebase leaves the previous snapshot full and is reported, not fatal.
    pub fn commit_and_rebase(
        self,
        counts: WalkReport,
        space_after: Option<SpaceSample>,
        finished_at: String,
    ) -> Result<(SnapshotMeta, std::result::Result<Rebase, String>)> {
        let store = Store { root: self.store.root.clone() };
        let lock = self.take_lock();
        let meta = self.commit(counts, space_after, finished_at)?;
        let rebase = match &lock {
            Some(l) => store.rebase_previous(l, &meta).map_err(|e| e.to_string()),
            None => Ok(Rebase::Nothing),
        };
        Ok((meta, rebase))
    }

    /// Ends the snapshot as FAILED or CANCELLED; its partial data is deleted.
    pub fn abort(mut self, status: Status, error: Option<String>, counts: Option<WalkReport>, finished_at: String) -> SnapshotMeta {
        if let Some(d) = self.data.take() {
            d.abort();
        }
        self.meta.status = status;
        self.meta.error = error;
        self.meta.counts = counts;
        self.meta.finished_at = Some(finished_at);
        let _ = self.store.write_meta(&self.meta);
        self.meta.clone()
    }
}

impl Drop for SnapshotWriter {
    fn drop(&mut self) {
        if let Some(d) = self.data.take() {
            d.abort();
            self.meta.status = Status::Failed;
            self.meta.error.get_or_insert_with(|| "writer dropped before commit".into());
            let _ = self.store.write_meta(&self.meta);
        }
    }
}

/// Keeps the contiguous run of records at or below `prefix`.
pub struct PrefixFilter {
    inner: RecordStream<'static>,
    prefix: Vec<Vec<u8>>,
    stack: PathStack,
    inside: bool,
    done: bool,
}

impl PrefixFilter {
    pub fn new(inner: RecordStream<'static>, prefix: Vec<Vec<u8>>, ctx: &[Vec<u8>]) -> Self {
        let mut stack = PathStack::new();
        for (d, c) in ctx.iter().enumerate() {
            let _ = stack.set(d as u16, c);
        }
        PrefixFilter { inner, prefix, stack, inside: false, done: false }
    }
}

impl Iterator for PrefixFilter {
    type Item = io::Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            let rec = match self.inner.next()? {
                Ok(r) => r,
                Err(e) => return Some(Err(e)),
            };
            let Some(name) = rec.name.as_unix_bytes() else {
                return Some(Err(io::Error::new(io::ErrorKind::InvalidData, "Windows names are not valid in DDS1 prefix queries")));
            };
            if let Err(e) = self.stack.set(rec.depth, name) {
                return Some(Err(e));
            }
            if self.stack.starts_with(&self.prefix) {
                self.inside = true;
                return Some(Ok(rec));
            }
            if self.inside {
                self.done = true;
                return None;
            }
        }
    }
}
