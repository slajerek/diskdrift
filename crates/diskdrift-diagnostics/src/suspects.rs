//! High-value storage locations (spec 4.2) and a bounded-time tree measurement.

use crate::measurement::Envelope;
use diskdrift_core::record::{Kind, Record, flags};
use diskdrift_core::walk::{self, DirLister, WalkOptions};
use serde::{Serialize, Serializer};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// How risky it is to remove data from a location.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Risk {
    /// Regenerated automatically; removing it costs time, not data.
    Low,
    /// Usually regenerable, but may hold state or offline data.
    Medium,
    /// User data or hard-to-recreate data.
    High,
    /// Managed by macOS; inspect only.
    SystemManaged,
}

impl Risk {
    pub fn label(self) -> &'static str {
        match self {
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
            Risk::SystemManaged => "system",
        }
    }
}

/// A known location worth measuring.
#[derive(Debug, Clone)]
pub struct Suspect {
    pub id: &'static str,
    pub category: &'static str,
    pub label: &'static str,
    /// Absolute path or `~/...`.
    pub path: &'static str,
    /// Id of a suspect whose path contains this one; measured during the parent's
    /// walk and never added to it.
    pub within: Option<&'static str>,
    pub risk: Risk,
    pub advice: &'static str,
}

macro_rules! s {
    ($id:literal, $cat:literal, $label:literal, $path:literal, $within:expr, $risk:ident, $advice:literal) => {
        Suspect {
            id: $id,
            category: $cat,
            label: $label,
            path: $path,
            within: $within,
            risk: Risk::$risk,
            advice: $advice,
        }
    };
}

