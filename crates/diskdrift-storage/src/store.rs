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
use crate::descriptor::StreamDescriptor;
use crate::disks::{Registry, Sighting};
use crate::fsops;
use crate::meta::{AllocationSemantics, Provenance};
use crate::meta::{
    LineageCompatibility, MetadataV3, RootValue, SnapshotMeta, SpaceSample, Status,
    VolumeDescriptor,
};
use diskdrift_core::delta::{Apply, DeltaBuilder, DeltaOp, PushDelta};
use diskdrift_core::diff::{RecordStream, with_root_name};
use diskdrift_core::native::{NativeName, NativePath};
use diskdrift_core::paths::{PathOrderError, PathStack, compare_paths};
use diskdrift_core::record::FileIdScope;
use diskdrift_core::record::{EntryFields, FileIdScheme, Record, TimestampResolution};
use diskdrift_core::walk::WalkReport;
#[cfg(test)]
use std::cell::Cell;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

pub const STORE_FORMAT: &str = "diskdrift-snapshots";
/// 2: files named after the scan start time instead of the id.
/// 3: the disk ID in front of the time; snapshots record their disk (ADR 0002).
/// 4: the validated metadata-v3/DDS2 namespace.
pub const STORE_VERSION: u32 = 4;
pub const LEGACY_STORE_VERSION: u32 = 3;

#[cfg(test)]
thread_local! {
    static STREAM_INVARIANT_VALIDATIONS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
fn reset_stream_invariant_validations() {
    STREAM_INVARIANT_VALIDATIONS.with(|count| count.set(0));
}

#[cfg(test)]
fn stream_invariant_validations() -> usize {
    STREAM_INVARIANT_VALIDATIONS.with(Cell::get)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoreMarker {
    pub format: String,
    pub version: u32,
    pub metadata_dir: String,
    pub registry: String,
}

impl StoreMarker {
    pub fn v4() -> Self {
        Self {
            format: STORE_FORMAT.into(),
            version: STORE_VERSION,
            metadata_dir: "snapshots-v4".into(),
            registry: "disks-v2.json".into(),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.format != STORE_FORMAT {
            return Err(StoreError::NotAStore(PathBuf::from("store.json")));
        }
        if self.version != STORE_VERSION {
            return Err(StoreError::UnsupportedVersion {
                found: u64::from(self.version),
            });
        }
        Ok(())
    }
}

/// All on-disk names selected by the store marker. v3 keeps its historical
/// namespace; v4 uses store-relative metadata and data references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreLayout {
    root: PathBuf,
    version: u32,
    metadata_relative: PathBuf,
    registry_relative: PathBuf,
}

impl StoreLayout {
    fn legacy(root: &Path, version: u32) -> Self {
        Self {
            root: root.to_path_buf(),
            version,
            metadata_relative: PathBuf::from("snapshots"),
            registry_relative: PathBuf::from("disks.json"),
        }
    }

    pub fn v4(root: &Path, marker: &StoreMarker) -> Result<Self> {
        marker.validate()?;
        let metadata_relative =
            validate_relative_namespace(&marker.metadata_dir, "metadataDir", false)?;
        let registry_relative = validate_relative_namespace(&marker.registry, "registry", true)?;
        if is_reserved_store_namespace(&metadata_relative)
            || is_reserved_store_namespace(&registry_relative)
        {
            return Err(StoreError::Invalid(
                "v4 marker namespaces collide with DiskDrift control files".into(),
            ));
        }
        if metadata_relative == registry_relative
            || metadata_relative == Path::new("snapshots")
            || registry_relative == Path::new("disks.json")
            || registry_relative.starts_with(&metadata_relative)
        {
            return Err(StoreError::Invalid(
                "v4 marker namespaces overlap a legacy or metadata namespace".into(),
            ));
        }
        Ok(Self {
            root: root.to_path_buf(),
            version: STORE_VERSION,
            metadata_relative,
            registry_relative,
        })
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn is_v4(&self) -> bool {
        self.version == STORE_VERSION
    }

    pub fn metadata_dir(&self) -> PathBuf {
        self.root.join(&self.metadata_relative)
    }

    pub fn registry_path(&self) -> PathBuf {
        self.root.join(&self.registry_relative)
    }

    pub fn registry_lock_path(&self) -> PathBuf {
        self.root.join("disks.lock")
    }

    pub fn metadata_relative(&self) -> &Path {
        &self.metadata_relative
    }

    pub fn registry_relative(&self) -> &Path {
        &self.registry_relative
    }

    fn metadata_path(&self, stem: &str) -> PathBuf {
        self.metadata_dir().join(format!("{stem}.json"))
    }

    fn data_reference(&self, file_name: &str) -> Result<String> {
        if self.is_v4() {
            let relative = validate_relative_namespace(file_name, "dataFile", true)?;
            Ok(join_store_path(&self.metadata_relative, &relative))
        } else {
            validate_relative_namespace(file_name, "dataFile", true)?;
            Ok(file_name.into())
        }
    }

    pub fn data_path(&self, data_file: &str) -> Result<PathBuf> {
        if self.is_v4() {
            let relative = validate_relative_namespace(data_file, "dataFile", true)?;
            Ok(self.root.join(relative))
        } else {
            let relative = validate_relative_namespace(data_file, "dataFile", true)?;
            Ok(self.metadata_dir().join(relative))
        }
    }

    fn data_path_for_name(&self, file_name: &str) -> Result<PathBuf> {
        Ok(self
            .metadata_dir()
            .join(validate_relative_namespace(file_name, "dataFile", true)?))
    }

    fn marker(&self) -> Option<StoreMarker> {
        self.is_v4().then(|| StoreMarker {
            format: STORE_FORMAT.into(),
            version: STORE_VERSION,
            metadata_dir: self.metadata_relative.to_string_lossy().into_owned(),
            registry: self.registry_relative.to_string_lossy().into_owned(),
        })
    }
}

fn is_reserved_store_namespace(path: &Path) -> bool {
    path == Path::new("store.json")
        || path == Path::new("lock")
        || path == Path::new("disks.lock")
        || path == Path::new("disks.json")
}

fn validate_relative_namespace(value: &str, field: &str, allow_nested: bool) -> Result<PathBuf> {
    if value.is_empty()
        || value.starts_with('/')
        || value.contains('\\')
        || value.contains(':')
        || value.chars().any(char::is_control)
    {
        return Err(StoreError::Invalid(format!(
            "v4 {field} must be a normalized store-relative path"
        )));
    }
    let mut out = PathBuf::new();
    for component in value.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(StoreError::Invalid(format!(
                "v4 {field} must be a normalized store-relative path"
            )));
        }
        if !allow_nested && out.components().next().is_some() {
            return Err(StoreError::Invalid(format!(
                "v4 {field} must name one metadata directory"
            )));
        }
        out.push(component);
    }
    if out.as_os_str().is_empty() {
        return Err(StoreError::Invalid(format!("v4 {field} must not be empty")));
    }
    Ok(out)
}

fn join_store_path(parent: &Path, child: &Path) -> String {
    parent
        .components()
        .chain(child.components())
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

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
    UnsupportedVersion { found: u64 },
    #[error("filesystem at {path} does not support a required DiskDrift operation: {source}")]
    UnsupportedFilesystem { path: PathBuf, source: io::Error },
    #[error("another scan is writing to this store (pid {pid}); wait for it to finish")]
    Locked { pid: String },
    #[error(
        "writer lock belongs to store {actual}, not {expected}",
        actual = actual.display(),
        expected = expected.display()
    )]
    LockMismatch { expected: PathBuf, actual: PathBuf },
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
    #[error("incompatible canonical path encodings: {0}")]
    PathOrder(#[from] PathOrderError),
    #[error(
        "disk registry format version {found} is newer than this build supports ({})",
        crate::disks::DISKS_VERSION_V2
    )]
    UnsupportedDisksVersion { found: u64 },
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
    let unix_full = matches!(
        e.raw_os_error(),
        Some(libc::ENOSPC) | Some(libc::EDQUOT) | Some(libc::EFBIG)
    );
    #[cfg(not(unix))]
    let unix_full = false;
    unix_full || e.kind() == io::ErrorKind::StorageFull
}

pub(crate) fn io_err(path: &Path, source: io::Error) -> StoreError {
    if source.kind() == io::ErrorKind::Unsupported {
        return StoreError::UnsupportedFilesystem {
            path: path.to_path_buf(),
            source,
        };
    }
    if source.kind() == io::ErrorKind::InvalidData {
        return StoreError::Corrupt(source.to_string());
    }
    if is_disk_full(&source) {
        return StoreError::DiskFull {
            path: path.to_path_buf(),
            source,
        };
    }
    StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
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
    layout: RwLock<StoreLayout>,
}

/// Exclusive writer lock (released when dropped or when the process dies).
pub struct WriteLock {
    store_root: PathBuf,
    _lock: fsops::Lock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum MigrationOutcome {
    None,
    LegacyStoreReady,
    V4Active,
}

impl MigrationOutcome {
    /// Human-readable notice for mutation flows. Read-only flows never acquire
    /// a lock and therefore never produce one.
    pub fn notice(self) -> Option<&'static str> {
        match self {
            MigrationOutcome::None => None,
            MigrationOutcome::LegacyStoreReady => Some(
                "DiskDrift upgraded the legacy snapshot layout; older store metadata may be rewritten on this write.",
            ),
            MigrationOutcome::V4Active => Some(
                "DiskDrift store format v4 is active; older DiskDrift versions cannot read this store.",
            ),
        }
    }
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    {
        fsops::failure_step()?;
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        fsops::flush_file(&f)?;
    }
    fsops::replace_atomic(&tmp, path)?;
    fsops::sync_parent(path)
}

/// How a snapshot root may be used while reconstructing its records.
///
/// Current metadata only has the legacy display string. It is deliberately not
/// treated as an exact comparison key; the decoded root record is renamed to
/// the requested exact target instead.
fn rooted_at(
    stream: RecordStream<'static>,
    from: RootValue,
    to: &NativePath,
) -> RecordStream<'static> {
    match from {
        RootValue::Exact(root) if root == *to => stream,
        RootValue::Exact(_) | RootValue::LegacyDisplayOnly(_) => with_root_name(stream, to.clone()),
    }
}

fn stored_root(meta: &SnapshotMeta) -> RootValue {
    meta.root_value()
}

fn root_from_records(stream: &mut RecordStream<'static>) -> Result<Option<NativePath>> {
    let Some(first) = stream
        .next()
        .transpose()
        .map_err(|e| StoreError::Corrupt(e.to_string()))?
    else {
        return Ok(None);
    };
    if first.depth != 0 {
        return Err(StoreError::Corrupt(
            "snapshot does not start with a root record".into(),
        ));
    }
    Ok(Some(match first.name {
        NativeName::Unix(bytes) => NativePath::unix(bytes),
        NativeName::Windows(units) => NativePath::windows(units),
    }))
}

