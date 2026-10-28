//! High-value storage locations (spec 4.2) and a bounded-time tree measurement.

use diskdrift_core::record::{Kind, Record, flags};
use diskdrift_core::walk::{self, DirLister, WalkOptions};
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
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
        s!("user-caches", "Caches", "User caches", "~/Library/Caches", None, Medium,
           "Apps rebuild most caches, but some hold offline data or are in use. Inspect the largest subfolders and prefer each app's own cache setting."),
        s!("homebrew-cache", "Caches", "Homebrew downloads", "~/Library/Caches/Homebrew", Some("user-caches"), Low,
           "Old bottles and downloads. Review `brew cleanup -n`, then run `brew cleanup`."),
        s!("pip-cache", "Caches", "pip cache", "~/Library/Caches/pip", Some("user-caches"), Low,
           "Python package downloads. `pip cache purge` clears it."),
        s!("xcode-deriveddata", "Developer", "Xcode DerivedData", "~/Library/Developer/Xcode/DerivedData", None, Low,
           "Build intermediates that Xcode regenerates. Remove with Xcode closed."),
        s!("xcode-archives", "Developer", "Xcode Archives", "~/Library/Developer/Xcode/Archives", None, High,
           "Archived app builds, needed to symbolicate crash reports of shipped versions. Review in Xcode Organizer."),
        s!("xcode-ios-devicesupport", "Developer", "Xcode iOS DeviceSupport", "~/Library/Developer/Xcode/iOS DeviceSupport", None, Low,
           "Debug symbols per device OS version; Xcode re-creates them when a device is connected. Old versions can go."),
        s!("xcode-watchos-devicesupport", "Developer", "Xcode watchOS DeviceSupport", "~/Library/Developer/Xcode/watchOS DeviceSupport", None, Low,
           "Debug symbols per device OS version; old versions can go."),
        s!("coresimulator-devices", "Developer", "Simulator devices", "~/Library/Developer/CoreSimulator/Devices", None, Medium,
           "Simulator data. `xcrun simctl delete unavailable` removes simulators of runtimes no longer installed."),
        s!("coresimulator-caches", "Developer", "Simulator caches", "~/Library/Developer/CoreSimulator/Caches", None, Low,
           "Simulator dyld caches; regenerated on demand."),
        s!("simulator-runtimes", "Developer", "Simulator runtimes (system)", "/Library/Developer/CoreSimulator", None, Medium,
           "Installed simulator runtimes. Manage them in Xcode > Settings > Components or with `xcrun simctl runtime list`. Mounted runtime images are separate volumes and not counted here."),
        s!("ios-backups", "Backups", "iPhone/iPad backups", "~/Library/Application Support/MobileSync/Backup", Some("app-support"), High,
           "Device backups. Manage them in Finder (select the device > Manage Backups); delete only backups you no longer need."),
        s!("app-support", "Application data", "Application Support", "~/Library/Application Support", None, Medium,
           "Per-app data (browsers, chat apps, game libraries, ML models). Check the largest subfolders and clean up inside each app."),
        s!("containers", "Application data", "App containers", "~/Library/Containers", None, Medium,
           "Data of sandboxed apps. Check the largest containers; remove data inside the owning app."),
        s!("group-containers", "Application data", "Group containers", "~/Library/Group Containers", None, Medium,
           "Data shared between apps of one vendor (Office, Messages, ...)."),
        s!("docker", "Containers & VMs", "Docker Desktop disk image", "~/Library/Containers/com.docker.docker/Data/vms", Some("containers"), Medium,
           "Sparse virtual disk: apparent size can far exceed allocated bytes. Check `docker system df`, prune what you do not need, or shrink the disk in Docker Desktop settings."),
        s!("utm", "Containers & VMs", "UTM virtual machines", "~/Library/Containers/com.utmapp.UTM/Data/Documents", Some("containers"), High,
           "Virtual machine images. Sparse images may allocate less than their apparent size."),
        s!("parallels", "Containers & VMs", "Parallels virtual machines", "~/Parallels", None, High,
           "Virtual machine images. Use Parallels' own 'Reclaim disk space' first."),
        s!("vmware", "Containers & VMs", "VMware virtual machines", "~/Virtual Machines.localized", None, High,
           "Virtual machine images."),
        s!("trash", "Trash", "Trash", "~/.Trash", None, Low,
           "Files already deleted in Finder. Review and empty the Trash."),
        s!("user-logs", "Logs", "User logs", "~/Library/Logs", None, Low,
           "Application logs and crash reports; review large files in Console."),
        s!("system-logs", "Logs", "System logs", "/Library/Logs", None, Low,
           "System-wide logs and diagnostic reports."),
        s!("var-log", "Logs", "Unix logs", "/private/var/log", None, SystemManaged,
           "Managed by macOS log rotation; inspect only."),
        s!("var-folders", "Temporary", "Per-user temporary files", "/private/var/folders", None, SystemManaged,
           "Temporary files and caches managed by macOS, cleared at restart or by the OS. Do not delete while apps run."),
        s!("swap", "System", "Swap and sleep image", "/System/Volumes/VM", None, SystemManaged,
           "Virtual memory, managed by macOS. Informational only; restarting frees swap."),
        s!("spotlight", "System", "Spotlight index", "/System/Volumes/Data/.Spotlight-V100", None, SystemManaged,
           "Search index, managed by macOS. If it is unexpectedly large, rebuild it from Spotlight privacy settings."),
        s!("mail", "User data", "Mail", "~/Library/Mail", None, High,
           "Mail storage and attachments. Manage inside Mail."),
        s!("messages", "User data", "Messages attachments", "~/Library/Messages", None, High,
           "Message history and attachments. Review in System Settings > General > Storage > Messages."),
        s!("photos", "User data", "Photos library", "~/Pictures/Photos Library.photoslibrary", None, High,
           "Photo library. 'Optimize Mac Storage' in Photos settings keeps originals in iCloud only."),
        s!("downloads", "User data", "Downloads", "~/Downloads", None, High,
           "Your downloaded files; review manually."),
        s!("dot-cache", "Developer", "~/.cache", "~/.cache", None, Medium,
           "Tool caches (Hugging Face models, pre-commit, uv, ...)."),
        s!("huggingface", "Developer", "Hugging Face cache", "~/.cache/huggingface", Some("dot-cache"), Medium,
           "Downloaded models and datasets. `hf cache scan` / `hf cache delete` manage it."),
        s!("ollama", "Developer", "Ollama models", "~/.ollama/models", None, Medium,
           "Local LLM weights. `ollama list` / `ollama rm` manage them."),
        s!("npm-cache", "Developer", "npm cache", "~/.npm", None, Low,
           "Package download cache. `npm cache clean --force` clears it."),
        s!("gradle-cache", "Developer", "Gradle caches", "~/.gradle/caches", None, Low,
           "Build and dependency caches; regenerated by the next build."),
        s!("maven-repo", "Developer", "Maven repository", "~/.m2/repository", None, Low,
           "Downloaded dependencies; re-downloaded when needed."),
        s!("cargo-registry", "Developer", "Cargo registry", "~/.cargo/registry", None, Low,
           "Crate sources and downloads; re-downloaded when needed."),
        s!("rustup-toolchains", "Developer", "Rust toolchains", "~/.rustup/toolchains", None, Medium,
           "Installed toolchains. `rustup toolchain list` / `rustup toolchain uninstall`."),
        s!("go-modcache", "Developer", "Go module cache", "~/go/pkg/mod", None, Low,
           "`go clean -modcache` clears it."),
    ]
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
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
    pub entries: u64,
    pub inaccessible_dirs: u64,
}

