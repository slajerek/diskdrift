//! `doctor --quick`: storage diagnosis without a baseline or a database.

use crate::access::{self, AccessCheck};
use crate::apfs::{self, ApfsContainer, LocalSnapshot};
use crate::openfiles::{self, OpenDeletedReport};
use crate::suspects::{self, LargeFile, LargeFiles, Measure, MeasureStatus, Risk};
use crate::usage::{self, UsageReport};
use diskdrift_core::walk::{DirLister, WalkOptions};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct DoctorOptions {
    pub home: PathBuf,
    /// Free space the user remembers having earlier (not verified).
    pub reference_free: Option<u64>,
    /// Total time budget for measuring suspect locations.
    pub budget: Duration,
    pub threads: usize,
    /// When set (e.g. by a key press), the location being measured is skipped;
    /// the flag is cleared before each location.
    pub skip: Option<Arc<AtomicBool>>,
    /// Sizes already known from a recent scan; locations found there are not
    /// measured again.
    pub known: Option<Arc<KnownSizes>>,
    /// The latest saved scan, e.g. "scan #4 of 2026-10-21 13:13 (3 hours ago)":
    /// growth over time can then be looked up by comparing scans.
    pub baseline: Option<String>,
}

/// Location sizes taken from a saved scan instead of measuring them now.
#[derive(Debug, Clone, Default)]
pub struct KnownSizes {
    /// Where they come from, e.g. "scan #4 of 2026-10-21 13:13 (25 min ago)".
    pub source: String,
    pub sizes: std::collections::HashMap<PathBuf, Measure>,
    /// Largest files inside the known locations.
    pub large_files: Vec<LargeFile>,
}