fn exact_root(store: &Store, meta: &SnapshotMeta) -> Result<NativePath> {
    if let RootValue::Exact(root) = meta.root_value() {
        return Ok(root);
    }
    let mut records = store.records(meta.id)?;
    root_from_records(&mut records)?.ok_or_else(|| {
        StoreError::Invalid(format!(
            "snapshot {} has only a legacy display-only root and no root record",
            meta.id
        ))
    })
}

fn legacy_volume_descriptor(meta: &SnapshotMeta) -> VolumeDescriptor {
    let mut descriptor = VolumeDescriptor::new(
        0,
        "unix",
        FileIdScheme::PosixInode64,
        FileIdScope::Volume,
        AllocationSemantics::FilesystemReported,
        TimestampResolution::Seconds,
        Provenance::Unknown,
    );
    let identity = meta
        .volume
        .uuid
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or(&meta.volume.mount_point);
    descriptor.identity = NativePath::unix(identity.as_bytes().to_vec());
    descriptor.identity_strength = "weak".into();
    descriptor.mount_key = NativePath::unix(meta.volume.mount_point.as_bytes().to_vec());
    descriptor
}

#[cfg(test)]
mod root_tests {
    use super::*;
    use diskdrift_core::record::{Kind, Record};

    #[test]
    fn legacy_display_root_never_shortcuts_exact_replacement() {
        let stream: RecordStream<'static> = Box::new(std::iter::once(Ok(Record {
            depth: 0,
            name: NativeName::unix(b"/decoded-root".to_vec()),
            kind: Kind::Dir,
            ..Default::default()
        })));
        let target = NativePath::unix(b"/target-root".to_vec());
        let records: Vec<_> = rooted_at(
            stream,
            RootValue::LegacyDisplayOnly("/target-root".into()),
            &target,
        )
        .collect::<io::Result<_>>()
        .unwrap();
        assert_eq!(records[0].name, NativeName::unix(b"/target-root".to_vec()));
    }
}

/// File-name stem of a snapshot (the zero-padded id in version 1 stores).
fn stem_of(m: &SnapshotMeta) -> String {
    m.storage
        .stem
        .clone()
        .unwrap_or_else(|| format!("{:08}", m.id))
}