/// The macOS catalog. Order is irrelevant; findings are ranked by size.
pub fn macos_catalog() -> Vec<Suspect> {
    vec![
        s!(
            "user-caches",
            "Caches",
            "User caches",
            "~/Library/Caches",
            None,
            Medium,
            "Apps rebuild most caches, but some hold offline data or are in use. Inspect the largest subfolders and prefer each app's own cache setting."
        ),
        s!(
            "homebrew-cache",
            "Caches",
            "Homebrew downloads",
            "~/Library/Caches/Homebrew",
            Some("user-caches"),
            Low,
            "Old bottles and downloads. Review `brew cleanup -n`, then run `brew cleanup`."
        ),
        s!(
            "pip-cache",
            "Caches",
            "pip cache",
            "~/Library/Caches/pip",
            Some("user-caches"),
            Low,
            "Python package downloads. `pip cache purge` clears it."
        ),
        s!(
            "xcode-deriveddata",
            "Developer",
            "Xcode DerivedData",
            "~/Library/Developer/Xcode/DerivedData",
            None,
            Low,
            "Build intermediates that Xcode regenerates. Remove with Xcode closed."
        ),
        s!(
            "xcode-archives",
            "Developer",
            "Xcode Archives",
            "~/Library/Developer/Xcode/Archives",
            None,
            High,
            "Archived app builds, needed to symbolicate crash reports of shipped versions. Review in Xcode Organizer."
        ),
        s!(
            "xcode-ios-devicesupport",
            "Developer",
            "Xcode iOS DeviceSupport",
            "~/Library/Developer/Xcode/iOS DeviceSupport",
            None,
            Low,
            "Debug symbols per device OS version; Xcode re-creates them when a device is connected. Old versions can go."
        ),
        s!(
            "xcode-watchos-devicesupport",
            "Developer",
            "Xcode watchOS DeviceSupport",
            "~/Library/Developer/Xcode/watchOS DeviceSupport",
            None,
            Low,
            "Debug symbols per device OS version; old versions can go."
        ),
        s!(
            "coresimulator-devices",
            "Developer",
            "Simulator devices",
            "~/Library/Developer/CoreSimulator/Devices",
            None,
            Medium,
            "Simulator data. `xcrun simctl delete unavailable` removes simulators of runtimes no longer installed."
        ),
        s!(
            "coresimulator-caches",
            "Developer",
            "Simulator caches",
            "~/Library/Developer/CoreSimulator/Caches",
            None,
            Low,
            "Simulator dyld caches; regenerated on demand."
        ),
        s!(
            "simulator-runtimes",
            "Developer",
            "Simulator runtimes (system)",
            "/Library/Developer/CoreSimulator",
            None,
            Medium,
            "Installed simulator runtimes. Manage them in Xcode > Settings > Components or with `xcrun simctl runtime list`. Mounted runtime images are separate volumes and not counted here."
        ),
        s!(
            "ios-backups",
            "Backups",
            "iPhone/iPad backups",
            "~/Library/Application Support/MobileSync/Backup",
            Some("app-support"),
            High,
            "Device backups. Manage them in Finder (select the device > Manage Backups); delete only backups you no longer need."
        ),
        s!(
            "app-support",
            "Application data",
            "Application Support",
            "~/Library/Application Support",
            None,
            Medium,
            "Per-app data (browsers, chat apps, game libraries, ML models). Check the largest subfolders and clean up inside each app."
        ),
        s!(
            "containers",
            "Application data",
            "App containers",
            "~/Library/Containers",
            None,
            Medium,
            "Data of sandboxed apps. Check the largest containers; remove data inside the owning app."
        ),
        s!(
            "group-containers",
            "Application data",
            "Group containers",
            "~/Library/Group Containers",
            None,
            Medium,
            "Data shared between apps of one vendor (Office, Messages, ...)."
        ),
        s!(
            "docker",
            "Containers & VMs",
            "Docker Desktop disk image",
            "~/Library/Containers/com.docker.docker/Data/vms",
            Some("containers"),
            Medium,
            "Sparse virtual disk: apparent size can far exceed allocated bytes. Check `docker system df`, prune what you do not need, or shrink the disk in Docker Desktop settings."
        ),
        s!(
            "utm",
            "Containers & VMs",
            "UTM virtual machines",
            "~/Library/Containers/com.utmapp.UTM/Data/Documents",
            Some("containers"),
            High,
            "Virtual machine images. Sparse images may allocate less than their apparent size."
        ),
        s!(
            "parallels",
            "Containers & VMs",
            "Parallels virtual machines",
            "~/Parallels",
            None,
            High,
            "Virtual machine images. Use Parallels' own 'Reclaim disk space' first."
        ),
        s!(
            "vmware",
            "Containers & VMs",
            "VMware virtual machines",
            "~/Virtual Machines.localized",
            None,
            High,
            "Virtual machine images."
        ),
        s!(
            "trash",
            "Trash",
            "Trash",
            "~/.Trash",
            None,
            Low,
            "Files already deleted in Finder. Review and empty the Trash."
        ),
        s!(
            "user-logs",
            "Logs",
            "User logs",
            "~/Library/Logs",
            None,
            Low,
            "Application logs and crash reports; review large files in Console."
        ),
        s!(
            "system-logs",
            "Logs",
            "System logs",
            "/Library/Logs",
            None,
            Low,
            "System-wide logs and diagnostic reports."
        ),
        s!(
            "var-log",
            "Logs",
            "Unix logs",
            "/private/var/log",
            None,
            SystemManaged,
            "Managed by macOS log rotation; inspect only."
        ),
        s!(
            "var-folders",
            "Temporary",
            "Per-user temporary files",
            "/private/var/folders",
            None,
            SystemManaged,
            "Temporary files and caches managed by macOS, cleared at restart or by the OS. Do not delete while apps run."
        ),
        s!(
            "swap",
            "System",
            "Swap and sleep image",
            "/System/Volumes/VM",
            None,
            SystemManaged,
            "Virtual memory, managed by macOS. Informational only; restarting frees swap."
        ),
        s!(
            "spotlight",
            "System",
            "Spotlight index",
            "/System/Volumes/Data/.Spotlight-V100",
            None,
            SystemManaged,
            "Search index, managed by macOS. If it is unexpectedly large, rebuild it from Spotlight privacy settings."
        ),
        s!(
            "mail",
            "User data",
            "Mail",
            "~/Library/Mail",
            None,
            High,
            "Mail storage and attachments. Manage inside Mail."
        ),
        s!(
            "messages",
            "User data",
            "Messages attachments",
            "~/Library/Messages",
            None,
            High,
            "Message history and attachments. Review in System Settings > General > Storage > Messages."
        ),
        s!(
            "photos",
            "User data",
            "Photos library",
            "~/Pictures/Photos Library.photoslibrary",
            None,
            High,
            "Photo library. 'Optimize Mac Storage' in Photos settings keeps originals in iCloud only."
        ),
        s!(
            "downloads",
            "User data",
            "Downloads",
            "~/Downloads",
            None,
            High,
            "Your downloaded files; review manually."
        ),
        s!(
            "dot-cache",
            "Developer",
            "~/.cache",
            "~/.cache",
            None,
            Medium,
            "Tool caches (Hugging Face models, pre-commit, uv, ...)."
        ),
        s!(
            "huggingface",
            "Developer",
            "Hugging Face cache",
            "~/.cache/huggingface",
            Some("dot-cache"),
            Medium,
            "Downloaded models and datasets. `hf cache scan` / `hf cache delete` manage it."
        ),
        s!(
            "ollama",
            "Developer",
            "Ollama models",
            "~/.ollama/models",
            None,
            Medium,
            "Local LLM weights. `ollama list` / `ollama rm` manage them."
        ),
        s!(
            "npm-cache",
            "Developer",
            "npm cache",
            "~/.npm",
            None,
            Low,
            "Package download cache. `npm cache clean --force` clears it."
        ),
        s!(
            "gradle-cache",
            "Developer",
            "Gradle caches",
            "~/.gradle/caches",
            None,
            Low,
            "Build and dependency caches; regenerated by the next build."
        ),
        s!(
            "maven-repo",
            "Developer",
            "Maven repository",
            "~/.m2/repository",
            None,
            Low,
            "Downloaded dependencies; re-downloaded when needed."
        ),
        s!(
            "cargo-registry",
            "Developer",
            "Cargo registry",
            "~/.cargo/registry",
            None,
            Low,
            "Crate sources and downloads; re-downloaded when needed."
        ),
        s!(
            "rustup-toolchains",
            "Developer",
            "Rust toolchains",
            "~/.rustup/toolchains",
            None,
            Medium,
            "Installed toolchains. `rustup toolchain list` / `rustup toolchain uninstall`."
        ),
        s!(
            "go-modcache",
            "Developer",
            "Go module cache",
            "~/go/pkg/mod",
            None,
            Low,
            "`go clean -modcache` clears it."
        ),
    ]
}

