//! Parallel metadata walker with deterministic, canonical output order.
//!
//! Worker threads list directories concurrently; the calling thread consumes the
//! listings in depth-first pre-order with children sorted by name and emits one
//! [`Record`] per entry. Workers always pick the pending directory that comes first
//! in that order, so they work just ahead of the consumer. Listings that have been
//! read but not consumed yet are bounded by `buffer_budget` entries (spec 2A: peak
//! memory independent of tree size); the directory the consumer is waiting for is
//! always allowed through, so the budget cannot deadlock the walk.

use crate::record::{Kind, Record, flags};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

/// One directory entry as returned by a platform lister.
#[derive(Debug, Clone, Default)]
pub struct RawEntry {
    pub name: Vec<u8>,
    pub kind: Kind,
    pub size: u64,
    pub alloc: u64,
    pub mtime: i64,
    pub btime: i64,
    pub ino: u64,
    pub nlink: u32,
    pub dev: u64,
    /// The lister knows this directory is a mount point.
    pub is_mount_point: bool,
    /// Automount trigger (autofs); never entered.
    pub is_trigger: bool,
    /// errno if the entry's own metadata could not be read.
    pub error: Option<i32>,
}

/// Platform-specific directory listing.
pub trait DirLister: Send + Sync {
    fn name(&self) -> &'static str;
    /// Metadata of `path` itself without following a final symlink. `name` is set to
    /// the full path bytes.
    fn stat(&self, path: &Path) -> io::Result<RawEntry>;
    /// Appends the entries of directory `dir` (excluding `.` and `..`) to `out`.
    fn list(&self, dir: &Path, out: &mut Vec<RawEntry>) -> io::Result<()>;
}

/// Exclusion rules (`scan.exclude` in config).
///
/// An entry is excluded when its absolute path equals or lies below one of the
/// absolute `paths`, or when its own name equals one of `names`.
#[derive(Debug, Clone, Default)]
pub struct ExcludeRules {
    pub paths: Vec<PathBuf>,
    pub names: Vec<OsString>,
}

impl ExcludeRules {
    /// Builds rules from config strings: entries starting with `/` are paths,
    /// anything else is a name.
    pub fn from_patterns(patterns: &[String]) -> Self {
        let mut rules = ExcludeRules::default();
        for p in patterns {
            if p.starts_with('/') {
                rules.paths.push(PathBuf::from(p.trim_end_matches('/').to_string()));
            } else if !p.is_empty() {
                rules.names.push(OsString::from(p));
            }
        }
        rules
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.names.is_empty()
    }

    fn matches(&self, path: &Path, name: &[u8]) -> bool {
        self.names.iter().any(|n| n.as_bytes() == name) || self.paths.iter().any(|p| path.starts_with(p))
    }

    /// Patterns in config notation, for snapshot metadata.
    pub fn patterns(&self) -> Vec<String> {
        let mut out: Vec<String> =
            self.paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        out.extend(self.names.iter().map(|n| n.to_string_lossy().into_owned()));
        out
    }
}

/// Walk configuration.
#[derive(Debug, Clone)]
pub struct WalkOptions {
    /// Worker threads; 0 selects automatically.
    pub threads: usize,
    /// Do not enter directories on other filesystems.
    pub stay_on_filesystem: bool,
    /// Devices treated as the root's own filesystem (e.g. the macOS System+Data
    /// volume group).
    pub same_filesystem_devs: Vec<u64>,
    /// Paths that are aliases of content reached elsewhere (macOS firmlink targets);
    /// they are omitted entirely so nothing is counted twice.
    pub alias_paths: HashSet<PathBuf>,
    pub exclude: ExcludeRules,
    /// Upper bound on listed-but-unconsumed entries held in memory.
    pub buffer_budget: usize,
    /// Maximum number of individual issues kept in the report (all are counted).
    pub max_issues: usize,
    /// Devices to place first in the volume table, so record volume indexes match
    /// those of a previous snapshot (keeps deltas small).
    pub known_devices: Vec<u64>,
    /// Called first on every worker thread (e.g. to lower its priority for a
    /// background scan).
    pub on_thread_start: Option<fn()>,
}

impl Default for WalkOptions {
    fn default() -> Self {
        WalkOptions {
            threads: 0,
            stay_on_filesystem: true,
            same_filesystem_devs: Vec::new(),
            alias_paths: HashSet::new(),
            exclude: ExcludeRules::default(),
            buffer_budget: 100_000,
            max_issues: 1000,
            known_devices: Vec::new(),
            on_thread_start: None,
        }
    }
}