impl Measure {
    fn new(status: MeasureStatus) -> Measure {
        Measure { status, logical_bytes: 0, allocated_bytes: 0, entries: 0, inaccessible_dirs: 0 }
    }

    pub fn absent() -> Measure {
        Measure::new(MeasureStatus::Absent)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LargeFile {
    pub path: String,
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
}

/// Keeps the N largest files seen across measurements, by allocated size.
pub struct LargeFiles {
    limit: usize,
    heap: BinaryHeap<Reverse<(u64, u64, String)>>,
    seen: HashSet<(u16, u64)>,
}

impl LargeFiles {
    pub fn new(limit: usize) -> Self {
        LargeFiles { limit, heap: BinaryHeap::new(), seen: HashSet::new() }
    }

    fn offer(&mut self, rec: &Record, path: &Path) {
        if self.heap.len() >= self.limit
            && self.heap.peek().is_some_and(|Reverse((a, _, _))| *a >= rec.alloc)
        {
            return;
        }
        if !self.seen.insert((rec.vol, rec.ino)) {
            return;
        }
        self.heap.push(Reverse((rec.alloc, rec.size, path.to_string_lossy().into_owned())));
        if self.heap.len() > self.limit {
            self.heap.pop();
        }
    }

    /// A file already known (from a saved scan), deduplicated by path.
    pub fn offer_known(&mut self, f: &LargeFile) {
        if self.heap.len() >= self.limit && self.heap.peek().is_some_and(|Reverse((a, _, _))| *a >= f.allocated_bytes) {
            return;
        }
        if self.heap.iter().any(|Reverse((_, _, p))| *p == f.path) {
            return;
        }
        self.heap.push(Reverse((f.allocated_bytes, f.logical_bytes, f.path.clone())));
        if self.heap.len() > self.limit {
            self.heap.pop();
        }
    }

    pub fn into_sorted(self) -> Vec<LargeFile> {
        let mut v: Vec<LargeFile> = self
            .heap
            .into_iter()
            .map(|Reverse((alloc, size, path))| LargeFile { path, logical_bytes: size, allocated_bytes: alloc })
            .collect();
        v.sort_by(|a, b| {
            (b.allocated_bytes, b.logical_bytes).cmp(&(a.allocated_bytes, a.logical_bytes))
        });
        v
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
    let absent = || (Measure::new(MeasureStatus::Absent), subs.iter().map(|_| Measure::new(MeasureStatus::Absent)).collect());
    match std::fs::symlink_metadata(root) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return absent(),
        Err(_) => {
            return (
                Measure::new(MeasureStatus::Inaccessible),
                subs.iter().map(|_| Measure::new(MeasureStatus::Inaccessible)).collect(),
            );
        }
        Ok(md) if md.file_type().is_symlink() => return absent(),
        Ok(_) => {}
    }
    let mut subs: Vec<Sub> = subs
        .iter()
        .map(|p| Sub {
            depth: p.strip_prefix(root).map(|r| r.components().count() as u16).unwrap_or(u16::MAX),
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
        if n.is_multiple_of(2048) && (Instant::now() >= deadline || cancel.load(Ordering::Relaxed)) {
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
                s.measure.status = if inaccessible { MeasureStatus::Inaccessible } else { MeasureStatus::Complete };
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
        if m.status == MeasureStatus::Complete && m.inaccessible_dirs > 0 {
            m.status = MeasureStatus::Partial;
        }
        if timed_out && matches!(m.status, MeasureStatus::Complete | MeasureStatus::Partial) {
            m.status = MeasureStatus::TimedOut;
        }
        if skipped && matches!(m.status, MeasureStatus::Complete | MeasureStatus::Partial | MeasureStatus::TimedOut) {
            m.status = MeasureStatus::Skipped;
        }
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
                s.measure.status = if skipped { MeasureStatus::Skipped } else { MeasureStatus::TimedOut };
            }
            finish(&mut s.measure);
            s.measure
        })
        .collect();
    (total, subs)
}

fn add(m: &mut Measure, rec: &Record) {
    m.entries += 1;
    m.logical_bytes += rec.counted_size();
    m.allocated_bytes += rec.counted_alloc();
    if rec.has(flags::INACCESSIBLE) {
        m.inaccessible_dirs += 1;
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
    use diskdrift_core::testutil::TestDir;
    use diskdrift_core::walk::StdLister;
    use std::time::Duration;

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
        assert!(total.logical_bytes >= 13_003);
        assert_eq!(subs[0].status, MeasureStatus::Complete);
        assert!(subs[0].logical_bytes >= 12_000 && subs[0].logical_bytes < 13_000);
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
        let (m, _) = measure_tree(&t.path().join("nope"), &[], &StdLister, &WalkOptions::default(), Instant::now(), &mut large, &cancel, &cancel);
        assert_eq!(m.status, MeasureStatus::Absent);
        for i in 0..3000 {
            t.file(&format!("many/{i}"), 1);
        }
        let (m, _) = measure_tree(&t.path().join("many"), &[], &StdLister, &WalkOptions::default(), Instant::now(), &mut large, &cancel, &cancel);
        assert_eq!(m.status, MeasureStatus::TimedOut);
        let far = Instant::now() + std::time::Duration::from_secs(60);
        let skip = AtomicBool::new(true);
        let (m, _) = measure_tree(&t.path().join("many"), &[], &StdLister, &WalkOptions::default(), far, &mut large, &cancel, &skip);
        assert_eq!(m.status, MeasureStatus::Skipped);
        assert!(m.entries < 3001, "stopped early");
    }

    #[test]
    fn catalog_is_consistent() {
        let cat = macos_catalog();
        let ids: HashSet<&str> = cat.iter().map(|s| s.id).collect();
        assert_eq!(ids.len(), cat.len(), "unique ids");
        let home = Path::new("/Users/test");
        for s in &cat {
            if let Some(parent) = s.within {
                let p = cat.iter().find(|x| x.id == parent).expect("parent exists");
                assert!(p.within.is_none(), "one nesting level");
                assert!(expand(s.path, home).starts_with(expand(p.path, home)), "{} inside {}", s.id, p.id);
            }
        }
    }
}