/// The snapshot is stored as version 3 names it: it has a disk, and its stem
/// starts with it.
fn is_current(m: &SnapshotMeta) -> bool {
    match (&m.disk, &m.storage.stem) {
        (Some(d), Some(s)) => s
            .strip_prefix(d.as_str())
            .is_some_and(|rest| rest.starts_with('-')),
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

impl Store {
    pub fn exists(path: &Path) -> bool {
        path.join("store.json").is_file()
    }

    /// Opens an existing store (never creates anything).
    pub fn open(path: &Path) -> Result<Store> {
        let marker = path.join("store.json");
        let text = match std::fs::read_to_string(&marker) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::NotAStore(path.to_path_buf()));
            }
            Err(e) => return Err(io_err(&marker, e)),
        };
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|_| StoreError::NotAStore(path.to_path_buf()))?;
        if v["format"] != STORE_FORMAT {
            return Err(StoreError::NotAStore(path.to_path_buf()));
        }
        let found = v["version"].as_u64().ok_or_else(|| {
            StoreError::Invalid(
                "store marker version must be an integer exactly representable as JSON integer"
                    .into(),
            )
        })?;
        let layout = if found == u64::from(STORE_VERSION) {
            let marker: StoreMarker = serde_json::from_value(v)
                .map_err(|e| StoreError::Invalid(format!("invalid v4 store marker: {e}")))?;
            StoreLayout::v4(path, &marker)?
        } else if (1..=u64::from(LEGACY_STORE_VERSION)).contains(&found) {
            StoreLayout::legacy(path, found as u32)
        } else {
            if found > u64::from(STORE_VERSION) {
                return Err(StoreError::UnsupportedVersion { found });
            }
            return Err(StoreError::Invalid(format!(
                "unsupported legacy store marker version {found}"
            )));
        };
        if layout.is_v4() {
            let metadata_dir = layout.metadata_dir();
            if !metadata_dir.is_dir() {
                return Err(StoreError::Corrupt(format!(
                    "v4 metadata directory is missing: {}",
                    metadata_dir.display()
                )));
            }
        }
        let store = Store {
            root: path.to_path_buf(),
            layout: RwLock::new(layout),
        };
        if store.layout().is_v4() {
            store.read_entries()?;
            if store.layout().registry_path().exists() {
                crate::disks::Registry::load_v2_at(&store.layout().registry_path())?;
            }
        }
        Ok(store)
    }

    /// Opens the store, creating it (user-only permissions) if needed.
    pub fn create(path: &Path) -> Result<Store> {
        if Self::exists(path) {
            return Self::open(path);
        }
        if path.exists() && std::fs::read_dir(path).at(path)?.next().is_some() {
            return Err(StoreError::NotAStore(path.to_path_buf()));
        }
        fsops::create_private_dir(path).at(path)?;
        let layout = StoreLayout::v4(path, &StoreMarker::v4())?;
        fsops::create_private_dir(&layout.metadata_dir()).at(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).at(path)?;
        }
        let marker = serde_json::to_value(
            layout
                .marker()
                .expect("new stores always use the v4 layout"),
        )
        .expect("store marker serializes");
        write_atomic(&path.join("store.json"), marker.to_string().as_bytes()).at(path)?;
        Ok(Store {
            root: path.to_path_buf(),
            layout: RwLock::new(layout),
        })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn layout(&self) -> StoreLayout {
        self.layout
            .read()
            .expect("store layout lock poisoned")
            .clone()
    }

    fn set_layout(&self, layout: StoreLayout) {
        *self.layout.write().expect("store layout lock poisoned") = layout;
    }

    fn snap_dir(&self) -> PathBuf {
        self.layout().metadata_dir()
    }

    fn meta_path(&self, m: &SnapshotMeta) -> PathBuf {
        self.layout().metadata_path(&stem_of(m))
    }

    /// `name` (`<stem>.<rest>`), or `<stem>-N.<rest>` if that file already exists.
    fn unused_name(&self, name: String) -> String {
        if !self.layout().metadata_dir().join(&name).exists() {
            return name;
        }
        let (stem, rest) = name.split_once('.').unwrap_or((&name, ""));
        (2..)
            .map(|n| format!("{stem}-{n}.{rest}"))
            .find(|c| !self.layout().metadata_dir().join(c).exists())
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
            metas
                .iter()
                .any(|x| x.id != m.id && x.storage.stem.as_deref() == Some(s))
                || (check_files
                    && self
                        .layout()
                        .metadata_dir()
                        .join(format!("{s}.json"))
                        .exists())
        };
        if !taken(&base) {
            return base;
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|s| !taken(s))
            .expect("unbounded")
    }

    fn write_meta_to_layout(&self, layout: &StoreLayout, m: &SnapshotMeta) -> Result<()> {
        m.validate_v4()
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        if let Some(data_file) = &m.storage.data_file {
            layout.data_path(data_file)?;
        }
        let path = layout.metadata_path(&stem_of(m));
        let bytes = serde_json::to_vec_pretty(m).expect("serializable");
        write_atomic(&path, &bytes).at(&path)
    }

    fn migrate_v4(&self, writer: &WriteLock) -> Result<bool> {
        self.validate_lock(writer)?;
        let legacy = self.layout();
        if legacy.is_v4() {
            return Ok(false);
        }
        let v4 = StoreLayout::v4(&self.root, &StoreMarker::v4())?;
        let entries = self.read_entries()?;
        let mut primary = Vec::new();
        for (_, meta) in entries {
            if primary
                .last()
                .is_none_or(|previous: &SnapshotMeta| previous.id != meta.id)
            {
                primary.push(meta);
            }
        }

        // The legacy view is input only. Registry changes are applied to this
        // in-memory copy and published into the new namespace below.
        let mut registry = self.registry()?;
        let mut disks = Vec::with_capacity(primary.len());
        for meta in &primary {
            let disk = match (&meta.disk, is_current(meta)) {
                (Some(id), true) if registry.get(id).is_some() => Some(id.clone()),
                _ => Some(registry.record(&Sighting::of_snapshot(meta))),
            };
            disks.push(disk);
        }

        let mut target = primary.clone();
        for (meta, disk) in target.iter_mut().zip(&disks) {
            meta.format_version = crate::meta::META_FORMAT_VERSION_V3;
            if meta.root_native.is_none()
                && let Ok(mut records) = self.records(meta.id)
                && let Ok(Some(root)) = root_from_records(&mut records)
                && let NativePath::Unix(path) = root
            {
                meta.root_native = Some(diskdrift_core::native::NativeRoot::posix(path));
            }
            if meta.volume.descriptor.is_none() {
                let descriptor = legacy_volume_descriptor(meta);
                meta.volume.descriptor = Some(descriptor.clone());
                meta.volumes = crate::meta::complete_volume_table(&descriptor);
            } else if meta.volumes.is_empty()
                && let Some(descriptor) = &meta.volume.descriptor
            {
                meta.volumes = crate::meta::complete_volume_table(descriptor);
            }
            if meta.root_in_volume_native.is_none()
                && let (Some(root), Some(descriptor)) = (&meta.root_native, &meta.volume.descriptor)
            {
                meta.root_in_volume_native =
                    crate::meta::root_in_volume_native(&root.path, &descriptor.mount_key);
            }
            if let Some(disk) = disk {
                meta.disk = Some(disk.clone());
                meta.root_in_volume = Some(meta.derived_root_in_volume());
            }
            if meta.status == Status::InProgress {
                meta.status = Status::Failed;
                meta.error.get_or_insert_with(|| {
                    "interrupted (process ended before the scan finished)".into()
                });
            }
            if meta.storage.stem.is_none() || !is_current(meta) {
                meta.storage.stem = None;
            }
        }
        for index in 0..target.len() {
            if target[index].storage.stem.is_none() {
                let stem = target[index].started_at.as_str();
                let stem = time_stem(stem).unwrap_or_else(|| format!("{:08}", target[index].id));
                let disk = target[index].disk.as_deref().unwrap_or("legacy");
                let base = format!("{disk}-{stem}");
                let mut candidate = base.clone();
                let mut suffix = 2;
                while target[..index]
                    .iter()
                    .any(|meta| meta.storage.stem.as_deref() == Some(candidate.as_str()))
                {
                    candidate = format!("{base}-{suffix}");
                    suffix += 1;
                }
                target[index].storage.stem = Some(candidate);
            }
        }

        // Any prior uncommitted v4 namespace belongs to this migration attempt.
        // It is safe to discard because the legacy marker is still authoritative.
        if v4.metadata_dir().exists() {
            std::fs::remove_dir_all(v4.metadata_dir()).at(&v4.metadata_dir())?;
        }
        fsops::create_private_dir(&v4.metadata_dir()).at(&v4.metadata_dir())?;
        migration_step()?;

        for (old, meta) in primary.iter().zip(target.iter_mut()) {
            if let Some(data_file) = old.storage.data_file.as_deref() {
                let relative = validate_relative_namespace(data_file, "dataFile", true)?;
                let old_path = legacy.data_path(data_file)?;
                if !old_path.starts_with(&self.root) {
                    return Err(StoreError::Invalid(
                        "legacy data path escaped the store".into(),
                    ));
                }
                meta.storage.data_file =
                    Some(join_store_path(legacy.metadata_relative(), &relative));
            }
            self.write_meta_to_layout(&v4, meta)?;
            migration_step()?;
        }
        fsops::sync_dir(&v4.metadata_dir()).at(&v4.metadata_dir())?;
        migration_step()?;

        let registry_path = v4.registry_path();
        if let Some(parent) = registry_path.parent() {
            fsops::create_private_dir(parent).at(parent)?;
        }
        let registry_lock = crate::disks::lock_at(&v4.registry_lock_path())?;
        registry.save_v2_at(&registry_path, &self.root, &registry_lock)?;
        drop(registry_lock);
        migration_step()?;

        // Validate the staged view before making the marker visible.
        let staged = Store {
            root: self.root.clone(),
            layout: RwLock::new(v4.clone()),
        };
        staged.read_entries()?;
        crate::disks::Registry::load_v2_at(&registry_path)?;
        migration_step()?;

        let marker = self.root.join("store.json");
        let bytes = serde_json::to_vec(&StoreMarker::v4()).expect("store marker serializes");
        if let Err(error) = write_atomic(&marker, &bytes) {
            // `replace_atomic` is the publication point. A later directory
            // durability failure leaves the v4 marker live even though this
            // call returns an error; keep this handle aligned with that
            // committed view so it cannot read the legacy namespace next.
            if std::fs::read(&marker)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<StoreMarker>(&bytes).ok())
                .and_then(|marker| StoreLayout::v4(&self.root, &marker).ok())
                .is_some_and(|layout| layout == v4)
            {
                self.set_layout(v4.clone());
            }
            return Err(io_err(&marker, error));
        }
        self.set_layout(v4);
        Ok(true)
    }

    fn migrate(&self, writer: &WriteLock) -> Result<bool> {
        self.migrate_v4(writer)
    }

    /// Brings the store to the current version: records every snapshot's disk
    /// in the registry and in its metadata, and renames its files to
    /// `<disk>-<time>` (version 1 and 2 stores; ADR 0002, 2.8). Order: the
    /// registry first, then per snapshot a hard link of the data to its new
    /// name, the new metadata, and the removal of the old names; the version
    /// marker last. An interruption at any step leaves a store that reads the
    /// same, and the next run finishes the job.
    #[cfg(test)]
    #[allow(dead_code)]
    fn migrate_legacy_v3(&self, writer: &WriteLock) -> Result<bool> {
        self.validate_lock(writer)?;
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
            self.write_version()?;
            return Ok(false);
        }
        let dir = self.snap_dir();
        // 1. The registry, in scan order, so "first seen" and "last seen" are right.
        let disks: Vec<Option<String>> = self.update_registry(writer, |reg| {
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
                if t.volumes.is_empty()
                    && let Some(descriptor) = &t.volume.descriptor
                {
                    t.volumes.push(descriptor.clone());
                }
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
                let name = match m
                    .storage
                    .base
                    .and_then(|b| target.iter().find(|x| x.id == b))
                {
                    Some(base) => format!("{}.r{}.dds", stem_of(&m), stem_of(base)),
                    None => format!("{}.dds", stem_of(&m)),
                };
                let (from, to) = (dir.join(old_data), dir.join(&name));
                if from.exists() && !fsops::same_file(&from, &to) {
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
        fsops::sync_dir(&dir).at(&dir)?;
        self.write_version()?;
        Ok(true)
    }

    /// Lock the registry, apply `f`, save. When `disks.json` does not exist yet,
    /// `f` starts from the registry derived from the metadata, so the disks it
    /// changes keep the IDs the snapshots already show.
    pub fn update_registry<T>(
        &self,
        writer: &WriteLock,
        f: impl FnOnce(&mut Registry) -> Result<T>,
    ) -> Result<T> {
        self.validate_lock(writer)?;
        let layout = self.layout();
        let lock = crate::disks::lock_at(&layout.registry_lock_path())?;
        let mut reg = self.registry()?;
        let out = f(&mut reg)?;
        if layout.is_v4() {
            let path = layout.registry_path();
            if let Some(parent) = path.parent() {
                fsops::create_private_dir(parent).at(parent)?;
            }
            reg.save_v2_at(&path, &self.root, &lock)?;
        } else {
            reg.save(&self.root, &lock)?;
        }
        Ok(out)
    }

    /// Joins the history of disk `from` to disk `into` (ADR 0002: one disk
    /// recognised as two, for example a FAT stick whose old scans have only a
    /// weak identity). The snapshots of `from` are re-pointed and renamed,
    /// `into` keeps its label (or takes `from`'s), gets both sets, and `from`
    /// leaves the registry. Returns the snapshots moved.
    pub fn merge_disks(&self, lock: &WriteLock, from: &str, into: &str) -> Result<Vec<u64>> {
        self.validate_lock(lock)?;
        if from == into {
            return Err(StoreError::Invalid(format!(
                "disk {from} cannot be merged into itself"
            )));
        }
        let reg = self.registry()?;
        for id in [from, into] {
            if reg.get(id).is_none() {
                return Err(StoreError::UnknownDisk(id.into()));
            }
        }
        let mut moved = Vec::new();
        for mut m in self
            .read_metas()?
            .into_iter()
            .filter(|m| m.disk.as_deref() == Some(from))
        {
            m.disk = Some(into.to_string());
            self.write_meta(&m)?;
            moved.push(m.id);
        }
        self.update_registry(lock, |reg| {
            let gone = reg.forget(from)?;
            let d = reg
                .disks
                .iter_mut()
                .find(|d| d.id == into)
                .ok_or_else(|| StoreError::UnknownDisk(into.into()))?;
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
        if !self.layout().is_v4() {
            self.migrate(lock)?;
        }
        Ok(moved)
    }

    /// The disk registry. When `disks.json` does not exist yet (a version 2
    /// store, or no scan has written it), it is derived in memory from the
    /// snapshots' metadata; nothing is written.
    pub fn registry(&self) -> Result<Registry> {
        let layout = self.layout();
        if layout.registry_path().exists() {
            return if layout.is_v4() {
                Registry::load_v2_at(&layout.registry_path())
            } else {
                Registry::load_at(&layout.registry_path())
            };
        }
        let mut reg = Registry::default();
        for m in self.read_entries()?.iter().map(|(_, m)| m) {
            reg.record(&Sighting::of_snapshot(m));
        }
        Ok(reg)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    fn write_version(&self) -> Result<()> {
        let marker = self.root.join("store.json");
        let text = std::fs::read_to_string(&marker).at(&marker)?;
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|_| StoreError::NotAStore(self.root.clone()))?;
        if v["version"].as_u64() == Some(LEGACY_STORE_VERSION as u64) {
            return Ok(());
        }
        let marker_json = serde_json::json!({
            "format": STORE_FORMAT,
            "version": LEGACY_STORE_VERSION
        });
        write_atomic(&marker, marker_json.to_string().as_bytes()).at(&marker)
    }

    fn validate_lock(&self, lock: &WriteLock) -> Result<()> {
        if self.root != lock.store_root {
            return Err(StoreError::LockMismatch {
                expected: self.root.clone(),
                actual: lock.store_root.clone(),
            });
        }
        Ok(())
    }

    /// Removes only stale files in the legacy snapshot namespace after the v4
    /// marker and metadata view have been committed. Root control files and
    /// referenced legacy data remain available for mixed DDS1/DDS2 history.
    fn cleanup_legacy_namespace(&self, writer: &WriteLock) -> Result<()> {
        self.validate_lock(writer)?;
        if !self.layout().is_v4() {
            return Ok(());
        }
        let legacy_dir = self.root.join("snapshots");
        let metadata = match std::fs::symlink_metadata(&legacy_dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_err(&legacy_dir, error)),
        };
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Ok(());
        }

        let referenced: std::collections::HashSet<PathBuf> = self
            .read_entries()?
            .into_iter()
            .filter_map(|(_, meta)| meta.storage.data_file)
            .filter_map(|name| self.layout().data_path(&name).ok())
            .filter(|path| path.starts_with(&legacy_dir))
            .collect();
        let mut parents = std::collections::BTreeSet::new();
        let mut pending = vec![legacy_dir.clone()];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(&directory).at(&directory)? {
                let path = entry.at(&directory)?.path();
                if !path.starts_with(&legacy_dir) {
                    return Err(StoreError::Invalid(
                        "legacy cleanup encountered a path outside the store".into(),
                    ));
                }
                let metadata = std::fs::symlink_metadata(&path).at(&path)?;
                let file_type = metadata.file_type();
                if file_type.is_symlink() {
                    continue;
                }
                if file_type.is_dir() {
                    pending.push(path);
                } else if !referenced.contains(&path) {
                    std::fs::remove_file(&path).at(&path)?;
                    if let Some(parent) = path.parent() {
                        parents.insert(parent.to_path_buf());
                    }
                }
            }
        }
        for parent in parents {
            fsops::sync_dir(&parent).at(&parent)?;
        }
        Ok(())
    }

    fn try_lock(&self) -> Result<Option<WriteLock>> {
        let p = self.root.join("lock");
        let Some(mut lock) = fsops::try_lock_exclusive(&p).at(&p)? else {
            return Ok(None);
        };
        lock.write_owner(&std::process::id().to_string()).at(&p)?;
        Ok(Some(WriteLock {
            store_root: self.root.clone(),
            _lock: lock,
        }))
    }

    fn validate_live_marker(&self) -> Result<()> {
        let path = self.root.join("store.json");
        let text = std::fs::read_to_string(&path).at(&path)?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| StoreError::Invalid(format!("invalid store marker: {error}")))?;
        if value["format"] != STORE_FORMAT {
            return Err(StoreError::Invalid("store marker format changed".into()));
        }
        let found = value["version"]
            .as_u64()
            .ok_or_else(|| StoreError::Invalid("store marker version must be an integer".into()))?;
        let layout = self.layout();
        if layout.is_v4() {
            if found != u64::from(STORE_VERSION) {
                return Err(StoreError::Invalid(
                    "store marker changed from the opened v4 layout".into(),
                ));
            }
            let marker: StoreMarker = serde_json::from_value(value).map_err(|error| {
                StoreError::Invalid(format!("invalid v4 store marker: {error}"))
            })?;
            let layout = StoreLayout::v4(&self.root, &marker)?;
            if layout != self.layout() {
                return Err(StoreError::Invalid(
                    "store marker changed from the opened v4 layout".into(),
                ));
            }
        } else if found < u64::from(self.layout().version)
            || found > u64::from(LEGACY_STORE_VERSION)
        {
            return Err(StoreError::Invalid(
                "store marker changed from the opened legacy layout".into(),
            ));
        }
        Ok(())
    }

    /// True while another process holds the writer lock.
    pub fn is_busy(&self) -> bool {
        matches!(fsops::probe_lock(&self.root.join("lock")), Ok(true))
    }

    /// Takes the writer lock and repairs the state left by an interrupted writer.
    pub fn lock(&self) -> Result<(WriteLock, MigrationOutcome)> {
        self.lock_with_outcome()
    }

    /// Takes the writer lock and reports which format view is active. v4
    /// activation is intentionally unavailable in default builds.
    pub fn lock_with_outcome(&self) -> Result<(WriteLock, MigrationOutcome)> {
        let Some(lock) = self.try_lock()? else {
            let pid = std::fs::read_to_string(self.root.join("lock")).unwrap_or_default();
            return Err(StoreError::Locked {
                pid: pid.trim().to_string(),
            });
        };
        if let Err(error) = self.validate_live_marker() {
            drop(lock);
            return Err(error);
        }
        let migrated = if !self.layout().is_v4() {
            self.migrate(&lock)?
        } else {
            false
        };
        // Recovery happens only after migration has published its new view. A
        // legacy IN_PROGRESS document is copied as FAILED into v4 staging; it
        // is never rewritten in the legacy namespace.
        for m in self.read_metas()? {
            if m.status == Status::InProgress {
                let mut m = m;
                m.status = Status::Failed;
                m.error.get_or_insert_with(|| {
                    "interrupted (process ended before the scan finished)".into()
                });
                self.write_meta(&m)?;
            }
        }
        self.cleanup_legacy_namespace(&lock)?;
        // Leftovers: temporary files and data files no snapshot refers to (an
        // interrupted rebase or removal).
        let referenced: std::collections::HashSet<PathBuf> = self
            .read_entries()?
            .into_iter()
            .filter_map(|(_, m)| m.storage.data_file)
            .filter_map(|name| self.layout().data_path(&name).ok())
            .collect();
        let dir = self.snap_dir();
        for e in std::fs::read_dir(&dir).at(&dir)?.flatten() {
            let p = e.path();
            let stale_data = p.extension().is_some_and(|x| x == "dds") && !referenced.contains(&p);
            if p.extension().is_some_and(|x| x == "tmp") || stale_data {
                let _ = std::fs::remove_file(&p);
            }
        }
        let outcome = if self.layout().is_v4() {
            MigrationOutcome::V4Active
        } else if migrated {
            MigrationOutcome::LegacyStoreReady
        } else {
            MigrationOutcome::None
        };
        Ok((lock, outcome))
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
            if self.layout().is_v4() {
                m.validate_v4()
                    .map_err(|e| StoreError::Corrupt(format!("{}: {e}", p.display())))?;
                if let Some(data_file) = &m.storage.data_file {
                    self.layout().data_path(data_file)?;
                }
            }
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
            let mut reg = self.registry()?;
            for m in out.iter_mut().filter(|m| m.disk.is_none()) {
                m.disk = Some(reg.record(&Sighting::of_snapshot(m)));
                m.root_in_volume = Some(m.derived_root_in_volume());
            }
        }
        Ok(out)
    }

    fn write_meta(&self, m: &SnapshotMeta) -> Result<()> {
        if self.layout().is_v4() {
            m.validate_v4()
                .map_err(|e| StoreError::Invalid(e.to_string()))?;
            if let Some(data_file) = &m.storage.data_file {
                self.layout().data_path(data_file)?;
            }
        }
        let p = self.meta_path(m);
        let text = serde_json::to_vec_pretty(m).expect("serializable");
        write_atomic(&p, &text).at(&p)
    }

    /// All snapshots, oldest first. Interrupted scans are reported as FAILED.
    pub fn list(&self) -> Result<Vec<SnapshotMeta>> {
        let mut v = self.read_metas()?;
        self.mark_damaged(&mut v)?;
        if v.iter().any(|m| m.status == Status::InProgress) && !self.is_busy() {
            for m in v.iter_mut().filter(|m| m.status == Status::InProgress) {
                m.status = Status::Failed;
                m.error.get_or_insert_with(|| "interrupted".into());
            }
        }
        Ok(v)
    }

    fn mark_damaged(&self, metas: &mut [SnapshotMeta]) -> Result<()> {
        let mut damaged = std::collections::HashMap::<u64, String>::new();
        for m in metas.iter().filter(|m| m.status.is_usable()) {
            let Some(name) = m.storage.data_file.as_deref() else {
                damaged.insert(m.id, "referenced snapshot data is missing".into());
                continue;
            };
            let path = self.layout().data_path(name)?;
            if !path.is_file() {
                damaged.insert(m.id, "referenced snapshot data is missing".into());
                continue;
            }
            if let Err(error) = self.data_file_unverified(m).and_then(|data| {
                if data.descriptor().is_dds2() {
                    Ok(())
                } else {
                    data.verify()
                        .map(|_| ())
                        .map_err(|error| StoreError::Corrupt(error.to_string()))
                }
            }) {
                damaged.insert(
                    m.id,
                    format!("referenced snapshot data is corrupt: {error}"),
                );
            }
        }

        // A cycle can contain otherwise valid files but can never be
        // reconstructed. Mark the cycle itself; dependents are propagated
        // below with the same damage rule as a missing base.
        for meta in metas.iter().filter(|m| m.status.is_usable()) {
            let mut chain = Vec::new();
            let mut positions = std::collections::HashMap::new();
            let mut current = meta.id;
            loop {
                if let Some(&start) = positions.get(&current) {
                    for id in &chain[start..] {
                        damaged.insert(*id, "snapshot reconstruction chain is cyclic".into());
                    }
                    break;
                }
                positions.insert(current, chain.len());
                chain.push(current);
                let Some(candidate) = metas.iter().find(|m| m.id == current) else {
                    break;
                };
                let Some(base) = candidate.storage.base else {
                    break;
                };
                current = base;
            }
        }

        for meta in metas.iter().filter(|m| m.status.is_usable()) {
            let Some(base) = meta.storage.base else {
                continue;
            };
            let Some(base_meta) = metas.iter().find(|candidate| candidate.id == base) else {
                continue;
            };
            if let Err(error) = self.ensure_lineage_compatible(base_meta, meta) {
                damaged.insert(meta.id, format!("incompatible snapshot lineage: {error}"));
            }
        }

        // Delta bases can point forward in the on-disk order (reverse deltas),
        // so a single pass over metadata cannot propagate damage reliably.
        loop {
            let mut changed = false;
            for m in metas.iter().filter(|m| m.status.is_usable()) {
                if damaged.contains_key(&m.id) {
                    continue;
                }
                let Some(base) = m.storage.base else {
                    continue;
                };
                let base_exists_and_usable = metas
                    .iter()
                    .find(|candidate| candidate.id == base)
                    .is_some_and(|candidate| candidate.status.is_usable());
                if !base_exists_and_usable || damaged.contains_key(&base) {
                    damaged.insert(m.id, "snapshot depends on a damaged chain".into());
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        for m in metas.iter_mut() {
            if m.status.is_usable()
                && let Some(reason) = damaged.remove(&m.id)
            {
                m.status = Status::Damaged;
                m.error.get_or_insert(reason);
            }
        }
        Ok(())
    }

    pub fn get(&self, id: u64) -> Result<SnapshotMeta> {
        self.list()?
            .into_iter()
            .find(|m| m.id == id)
            .ok_or_else(|| StoreError::NotFound(id.to_string()))
    }

    /// Resolves a snapshot reference: numeric id, name, or `latest` (most recent
    /// COMPLETED or PARTIAL snapshot). `latest~1` is the one before it. With a
    /// disk in front (`backupy-2014:latest`, `d1a2b3c4:latest~1`), only that
    /// disk's snapshots count.
    pub fn resolve(&self, reference: &str) -> Result<SnapshotMeta> {
        let all = self.list()?;
        if let Some((disk, rest)) = reference.split_once(':') {
            let reg = self.registry()?;
            let d = reg
                .resolve(disk)
                .ok_or_else(|| StoreError::UnknownDisk(disk.into()))?;
            let mine: Vec<SnapshotMeta> = all
                .into_iter()
                .filter(|m| m.disk.as_deref() == Some(d.id.as_str()))
                .collect();
            return Self::resolve_in(
                &mine,
                if rest.is_empty() { "latest" } else { rest },
                reference,
            );
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
            && let Some(m) = all.iter().find(|m| m.id == id)
        {
            return Ok(m.clone());
        }
        Err(StoreError::NotFound(shown.into()))
    }

    fn data_file(&self, m: &SnapshotMeta) -> Result<DataFile> {
        let name = m
            .storage
            .data_file
            .as_ref()
            .filter(|_| m.status.is_usable());
        let Some(name) = name else {
            return Err(StoreError::NoData {
                id: m.id,
                status: m.status,
            });
        };
        let p = self.layout().data_path(name)?;
        let data = if self.layout().is_v4() {
            let metadata = MetadataV3 {
                format_version: crate::meta::META_FORMAT_VERSION_V3,
                volumes: m.volumes.clone(),
            };
            DataFile::open_with_metadata(&p, &metadata).at(&p)?
        } else {
            DataFile::open(&p).at(&p)?
        };
        if let Some(root) = &m.root_native
            && data.descriptor().path_encoding != root.path.encoding()
        {
            return Err(StoreError::Corrupt(format!(
                "snapshot {} data path encoding does not match its native root",
                m.id
            )));
        }
        if data.descriptor().is_dds2() && m.volumes.is_empty() {
            return Err(StoreError::Corrupt(format!(
                "snapshot {} DDS2 data has no volume table",
                m.id
            )));
        }
        Ok(data)
    }

    fn data_file_unverified(&self, m: &SnapshotMeta) -> Result<DataFile> {
        let name = m
            .storage
            .data_file
            .as_ref()
            .filter(|_| m.status.is_usable());
        let Some(name) = name else {
            return Err(StoreError::NoData {
                id: m.id,
                status: m.status,
            });
        };
        let path = self.layout().data_path(name)?;
        let data = if self.layout().is_v4() {
            let metadata = MetadataV3 {
                format_version: crate::meta::META_FORMAT_VERSION_V3,
                volumes: m.volumes.clone(),
            };
            DataFile::open_with_metadata_unverified(&path, &metadata).at(&path)?
        } else {
            DataFile::open(&path).at(&path)?
        };
        if let Some(root) = &m.root_native
            && data.descriptor().path_encoding != root.path.encoding()
        {
            return Err(StoreError::Corrupt(format!(
                "snapshot {} data path encoding does not match its native root",
                m.id
            )));
        }
        if data.descriptor().is_dds2() && m.volumes.is_empty() {
            return Err(StoreError::Corrupt(format!(
                "snapshot {} DDS2 data has no volume table",
                m.id
            )));
        }
        Ok(data)
    }

    fn stream_descriptor_for_meta(&self, meta: &SnapshotMeta) -> Result<StreamDescriptor> {
        if meta.storage.data_file.is_some() && meta.status.is_usable() {
            return Ok(self.data_file(meta)?.descriptor());
        }
        self.stream_descriptor(FileKind::Records, meta)
    }

    /// Enforces only immutable lineage fences. Root, exclusions and filesystem
    /// boundaries are intentionally left to the existing comparison policy.
    fn ensure_lineage_compatible(
        &self,
        base: &SnapshotMeta,
        target: &SnapshotMeta,
    ) -> Result<LineageCompatibility> {
        let left = self.stream_descriptor_for_meta(base)?;
        let right = self.stream_descriptor_for_meta(target)?;
        if left.path_encoding != right.path_encoding {
            return Err(StoreError::Invalid(
                "incompatible stream path encodings".into(),
            ));
        }
        if left.flags != right.flags {
            return Err(StoreError::Invalid("incompatible stream flags".into()));
        }
        if !left.lineage_compatible(right) {
            return Err(StoreError::Invalid(
                "incompatible stream record schemas".into(),
            ));
        }
        if let (Some(left), Some(right)) = (&base.disk, &target.disk)
            && left != right
        {
            return Err(StoreError::Invalid(format!(
                "different disks ({left} vs {right})"
            )));
        }
        if let Some(reason) = base.volume_identity_incompatibility(target) {
            return Err(StoreError::Invalid(reason));
        }
        if let Some(reason) = base.volume_table_incompatibility(target) {
            return Err(StoreError::Invalid(reason));
        }
        Ok(base.lineage_compatibility(target))
    }

    fn validate_lineage_chain(&self, all: &[SnapshotMeta], id: u64) -> Result<()> {
        let mut current = all
            .iter()
            .find(|meta| meta.id == id)
            .ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        let mut depth = 0;
        while let Some(base_id) = current.storage.base {
            let base = all.iter().find(|meta| meta.id == base_id).ok_or_else(|| {
                StoreError::Corrupt(format!("snapshot {id} has missing base {base_id}"))
            })?;
            self.ensure_lineage_compatible(base, current)?;
            current = base;
            depth += 1;
            if depth > 64 {
                return Err(StoreError::Corrupt(format!(
                    "delta chain of snapshot {id} is too long or cyclic"
                )));
            }
        }
        Ok(())
    }

    /// Streams the records of a snapshot, reconstructing deltas.
    pub fn records(&self, id: u64) -> Result<RecordStream<'static>> {
        let all = self.list()?;
        self.records_in(&all, id, 0)
    }

    /// The records of snapshot `id` as if it had been scanned at `root`: for
    /// comparing scans of one disk mounted at different places.
    pub fn records_as(&self, id: u64, root: &NativePath) -> Result<RecordStream<'static>> {
        let all = self.list()?;
        let m = all
            .iter()
            .find(|m| m.id == id)
            .ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        Ok(rooted_at(
            self.records_in(&all, id, 0)?,
            stored_root(m),
            root,
        ))
    }

    fn records_in(
        &self,
        all: &[SnapshotMeta],
        id: u64,
        depth: usize,
    ) -> Result<RecordStream<'static>> {
        let m = all
            .iter()
            .find(|m| m.id == id)
            .ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        if depth > 64 {
            return Err(StoreError::Corrupt(format!(
                "delta chain of snapshot {id} is too long or cyclic"
            )));
        }
        if let Some(base) = m.storage.base {
            let base_meta = all
                .iter()
                .find(|candidate| candidate.id == base)
                .ok_or_else(|| {
                    StoreError::Corrupt(format!("snapshot {id} has missing base {base}"))
                })?;
            self.ensure_lineage_compatible(base_meta, m)?;
        }
        let df = self.data_file(m)?;
        let path = df.path.clone();
        match m.storage.base {
            None => Ok(Box::new(df.records_from(0).at(&path)?)),
            Some(base) => {
                // Ops are always built against the base renamed to this snapshot's root.
                let base_root = all
                    .iter()
                    .find(|b| b.id == base)
                    .map(stored_root)
                    .unwrap_or_else(|| stored_root(m));
                let mut ops = df.ops().at(&path)?;
                let first = if matches!(m.root_value(), RootValue::LegacyDisplayOnly(_)) {
                    ops.next()
                } else {
                    None
                };
                let target_root = match m.root_value() {
                    RootValue::Exact(root) => Some(root),
                    RootValue::LegacyDisplayOnly(_) => first
                        .as_ref()
                        .and_then(|op| op.as_ref().ok())
                        .and_then(|op| op.parent.first().cloned())
                        .map(|root| match root {
                            NativeName::Unix(bytes) => NativePath::unix(bytes),
                            NativeName::Windows(units) => NativePath::windows(units),
                        }),
                };
                let base_stream = self.records_in(all, base, depth + 1)?;
                let base_stream = if let Some(root) = target_root.as_ref() {
                    rooted_at(base_stream, base_root, root)
                } else {
                    base_stream
                };
                let ops = Box::new(first.into_iter().chain(ops));
                Ok(Box::new(Apply::new(base_stream, ops)))
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
    pub fn records_under(&self, id: u64, prefix: &[NativeName]) -> Result<RecordStream<'static>> {
        let all = self.list()?;
        let m = all
            .iter()
            .find(|m| m.id == id)
            .ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        if m.storage.base.is_some() {
            return Ok(Box::new(PrefixFilter::new(
                self.records_in(&all, id, 0)?,
                prefix.to_vec(),
                &[],
            )));
        }
        let df = self.data_file(m)?;
        let path = df.path.clone();
        let mut partition = 0;
        while partition < df.index.len()
            && compare_paths(&df.index[partition].first_path, prefix)?
                != std::cmp::Ordering::Greater
        {
            partition += 1;
        }
        let start = partition.saturating_sub(1);
        let ctx = if df.index.is_empty() {
            Vec::new()
        } else {
            df.block_context(start).at(&path)?
        };
        let stream = df.records_from(start).at(&path)?;
        Ok(Box::new(PrefixFilter::new(
            Box::new(stream),
            prefix.to_vec(),
            &ctx,
        )))
    }

    /// Reads every block of a snapshot (and its base) and checks checksums and
    /// structure. Returns the number of records.
    pub fn verify(&self, id: u64) -> Result<u64> {
        // Do not turn a verification error into `NoData` by first marking the
        // snapshot damaged; callers of verify need the underlying corruption.
        let all = self.read_metas()?;
        let m = all
            .iter()
            .find(|m| m.id == id)
            .ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        self.validate_lineage_chain(&all, id)?;
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
        fn size(path: &Path) -> u64 {
            if path.is_dir() {
                std::fs::read_dir(path)
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter_map(std::result::Result::ok)
                    .map(|entry| size(&entry.path()))
                    .sum()
            } else {
                fsops::allocated_size(path)
            }
        }
        size(&self.root)
    }

    /// Allocated bytes of one snapshot's own files (not its base).
    pub fn snapshot_size(&self, m: &SnapshotMeta) -> u64 {
        let data = m
            .storage
            .data_file
            .as_ref()
            .and_then(|n| self.layout().data_path(n).ok())
            .map(|path| fsops::allocated_size(&path))
            .unwrap_or(0);
        data + fsops::allocated_size(&self.meta_path(m))
    }

    #[doc(hidden)]
    #[cfg(test)]
    pub(crate) fn rewrite_for_test(
        &self,
        lock: &WriteLock,
        d: &SnapshotMeta,
        base: Option<&SnapshotMeta>,
    ) -> Result<u64> {
        self.rewrite(lock, d, base)
    }

    #[doc(hidden)]
    pub fn chains_for_test(all: &[SnapshotMeta]) -> std::collections::HashMap<u64, (usize, u64)> {
        Self::chains(all)
    }

    /// Reconstruction steps needed for each snapshot (0 for full ones) and the
    /// full snapshot its chain ends at.
    fn chains(all: &[SnapshotMeta]) -> std::collections::HashMap<u64, (usize, u64)> {
        let by_id: std::collections::HashMap<u64, &SnapshotMeta> =
            all.iter().map(|m| (m.id, m)).collect();
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

    fn stream_descriptor(
        &self,
        kind: FileKind,
        meta: &SnapshotMeta,
    ) -> Result<crate::descriptor::StreamDescriptor> {
        if !self.layout().is_v4() {
            return Ok(crate::descriptor::StreamDescriptor::dds1(kind));
        }
        let root = meta.root_native.as_ref().ok_or_else(|| {
            StoreError::Invalid("store-v4 snapshots require an exact native root for DDS2".into())
        })?;
        crate::descriptor::StreamDescriptor::dds2(kind, root.path.encoding())
            .map_err(|e| StoreError::Corrupt(e.to_string()))
    }

    fn create_data_writer(
        &self,
        tmp: PathBuf,
        descriptor: crate::descriptor::StreamDescriptor,
    ) -> io::Result<DataWriter> {
        if descriptor.is_dds2() {
            return DataWriter::create_dds2(tmp, descriptor);
        }
        DataWriter::create(tmp, descriptor.kind)
    }

    fn create_record_encoder(
        &self,
        descriptor: crate::descriptor::StreamDescriptor,
        volumes: &[VolumeDescriptor],
    ) -> io::Result<RecordEncoder> {
        if descriptor.is_dds2() {
            return RecordEncoder::new_dds2_with_volumes(descriptor, volumes);
        }
        Ok(RecordEncoder::new())
    }

    fn create_op_encoder(
        &self,
        descriptor: crate::descriptor::StreamDescriptor,
        volumes: &[VolumeDescriptor],
    ) -> io::Result<OpEncoderBlocks> {
        if descriptor.is_dds2() {
            return OpEncoderBlocks::new_dds2_with_volumes(descriptor, volumes);
        }
        Ok(OpEncoderBlocks::new())
    }

    /// Writes ops turning `base`'s records into `target`'s, as `dest`. Returns
    /// (items, blocks, records of `target`).
    fn write_delta_file(
        &self,
        base: &SnapshotMeta,
        target: &SnapshotMeta,
        dest: &Path,
    ) -> Result<(u64, u64, u64)> {
        self.ensure_lineage_compatible(base, target)?;
        let tmp = dest.with_extension("dds.tmp");
        let target_root = exact_root(self, target)?;
        let base_stream = self.records_as(base.id, &target_root)?;
        let records = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let seen = records.clone();
        let target_stream: RecordStream<'static> = Box::new(
            self.records(target.id)?
                .inspect(move |_| seen.set(seen.get() + 1)),
        );
        let descriptor = self.stream_descriptor(FileKind::Ops, target)?;
        let mut data = self.create_data_writer(tmp.clone(), descriptor).at(&tmp)?;
        let mut ops = self
            .create_op_encoder(descriptor, &target.volumes)
            .at(&tmp)?;
        let r = (|| -> io::Result<(u64, u64, u64)> {
            {
                let mut b = DeltaBuilder::with_allocated_deltas(
                    |op: &DeltaOp| {
                        if let Some(block) = ops.push(op)? {
                            data.block(block)?;
                        }
                        Ok(())
                    },
                    if descriptor.is_dds2() {
                        base.lineage_compatibility(target).allocated_deltas
                    } else {
                        true
                    },
                );
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
        let meta = self.get(id)?;
        let stream = self.records(id)?;
        let descriptor = self.stream_descriptor(FileKind::Records, &meta)?;
        let mut data = self.create_data_writer(tmp.clone(), descriptor).at(&tmp)?;
        let mut enc = self
            .create_record_encoder(descriptor, &meta.volumes)
            .at(&tmp)?;
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
    fn switch_data(
        &self,
        lock: &WriteLock,
        mut m: SnapshotMeta,
        base: Option<u64>,
        name: String,
        items: u64,
        blocks: u64,
    ) -> Result<SnapshotMeta> {
        self.validate_lock(lock)?;
        let new_reference = self.layout().data_reference(&name)?;
        let old = m.storage.data_file.replace(new_reference.clone());
        m.storage.base = base;
        m.storage.items = items;
        m.storage.blocks = blocks;
        m.storage.bytes_on_disk = fsops::allocated_size(&self.layout().data_path_for_name(&name)?);
        self.write_meta(&m)?;
        if let Some(old) = old.filter(|o| *o != new_reference) {
            let p = self.layout().data_path(&old)?;
            let _ = std::fs::remove_file(&p);
            fsops::sync_dir(&self.snap_dir()).at(&self.snap_dir())?;
        }
        Ok(m)
    }

    /// After `newest` was committed as a full snapshot: rewrite the previous
    /// comparable full snapshot as a reverse delta against it, unless that would
    /// make a delta chain longer than [`MAX_CHAIN`] or save too little.
    pub fn rebase_previous(&self, lock: &WriteLock, newest: &SnapshotMeta) -> Result<Rebase> {
        self.validate_lock(lock)?;
        let all = self.list()?;
        let Some(prev) = all.iter().rev().find(|m| {
            m.id < newest.id
                && m.status.is_usable()
                && m.incompatibility(newest).is_none()
                && self.ensure_lineage_compatible(m, newest).is_ok()
        }) else {
            return Ok(Rebase::Nothing);
        };
        if prev.storage.base.is_some() {
            return Ok(Rebase::Nothing);
        }
        let chains = Self::chains(&all);
        let deepest = chains
            .values()
            .filter(|(_, end)| *end == prev.id)
            .map(|(d, _)| *d)
            .max()
            .unwrap_or(0);
        if deepest + 1 > MAX_CHAIN {
            return Ok(Rebase::KeptFull {
                id: prev.id,
                reason: format!("keyframe (chains limited to {MAX_CHAIN} steps)"),
            });
        }
        let name = self.unused_name(format!("{}.r{}.dds", stem_of(prev), stem_of(newest)));
        let dest = self.layout().data_path_for_name(&name)?;
        let (items, blocks, _) = self.write_delta_file(newest, prev, &dest)?;
        let len = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let prev_file = prev
            .storage
            .data_file
            .as_deref()
            .map(|name| self.layout().data_path(name))
            .transpose()?
            .unwrap_or_default();
        let (delta_len, full_len) = (len(&dest), len(&prev_file));
        // Below 1 MB the space hardly matters and one consistent layout is simpler.
        if full_len >= 1 << 20 && delta_len as f64 > full_len as f64 * MAX_REVERSE_RATIO {
            let _ = std::fs::remove_file(&dest);
            return Ok(Rebase::KeptFull {
                id: prev.id,
                reason: "changes too large for a delta to pay off".into(),
            });
        }
        let freed = fsops::allocated_size(&prev_file).saturating_sub(fsops::allocated_size(&dest));
        self.switch_data(lock, prev.clone(), Some(newest.id), name, items, blocks)?;
        Ok(Rebase::Converted {
            id: prev.id,
            freed_bytes: freed,
        })
    }

    fn next_id(&self) -> Result<u64> {
        Ok(self.read_metas()?.iter().map(|m| m.id).max().unwrap_or(0) + 1)
    }

    /// Starts a snapshot. `template` supplies name, root, volume, options and
    /// `space_before`, and its disk when the caller knows it; id, status and
    /// storage are filled in here. Without a disk, the disk is the one the
    /// volume in `template` belongs to, recorded in the registry. With a
    /// `base`, the data is written as a delta against it.
    pub fn begin(
        &self,
        lock: WriteLock,
        mut template: SnapshotMeta,
        base: Option<&SnapshotMeta>,
    ) -> Result<SnapshotWriter> {
        self.validate_lock(&lock)?;
        if self.layout().is_v4() && template.volumes.is_empty() {
            return Err(StoreError::Invalid(
                "store-v4 snapshots require the complete ordered volume table".into(),
            ));
        }
        let metas = self.read_metas()?;
        if metas.iter().any(|m| m.name == template.name) {
            return Err(StoreError::DuplicateName(template.name));
        }
        template.id = self.next_id()?;
        if self.layout().is_v4() {
            template.format_version = crate::meta::META_FORMAT_VERSION_V3;
            template
                .validate_v4()
                .map_err(|error| StoreError::Invalid(error.to_string()))?;
        }
        template.status = Status::InProgress;
        template.storage = Default::default();
        template.storage.base = base.map(|b| b.id);
        let kind = if base.is_some() {
            FileKind::Ops
        } else {
            FileKind::Records
        };
        let descriptor = self.stream_descriptor(kind, &template)?;
        descriptor
            .validate()
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        if self.layout().is_v4() {
            descriptor
                .validate_volume_table(&template.volumes)
                .map_err(|error| StoreError::Invalid(error.to_string()))?;
        }
        #[cfg(test)]
        STREAM_INVARIANT_VALIDATIONS.with(|count| count.set(count.get() + 1));
        let mut lineage = LineageCompatibility {
            common_timestamp_resolution: TimestampResolution::Unknown,
            allocated_deltas: true,
            file_id_comparable: true,
        };
        if let Some(base) = base {
            let current_base = self.get(base.id)?;
            if !current_base.status.is_usable() {
                return Err(StoreError::Invalid(format!(
                    "cannot create a delta from damaged snapshot {}",
                    base.id
                )));
            }
            lineage = self.ensure_lineage_compatible(&current_base, &template)?;
        }
        if template.disk.is_none() {
            let s = Sighting::of_snapshot(&template);
            template.disk = Some(self.update_registry(&lock, |reg| Ok(reg.record(&s)))?);
        }
        if let Some(root) = &template.root_native
            && matches!(&root.path, NativePath::Unix(_))
        {
            let mount = NativePath::unix(template.volume.mount_point.as_bytes().to_vec());
            template.root_in_volume_native = crate::meta::root_in_volume_native(&root.path, &mount);
        }
        template.root_in_volume = Some(template.derived_root_in_volume());
        let stem = self.new_stem(&template, &metas, true);
        let data_name = self.unused_name(match base {
            Some(b) => format!("{stem}.r{}.dds", stem_of(b)),
            None => format!("{stem}.dds"),
        });
        template.storage.stem = Some(stem);
        let tmp = self
            .layout()
            .data_path_for_name(&data_name)?
            .with_extension("dds.tmp");
        self.write_meta(&template)?;
        let data = match self.create_data_writer(tmp.clone(), descriptor) {
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
            None => Encoder::Full(Box::new(
                self.create_record_encoder(descriptor, &template.volumes)
                    .at(&tmp)?,
            )),
            Some(b) => Encoder::Delta {
                push: Box::new(PushDelta::new_with_allocated_deltas(
                    {
                        let target_root = match template.root_value() {
                            RootValue::Exact(root) => root,
                            RootValue::LegacyDisplayOnly(_) => {
                                let base_root = exact_root(self, b)?;
                                if b.root != template.root {
                                    return Err(StoreError::Invalid(
                                    "a new delta needs an exact native root when its legacy display root differs from the base".into(),
                                ));
                                }
                                base_root
                            }
                        };
                        self.records_as(b.id, &target_root)?
                    },
                    if descriptor.is_dds2() {
                        lineage.allocated_deltas
                    } else {
                        true
                    },
                )),
                ops: self
                    .create_op_encoder(descriptor, &template.volumes)
                    .at(&tmp)?,
            },
        };
        Ok(SnapshotWriter {
            store: Store {
                root: self.root.clone(),
                layout: RwLock::new(self.layout()),
            },
            lock: Some(std::sync::Arc::new(lock)),
            meta: template,
            data: Some(data),
            encoder,
            data_name,
            tmp,
            records: 0,
            descriptor,
            root_seen: false,
            poisoned: false,
            data_published: false,
        })
    }

    /// Removes a snapshot. Snapshots stored as deltas against it are first
    /// rewritten against its own base (or as full snapshots), so no history is lost.
    /// Returns the bytes freed and the ids of the rewritten snapshots.
    pub fn remove(&self, lock: &WriteLock, id: u64) -> Result<(u64, Vec<u64>)> {
        self.validate_lock(lock)?;
        let r = self.remove_many(lock, &[id], &AtomicBool::new(false))?;
        Ok((r.freed_bytes, r.rewritten))
    }

    /// What removing `ids` would do, without changing anything.
    pub fn plan_removal(&self, ids: &[u64]) -> Result<RemovalPlan> {
        let all = self.list()?;
        Self::plan_in(&all, ids, |m| self.snapshot_size(m))
    }

    fn plan_in(
        all: &[SnapshotMeta],
        ids: &[u64],
        size: impl Fn(&SnapshotMeta) -> u64,
    ) -> Result<RemovalPlan> {
        let by_id: std::collections::HashMap<u64, &SnapshotMeta> =
            all.iter().map(|m| (m.id, m)).collect();
        let gone: std::collections::BTreeSet<u64> = ids.iter().copied().collect();
        if let Some(id) = gone.iter().find(|id| !by_id.contains_key(id)) {
            return Err(StoreError::NotFound(id.to_string()));
        }
        // A kept snapshot whose base goes is rewritten once, against the first
        // kept snapshot down its chain (or as a full one): never once per removal.
        let mut rewrites = Vec::new();
        for m in all
            .iter()
            .filter(|m| !gone.contains(&m.id) && m.status.is_usable())
        {
            let (mut base, mut steps) = (m.storage.base, 0);
            while let Some(b) = base.filter(|b| gone.contains(b)) {
                base = by_id.get(&b).and_then(|x| x.storage.base);
                steps += 1;
                if steps > 1000 {
                    return Err(StoreError::Corrupt(format!(
                        "delta chain of snapshot {} is cyclic",
                        m.id
                    )));
                }
            }
            if base != m.storage.base {
                rewrites.push((m.id, base));
            }
        }
        let bytes = gone.iter().map(|id| size(by_id[id])).sum();
        Ok(RemovalPlan {
            removed: gone.into_iter().collect(),
            rewrites,
            bytes,
        })
    }

    /// Removes the snapshots `ids` (see [`Store::plan_removal`]). Kept snapshots
    /// that depended on removed ones are rewritten first, each once; then the
    /// removed ones go, those nothing depends on first, metadata before data,
    /// so a crash at any point leaves every remaining snapshot readable (a data
    /// file left behind is removed when the lock is next taken). `cancel` is
    /// checked between rewrites: a stop leaves everything in place.
    pub fn remove_many(
        &self,
        lock: &WriteLock,
        ids: &[u64],
        cancel: &AtomicBool,
    ) -> Result<Removal> {
        self.validate_lock(lock)?;
        let before = self.size_on_disk();
        let all = self.list()?;
        let plan = Self::plan_in(&all, ids, |_| 0)?;
        let find = |id: u64| {
            all.iter()
                .find(|m| m.id == id)
                .expect("planned from this list")
        };
        let mut rewritten = Vec::new();
        for &(id, base) in &plan.rewrites {
            if cancel.load(Ordering::Relaxed) {
                return Ok(Removal {
                    removed: Vec::new(),
                    rewritten,
                    freed_bytes: before.saturating_sub(self.size_on_disk()),
                    cancelled: true,
                });
            }
            let d = self.get(id)?;
            self.rewrite(lock, &d, base.map(find))?;
            rewritten.push(id);
        }
        // Leaves first: a removed snapshot is deleted once no remaining one points at it.
        let mut left: Vec<SnapshotMeta> = plan.removed.iter().map(|&id| find(id).clone()).collect();
        while !left.is_empty() {
            let i = left
                .iter()
                .position(|m| !left.iter().any(|x| x.storage.base == Some(m.id)))
                .unwrap_or(0);
            let m = left.remove(i);
            let p = self.meta_path(&m);
            std::fs::remove_file(&p).at(&p)?;
            fsops::sync_dir(&self.snap_dir()).at(&self.snap_dir())?;
            if let Some(name) = &m.storage.data_file {
                let p = self.layout().data_path(name)?;
                match std::fs::remove_file(&p) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(io_err(&p, e)),
                }
            }
        }
        fsops::sync_dir(&self.snap_dir()).at(&self.snap_dir())?;
        Ok(Removal {
            removed: plan.removed,
            rewritten,
            freed_bytes: before.saturating_sub(self.size_on_disk()),
            cancelled: false,
        })
    }

    /// Rewrites `d` as a delta against `base`, or as a full snapshot. Returns
    /// the number of records `d` has.
    fn rewrite(
        &self,
        lock: &WriteLock,
        d: &SnapshotMeta,
        base: Option<&SnapshotMeta>,
    ) -> Result<u64> {
        self.validate_lock(lock)?;
        let name = self.unused_name(match base {
            Some(b) => format!("{}.r{}.dds", stem_of(d), stem_of(b)),
            None => format!("{}.dds", stem_of(d)),
        });
        let dest = self.layout().data_path_for_name(&name)?;
        let (items, blocks, records) = match base {
            Some(b) => self.write_delta_file(b, d, &dest)?,
            None => {
                let (items, blocks) = self.write_full_file(d.id, &dest)?;
                (items, blocks, items)
            }
        };
        self.switch_data(lock, d.clone(), base.map(|b| b.id), name, items, blocks)?;
        Ok(records)
    }

    /// The layout the usable snapshots would have if they had been written one
    /// by one in order (see [`Store::rebase_previous`]): for each, the base it
    /// would be a delta against, or `None` for full. Records do not change
    /// with the layout, only how they are stored.
    fn written_layout(all: &[SnapshotMeta]) -> std::collections::BTreeMap<u64, Option<u64>> {
        let usable: Vec<&SnapshotMeta> = all.iter().filter(|m| m.status.is_usable()).collect();
        let mut target: std::collections::BTreeMap<u64, Option<u64>> =
            usable.iter().map(|m| (m.id, None)).collect();
        for (i, s) in usable.iter().enumerate() {
            let Some(prev) = usable[..i]
                .iter()
                .rev()
                .find(|p| p.incompatibility(s).is_none())
            else {
                continue;
            };
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
            let deepest = target
                .keys()
                .filter_map(|&id| depth_to(id))
                .max()
                .unwrap_or(0);
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
        Ok(all
            .iter()
            .filter(|m| target.get(&m.id).is_some_and(|b| *b != m.storage.base))
            .count())
    }

    /// Rewrites every snapshot whose storage differs from what writing the
    /// scans one by one would have produced (after removals, a delta may span
    /// a long gap or a snapshot may have been made full), newest first, then
    /// checks the checksums of every data file and re-reads each rewritten
    /// snapshot. Every step leaves a readable store; `cancel` stops between
    /// snapshots. `progress` gets (done, total) as it goes.
    pub fn compact(
        &self,
        lock: &WriteLock,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(CompactStep),
    ) -> Result<Compaction> {
        self.validate_lock(lock)?;
        let before = self.size_on_disk();
        let all = self.list()?;
        let target = Self::written_layout(&all);
        let todo: Vec<(u64, Option<u64>)> = target
            .iter()
            .rev()
            .filter(|(id, base)| {
                all.iter()
                    .find(|m| m.id == **id)
                    .is_some_and(|m| m.storage.base != **base)
            })
            .map(|(id, base)| (*id, *base))
            .collect();
        let mut out = Compaction {
            checked: 0,
            rewritten: Vec::new(),
            left: Vec::new(),
            bytes_before: before,
            bytes_after: before,
            cancelled: false,
        };
        let mut counts = Vec::new();
        for (done, &(id, base)) in todo.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                out.cancelled = true;
                break;
            }
            progress(CompactStep::Rewriting {
                id,
                done,
                total: todo.len(),
            });
            let m = self.get(id)?;
            let b = match base {
                Some(b) => Some(self.get(b)?),
                None => None,
            };
            let records = match &b {
                // A delta only when it pays off, as at write time; otherwise the
                // snapshot stays as it is (a newer base cannot form a cycle).
                Some(b) => match self.delta_if_smaller(lock, &m, b)? {
                    Some(n) => n,
                    None if m.storage.base.is_none_or(|old| old > id) => {
                        out.left.push(id);
                        continue;
                    }
                    None => self.rewrite(lock, &m, None)?,
                },
                None => self.rewrite(lock, &m, None)?,
            };
            counts.push((id, records));
            out.rewritten.push(id);
        }
        if !out.cancelled {
            let metas: Vec<SnapshotMeta> = self
                .list()?
                .into_iter()
                .filter(|m| m.status.is_usable())
                .collect();
            for (done, m) in metas.iter().enumerate() {
                if cancel.load(Ordering::Relaxed) {
                    out.cancelled = true;
                    break;
                }
                progress(CompactStep::Checking {
                    id: m.id,
                    done,
                    total: metas.len(),
                });
                let df = self.data_file(m)?;
                df.verify()
                    .map_err(|e| StoreError::Corrupt(format!("snapshot {}: {e}", m.id)))?;
                out.checked += 1;
            }
            for &(id, records) in &counts {
                let n = self.verify(id)?;
                if n != records {
                    return Err(StoreError::Corrupt(format!(
                        "snapshot {id} reads back {n} records after compaction, {records} before"
                    )));
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
    fn delta_if_smaller(
        &self,
        lock: &WriteLock,
        m: &SnapshotMeta,
        base: &SnapshotMeta,
    ) -> Result<Option<u64>> {
        self.validate_lock(lock)?;
        let name = self.unused_name(format!("{}.r{}.dds", stem_of(m), stem_of(base)));
        let dest = self.layout().data_path_for_name(&name)?;
        let (items, blocks, records) = self.write_delta_file(base, m, &dest)?;
        let len = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let full_len = match m.storage.base {
            None => m
                .storage
                .data_file
                .as_deref()
                .and_then(|name| self.layout().data_path(name).ok())
                .map(|path| len(&path))
                .unwrap_or(0),
            Some(_) => {
                let all = self.list()?;
                let end = Self::chains(&all)
                    .get(&base.id)
                    .map(|(_, end)| *end)
                    .unwrap_or(base.id);
                let k = self.get(end)?;
                let entries =
                    |x: &SnapshotMeta| x.counts.as_ref().map(|c| c.entries).filter(|n| *n > 0);
                let k_len = k
                    .storage
                    .data_file
                    .as_deref()
                    .and_then(|name| self.layout().data_path(name).ok())
                    .map(|path| len(&path))
                    .unwrap_or(0);
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
        self.switch_data(lock, m.clone(), Some(base.id), name, items, blocks)?;
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
    Full(Box<RecordEncoder>),
    Delta {
        push: Box<PushDelta<'static>>,
        ops: OpEncoderBlocks,
    },
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
    descriptor: StreamDescriptor,
    root_seen: bool,
    poisoned: bool,
    data_published: bool,
}

impl SnapshotWriter {
    /// A second handle on the writer lock, so it outlives `commit`.
    fn take_lock(&self) -> Option<std::sync::Arc<WriteLock>> {
        self.lock.clone()
    }

    fn validate_lock(&self) -> Result<()> {
        let lock = self
            .lock
            .as_deref()
            .ok_or_else(|| StoreError::Invalid("snapshot writer has no writer lock".into()))?;
        self.store.validate_lock(lock)
    }

    fn abort_with_error(&mut self, error: StoreError) -> StoreError {
        if let Some(data) = self.data.take() {
            data.abort();
        }
        self.poisoned = true;
        self.meta.status = Status::Failed;
        self.meta.error = Some(error.to_string());
        let _ = self.store.write_meta(&self.meta);
        error
    }

    fn validate_record(&self, rec: &Record) -> Result<()> {
        if !self.descriptor.is_dds2() {
            if rec.name.encoding() != self.descriptor.path_encoding {
                return Err(StoreError::Invalid(
                    "record path encoding does not match stream descriptor".into(),
                ));
            }
            return Ok(());
        }
        // The encoder owns descriptor, name, flag and availability validation.
        // The store-only identity and timestamp contracts need the snapshot metadata.
        let needs_volume = rec.available.contains(EntryFields::MTIME)
            || rec.available.contains(EntryFields::BIRTH_TIME)
            || rec.available.contains(EntryFields::FILE_ID);
        if needs_volume {
            let volume =
                self.meta.volumes.get(rec.vol as usize).ok_or_else(|| {
                    StoreError::Invalid(format!("invalid volume index {}", rec.vol))
                })?;
            if (rec.available.contains(EntryFields::MTIME)
                || rec.available.contains(EntryFields::BIRTH_TIME))
                && rec.timestamp_resolution != volume.timestamp_resolution
            {
                return Err(StoreError::Invalid(
                    "record timestamp resolution does not match its volume descriptor".into(),
                ));
            }
            if rec.available.contains(EntryFields::FILE_ID) {
                if matches!(volume.file_id_scheme, FileIdScheme::Unavailable)
                    || rec.file_id_scheme != volume.file_id_scheme
                    || rec.file_id_scope != volume.file_id_scope
                    || rec.file_id == diskdrift_core::record::FileId::ZERO
                {
                    return Err(StoreError::Invalid(
                        "record file identity does not match its volume descriptor".into(),
                    ));
                }
                if matches!(rec.file_id_scheme, FileIdScheme::PosixInode64)
                    && (rec.file_id.high != 0 || rec.ino != rec.file_id.low)
                {
                    return Err(StoreError::Invalid(
                        "POSIX file identity exceeds its 64-bit field width".into(),
                    ));
                }
            }
        }
        if !self.root_seen {
            let root_name = match &self.meta.root_native {
                Some(root) => match &root.path {
                    NativePath::Unix(bytes) => NativeName::unix(bytes.clone()),
                    NativePath::Windows(units) => NativeName::windows(units.clone()),
                },
                None => {
                    return Err(StoreError::Invalid(
                        "DDS2 snapshot requires an exact native root".into(),
                    ));
                }
            };
            if rec.depth != 0 || rec.name != root_name {
                return Err(StoreError::Invalid(
                    "first record must be the exact snapshot root".into(),
                ));
            }
        } else if rec.depth == 0 {
            return Err(StoreError::Invalid(
                "record stream contains more than one root record".into(),
            ));
        }
        Ok(())
    }

    fn validate_final_stream(&self) -> Result<()> {
        if self.poisoned || self.data.is_none() {
            return Err(StoreError::Invalid(
                "snapshot writer was aborted after a stream validation failure".into(),
            ));
        }
        if self.descriptor.is_dds2() && (self.records == 0 || !self.root_seen) {
            return Err(StoreError::Invalid(
                "snapshot stream must contain an exact root record".into(),
            ));
        }
        if self.store.layout().is_v4() {
            self.meta
                .validate_v4()
                .map_err(|error| StoreError::Invalid(error.to_string()))?;
            let metadata = MetadataV3 {
                format_version: crate::meta::META_FORMAT_VERSION_V3,
                volumes: self.meta.volumes.clone(),
            };
            self.descriptor
                .validate_metadata(&metadata)
                .map_err(|error| StoreError::Invalid(error.to_string()))?;
        }
        Ok(())
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

    /// Aligns the root volume descriptor with the walk's stable volume table.
    /// The descriptor starts at zero for template construction, but a previous
    /// snapshot may reserve another device-table slot for delta stability.
    pub fn set_root_volume_index(&mut self, dev: u64, devices: &[u64]) {
        let Some(index) = devices.iter().position(|known| *known == dev) else {
            return;
        };
        if let Some(descriptor) = self.meta.volume.descriptor.as_mut() {
            let index = index as u16;
            descriptor.index = index;
            if let Some(table_descriptor) = self.meta.volumes.get_mut(index as usize) {
                *table_descriptor = descriptor.clone();
            }
        }
    }

    pub fn push(&mut self, rec: &Record) -> Result<()> {
        self.validate_lock()?;
        if let Err(error) = self.validate_record(rec) {
            return Err(self.abort_with_error(error));
        }
        let Some(data) = self.data.as_mut() else {
            return Err(
                self.abort_with_error(StoreError::Invalid("snapshot writer is closed".into()))
            );
        };
        let tmp = &self.tmp;
        let result = match &mut self.encoder {
            Encoder::Full(enc) => enc
                .push(rec)
                .and_then(|block| block.map_or(Ok(()), |b| data.block(b))),
            Encoder::Delta { push, ops } => {
                let mut sink = |op: &DeltaOp| -> io::Result<()> {
                    if let Some(b) = ops.push(op)? {
                        data.block(b)?;
                    }
                    Ok(())
                };
                push.push(rec, &mut sink)
            }
        };
        if let Err(error) = result {
            return Err(self.abort_with_error(io_err(tmp, error)));
        }
        self.records += 1;
        self.root_seen = true;
        Ok(())
    }

    fn finish_data(&mut self) -> Result<u64> {
        self.validate_final_stream()?;
        let Some(mut data) = self.data.take() else {
            return Err(StoreError::Invalid("snapshot writer is closed".into()));
        };
        let tmp = self.tmp.clone();
        let destination = self.store.layout().data_path_for_name(&self.data_name)?;
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
            let items = data.commit(&destination)?;
            self.data_published = true;
            self.meta.storage.blocks = blocks;
            Ok(items)
        })();
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        r.at(&tmp)
    }

    /// Finalizes the snapshot as COMPLETED or PARTIAL.
    pub fn commit(
        mut self,
        counts: WalkReport,
        space_after: Option<SpaceSample>,
        finished_at: String,
    ) -> Result<SnapshotMeta> {
        self.validate_lock()?;
        match self.finish_data() {
            Ok(items) => {
                self.meta.storage.items = items;
                self.meta.storage.data_file =
                    Some(self.store.layout().data_reference(&self.data_name)?);
                let data_path = self.store.layout().data_path_for_name(&self.data_name)?;
                self.meta.storage.bytes_on_disk = fsops::allocated_size(&data_path);
                self.meta.status = if counts.is_partial() {
                    Status::Partial
                } else {
                    Status::Completed
                };
                self.meta.counts = Some(counts);
                self.meta.space_after = space_after;
                self.meta.finished_at = Some(finished_at);
                let meta = self.meta.clone();
                if let Err(error) = self.store.write_meta(&meta) {
                    self.meta.status = Status::Failed;
                    self.meta.error = Some(error.to_string());
                    self.meta.storage.data_file = None;
                    self.meta.storage.bytes_on_disk = 0;
                    self.meta.storage.blocks = 0;
                    self.meta.storage.items = 0;
                    return Err(error);
                }
                self.data_published = false;
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
        let store = Store {
            root: self.store.root.clone(),
            layout: RwLock::new(self.store.layout()),
        };
        let lock = self.take_lock();
        let meta = self.commit(counts, space_after, finished_at)?;
        let rebase = match &lock {
            Some(l) => store.rebase_previous(l, &meta).map_err(|e| e.to_string()),
            None => Ok(Rebase::Nothing),
        };
        Ok((meta, rebase))
    }

    /// Ends the snapshot as FAILED or CANCELLED; its partial data is deleted.
    pub fn abort(
        mut self,
        status: Status,
        error: Option<String>,
        counts: Option<WalkReport>,
        finished_at: String,
    ) -> SnapshotMeta {
        if self.validate_lock().is_err() {
            return self.meta.clone();
        }
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
        if self.validate_lock().is_err() {
            return;
        }
        if self.data_published {
            if let Ok(data_path) = self.store.layout().data_path_for_name(&self.data_name) {
                let _ = std::fs::remove_file(data_path);
            }
            self.data_published = false;
            self.meta.storage.data_file = None;
            self.meta.storage.bytes_on_disk = 0;
            self.meta.storage.blocks = 0;
            self.meta.storage.items = 0;
            self.meta.status = Status::Failed;
            self.meta
                .error
                .get_or_insert_with(|| "metadata publication failed".into());
            let _ = self.store.write_meta(&self.meta);
        } else if let Some(d) = self.data.take() {
            d.abort();
            self.meta.status = Status::Failed;
            self.meta
                .error
                .get_or_insert_with(|| "writer dropped before commit".into());
            let _ = self.store.write_meta(&self.meta);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{META_FORMAT_VERSION, complete_volume_table};
    use diskdrift_core::native::{NativeRoot, RootKind};
    use diskdrift_core::testutil::TestDir;

    fn benchmark_meta() -> SnapshotMeta {
        let root_native = NativeRoot::new(
            NativePath::unix(b"/synthetic".to_vec()),
            RootKind::Posix,
            false,
        )
        .unwrap();
        let mut descriptor = VolumeDescriptor::new(
            0,
            "linux",
            FileIdScheme::PosixInode64,
            FileIdScope::Volume,
            AllocationSemantics::FilesystemReported,
            TimestampResolution::Seconds,
            Provenance::Observed("test".into()),
        );
        descriptor.identity = NativePath::unix(b"test".to_vec());
        descriptor.mount_key = NativePath::unix(b"/synthetic".to_vec());
        descriptor.identity_strength = "uuid".into();
        SnapshotMeta {
            format_version: META_FORMAT_VERSION,
            id: 0,
            name: "test".into(),
            root: "/synthetic".into(),
            root_native: Some(root_native),
            disk: None,
            root_in_volume: None,
            root_in_volume_native: Some(NativePath::unix(b"/synthetic".to_vec())),
            status: Status::InProgress,
            started_at: String::new(),
            finished_at: None,
            volume: crate::meta::VolumeIdentity {
                uuid: Some("test".into()),
                fs_type: "synthetic".into(),
                mount_point: "/synthetic".into(),
                descriptor: Some(descriptor.clone()),
                ..Default::default()
            },
            volumes: complete_volume_table(&descriptor),
            options: crate::meta::ScanOptions {
                stay_on_filesystem: true,
                ..Default::default()
            },
            space_before: None,
            space_after: None,
            storage: Default::default(),
            counts: None,
            error: None,
            diskdrift_version: "test".into(),
        }
    }

    #[test]
    fn writer_validates_stream_invariants_once_at_begin() {
        let dir = TestDir::new("writer-stream-invariants");
        let store = Store::create(dir.path()).unwrap();
        let (lock, _) = store.lock().unwrap();
        reset_stream_invariant_validations();
        let mut writer = store.begin(lock, benchmark_meta(), None).unwrap();

        writer
            .push(&Record {
                name: NativeName::unix(b"/synthetic".to_vec()),
                ..Default::default()
            })
            .unwrap();
        writer
            .push(&Record {
                depth: 1,
                name: NativeName::unix(b"child".to_vec()),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(stream_invariant_validations(), 1);
    }
}

/// Keeps the contiguous run of records at or below `prefix`.
pub struct PrefixFilter {
    inner: RecordStream<'static>,
    prefix: Vec<NativeName>,
    stack: PathStack,
    inside: bool,
    done: bool,
}

impl PrefixFilter {
    pub fn new(inner: RecordStream<'static>, prefix: Vec<NativeName>, ctx: &[NativeName]) -> Self {
        let mut stack = PathStack::new();
        for (d, c) in ctx.iter().enumerate() {
            let _ = stack.set(d as u16, c);
        }
        PrefixFilter {
            inner,
            prefix,
            stack,
            inside: false,
            done: false,
        }
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
            if let Err(e) = self.stack.set(rec.depth, &rec.name) {
                return Some(Err(e));
            }
            let starts = match self.stack.starts_with(&self.prefix) {
                Ok(value) => value,
                Err(error) => {
                    return Some(Err(io::Error::new(io::ErrorKind::InvalidInput, error)));
                }
            };
            if starts {
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