impl WalkOptions {
    pub fn effective_threads(&self) -> usize {
        if self.threads > 0 {
            return self.threads;
        }
        // M1 benchmarks: APFS metadata throughput peaks at 6-8 threads and drops
        // beyond (kernel contention), so more cores do not mean more threads.
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8)
    }
}

/// Live progress counters, readable from another thread.
#[derive(Debug, Default)]
pub struct Progress {
    pub entries: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes_alloc: AtomicU64,
    pub issues: AtomicU64,
    pub current: Mutex<String>,
}

/// Receives records in canonical order.
pub trait RecordSink {
    fn record(&mut self, rec: &Record, path: &Path) -> io::Result<()>;
}

impl<F: FnMut(&Record, &Path) -> io::Result<()>> RecordSink for F {
    fn record(&mut self, rec: &Record, path: &Path) -> io::Result<()> {
        self(rec, path)
    }
}

/// Category of a problem met during the walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    PermissionDenied,
    ListFailed,
    StatFailed,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalkIssue {
    pub kind: IssueKind,
    pub path: String,
    pub message: String,
}

/// Summary of a finished (or cancelled) walk.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalkReport {
    pub entries: u64,
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub others: u64,
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
    pub hardlink_duplicates: u64,
    pub inaccessible_dirs: u64,
    pub stat_errors: u64,
    pub skipped_mounts: u64,
    pub excluded: u64,
    pub issue_count: u64,
    pub issues: Vec<WalkIssue>,
    /// Device numbers in volume-table order (record `vol` indexes into this).
    pub devices: Vec<u64>,
    pub cancelled: bool,
    pub elapsed_ms: u64,
    /// Diagnostics: peak directories queued, peak listed-but-unconsumed entries,
    /// hardlinked inodes tracked.
    #[serde(default)]
    pub peak_pending_dirs: u64,
    #[serde(default)]
    pub peak_buffered_entries: u64,
    #[serde(default)]
    pub hardlink_inodes: u64,
}