/// Locations appropriate for the current platform. The macOS catalog is not
/// used for other operating systems because its paths and advice are not valid
/// there.
pub fn catalog() -> Vec<Suspect> {
    #[cfg(target_os = "macos")]
    {
        macos_catalog()
    }
    #[cfg(not(target_os = "macos"))]
    {
        vec![
            s!(
                "downloads",
                "User data",
                "Downloads",
                "~/Downloads",
                None,
                High,
                "Review downloaded files manually."
            ),
            s!(
                "user-cache",
                "Caches",
                "User cache",
                "~/.cache",
                None,
                Medium,
                "Review large application and tool caches before removing them."
            ),
            s!(
                "documents",
                "User data",
                "Documents",
                "~/Documents",
                None,
                High,
                "Review personal documents manually."
            ),
        ]
    }
}

/// Outcome of measuring one location.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MeasureStatus {
    /// Path does not exist.
    Absent,
    /// The location itself could not be read.
    Inaccessible,
    /// Every entry was measured.
    Complete,
    /// Measured, but some subdirectories could not be read.
    Partial,
    /// The time budget ran out; totals are a lower bound.
    TimedOut,
    /// The user skipped it; totals are a lower bound.
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Measure {
    pub status: MeasureStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logical_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allocated_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entries: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inaccessible_dirs: Option<u64>,
    #[serde(skip)]
    pub logical_known: bool,
    #[serde(skip)]
    pub allocated_known: bool,
    pub measurement: Envelope,
}

impl Measure {
    fn new(status: MeasureStatus) -> Measure {
        let known = !matches!(
            status,
            MeasureStatus::Absent | MeasureStatus::Inaccessible | MeasureStatus::Skipped
        );
        Measure {
            status,
            logical_bytes: known.then_some(0),
            allocated_bytes: known.then_some(0),
            entries: known.then_some(0),
            inaccessible_dirs: known.then_some(0),
            logical_known: false,
            allocated_known: false,
            measurement: envelope_for_status(status),
        }
    }

    pub fn absent() -> Measure {
        Measure::new(MeasureStatus::Absent)
    }
}

