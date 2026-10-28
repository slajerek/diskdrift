//! `analyze <dir>`: the direct children of one folder, measured in a single
//! bounded-time walk, each with its size, last change and an assessment of what
//! it is and how much it matters. Read-only.

use crate::config::Effective;
use crate::snapshots::{AppError, Result};
use diskdrift_core::record::{Record, flags};
use diskdrift_core::walk::{self, Progress, WalkOptions};
use diskdrift_diagnostics::classify::{Assessment, Classifier, verdict};
use diskdrift_diagnostics::suspects::{self, MeasureStatus, Risk};
use diskdrift_platform::ListerKind;
use serde::Serialize;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalyzeItem {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    /// `Complete`, `Partial` (unreadable subfolders), `TimedOut` (lower bound),
    /// `Inaccessible`, or `Absent` (not reached before the time budget ran out).
    pub status: MeasureStatus,
    pub allocated_bytes: u64,
    pub logical_bytes: u64,
    pub entries: u64,
    /// Newest modification time of anything inside, seconds since the epoch.
    pub newest_mtime: Option<i64>,
    pub assessment: Assessment,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerdictTotal {
    pub verdict: String,
    pub allocated_bytes: u64,
    pub items: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirAnalysis {
    pub path: String,
    /// What the analysed folder itself (or its nearest known ancestor) is;
    /// children nothing else is known about inherit it.
    pub context: Option<Context>,
    pub status: MeasureStatus,
    pub allocated_bytes: u64,
    pub logical_bytes: u64,
    pub entries: u64,
    /// Largest first, at most the requested limit.
    pub items: Vec<AnalyzeItem>,
    /// Children left out by the limit, and their total allocated size.
    pub more_items: usize,
    pub more_allocated_bytes: u64,
    /// Allocated bytes per verdict over all children.
    pub by_verdict: Vec<VerdictTotal>,
    pub installed_apps: usize,
    /// When the analysis ran (seconds since the epoch), for "changed N ago".
    pub analyzed_at: i64,
    pub elapsed_ms: u64,
    /// Set when the sizes come from a saved scan instead of being measured now.
    pub sizes_from: Option<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    pub what: String,
    pub risk: Risk,
    /// The folder the context comes from (the analysed one or an ancestor).
    pub from: String,
}

/// The nearest of `path` and its ancestors (up to the home folder or `/`) that is
/// a known location or has a known name.
fn context_for(path: &Path, home: &Path, classifier: &Classifier) -> Option<Context> {
    let catalog = suspects::macos_catalog();
    for p in path.ancestors() {
        if p == home || p.parent().is_none() {
            break;
        }
        if let Some(s) = catalog.iter().find(|s| suspects::expand(s.path, home) == p) {
            return Some(Context { what: s.label.into(), risk: s.risk, from: p.display().to_string() });
        }
        let a = classifier.assess(p, true);
        if let Some(risk) = a.risk {
            return Some(Context { what: a.what, risk, from: p.display().to_string() });
        }
    }
    None
}

/// Progress events, in order.
#[derive(Debug, Clone)]
pub enum AnalyzeStep {
    Listed { children: usize, installed_apps: usize },
    Measured { name: String, allocated_bytes: u64, entries: u64, elapsed: Duration },
}

struct Acc {
    status: MeasureStatus,
    allocated: u64,
    logical: u64,
    entries: u64,
    newest: Option<i64>,
    inaccessible: u64,
}

impl Acc {
    fn new() -> Acc {
        Acc { status: MeasureStatus::Absent, allocated: 0, logical: 0, entries: 0, newest: None, inaccessible: 0 }
    }

    fn add(&mut self, rec: &Record) {
        self.entries += 1;
        self.allocated += rec.counted_alloc();
        self.logical += rec.counted_size();
        if rec.mtime > 0 {
            self.newest = Some(self.newest.map_or(rec.mtime, |n| n.max(rec.mtime)));
        }
        if rec.has(flags::INACCESSIBLE) {
            self.inaccessible += 1;
        }
    }
}

/// Expands `~` and makes `path` absolute against the current directory.
pub fn resolve_path(settings: &Effective, path: &str) -> PathBuf {
    let p = suspects::expand(path, &settings.home);
    if p.is_absolute() { p } else { std::env::current_dir().map(|c| c.join(&p)).unwrap_or(p) }
}

pub fn analyze(
    settings: &Effective,
    path: &Path,
    limit: usize,
    budget: Duration,
    cancel: &AtomicBool,
    progress: Option<&Progress>,
    step: &dyn Fn(AnalyzeStep),
) -> Result<DirAnalysis> {
    let started = Instant::now();
    let md = std::fs::symlink_metadata(path).map_err(|e| AppError::Usage(format!("{}: {e}", path.display())))?;
    if !md.is_dir() {
        return Err(AppError::Usage(format!("{} is not a directory", path.display())));
    }
    // List the children first, so ones the time budget does not reach still appear.
    let mut children: Vec<(OsString, bool)> = std::fs::read_dir(path)
        .map_err(|e| AppError::Runtime(format!("{}: {e}", path.display())))?
        .flatten()
        .map(|e| (e.file_name(), e.file_type().map(|t| t.is_dir()).unwrap_or(false)))
        .collect();
    children.sort();
    let classifier = Classifier::load(&settings.home);
    let installed_apps = classifier.installed_apps();
    let context = context_for(path, &settings.home, &classifier);
    step(AnalyzeStep::Listed { children: children.len(), installed_apps });

    let mut accs: HashMap<OsString, Acc> = children.iter().map(|(n, _)| (n.clone(), Acc::new())).collect();
    let mut total = Acc::new();
    let lister = diskdrift_platform::lister(ListerKind::Auto);
    let opts = WalkOptions { threads: settings.threads.value, ..WalkOptions::default() };
    let deadline = started + budget;
    let stop = AtomicBool::new(false);
    let mut timed_out = false;
    let mut n = 0u64;
    let mut current: Option<(OsString, Instant)> = None;
    let finish = |cur: &mut Option<(OsString, Instant)>, accs: &mut HashMap<OsString, Acc>, done: bool| {
        if let Some((name, t0)) = cur.take()
            && let Some(a) = accs.get_mut(&name)
        {
            if a.status == MeasureStatus::Complete && a.inaccessible > 0 {
                a.status = MeasureStatus::Partial;
            }
            if !done && matches!(a.status, MeasureStatus::Complete | MeasureStatus::Partial) {
                a.status = MeasureStatus::TimedOut;
            }
            step(AnalyzeStep::Measured {
                name: name.to_string_lossy().into_owned(),
                allocated_bytes: a.allocated,
                entries: a.entries,
                elapsed: t0.elapsed(),
            });
        }
    };
    let mut sink = |rec: &Record, p: &Path| -> io::Result<()> {
        n += 1;
        if n.is_multiple_of(2048) && (Instant::now() >= deadline || cancel.load(Ordering::Relaxed)) {
            timed_out = Instant::now() >= deadline;
            stop.store(true, Ordering::Relaxed);
        }
        total.add(rec);
        if rec.depth == 0 {
            return Ok(());
        }
        if rec.depth == 1 {
            finish(&mut current, &mut accs, true);
            let name = p.file_name().map(OsString::from).unwrap_or_default();
            if let Some(a) = accs.get_mut(&name) {
                a.status = if rec.has(flags::INACCESSIBLE) { MeasureStatus::Inaccessible } else { MeasureStatus::Complete };
            }
            current = Some((name, Instant::now()));
        }
        if let Some(a) = current.as_ref().and_then(|(name, _)| accs.get_mut(name)) {
            a.add(rec);
        }
        Ok(())
    };
    let walked = walk::walk(path, lister.as_ref(), &opts, &mut sink, &stop, progress);
    let stopped = stop.load(Ordering::Relaxed);
    finish(&mut current, &mut accs, !stopped);
    let report = walked.map_err(|e| AppError::Runtime(e.to_string()))?;

    let measured: Vec<(String, bool, MeasureStatus, Acc)> = children
        .into_iter()
        .map(|(name, is_dir)| {
            let a = accs.remove(&name).unwrap_or_else(Acc::new);
            let status = if a.status == MeasureStatus::Absent && !stopped {
                // Listed but never walked: removed meanwhile, or another filesystem.
                MeasureStatus::Inaccessible
            } else {
                a.status
            };
            (name.to_string_lossy().into_owned(), is_dir, status, a)
        })
        .collect();
    let (items, more_items, more_allocated_bytes, by) = assemble(path, measured, &classifier, context.as_ref(), limit);

    let status = if stopped && timed_out {
        MeasureStatus::TimedOut
    } else if report.is_partial() || total.inaccessible > 0 {
        MeasureStatus::Partial
    } else {
        MeasureStatus::Complete
    };
    let mut notes = vec![
        "Assessments are heuristics from folder names, locations and installed apps; look inside before deleting anything.".into(),
        "Sibling sizes do not overlap, but clones, hard links or snapshots shared with other places can keep space allocated after a deletion.".into(),
    ];
    if status == MeasureStatus::TimedOut {
        notes.push(format!(
            "The {}s time budget ran out: sizes are lower bounds and items marked 'not measured' were not reached.",
            budget.as_secs()
        ));
    }
    if stopped && !timed_out {
        notes.push("Cancelled: sizes are incomplete.".into());
    }
    if total.inaccessible > 0 {
        notes.push(format!(
            "{} folder(s) could not be read (Full Disk Access may be needed); their contents are not counted.",
            total.inaccessible
        ));
    }
    Ok(DirAnalysis {
        path: path.display().to_string(),
        context,
        status,
        allocated_bytes: total.allocated,
        logical_bytes: total.logical,
        entries: total.entries,
        items,
        more_items,
        more_allocated_bytes,
        by_verdict: by,
        installed_apps,
        analyzed_at: chrono::Local::now().timestamp(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        sizes_from: None,
        notes,
    })
}

type Assembled = (Vec<AnalyzeItem>, usize, u64, Vec<VerdictTotal>);

/// Assesses measured children, sorts them largest first, totals them per
/// verdict and applies the limit.
fn assemble(path: &Path, measured: Vec<(String, bool, MeasureStatus, Acc)>, classifier: &Classifier, context: Option<&Context>, limit: usize) -> Assembled {
    let mut items: Vec<AnalyzeItem> = measured
        .into_iter()
        .map(|(name, is_dir, status, a)| {
            let child = path.join(&name);
            let mut assessment = classifier.assess(&child, is_dir);
            if assessment.risk.is_none()
                && let Some(c) = context
            {
                assessment.what = format!("part of: {}", c.what);
                assessment.risk = Some(c.risk);
            }
            AnalyzeItem {
                name,
                assessment,
                path: child.display().to_string(),
                is_dir,
                status,
                allocated_bytes: a.allocated,
                logical_bytes: a.logical,
                entries: a.entries,
                newest_mtime: a.newest,
            }
        })
        .collect();
    items.sort_by(|a, b| b.allocated_bytes.cmp(&a.allocated_bytes).then_with(|| a.name.cmp(&b.name)));
    let mut by: Vec<VerdictTotal> = Vec::new();
    for it in &items {
        let v = verdict(it.assessment.risk);
        match by.iter_mut().find(|t| t.verdict == v) {
            Some(t) => {
                t.allocated_bytes += it.allocated_bytes;
                t.items += 1;
            }
            None => by.push(VerdictTotal { verdict: v.into(), allocated_bytes: it.allocated_bytes, items: 1 }),
        }
    }
    by.sort_by_key(|v| std::cmp::Reverse(v.allocated_bytes));
    let more_items = items.len().saturating_sub(limit);
    let more_allocated_bytes = items.iter().skip(limit).map(|i| i.allocated_bytes).sum();
    items.truncate(limit);
    (items, more_items, more_allocated_bytes, by)
}

/// The same analysis from the latest scan, without walking the folder: `None`
/// when that scan is older than `max_age` seconds or does not contain `path`.
#[cfg(unix)]
pub fn analyze_from_scan(settings: &Effective, path: &Path, limit: usize, max_age: i64, now: i64) -> Result<Option<DirAnalysis>> {
    use diskdrift_core::paths::components_under;
    use std::os::unix::ffi::OsStrExt;
    let started = Instant::now();
    let Some(store) = crate::snapshots::open_store(settings)? else { return Ok(None) };
    let Some(m) = store.list()?.into_iter().rev().find(|m| m.status.is_usable()) else { return Ok(None) };
    let scanned_at = chrono::DateTime::parse_from_rfc3339(&m.started_at).map(|t| t.timestamp()).unwrap_or(0);
    if now - scanned_at > max_age {
        return Ok(None);
    }
    let Some(dir) = components_under(m.root.as_bytes(), path.as_os_str().as_bytes()) else { return Ok(None) };
    let listing = diskdrift_core::browse::children(store.records(m.id)?, &dir, usize::MAX)
        .map_err(|e| AppError::Store(diskdrift_storage::StoreError::Corrupt(e.to_string())))?;
    let Some(d) = listing.dir.filter(|d| d.is_dir()) else { return Ok(None) };
    let classifier = Classifier::load(&settings.home);
    let context = context_for(path, &settings.home, &classifier);
    let measured = listing
        .items
        .into_iter()
        .map(|c| {
            let status = match () {
                _ if c.incomplete && c.entries == 1 && c.is_dir() => MeasureStatus::Inaccessible,
                _ if c.incomplete => MeasureStatus::Partial,
                _ => MeasureStatus::Complete,
            };
            let acc = Acc {
                status,
                allocated: c.allocated_bytes,
                logical: c.logical_bytes,
                entries: c.entries,
                newest: Some(c.newest_mtime),
                inaccessible: 0,
            };
            (c.name(), c.is_dir(), status, acc)
        })
        .collect();
    let (items, more_items, more_allocated_bytes, by) = assemble(path, measured, &classifier, context.as_ref(), limit);
    let source = crate::snapshots::scan_label(&m, now);
    let mut notes = vec![
        format!("Sizes from {source}, not measured now; changes since then are not shown."),
        "Assessments are heuristics from folder names, locations and installed apps; look inside before deleting anything.".into(),
    ];
    if d.incomplete {
        notes.push("Parts of this folder could not be read or were excluded in that scan; sizes are lower bounds there.".into());
    }
    Ok(Some(DirAnalysis {
        path: path.display().to_string(),
        context,
        status: if d.incomplete { MeasureStatus::Partial } else { MeasureStatus::Complete },
        allocated_bytes: d.allocated_bytes,
        logical_bytes: d.logical_bytes,
        entries: d.entries,
        items,
        more_items,
        more_allocated_bytes,
        by_verdict: by,
        installed_apps: classifier.installed_apps(),
        // "Changed N ago" counts back from the scan, as the sizes do.
        analyzed_at: scanned_at,
        elapsed_ms: started.elapsed().as_millis() as u64,
        sizes_from: Some(source),
        notes,
    }))
}

#[cfg(not(unix))]
pub fn analyze_from_scan(_settings: &Effective, _path: &Path, _limit: usize, _max_age: i64, _now: i64) -> Result<Option<DirAnalysis>> {
    Ok(None)
}

/// "3 days", "5 months", "2.1 years".
pub fn age(seconds: i64) -> String {
    let s = seconds.max(0) as f64;
    let (min, hour, day) = (60.0, 3600.0, 86_400.0);
    let n = |v: f64, unit: &str| {
        let v = v.round() as i64;
        if v == 1 { format!("1 {unit}") } else { format!("{v} {unit}s") }
    };
    if s < min {
        "just now".into()
    } else if s < hour {
        format!("{:.0} min", (s / min).floor())
    } else if s < 2.0 * day {
        n(s / hour, "hour")
    } else if s < 60.0 * day {
        n(s / day, "day")
    } else if s < 730.0 * day {
        n(s / (30.44 * day), "month")
    } else {
        format!("{:.1} years", s / (365.25 * day))
    }
}

/// "3 days ago", or "just now".
pub fn ago(seconds: i64) -> String {
    match age(seconds) {
        a if a == "just now" => a,
        a => format!("{a} ago"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Overrides, resolve_with};
    use diskdrift_core::testutil::TestDir;

    #[test]
    fn measures_each_child_and_assesses_it() {
        let t = TestDir::new("analyze");
        t.file("root/big/a", 300_000);
        t.file("root/big/deeper/b", 200_000);
        t.file("root/Caches/x", 50_000);
        t.file("root/node_modules/m.js", 10_000);
        t.file("root/loose.dmg", 70_000);
        std::fs::create_dir_all(t.path().join("root/empty")).unwrap();
        let s = resolve_with(&Overrides::default(), Path::new("/nonexistent-home"), Path::new("/")).unwrap();
        let steps = std::cell::RefCell::new(Vec::new());
        let r = analyze(&s, &t.path().join("root"), 3, Duration::from_secs(30), &AtomicBool::new(false), None, &|st| {
            steps.borrow_mut().push(st)
        })
        .unwrap();
        assert_eq!(r.status, MeasureStatus::Complete);
        assert_eq!(r.items.len(), 3);
        assert_eq!(r.more_items, 2);
        assert_eq!(r.items[0].name, "big");
        assert!(r.items[0].logical_bytes >= 500_000);
        assert_eq!(r.items[0].entries, 4, "big, a, deeper, b");
        assert!(r.items[0].newest_mtime.is_some());
        assert_eq!(r.items[1].name, "loose.dmg");
        assert_eq!(r.items[1].assessment.what, "disk image / installer");
        assert!(!r.items[1].is_dir);
        let all: u64 = r.by_verdict.iter().map(|v| v.items as u64).sum();
        assert_eq!(all, 5);
        let steps = steps.into_inner();
        assert!(matches!(steps[0], AnalyzeStep::Listed { children: 5, .. }));
        assert_eq!(steps.iter().filter(|s| matches!(s, AnalyzeStep::Measured { .. })).count(), 5);
        assert!(analyze(&s, &t.path().join("root/loose.dmg"), 3, Duration::from_secs(1), &AtomicBool::new(false), None, &|_| {}).is_err());
    }

    #[test]
    fn ages_read_naturally() {
        assert_eq!(age(30), "just now");
        assert_eq!(age(45 * 60), "45 min");
        assert_eq!(age(3600), "1 hour");
        assert_eq!(age(5 * 3600), "5 hours");
        assert_eq!(ago(86_400 * 3), "3 days ago");
        assert_eq!(ago(10), "just now");
        assert_eq!(age(5 * 86_400), "5 days");
        assert_eq!(age(400 * 86_400), "13 months");
        assert_eq!(age(3 * 365 * 86_400), "3.0 years");
    }
}