impl WalkReport {
    /// True if some part of the tree could not be read.
    pub fn is_partial(&self) -> bool {
        self.inaccessible_dirs > 0 || self.stat_errors > 0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WalkError {
    #[error("scan root {path}: {source}")]
    Root { path: String, source: io::Error },
    #[error("{0}")]
    Sink(io::Error),
}

#[derive(Debug, Clone, Copy)]
enum Descend {
    No,
    /// To be entered; carries the device. Not queued yet.
    Dev(u64),
    /// Queued for listing under this slot.
    Queued(u64),
    Skip(u8),
    Omit,
}

/// Child directories of one directory queued ahead of the consumer. Very wide
/// directories (100k+ subdirectories exist in the wild) would otherwise put every
/// subdirectory in the queue at once.
const QUEUE_WINDOW: usize = 512;

struct Child {
    raw: RawEntry,
    descend: Descend,
}

struct Task {
    key: Vec<u32>,
    slot: u64,
    path: PathBuf,
    dev: u64,
}

impl PartialEq for Task {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
impl Eq for Task {}
impl PartialOrd for Task {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Task {
    // BinaryHeap is a max-heap; invert so the earliest key in DFS order is on top.
    fn cmp(&self, other: &Self) -> Ordering {
        other.key.cmp(&self.key)
    }
}

type Listing = Result<Vec<Child>, io::Error>;

/// A finished listing and the DFS key of the directory it belongs to.
type Done = (Vec<u32>, Listing);

#[derive(Default)]
struct State {
    peak_pending: usize,
    peak_buffered: usize,
    pending: BinaryHeap<Task>,
    results: HashMap<u64, Done>,
    buffered: usize,
    awaited: Option<u64>,
    next_slot: u64,
    shutdown: bool,
}

struct Shared<'a> {
    state: Mutex<State>,
    work: Condvar,
    done: Condvar,
    lister: &'a dyn DirLister,
    opts: &'a WalkOptions,
    root_dev: u64,
    cancel: &'a AtomicBool,
}

impl State {
    /// Queues up to `QUEUE_WINDOW` not-yet-queued child directories from `from` on.
    fn enqueue_window(&mut self, key: &[u32], dir: &Path, children: &mut [Child], from: usize) {
        let mut queued = 0;
        for (i, child) in children.iter_mut().enumerate().skip(from) {
            if queued >= QUEUE_WINDOW {
                break;
            }
            if let Descend::Dev(dev) = child.descend {
                let slot = self.next_slot;
                self.next_slot += 1;
                let mut k = Vec::with_capacity(key.len() + 1);
                k.extend_from_slice(key);
                k.push(i as u32);
                self.pending.push(Task { key: k, slot, path: dir.join(OsStr::from_bytes(&child.raw.name)), dev });
                child.descend = Descend::Queued(slot);
                queued += 1;
            }
        }
        self.peak_pending = self.peak_pending.max(self.pending.len());
    }
}

/// Errors worth one retry: anything but "no permission" and "gone".
fn transient(e: &io::Error) -> bool {
    !matches!(e.kind(), io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
}

impl Shared<'_> {
    fn same_fs(&self, dev: u64) -> bool {
        dev == self.root_dev || self.opts.same_filesystem_devs.contains(&dev)
    }

    fn worker(&self) {
        let mut buf = Vec::new();
        loop {
            let task = {
                let mut st = self.state.lock().unwrap();
                loop {
                    if st.shutdown {
                        return;
                    }
                    let runnable = match st.pending.peek() {
                        Some(top) => {
                            st.buffered < self.opts.buffer_budget || st.awaited == Some(top.slot)
                        }
                        None => false,
                    };
                    if runnable {
                        break st.pending.pop().unwrap();
                    }
                    st = self.work.wait(st).unwrap();
                }
            };
            let listing = if self.cancel.load(AtomicOrdering::Relaxed) {
                Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"))
            } else {
                buf.clear();
                let mut r = self.lister.list(&task.path, &mut buf);
                if r.as_ref().is_err_and(transient) {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    buf.clear();
                    r = self.lister.list(&task.path, &mut buf);
                }
                r.map(|()| {
                    buf.sort_unstable_by(|a, b| a.name.cmp(&b.name));
                    buf.drain(..).map(|raw| Child { raw, descend: Descend::No }).collect::<Vec<_>>()
                })
            };
            if buf.capacity() > 16 * 1024 {
                // Do not keep a huge directory's buffer around for the rest of the walk.
                buf = Vec::new();
            }
            let listing = listing.map(|mut children| {
                self.classify(&task, &mut children);
                children
            });
            let mut st = self.state.lock().unwrap();
            let listing = listing.map(|mut children| {
                st.enqueue_window(&task.key, &task.path, &mut children, 0);
                children
            });
            st.buffered += listing.as_ref().map(|c| c.len()).unwrap_or(0);
            st.peak_buffered = st.peak_buffered.max(st.buffered);
            st.results.insert(task.slot, (task.key, listing));
            drop(st);
            self.done.notify_all();
            self.work.notify_all();
        }
    }

    /// Decides for each child directory whether to descend (`Descend::Dev`).
    fn classify(&self, task: &Task, children: &mut [Child]) {
        let opts = self.opts;
        for child in children.iter_mut() {
            if child.raw.kind != Kind::Dir {
                if !opts.exclude.is_empty() {
                    let path = task.path.join(OsStr::from_bytes(&child.raw.name));
                    if opts.exclude.matches(&path, &child.raw.name) {
                        child.descend = Descend::Omit;
                    }
                }
                continue;
            }
            if child.raw.error.is_some() {
                child.descend = Descend::No;
                continue;
            }
            let path = task.path.join(OsStr::from_bytes(&child.raw.name));
            if !opts.alias_paths.is_empty() && opts.alias_paths.contains(&path) {
                child.descend = Descend::Omit;
                continue;
            }
            if !opts.exclude.is_empty() && opts.exclude.matches(&path, &child.raw.name) {
                child.descend = Descend::Skip(flags::EXCLUDED);
                continue;
            }
            if child.raw.is_trigger {
                child.descend = Descend::Skip(flags::SKIPPED_MOUNT);
                continue;
            }
            let mut dev = child.raw.dev;
            if child.raw.is_mount_point || dev != task.dev {
                // The listed attributes may belong to the covered directory; look at
                // what is actually mounted there.
                match self.lister.stat(&path) {
                    Ok(st) => dev = st.dev,
                    Err(_) => {
                        child.descend = Descend::Skip(flags::SKIPPED_MOUNT);
                        continue;
                    }
                }
            }
            if opts.stay_on_filesystem && !self.same_fs(dev) {
                child.descend = Descend::Skip(flags::SKIPPED_MOUNT);
                continue;
            }
            child.descend = Descend::Dev(dev);
        }
    }

    fn wait_for(&self, slot: u64) -> Done {
        let mut st = self.state.lock().unwrap();
        loop {
            if let Some(listing) = st.results.remove(&slot) {
                st.buffered -= listing.1.as_ref().map(|c| c.len()).unwrap_or(0);
                st.awaited = None;
                drop(st);
                self.work.notify_all();
                return listing;
            }
            if st.awaited != Some(slot) {
                st.awaited = Some(slot);
                self.work.notify_all();
            }
            st = self.done.wait(st).unwrap();
        }
    }

    fn shutdown(&self) {
        self.state.lock().unwrap().shutdown = true;
        self.work.notify_all();
        self.done.notify_all();
    }
}

struct Frame {
    children: Vec<Child>,
    next: usize,
    key: Vec<u32>,
}

/// Emitting side: converts entries to records, tracks volumes, hardlinks and counts.
struct Emitter<'s> {
    sink: &'s mut dyn RecordSink,
    report: WalkReport,
    dev_index: HashMap<u64, u16>,
    hardlinks: HashSet<(u64, u64)>,
    rec: Record,
    max_issues: usize,
    progress: Option<&'s Progress>,
}

impl Emitter<'_> {
    fn issue(&mut self, kind: IssueKind, path: &Path, err: &io::Error) {
        self.report.issue_count += 1;
        if let Some(p) = self.progress {
            p.issues.fetch_add(1, AtomicOrdering::Relaxed);
        }
        if self.report.issues.len() < self.max_issues {
            self.report.issues.push(WalkIssue {
                kind,
                path: path.to_string_lossy().into_owned(),
                message: err.to_string(),
            });
        }
    }

    fn emit(&mut self, depth: u16, raw: &RawEntry, extra_flags: u8, path: &Path) -> io::Result<()> {
        let next_index = self.dev_index.len() as u16;
        let vol = *self.dev_index.entry(raw.dev).or_insert_with(|| {
            self.report.devices.push(raw.dev);
            next_index
        });
        let mut fl = extra_flags;
        if raw.error.is_some() {
            fl |= flags::STAT_ERROR;
        }
        if raw.kind != Kind::Dir && raw.nlink > 1 && !self.hardlinks.insert((raw.dev, raw.ino)) {
            fl |= flags::HARDLINK_DUP;
        }
        let rec = &mut self.rec;
        rec.depth = depth;
        rec.name.clear();
        rec.name.extend_from_slice(&raw.name);
        rec.kind = raw.kind;
        rec.flags = fl;
        rec.size = raw.size;
        rec.alloc = raw.alloc;
        rec.mtime = raw.mtime;
        rec.btime = raw.btime;
        rec.ino = raw.ino;
        rec.nlink = raw.nlink;
        rec.vol = vol;

        let r = &mut self.report;
        r.entries += 1;
        match raw.kind {
            Kind::File => r.files += 1,
            Kind::Dir => r.dirs += 1,
            Kind::Symlink => r.symlinks += 1,
            Kind::Other => r.others += 1,
        }
        if fl & flags::HARDLINK_DUP != 0 {
            r.hardlink_duplicates += 1;
        } else {
            r.logical_bytes += raw.size;
            r.allocated_bytes += raw.alloc;
        }
        if fl & flags::INACCESSIBLE != 0 {
            r.inaccessible_dirs += 1;
        }
        if fl & flags::STAT_ERROR != 0 {
            r.stat_errors += 1;
        }
        if fl & flags::SKIPPED_MOUNT != 0 {
            r.skipped_mounts += 1;
        }
        if fl & flags::EXCLUDED != 0 {
            r.excluded += 1;
        }
        if let Some(p) = self.progress {
            let n = p.entries.fetch_add(1, AtomicOrdering::Relaxed);
            if fl & flags::HARDLINK_DUP == 0 {
                p.bytes_alloc.fetch_add(raw.alloc, AtomicOrdering::Relaxed);
            }
            if raw.kind == Kind::Dir {
                p.dirs.fetch_add(1, AtomicOrdering::Relaxed);
            }
            if n % 4096 == 0
                && let Ok(mut cur) = p.current.try_lock() {
                    cur.clear();
                    cur.push_str(&path.to_string_lossy());
                }
        }
        self.sink.record(&self.rec, path)
    }
}

/// Walks `root` and streams records to `sink` in canonical order.
///
/// Per-entry problems (permission denied, vanished files) are recorded in the
/// report and never abort the walk. Setting `cancel` stops the walk promptly; the
/// returned report then has `cancelled = true`. An error from the sink aborts.
pub fn walk(
    root: &Path,
    lister: &dyn DirLister,
    opts: &WalkOptions,
    sink: &mut dyn RecordSink,
    cancel: &AtomicBool,
    progress: Option<&Progress>,
) -> Result<WalkReport, WalkError> {
    let started = Instant::now();
    let mut root_raw = lister.stat(root).map_err(|source| WalkError::Root {
        path: root.display().to_string(),
        source,
    })?;
    root_raw.name = root.as_os_str().as_bytes().to_vec();

    let shared = Arc::new(Shared {
        state: Mutex::new(State::default()),
        work: Condvar::new(),
        done: Condvar::new(),
        lister,
        opts,
        root_dev: root_raw.dev,
        cancel,
    });

    let mut report = WalkReport::default();
    let mut dev_index = HashMap::new();
    for &d in &opts.known_devices {
        if let std::collections::hash_map::Entry::Vacant(e) = dev_index.entry(d) {
            e.insert(report.devices.len() as u16);
            report.devices.push(d);
        }
    }
    let mut emitter = Emitter {
        sink,
        report,
        dev_index,
        hardlinks: HashSet::new(),
        rec: Record::default(),
        max_issues: opts.max_issues,
        progress,
    };

    let result = std::thread::scope(|scope| {
        for _ in 0..opts.effective_threads() {
            let shared = &shared;
            let start = opts.on_thread_start;
            scope.spawn(move || {
                if let Some(f) = start {
                    f();
                }
                shared.worker()
            });
        }
        let r = consume(&shared, root, root_raw, &mut emitter, cancel);
        shared.shutdown();
        r
    });

    let mut report = emitter.report;
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    report.hardlink_inodes = emitter.hardlinks.len() as u64;
    {
        let st = shared.state.lock().unwrap();
        report.peak_pending_dirs = st.peak_pending as u64;
        report.peak_buffered_entries = st.peak_buffered as u64;
    }
    match result {
        Ok(()) => {}
        Err(ConsumeError::Cancelled) => report.cancelled = true,
        Err(ConsumeError::Sink(e)) => return Err(WalkError::Sink(e)),
    }
    Ok(report)
}

enum ConsumeError {
    Cancelled,
    Sink(io::Error),
}

fn consume(
    shared: &Shared<'_>,
    root: &Path,
    root_raw: RawEntry,
    em: &mut Emitter<'_>,
    cancel: &AtomicBool,
) -> Result<(), ConsumeError> {
    let mut path = root.to_path_buf();
    let mut frames: Vec<Frame> = Vec::new();

    if root_raw.kind == Kind::Dir {
        {
            let mut st = shared.state.lock().unwrap();
            st.next_slot = 1;
            st.pending.push(Task { key: Vec::new(), slot: 0, path: root.to_path_buf(), dev: root_raw.dev });
        }
        shared.work.notify_all();
        let (key, listing) = shared.wait_for(0);
        let children = open_listing(em, &root_raw, listing, 0, &path)?;
        frames.push(Frame { children, next: 0, key });
    } else {
        em.emit(0, &root_raw, 0, &path).map_err(ConsumeError::Sink)?;
    }

    while let Some(frame) = frames.last_mut() {
        if cancel.load(AtomicOrdering::Relaxed) {
            return Err(ConsumeError::Cancelled);
        }
        let i = frame.next;
        if i >= frame.children.len() {
            frames.pop();
            path.pop();
            continue;
        }
        frame.next += 1;
        if let Descend::Dev(_) = frame.children[i].descend {
            // Release the next window of this directory's subdirectories.
            let mut st = shared.state.lock().unwrap();
            st.enqueue_window(&frame.key, &path, &mut frame.children, i);
            drop(st);
            shared.work.notify_all();
        }
        let depth = frames.len() as u16;
        let frame = frames.last_mut().unwrap();
        let child = std::mem::take(&mut frame.children[i].raw);
        let descend = frame.children[i].descend;
        path.push(OsStr::from_bytes(&child.name));
        if let Some(errno) = child.error {
            em.issue(IssueKind::StatFailed, &path, &io::Error::from_raw_os_error(errno));
        }
        match descend {
            Descend::Queued(slot) => {
                let (key, listing) = shared.wait_for(slot);
                let children = open_listing(em, &child, listing, depth, &path)?;
                frames.push(Frame { children, next: 0, key });
                continue; // path stays pushed while inside this directory
            }
            Descend::Dev(_) => unreachable!("queued above"),
            Descend::No => em.emit(depth, &child, 0, &path).map_err(ConsumeError::Sink)?,
            Descend::Skip(flag) => em.emit(depth, &child, flag, &path).map_err(ConsumeError::Sink)?,
            Descend::Omit => {}
        }
        path.pop();
    }
    Ok(())
}

/// Emits a directory record once its listing is known, returning its children.
fn open_listing(
    em: &mut Emitter<'_>,
    raw: &RawEntry,
    listing: Listing,
    depth: u16,
    path: &Path,
) -> Result<Vec<Child>, ConsumeError> {
    match listing {
        Ok(children) => {
            em.emit(depth, raw, 0, path).map_err(ConsumeError::Sink)?;
            Ok(children)
        }
        Err(e) if e.kind() == io::ErrorKind::Interrupted => Err(ConsumeError::Cancelled),
        Err(e) => {
            let kind = if e.kind() == io::ErrorKind::PermissionDenied {
                IssueKind::PermissionDenied
            } else {
                IssueKind::ListFailed
            };
            em.issue(kind, path, &e);
            em.emit(depth, raw, flags::INACCESSIBLE, path).map_err(ConsumeError::Sink)?;
            Ok(Vec::new())
        }
    }
}

/// Portable lister built on `std::fs::read_dir` + `lstat`.
pub struct StdLister;

impl StdLister {
    fn raw_from_metadata(name: Vec<u8>, md: &std::fs::Metadata) -> RawEntry {
        use std::os::unix::fs::MetadataExt;
        let ft = md.file_type();
        let kind = if ft.is_dir() {
            Kind::Dir
        } else if ft.is_file() {
            Kind::File
        } else if ft.is_symlink() {
            Kind::Symlink
        } else {
            Kind::Other
        };
        let btime = md
            .created()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        RawEntry {
            name,
            kind,
            size: md.len(),
            alloc: md.blocks().saturating_mul(512),
            mtime: md.mtime(),
            btime,
            ino: md.ino(),
            nlink: md.nlink() as u32,
            dev: md.dev(),
            is_mount_point: false,
            is_trigger: false,
            error: None,
        }
    }
}

impl DirLister for StdLister {
    fn name(&self) -> &'static str {
        "readdir"
    }