#[derive(Debug, Clone)]
pub struct LargeFile {
    pub path: String,
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
    pub measurement: Envelope,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LargeFileWire<'a> {
    path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    logical_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allocated_bytes: Option<u64>,
    #[serde(flatten)]
    measurement: &'a Envelope,
}

impl Serialize for LargeFile {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let known = matches!(
            self.measurement.quality,
            Some(crate::measurement::Quality::Observed)
                | Some(crate::measurement::Quality::Estimated)
                | Some(crate::measurement::Quality::LowerBound)
        );
        LargeFileWire {
            path: &self.path,
            logical_bytes: known.then_some(self.logical_bytes),
            allocated_bytes: known.then_some(self.allocated_bytes),
            measurement: &self.measurement,
        }
        .serialize(serializer)
    }
}

/// Keeps the N largest files seen across measurements, by allocated size.
pub struct LargeFiles {
    limit: usize,
    heap: BinaryHeap<Reverse<(u64, u64, String)>>,
    seen: HashSet<(u16, u64)>,
    measurements: HashMap<String, Envelope>,
}

impl LargeFiles {
    pub fn new(limit: usize) -> Self {
        LargeFiles {
            limit,
            heap: BinaryHeap::new(),
            seen: HashSet::new(),
            measurements: HashMap::new(),
        }
    }

    fn offer(&mut self, rec: &Record, path: &Path) {
        if rec.has(flags::STAT_ERROR)
            || !rec
                .available
                .contains(diskdrift_core::record::EntryFields::LOGICAL_SIZE)
            || !rec
                .available
                .contains(diskdrift_core::record::EntryFields::ALLOCATED_SIZE)
        {
            return;
        }
        if self.heap.len() >= self.limit
            && self
                .heap
                .peek()
                .is_some_and(|Reverse((a, _, _))| *a >= rec.alloc)
        {
            return;
        }
        if !self.seen.insert((rec.vol, rec.ino)) {
            return;
        }
        let path = path.to_string_lossy().into_owned();
        self.measurements.insert(
            path.clone(),
            Envelope::observed("filesystem", "path", "bounded read-only walk"),
        );
        self.heap.push(Reverse((rec.alloc, rec.size, path)));
        if self.heap.len() > self.limit {
            self.heap.pop();
        }
    }

    /// A file already known (from a saved scan), deduplicated by path.
    pub fn offer_known(&mut self, f: &LargeFile) {
        if !matches!(
            f.measurement.quality,
            Some(crate::measurement::Quality::Observed)
                | Some(crate::measurement::Quality::Estimated)
                | Some(crate::measurement::Quality::LowerBound)
        ) {
            return;
        }
        if self.heap.len() >= self.limit
            && self
                .heap
                .peek()
                .is_some_and(|Reverse((a, _, _))| *a >= f.allocated_bytes)
        {
            return;
        }
        if self.heap.iter().any(|Reverse((_, _, p))| *p == f.path) {
            return;
        }
        self.heap.push(Reverse((
            f.allocated_bytes,
            f.logical_bytes,
            f.path.clone(),
        )));
        self.measurements
            .insert(f.path.clone(), f.measurement.clone());
        if self.heap.len() > self.limit {
            self.heap.pop();
        }
    }

    pub fn into_sorted(self) -> Vec<LargeFile> {
        let measurements = self.measurements;
        let mut v: Vec<LargeFile> = self
            .heap
            .into_iter()
            .map(|Reverse((alloc, size, path))| LargeFile {
                measurement: measurements.get(&path).cloned().unwrap_or_default(),
                path,
                logical_bytes: size,
                allocated_bytes: alloc,
            })
            .collect();
        v.sort_by(|a, b| {
            (b.allocated_bytes, b.logical_bytes).cmp(&(a.allocated_bytes, a.logical_bytes))
        });
        v
    }
}

pub fn envelope_for_status(status: MeasureStatus) -> Envelope {
    match status {
        MeasureStatus::Complete => {
            Envelope::observed("filesystem", "path", "bounded read-only walk")
        }
        MeasureStatus::Partial | MeasureStatus::TimedOut | MeasureStatus::Skipped => {
            Envelope::lower_bound("filesystem", "path", "bounded read-only walk")
        }
        MeasureStatus::Absent | MeasureStatus::Inaccessible => Envelope::default(),
    }
}