/// Progress reported while the doctor runs.
#[derive(Debug, Clone)]
pub enum DoctorStep {
    Volumes,
    Snapshots,
    /// Location sizes come from a saved scan (`source`); only the others are measured.
    FromScan { source: String, locations: usize },
    Measuring { label: String, index: usize, total: usize },
    /// A location (and the ones nested in it) was measured.
    Measured { label: String, path: PathBuf, status: MeasureStatus, allocated_bytes: u64, entries: u64, elapsed: Duration },
    OpenFiles,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Confidence {
    /// Every entry was read.
    Observed,
    /// A lower bound: parts could not be read or time ran out.
    LowerBound,
    /// Nothing could be measured.
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub id: String,
    pub category: String,
    pub label: String,
    pub path: PathBuf,
    /// Id of the finding that contains this one. Nested sizes are already part of
    /// the parent; never add them together.
    pub within: Option<String>,
    pub status: MeasureStatus,
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
    pub entries: u64,
    pub inaccessible_dirs: u64,
    pub confidence: Confidence,
    pub risk: Risk,
    pub recommendation: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotInventory {
    pub snapshots: Vec<LocalSnapshot>,
    /// APFS does not report per-snapshot sizes; this stays "unknown" by design.
    pub reclaimable: String,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub name: String,
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorReport {
    pub generated_at: String,
    pub usage: UsageReport,
    pub checks: Vec<Check>,
    pub access: AccessCheck,
    pub snapshots: SnapshotInventory,
    pub findings: Vec<Finding>,
    pub largest_files: Vec<LargeFile>,
    pub open_deleted: OpenDeletedReport,
    pub reference_free_bytes: Option<u64>,
    /// Reference minus important-usage available now (positive = space lost).
    pub unexplained_gap_bytes: Option<i128>,
    pub conclusion: Vec<String>,
    /// Set when location sizes were taken from a saved scan instead of measured now.
    pub sizes_from: Option<String>,
    pub files_modified: bool,
    pub elapsed_ms: u64,
}

pub fn run(
    opts: &DoctorOptions,
    lister: &dyn DirLister,
    cancel: &AtomicBool,
    progress: &dyn Fn(DoctorStep),
) -> DoctorReport {
    let started = Instant::now();
    progress(DoctorStep::Volumes);
    let mounts = diskdrift_platform::mounts().unwrap_or_default();
    let topology = apfs::topology(&mounts);
    let containers: &[ApfsContainer] = topology.as_deref().unwrap_or(&[]);
    let usage = usage::build(&mounts, topology.as_deref().map_err(|e| e.to_string()), false);

    progress(DoctorStep::Snapshots);
    let (snaps, snap_errors) = apfs::local_snapshots(containers);
    let access = access::full_disk_access();

    let mut checks = Vec::new();
    checks.push(Check {
        name: "APFS container / volume mapping".into(),
        status: match &topology {
            Ok(c) if !c.is_empty() => "OK".into(),
            Ok(_) => "NONE".into(),
            Err(_) => "ERROR".into(),
        },
        detail: match &topology {
            Ok(c) => format!("{} container(s)", c.len()),
            Err(e) => e.to_string(),
        },
    });
    let tm = snaps.iter().filter(|s| s.kind == apfs::SnapshotKind::TimeMachine).count();
    checks.push(Check {
        name: "Local snapshots".into(),
        status: format!("{} detected", snaps.len()),
        detail: format!(
            "{tm} Time Machine, {} OS update, {} other; reclaimable: unknown",
            snaps.iter().filter(|s| s.kind == apfs::SnapshotKind::OsUpdate).count(),
            snaps.iter().filter(|s| s.kind == apfs::SnapshotKind::Other).count()
        ),
    });
    checks.push(Check {
        name: "Full Disk Access coverage".into(),
        status: match access.full_disk_access {
            access::Access::Granted => "FULL".into(),
            access::Access::NotGranted => "PARTIAL".into(),
            access::Access::Unknown => "UNKNOWN".into(),
        },
        detail: access.detail.clone(),
    });

    let (findings, largest_files) = measure_suspects(opts, lister, cancel, progress);

    progress(DoctorStep::OpenFiles);
    let open_deleted = openfiles::scan(Duration::from_secs(20));

    let primary = usage.primary();
    let available_now = primary.map(|v| v.available_important_bytes.unwrap_or(v.available_bytes));
    let unexplained_gap_bytes = match (opts.reference_free, available_now) {
        (Some(r), Some(a)) => Some(r as i128 - a as i128),
        _ => None,
    };

    let mut conclusion = Vec::new();
    if let Some(v) = primary
        && let Some(p) = v.purgeable_estimate_bytes.filter(|&p| p > 0) {
            conclusion.push(format!(
                "macOS estimates {} of purgeable space on {} (snapshots, evictable caches) that it can free on demand.",
                diskdrift_core::units::format_size(p, Default::default()),
                v.name
            ));
        }
    if tm > 0 {
        conclusion.push(format!(
            "{tm} local Time Machine snapshot(s) may hold space of deleted or changed files; APFS does not report how much."
        ));
    }
    let measurable: Vec<&Finding> = findings.iter().filter(|f| f.within.is_none() && f.allocated_bytes > 0).collect();
    if let Some(top) = measurable.first() {
        conclusion.push(format!(
            "Largest measured location: {} ({}).",
            top.label,
            diskdrift_core::units::format_size(top.allocated_bytes, Default::default())
        ));
    }
    if access.full_disk_access != access::Access::Granted {
        conclusion.push("Some protected locations could not be measured without Full Disk Access.".into());
    }
    match unexplained_gap_bytes {
        Some(g) if g > 0 => conclusion.push(match &opts.baseline {
            Some(b) => format!(
                "Current consumers identified; to see where the {} loss since the reference went, compare saved scans. The latest is {b}.",
                diskdrift_core::units::format_size(g as u64, Default::default())
            ),
            None => format!(
                "Current consumers identified, but the {} loss since the reference cannot be attributed without a previous DiskDrift inventory. Scan now to have a baseline next time.",
                diskdrift_core::units::format_size(g as u64, Default::default())
            ),
        }),
        Some(_) => conclusion.push("Available space is not below the reference you supplied.".into()),
        None => conclusion.push(match &opts.baseline {
            Some(b) => format!("To see what grew over time, compare saved scans. The latest is {b}; a new scan of the same folder then shows the growth since."),
            None => "Without a previous DiskDrift inventory, growth over time cannot be attributed. Scan once to create a baseline.".into(),
        }),
    }

    DoctorReport {
        generated_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        usage,
        checks,
        access,
        snapshots: SnapshotInventory { snapshots: snaps, reclaimable: "unknown".into(), errors: snap_errors },
        findings,
        largest_files,
        open_deleted,
        reference_free_bytes: opts.reference_free,
        unexplained_gap_bytes,
        conclusion,
        sizes_from: opts.known.as_ref().filter(|k| !k.sizes.is_empty()).map(|k| k.source.clone()),
        files_modified: false,
        elapsed_ms: started.elapsed().as_millis() as u64,
    }
}

fn measure_suspects(
    opts: &DoctorOptions,
    lister: &dyn DirLister,
    cancel: &AtomicBool,
    progress: &dyn Fn(DoctorStep),
) -> (Vec<Finding>, Vec<LargeFile>) {
    let catalog = suspects::macos_catalog();
    let deadline = Instant::now() + opts.budget;
    let walk_opts = WalkOptions { threads: opts.threads, ..WalkOptions::default() };
    let mut large = LargeFiles::new(20);
    let mut findings = Vec::new();
    let tops: Vec<&suspects::Suspect> = catalog.iter().filter(|s| s.within.is_none()).collect();
    let known = opts.known.as_deref().filter(|k| !k.sizes.is_empty());
    if let Some(k) = known {
        progress(DoctorStep::FromScan { source: k.source.clone(), locations: k.sizes.len() });
        for f in &k.large_files {
            large.offer_known(f);
        }
    }
    for (i, s) in tops.iter().enumerate() {
        let root = suspects::expand(s.path, &opts.home);
        let children: Vec<&suspects::Suspect> = catalog.iter().filter(|c| c.within == Some(s.id)).collect();
        let child_paths: Vec<PathBuf> = children.iter().map(|c| suspects::expand(c.path, &opts.home)).collect();
        if let Some(m) = known.and_then(|k| k.sizes.get(&root)) {
            let k = known.unwrap();
            progress(DoctorStep::Measured {
                label: format!("{} (from the scan)", s.label),
                path: root.clone(),
                status: m.status,
                allocated_bytes: m.allocated_bytes,
                entries: m.entries,
                elapsed: Duration::ZERO,
            });
            findings.push(finding(s, &root, m));
            for (c, p) in children.iter().zip(child_paths.iter()) {
                let m = k.sizes.get(p).cloned().unwrap_or_else(Measure::absent);
                findings.push(finding(c, p, &m));
            }
            continue;
        }
        progress(DoctorStep::Measuring { label: s.label.to_string(), index: i, total: tops.len() });
        let t0 = Instant::now();
        let no_skip = AtomicBool::new(false);
        let skip = opts.skip.as_deref().unwrap_or(&no_skip);
        skip.store(false, Ordering::SeqCst);
        let (m, subs) = suspects::measure_tree(&root, &child_paths, lister, &walk_opts, deadline, &mut large, cancel, skip);
        progress(DoctorStep::Measured {
            label: s.label.to_string(),
            path: root.clone(),
            status: m.status,
            allocated_bytes: m.allocated_bytes,
            entries: m.entries,
            elapsed: t0.elapsed(),
        });
        findings.push(finding(s, &root, &m));
        for ((c, p), m) in children.iter().zip(child_paths.iter()).zip(subs.iter()) {
            findings.push(finding(c, p, m));
        }
    }
    findings.retain(|f| f.status != MeasureStatus::Absent);
    findings.sort_by(|a, b| b.allocated_bytes.cmp(&a.allocated_bytes).then_with(|| a.id.cmp(&b.id)));
    (findings, large.into_sorted())
}

fn finding(s: &suspects::Suspect, path: &Path, m: &Measure) -> Finding {
    let confidence = match m.status {
        MeasureStatus::Complete => Confidence::Observed,
        MeasureStatus::Partial | MeasureStatus::TimedOut => Confidence::LowerBound,
        MeasureStatus::Inaccessible | MeasureStatus::Absent | MeasureStatus::Skipped => Confidence::Unknown,
    };
    Finding {
        id: s.id.into(),
        category: s.category.into(),
        label: s.label.into(),
        path: path.to_path_buf(),
        within: s.within.map(String::from),
        status: m.status,
        logical_bytes: m.logical_bytes,
        allocated_bytes: m.allocated_bytes,
        entries: m.entries,
        inaccessible_dirs: m.inaccessible_dirs,
        confidence,
        risk: s.risk,
        recommendation: s.advice.into(),
    }
}