    fn stat(&self, path: &Path) -> io::Result<RawEntry> {
        let md = std::fs::symlink_metadata(path)?;
        Ok(Self::raw_from_metadata(path.as_os_str().as_bytes().to_vec(), &md))
    }

    fn list(&self, dir: &Path, out: &mut Vec<RawEntry>) -> io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().as_bytes().to_vec();
            match entry.metadata() {
                Ok(md) => out.push(Self::raw_from_metadata(name, &md)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {} // vanished during the scan
                Err(e) => out.push(RawEntry {
                    name,
                    error: Some(e.raw_os_error().unwrap_or(libc_eio())),
                    ..RawEntry::default()
                }),
            }
        }
        Ok(())
    }
}

fn libc_eio() -> i32 {
    5
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TestDir;

    fn collect(root: &Path, opts: &WalkOptions) -> (Vec<(String, Record)>, WalkReport) {
        let mut out = Vec::new();
        let mut sink = |rec: &Record, path: &Path| {
            out.push((path.to_string_lossy().into_owned(), rec.clone()));
            Ok(())
        };
        let cancel = AtomicBool::new(false);
        let report = walk(root, &StdLister, opts, &mut sink, &cancel, None).unwrap();
        (out, report)
    }

    #[test]
    fn emits_canonical_order_and_totals() {
        let t = TestDir::new("walk-order");
        t.file("b.txt", 10);
        t.file("a/z.bin", 3);
        t.file("a/b/c.dat", 5);
        t.file("a-b/x", 1);
        t.dir("empty");
        let (records, report) = collect(t.path(), &WalkOptions { threads: 4, ..Default::default() });
        let rel: Vec<String> = records
            .iter()
            .map(|(p, _)| p.strip_prefix(&*t.path().to_string_lossy()).unwrap().to_string())
            .collect();
        assert_eq!(rel, ["", "/a", "/a/b", "/a/b/c.dat", "/a/z.bin", "/a-b", "/a-b/x", "/b.txt", "/empty"]);
        let depths: Vec<u16> = records.iter().map(|(_, r)| r.depth).collect();
        assert_eq!(depths, [0, 1, 2, 3, 2, 1, 2, 1, 1]);
        assert_eq!(report.files, 4);
        assert_eq!(report.dirs, 5);
        assert_eq!(report.logical_bytes - records.iter().filter(|(_, r)| r.is_dir()).map(|(_, r)| r.size).sum::<u64>(), 19);
        assert!(!report.is_partial());
    }

    #[test]
    fn deterministic_across_thread_counts_and_budgets() {
        let t = TestDir::new("walk-determinism");
        for i in 0..20 {
            for j in 0..15 {
                t.file(&format!("d{i:02}/s{j:02}/f.txt"), (i * j) as usize);
            }
        }
        let base = collect(t.path(), &WalkOptions { threads: 1, ..Default::default() }).0;
        for (threads, budget) in [(2, 1), (8, 10), (16, 100_000)] {
            let opts = WalkOptions { threads, buffer_budget: budget, ..Default::default() };
            let other = collect(t.path(), &opts).0;
            assert_eq!(base.len(), other.len());
            for (a, b) in base.iter().zip(other.iter()) {
                assert_eq!(a.0, b.0);
            }
        }
    }

    #[test]
    fn hardlinks_counted_once() {
        let t = TestDir::new("walk-hardlinks");
        t.file("a/original", 5000);
        std::fs::hard_link(t.path().join("a/original"), t.path().join("b-link")).unwrap();
        let (records, report) = collect(t.path(), &WalkOptions::default());
        let dups: Vec<&String> =
            records.iter().filter(|(_, r)| r.has(flags::HARDLINK_DUP)).map(|(p, _)| p).collect();
        assert_eq!(dups.len(), 1);
        assert!(dups[0].ends_with("/b-link"), "first path in canonical order owns the allocation");
        assert_eq!(report.hardlink_duplicates, 1);
    }

    #[test]
    fn symlinks_are_not_followed() {
        let t = TestDir::new("walk-symlinks");
        t.file("real/data", 100);
        std::os::unix::fs::symlink(t.path().join("real"), t.path().join("link")).unwrap();
        let (records, _) = collect(t.path(), &WalkOptions::default());
        let link = records.iter().find(|(p, _)| p.ends_with("/link")).unwrap();
        assert_eq!(link.1.kind, Kind::Symlink);
        assert!(!records.iter().any(|(p, _)| p.ends_with("/link/data")));
    }

    #[test]
    fn excludes_and_aliases() {
        let t = TestDir::new("walk-exclude");
        t.file("keep/a", 1);
        t.file("node_modules/x/y", 1);
        t.file("skipme/z", 1);
        t.file("alias/q", 1);
        let mut opts = WalkOptions {
            exclude: ExcludeRules::from_patterns(&[
                "node_modules".into(),
                t.path().join("skipme").to_string_lossy().into_owned(),
            ]),
            ..Default::default()
        };
        opts.alias_paths.insert(t.path().join("alias"));
        let (records, report) = collect(t.path(), &opts);
        let names: Vec<&str> = records.iter().map(|(p, _)| p.rsplit('/').next().unwrap()).collect();
        assert!(names.contains(&"keep") && names.contains(&"a"));
        assert!(names.contains(&"node_modules"), "excluded dirs stay visible as markers");
        assert!(!names.contains(&"y") && !names.contains(&"z"));
        assert!(!names.contains(&"alias") && !names.contains(&"q"));
        assert_eq!(report.excluded, 2);
    }

    #[test]
    fn unreadable_directory_is_partial_not_fatal() {
        use std::os::unix::fs::PermissionsExt;
        let t = TestDir::new("walk-perm");
        t.file("open/a", 1);
        t.file("locked/secret", 1);
        let locked = t.path().join("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let (records, report) = collect(t.path(), &WalkOptions::default());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if unsafe_is_root() {
            return; // root can read anything; nothing to assert
        }
        let rec = records.iter().find(|(p, _)| p.ends_with("/locked")).unwrap();
        assert!(rec.1.has(flags::INACCESSIBLE));
        assert!(report.is_partial());
        assert_eq!(report.issues[0].kind, IssueKind::PermissionDenied);
    }

    fn unsafe_is_root() -> bool {
        std::env::var("USER").map(|u| u == "root").unwrap_or(false)
    }

    #[test]
    fn cancellation_stops_walk() {
        let t = TestDir::new("walk-cancel");
        for i in 0..50 {
            t.file(&format!("d{i}/f"), 1);
        }
        let cancel = AtomicBool::new(false);
        let mut n = 0;
        let mut sink = |_: &Record, _: &Path| {
            n += 1;
            if n == 5 {
                cancel.store(true, AtomicOrdering::Relaxed);
            }
            Ok(())
        };
        let report = walk(t.path(), &StdLister, &WalkOptions::default(), &mut sink, &cancel, None).unwrap();
        assert!(report.cancelled);
        assert!(report.entries < 100);
    }

    #[test]
    fn sink_error_aborts() {
        let t = TestDir::new("walk-sinkerr");
        t.file("a", 1);
        let cancel = AtomicBool::new(false);
        let mut sink = |_: &Record, _: &Path| Err(io::Error::other("disk full"));
        let err = walk(t.path(), &StdLister, &WalkOptions::default(), &mut sink, &cancel, None).unwrap_err();
        assert!(matches!(err, WalkError::Sink(_)));
    }

    /// In-memory lister: paths map to (entry, children).
    struct FakeLister {
        entries: HashMap<PathBuf, (RawEntry, Vec<String>)>,
    }

    impl FakeLister {
        fn new() -> Self {
            FakeLister { entries: HashMap::new() }
        }
        fn add(&mut self, path: &str, kind: Kind, dev: u64, mount: bool, trigger: bool) {
            let p = PathBuf::from(path);
            let name = p.file_name().map(|n| n.as_bytes().to_vec()).unwrap_or_else(|| path.as_bytes().to_vec());
            let e = RawEntry { name, kind, dev, size: 10, alloc: 4096, nlink: 1, is_mount_point: mount, is_trigger: trigger, ..Default::default() };
            if let Some(parent) = p.parent()
                && let Some(pe) = self.entries.get_mut(parent) {
                    pe.1.push(p.file_name().unwrap().to_string_lossy().into_owned());
                }
            self.entries.insert(p, (e, Vec::new()));
        }
    }

    impl DirLister for FakeLister {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn stat(&self, path: &Path) -> io::Result<RawEntry> {
            self.entries.get(path).map(|e| e.0.clone()).ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }
        fn list(&self, dir: &Path, out: &mut Vec<RawEntry>) -> io::Result<()> {
            let (_, children) = self.entries.get(dir).ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            for c in children {
                // A child listed but gone by the time it is examined: skipped silently.
                if let Some(e) = self.entries.get(&dir.join(c)) {
                    let mut e = e.0.clone();
                    if e.is_mount_point {
                        // Like getattrlistbulk: attributes of the covered directory.
                        e.dev = self.entries[dir].0.dev;
                    }
                    out.push(e);
                }
            }
            Ok(())
        }
    }

    #[test]
    fn filesystem_boundaries() {
        let mut l = FakeLister::new();
        l.add("/root", Kind::Dir, 1, false, false);
        l.add("/root/local", Kind::Dir, 1, false, false);
        l.add("/root/local/f", Kind::File, 1, false, false);
        l.add("/root/usb", Kind::Dir, 2, true, false); // other filesystem
        l.add("/root/usb/photo", Kind::File, 2, false, false);
        l.add("/root/data", Kind::Dir, 3, true, false); // same volume group
        l.add("/root/data/doc", Kind::File, 3, false, false);
        l.add("/root/net", Kind::Dir, 1, false, true); // automount trigger
        l.add("/root/net/share", Kind::File, 9, false, false);
        let mut seen = Vec::new();
        let mut sink = |r: &Record, p: &Path| {
            seen.push((p.to_string_lossy().into_owned(), r.flags));
            Ok(())
        };
        let opts = WalkOptions { same_filesystem_devs: vec![3], ..Default::default() };
        let cancel = AtomicBool::new(false);
        let report = walk(Path::new("/root"), &l, &opts, &mut sink, &cancel, None).unwrap();
        let paths: Vec<&str> = seen.iter().map(|(p, _)| p.as_str()).collect();
        assert!(paths.contains(&"/root/local/f"));
        assert!(paths.contains(&"/root/data/doc"), "volume-group device is entered");
        assert!(!paths.contains(&"/root/usb/photo"), "other filesystem is not entered");
        assert!(!paths.contains(&"/root/net/share"), "triggers are never entered");
        let usb = seen.iter().find(|(p, _)| p == "/root/usb").unwrap();
        assert!(usb.1 & flags::SKIPPED_MOUNT != 0);
        assert_eq!(report.skipped_mounts, 2);
        assert!(!report.is_partial());

        // Without the boundary rule, the other filesystem is entered.
        let mut n = 0;
        let mut count = |_: &Record, _: &Path| {
            n += 1;
            Ok(())
        };
        let all = WalkOptions { stay_on_filesystem: false, ..Default::default() };
        walk(Path::new("/root"), &l, &all, &mut count, &cancel, None).unwrap();
        assert_eq!(n, 9 - 1, "everything except the trigger's contents");
    }

    #[test]
    fn vanished_children_are_skipped() {
        let mut l = FakeLister::new();
        l.add("/r", Kind::Dir, 1, false, false);
        l.add("/r/a", Kind::File, 1, false, false);
        l.add("/r/b", Kind::Dir, 1, false, false);
        l.entries.get_mut(Path::new("/r")).unwrap().1.push("ghost".into()); // listed, never existed
        let mut n = 0;
        let mut sink = |_: &Record, _: &Path| {
            n += 1;
            Ok(())
        };
        let cancel = AtomicBool::new(false);
        let r = walk(Path::new("/r"), &l, &WalkOptions::default(), &mut sink, &cancel, None).unwrap();
        assert_eq!(n, 3);
        assert_eq!(r.issue_count, 0);
    }
}