fn envelope_for_measure(m: &Measure) -> Envelope {
    match m.status {
        MeasureStatus::Absent | MeasureStatus::Inaccessible => Envelope::default(),
        MeasureStatus::Partial | MeasureStatus::TimedOut | MeasureStatus::Skipped => {
            if m.logical_known || m.allocated_known {
                Envelope::lower_bound("filesystem", "path", "bounded read-only walk")
            } else {
                Envelope::default()
            }
        }
        MeasureStatus::Complete if m.logical_known && m.allocated_known => {
            Envelope::observed("filesystem", "path", "bounded read-only walk")
        }
        MeasureStatus::Complete if m.logical_known || m.allocated_known => {
            Envelope::lower_bound("filesystem", "path", "bounded read-only walk")
        }
        MeasureStatus::Complete => Envelope::default(),
    }
}

struct Sub {
    path: PathBuf,
    depth: u16,
    active: bool,
    seen: bool,
    measure: Measure,
}

/// Measures the tree at `root` until `deadline`, also totalling the given nested
/// paths (`subs`) in the same walk. `skip` ends this one measurement early.
#[allow(clippy::too_many_arguments)]
pub fn measure_tree(
    root: &Path,
    subs: &[PathBuf],
    lister: &dyn DirLister,
    opts: &WalkOptions,
    deadline: Instant,
    large: &mut LargeFiles,
    cancel: &AtomicBool,
    skip: &AtomicBool,
) -> (Measure, Vec<Measure>) {
    let absent = || {
        (
            Measure::new(MeasureStatus::Absent),
            subs.iter()
                .map(|_| Measure::new(MeasureStatus::Absent))
                .collect(),
        )
    };
    match std::fs::symlink_metadata(root) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return absent(),
        Err(_) => {
            return (
                Measure::new(MeasureStatus::Inaccessible),
                subs.iter()
                    .map(|_| Measure::new(MeasureStatus::Inaccessible))
                    .collect(),
            );
        }
        Ok(md) if md.file_type().is_symlink() => return absent(),
        Ok(_) => {}
    }
    let mut subs: Vec<Sub> = subs
        .iter()
        .map(|p| Sub {
            depth: p
                .strip_prefix(root)
                .map(|r| r.components().count() as u16)
                .unwrap_or(u16::MAX),
            path: p.clone(),
            active: false,
            seen: false,
            measure: Measure::new(MeasureStatus::Absent),
        })
        .collect();
    let mut total = Measure::new(MeasureStatus::Complete);
    let stop = AtomicBool::new(false);
    let mut timed_out = false;
    let mut skipped = false;
    let mut root_inaccessible = false;
    let mut n = 0u64;

    let mut sink = |rec: &Record, path: &Path| -> io::Result<()> {
        n += 1;
        if n.is_multiple_of(256) && skip.load(Ordering::Relaxed) {
            skipped = true;
            stop.store(true, Ordering::Relaxed);
        }
        if n.is_multiple_of(2048) && (Instant::now() >= deadline || cancel.load(Ordering::Relaxed))
        {
            timed_out = Instant::now() >= deadline;
            stop.store(true, Ordering::Relaxed);
        }
        if rec.depth == 0 && rec.has(flags::INACCESSIBLE) {
            root_inaccessible = true;
        }
        let inaccessible = rec.has(flags::INACCESSIBLE);
        add(&mut total, rec);
        for s in subs.iter_mut() {
            if s.active && rec.depth <= s.depth {
                s.active = false;
            }
            if !s.active && rec.depth == s.depth && path == s.path {
                s.active = true;
                s.seen = true;
                s.measure.status = if inaccessible {
                    MeasureStatus::Inaccessible
                } else {
                    MeasureStatus::Complete
                };
            } else if inaccessible && !s.seen && s.path.starts_with(path) {
                s.measure.status = MeasureStatus::Inaccessible;
            }
            if s.active {
                add(&mut s.measure, rec);
            }
        }
        if rec.kind == Kind::File && !rec.has(flags::HARDLINK_DUP) {
            large.offer(rec, path);
        }
        Ok(())
    };
    let result = walk::walk(root, lister, opts, &mut sink, &stop, None);
    let finish = |m: &mut Measure| {
        if m.status == MeasureStatus::Complete && m.inaccessible_dirs.is_some_and(|n| n > 0) {
            m.status = MeasureStatus::Partial;
        }
        if timed_out && matches!(m.status, MeasureStatus::Complete | MeasureStatus::Partial) {
            m.status = MeasureStatus::TimedOut;
        }
        if skipped
            && matches!(
                m.status,
                MeasureStatus::Complete | MeasureStatus::Partial | MeasureStatus::TimedOut
            )
        {
            m.status = MeasureStatus::Skipped;
        }
        m.measurement = envelope_for_measure(m);
    };
    match result {
        Ok(_) => {}
        Err(_) => total.status = MeasureStatus::Inaccessible,
    }
    if root_inaccessible {
        total.status = MeasureStatus::Inaccessible;
    }
    finish(&mut total);
    let subs = subs
        .into_iter()
        .map(|mut s| {
            if !s.seen && (timed_out || skipped) && s.measure.status == MeasureStatus::Absent {
                s.measure.status = if skipped {
                    MeasureStatus::Skipped
                } else {
                    MeasureStatus::TimedOut
                };
            }
            finish(&mut s.measure);
            s.measure
        })
        .collect();
    (total, subs)
}

fn add(m: &mut Measure, rec: &Record) {
    *m.entries.get_or_insert(0) += 1;
    if !rec.has(flags::STAT_ERROR)
        && rec
            .available
            .contains(diskdrift_core::record::EntryFields::LOGICAL_SIZE)
    {
        m.logical_known = true;
        *m.logical_bytes.get_or_insert(0) += rec.counted_size();
    }
    if !rec.has(flags::STAT_ERROR)
        && rec
            .available
            .contains(diskdrift_core::record::EntryFields::ALLOCATED_SIZE)
    {
        m.allocated_known = true;
        *m.allocated_bytes.get_or_insert(0) += rec.counted_alloc();
    }
    if rec.has(flags::INACCESSIBLE) {
        *m.inaccessible_dirs.get_or_insert(0) += 1;
    }
}

/// Expands a leading `~/` against `home`.
pub fn expand(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if path == "~" => home.to_path_buf(),
        None => PathBuf::from(path),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use diskdrift_core::native::NativeName;
    use diskdrift_core::testutil::TestDir;
    use diskdrift_core::walk::{DirLister, RawEntry, StdLister};
    use std::time::Duration;

    struct StatErrorLister {
        root: PathBuf,
    }

    impl DirLister for StatErrorLister {
        fn name(&self) -> &'static str {
            "stat-error-test"
        }

        fn stat(&self, path: &Path) -> io::Result<RawEntry> {
            if path == self.root {
                Ok(RawEntry {
                    name: NativeName::unix(self.root.to_string_lossy().as_bytes().to_vec()),
                    kind: Kind::Dir,
                    dev: 1,
                    ..Default::default()
                })
            } else {
                Err(io::Error::from(io::ErrorKind::NotFound))
            }
        }

        fn list(&self, dir: &Path, out: &mut Vec<RawEntry>) -> io::Result<()> {
            if dir == self.root {
                out.push(RawEntry {
                    name: NativeName::unix(b"bad".to_vec()),
                    kind: Kind::File,
                    size: 1234,
                    alloc: 4096,
                    mtime: 77,
                    error: Some(5),
                    ..Default::default()
                });
            }
            Ok(())
        }
    }

    #[test]
    fn measures_tree_and_nested_paths() {
        let t = TestDir::new("measure");
        t.file("a/x", 1000);
        t.file("a/nested/y", 5000);
        t.file("a/nested/deeper/z", 7000);
        t.file("a/nested-not/w", 3);
        let nested = t.path().join("a/nested");
        let missing = t.path().join("a/missing");
        let mut large = LargeFiles::new(2);
        let cancel = AtomicBool::new(false);
        let (total, subs) = measure_tree(
            &t.path().join("a"),
            &[nested, missing],
            &StdLister,
            &WalkOptions::default(),
            Instant::now() + Duration::from_secs(30),
            &mut large,
            &cancel,
            &AtomicBool::new(false),
        );
        assert_eq!(total.status, MeasureStatus::Complete);
        assert!(total.logical_bytes.unwrap() >= 13_003);
        assert_eq!(subs[0].status, MeasureStatus::Complete);
        assert!(
            subs[0].logical_bytes.unwrap() >= 12_000 && subs[0].logical_bytes.unwrap() < 13_000
        );
        assert_eq!(subs[1].status, MeasureStatus::Absent);
        let top = large.into_sorted();
        assert_eq!(top.len(), 2);
        assert!(top[0].path.ends_with("/z"));
    }

    #[test]
    fn absent_and_expired() {
        let t = TestDir::new("measure-absent");
        let mut large = LargeFiles::new(5);
        let cancel = AtomicBool::new(false);
        let (m, _) = measure_tree(
            &t.path().join("nope"),
            &[],
            &StdLister,
            &WalkOptions::default(),
            Instant::now(),
            &mut large,
            &cancel,
            &cancel,
        );
        assert_eq!(m.status, MeasureStatus::Absent);
        for i in 0..3000 {
            t.file(&format!("many/{i}"), 1);
        }
        let (m, _) = measure_tree(
            &t.path().join("many"),
            &[],
            &StdLister,
            &WalkOptions::default(),
            Instant::now(),
            &mut large,
            &cancel,
            &cancel,
        );
        assert_eq!(m.status, MeasureStatus::TimedOut);
        let far = Instant::now() + std::time::Duration::from_secs(60);
        let skip = AtomicBool::new(true);
        let (m, _) = measure_tree(
            &t.path().join("many"),
            &[],
            &StdLister,
            &WalkOptions::default(),
            far,
            &mut large,
            &cancel,
            &skip,
        );
        assert_eq!(m.status, MeasureStatus::Skipped);
        assert!(m.entries.unwrap() < 3001, "stopped early");
    }

    #[test]
    fn status_changes_update_aggregate_and_nested_envelopes() {
        let t = TestDir::new("measure-envelope-status");
        for i in 0..512 {
            t.file(&format!("nested/{i}"), 1);
        }
        let nested = t.path().join("nested");
        let mut large = LargeFiles::new(1);
        let cancel = AtomicBool::new(false);
        let skip = AtomicBool::new(true);
        let (total, subs) = measure_tree(
            t.path(),
            &[nested],
            &StdLister,
            &WalkOptions::default(),
            Instant::now() + Duration::from_secs(30),
            &mut large,
            &cancel,
            &skip,
        );

        assert_eq!(total.status, MeasureStatus::Skipped);
        assert_eq!(subs[0].status, MeasureStatus::Skipped);
        assert_eq!(
            total.measurement.quality,
            Some(crate::measurement::Quality::LowerBound)
        );
        assert_eq!(
            subs[0].measurement.quality,
            Some(crate::measurement::Quality::LowerBound)
        );
    }

    #[test]
    fn stat_error_records_make_live_and_nested_measurements_non_observed() {
        let t = TestDir::new("measure-stat-error");
        t.dir("nested");
        t.file("nested/bad", 1);
        let lister = StatErrorLister {
            root: t.path().to_path_buf(),
        };
        let mut large = LargeFiles::new(5);
        let (total, nested) = measure_tree(
            t.path(),
            &[t.path().join("nested")],
            &lister,
            &WalkOptions::default(),
            Instant::now() + Duration::from_secs(30),
            &mut large,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        assert_ne!(
            total.measurement.quality,
            Some(crate::measurement::Quality::Observed)
        );
        assert_ne!(
            nested[0].measurement.quality,
            Some(crate::measurement::Quality::Observed)
        );
        assert!(
            large.into_sorted().is_empty(),
            "unavailable sizes are not top files"
        );
    }

    #[test]
    fn catalog_is_consistent() {
        let cat = catalog();
        let ids: HashSet<&str> = cat.iter().map(|s| s.id).collect();
        assert_eq!(ids.len(), cat.len(), "unique ids");
        let home = Path::new("/Users/test");
        for s in &cat {
            if let Some(parent) = s.within {
                let p = cat.iter().find(|x| x.id == parent).expect("parent exists");
                assert!(p.within.is_none(), "one nesting level");
                assert!(
                    expand(s.path, home).starts_with(expand(p.path, home)),
                    "{} inside {}",
                    s.id,
                    p.id
                );
            }
        }
    }
}
