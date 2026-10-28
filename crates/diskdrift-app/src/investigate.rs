//! `doctor --deep` and `recover`: consolidated, read-only investigation (M3).

use crate::config::Effective;
use crate::snapshots::{self, AppError, DiffResult};
use diskdrift_core::diff::DiffOptions;
use diskdrift_core::top::{TopOptions, TopReport};
use diskdrift_core::units::{SizeUnits, format_size};
use diskdrift_diagnostics::doctor::{self, DoctorOptions, DoctorReport, DoctorStep, KnownSizes};
use diskdrift_diagnostics::suspects::{LargeFile, Measure, MeasureStatus, Risk};
use diskdrift_platform::ListerKind;
use diskdrift_storage::SnapshotMeta;
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepReport {
    pub quick: DoctorReport,
    /// Latest snapshot used as the current inventory.
    pub latest: Option<SnapshotMeta>,
    /// Largest directories in the latest snapshot.
    pub latest_top: Option<TopReport>,
    /// Comparison of the two most recent comparable snapshots.
    pub change: Option<DiffResult>,
    /// Possible explanations for an unreconciled difference, with what is known.
    pub unexplained_candidates: Vec<String>,
    pub conclusion: Vec<String>,
}

fn doctor_opts(settings: &Effective, reference_free: Option<u64>, budget: Duration, skip: Option<Arc<AtomicBool>>) -> DoctorOptions {
    DoctorOptions {
        home: settings.home.clone(),
        reference_free,
        budget,
        threads: settings.threads.value,
        skip,
        known: None,
        baseline: latest_baseline(settings),
    }
}

/// [`baseline`] as of now.
pub fn latest_baseline(settings: &Effective) -> Option<String> {
    baseline(settings, chrono::Local::now().timestamp())
}

/// The latest saved scan with data, labelled for the doctor's conclusion.
/// Reads the store only if it exists (nothing is created).
pub fn baseline(settings: &Effective, now: i64) -> Option<String> {
    let store = snapshots::open_store(settings).ok()??;
    let m = store.list().ok()?.into_iter().rev().find(|m| m.status.is_usable())?;
    Some(snapshots::scan_label(&m, now))
}

/// Sizes of the doctor's locations from the latest scan, if it is younger than
/// `max_age` seconds and covers them. Locations it does not contain are left
/// out (the doctor measures those now).
pub fn known_sizes(settings: &Effective, max_age: i64, now: i64) -> Result<Option<KnownSizes>, AppError> {
    use diskdrift_core::paths::components_under;
    let Some(store) = snapshots::open_store(settings)? else { return Ok(None) };
    let Some(m) = store.list()?.into_iter().rev().find(|m| m.status.is_usable()) else { return Ok(None) };
    let started = chrono::DateTime::parse_from_rfc3339(&m.started_at).map(|t| t.timestamp()).unwrap_or(0);
    if now - started > max_age {
        return Ok(None);
    }
    let paths: Vec<std::path::PathBuf> =
        diskdrift_diagnostics::suspects::macos_catalog().iter().map(|s| diskdrift_diagnostics::suspects::expand(s.path, &settings.home)).collect();
    let (paths, dirs): (Vec<_>, Vec<_>) =
        paths.into_iter().filter_map(|p| components_under(m.root.as_bytes(), &path_bytes(&p)).map(|c| (p, c))).unzip();
    if dirs.is_empty() {
        return Ok(None);
    }
    let corrupt = |e: std::io::Error| AppError::Store(diskdrift_storage::StoreError::Corrupt(e.to_string()));
    let (totals, large) = diskdrift_core::browse::totals(store.records(m.id)?, &dirs, 20).map_err(corrupt)?;
    let sizes = paths
        .into_iter()
        .zip(totals)
        .filter_map(|(p, t)| {
            let t = t?;
            let status = match () {
                _ if t.unreadable_dirs > 0 && t.entries == t.unreadable_dirs => MeasureStatus::Inaccessible,
                _ if t.incomplete => MeasureStatus::Partial,
                _ => MeasureStatus::Complete,
            };
            let measure = Measure {
                status,
                logical_bytes: t.logical_bytes,
                allocated_bytes: t.allocated_bytes,
                entries: t.entries,
                inaccessible_dirs: t.unreadable_dirs,
            };
            Some((p, measure))
        })
        .collect();
    let large_files = large
        .into_iter()
        .map(|c| LargeFile { path: c.path.lossy(), logical_bytes: c.logical_bytes, allocated_bytes: c.allocated_bytes })
        .collect();
    let source = snapshots::scan_label(&m, now);
    Ok(Some(KnownSizes { source, sizes, large_files }))
}

#[cfg(unix)]
fn path_bytes(path: &std::path::Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &std::path::Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

fn is_inside(path: &str, dir: &str) -> bool {
    path.len() > dir.len() && path.starts_with(dir) && path.as_bytes()[dir.len()] == b'/'
}

/// A candidate inside another: say so (the nearest), its size is counted in both.
fn mark_within(candidates: &mut [Candidate]) {
    let paths: Vec<(String, String)> = candidates.iter().filter_map(|c| Some((c.path.clone()?, c.label.clone()))).collect();
    for c in candidates.iter_mut() {
        let Some(p) = &c.path else { continue };
        c.within = paths.iter().filter(|(q, _)| is_inside(p, q)).max_by_key(|(q, _)| q.len()).map(|(_, label)| label.clone());
    }
}

/// Where growth sits, each change listed once: a folder is dropped when one of
/// its subfolders accounts for nearly all (90%) of its growth, and what is left
/// inside a listed folder is dropped too (browsing the folder shows it).
fn growth_targets<'a>(grew: &[&'a diskdrift_core::diff::DiffItem]) -> Vec<&'a diskdrift_core::diff::DiffItem> {
    let held: Vec<&diskdrift_core::diff::DiffItem> = grew
        .iter()
        .filter(|d| {
            let p = d.path.lossy();
            !grew.iter().any(|c| is_inside(&c.path.lossy(), &p) && c.delta_allocated * 10 >= d.delta_allocated * 9)
        })
        .copied()
        .collect();
    held.iter().filter(|d| !held.iter().any(|o| is_inside(&d.path.lossy(), &o.path.lossy()))).copied().collect()
}

/// The two most recent snapshots that can be compared with each other.
fn comparable_pair(settings: &Effective) -> Result<Option<(SnapshotMeta, SnapshotMeta)>, AppError> {
    let list = snapshots::list(settings)?;
    let usable: Vec<&SnapshotMeta> = list.snapshots.iter().map(|r| &r.meta).filter(|m| m.status.is_usable()).collect();
    let Some(latest) = usable.last() else { return Ok(None) };
    let prev = usable.iter().rev().skip(1).find(|m| m.incompatibility(latest).is_none());
    Ok(prev.map(|p| ((*p).clone(), (*latest).clone())))
}

pub fn deep(
    settings: &Effective,
    reference_free: Option<u64>,
    budget: Duration,
    cancel: &AtomicBool,
    progress: &dyn Fn(DoctorStep),
) -> Result<DeepReport, AppError> {
    let lister = diskdrift_platform::lister(ListerKind::Auto);
    let quick = doctor::run(&doctor_opts(settings, reference_free, budget, None), lister.as_ref(), cancel, progress);
    let u = settings.size_units.value;
    let list = snapshots::list(settings)?;
    let latest = list.snapshots.iter().rev().map(|r| &r.meta).find(|m| m.status.is_usable()).cloned();
    let latest_top = match &latest {
        Some(m) => Some(
            snapshots::top(settings, &m.id.to_string(), None, TopOptions { limit: 15, files: false, ..Default::default() })?.report,
        ),
        None => None,
    };
    let change = match comparable_pair(settings)? {
        Some((a, b)) => Some(snapshots::diff(
            settings,
            &a.id.to_string(),
            &b.id.to_string(),
            None,
            false,
            DiffOptions { limit: 15, ..Default::default() },
        )?),
        None => None,
    };

    let mut candidates = Vec::new();
    // OS-update snapshots belong to the sealed System volume and hold no user data.
    let holding = quick
        .snapshots
        .snapshots
        .iter()
        .filter(|s| !s.dataless && s.kind != diskdrift_diagnostics::apfs::SnapshotKind::OsUpdate)
        .count();
    if holding > 0 {
        candidates.push(format!(
            "{holding} local snapshot(s) of the data volume can keep the blocks of files deleted or rewritten since they were taken (size not reported by APFS)."
        ));
    }
    if quick.open_deleted.total_bytes > 0 {
        candidates.push(format!(
            "Deleted files still held open by running processes: {} observed ({}).",
            format_size(quick.open_deleted.total_bytes, u),
            quick.open_deleted.scope
        ));
    }
    if let Some(m) = &latest {
        if let Some(c) = &m.counts {
            if c.inaccessible_dirs > 0 {
                candidates.push(format!(
                    "{} directories could not be read in the latest scan; growth inside them is invisible.",
                    c.inaccessible_dirs
                ));
            }
            if c.skipped_mounts > 0 {
                candidates.push(format!(
                    "{} other filesystems were not entered; space on volumes in the same APFS container still counts against free space.",
                    c.skipped_mounts
                ));
            }
        }
        if m.root != "/" {
            candidates.push(format!("The scans cover {} only; changes elsewhere on the volume are not tracked.", m.root));
        }
    }
    if let Some(v) = quick.usage.primary()
        && let Some(p) = v.purgeable_estimate_bytes.filter(|&p| p > 0) {
            candidates.push(format!("macOS reports {} as purgeable (it frees this on demand).", format_size(p, u)));
        }
    candidates.push("APFS clones share blocks: copies count fully in each path's allocated size but not in physical usage.".into());

    let mut conclusion = Vec::new();
    match &change {
        Some(d) => {
            let s = &d.report.summary;
            conclusion.push(format!(
                "Between snapshot {} ({}) and {} ({}): tracked allocation changed by {}.",
                d.old.id,
                d.old.name,
                d.new.id,
                d.new.name,
                diskdrift_core::units::format_delta(s.net_allocated_bytes, u)
            ));
            if let Some(sp) = &d.space {
                conclusion.push(format!(
                    "The OS reports free space changed by {} over the same period.",
                    diskdrift_core::units::format_delta(-sp.free_consumed_bytes, u)
                ));
            }
            if let Some(top) = d.report.growing_dirs.iter().find(|x| x.path.lossy() != d.new.root) {
                conclusion.push(format!(
                    "Largest growth: {} ({}).",
                    top.path.lossy(),
                    diskdrift_core::units::format_delta(top.delta_allocated as i128, u)
                ));
            }
            match d.unreconciled_bytes {
                Some(x) if x.unsigned_abs() >= 1_000_000_000 => conclusion.push(format!(
                    "Unreconciled: {} of the change is not explained by the scanned inventory (see candidates; this is a diagnostic, not proof of missing files).",
                    diskdrift_core::units::format_delta(x, u)
                )),
                Some(_) => conclusion.push("The tracked inventory explains the OS-reported change to within 1 GB.".into()),
                None => {}
            }
        }
        None if latest.is_some() => conclusion.push(
            "Only one comparable snapshot exists: current consumers are listed, but change over time needs a second scan later (`diskdrift scan`).".into(),
        ),
        None => conclusion.push("No snapshots yet: run `diskdrift scan` to record a baseline, then again later to see what grew.".into()),
    }
    conclusion.push("No files were changed.".into());
    Ok(DeepReport { quick, latest, latest_top, change, unexplained_candidates: candidates, conclusion })
}

/// One read-only recovery suggestion.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub rank: usize,
    pub label: String,
    pub path: Option<String>,
    /// Observed allocated size, if measured.
    pub observed_bytes: Option<u64>,
    /// What could be reclaimed: "up to X" or "unknown". Never a promise.
    pub reclaimable: String,
    pub risk: String,
    pub confidence: String,
    pub how: String,
    /// The user skipped measuring it; size unknown.
    #[serde(default)]
    pub skipped: bool,
    /// For growth found by comparing scans: the two snapshot ids compared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub growth: Option<(u64, u64)>,
    /// A larger candidate that contains this one: its size is counted there too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub within: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoverReport {
    pub candidates: Vec<Candidate>,
    pub notes: Vec<String>,
    pub files_modified: bool,
}

fn risk_weight(r: Risk) -> f64 {
    match r {
        Risk::Low => 1.0,
        Risk::Medium => 0.6,
        Risk::High => 0.3,
        Risk::SystemManaged => 0.1,
    }
}

/// `skip`, when set while a location is measured, skips that location (it is
/// listed as skipped, with an unknown size).
pub fn recover(
    settings: &Effective,
    budget: Duration,
    cancel: &AtomicBool,
    skip: Option<Arc<AtomicBool>>,
    progress: &dyn Fn(DoctorStep),
) -> Result<RecoverReport, AppError> {
    let lister = diskdrift_platform::lister(ListerKind::Auto);
    let quick = doctor::run(&doctor_opts(settings, None, budget, skip), lister.as_ref(), cancel, progress);
    recover_from(settings, &quick)
}

/// Recovery suggestions from a diagnosis already made (no measuring), plus
/// recent growth from saved scans.
pub fn recover_from(settings: &Effective, quick: &DoctorReport) -> Result<RecoverReport, AppError> {
    let u: SizeUnits = settings.size_units.value;
    let mut scored: Vec<(f64, Candidate)> = Vec::new();
    for f in quick.findings.iter().filter(|f| {
        f.allocated_bytes >= 100_000_000 || matches!(f.status, MeasureStatus::Inaccessible | MeasureStatus::Skipped)
    }) {
        // Nested findings are listed on their own when they are the cleaner target.
        let skipped = f.status == MeasureStatus::Skipped;
        let observed = (!matches!(f.status, MeasureStatus::Inaccessible | MeasureStatus::Skipped)).then_some(f.allocated_bytes);
        let reclaimable = match (f.risk, observed) {
            _ if skipped => format!("unknown (skipped after {} measured)", format_size(f.allocated_bytes, u)),
            (Risk::SystemManaged, _) => "unknown (managed by macOS)".to_string(),
            (_, Some(b)) if f.status == MeasureStatus::Complete => format!("up to {}", format_size(b, u)),
            (_, Some(b)) => format!("at least {} measured; total unknown", format_size(b, u)),
            (_, None) => "unknown (not readable without Full Disk Access)".to_string(),
        };
        // Skipped ones go last: nothing is known about them.
        let score = if skipped { -1.0 } else { observed.unwrap_or(0) as f64 * risk_weight(f.risk) };
        scored.push((
            score,
            Candidate {
                rank: 0,
                label: f.label.clone(),
                path: Some(f.path.display().to_string()),
                observed_bytes: observed,
                reclaimable,
                risk: f.risk.label().into(),
                confidence: match f.confidence {
                    _ if skipped => "skipped".into(),
                    doctor::Confidence::Observed => "observed".into(),
                    doctor::Confidence::LowerBound => "lower bound".into(),
                    doctor::Confidence::Unknown => "unknown".into(),
                },
                how: f.recommendation.clone(),
                skipped,
                growth: None,
                within: None,
            },
        ));
    }
    let local = quick.snapshots.snapshots.iter().filter(|s| s.kind == diskdrift_diagnostics::apfs::SnapshotKind::TimeMachine && !s.dataless).count();
    if local > 0 {
        scored.push((
            f64::MAX / 4.0,
            Candidate {
                rank: 0,
                skipped: false,
                growth: None,
                within: None,
                label: format!("{local} local Time Machine snapshot(s)"),
                path: None,
                observed_bytes: None,
                reclaimable: "unknown (APFS does not report snapshot sizes)".into(),
                risk: "low".into(),
                confidence: "unknown".into(),
                how: "macOS deletes them automatically when space runs low. To review: `tmutil listlocalsnapshots /`. Deleting them (`tmutil deletelocalsnapshots <date>`) is your decision; DiskDrift never runs it.".into(),
            },
        ));
    }
    if quick.open_deleted.total_bytes >= 100_000_000 {
        let top = quick.open_deleted.files.first();
        scored.push((
            quick.open_deleted.total_bytes as f64,
            Candidate {
                rank: 0,
                skipped: false,
                growth: None,
                within: None,
                label: "Deleted files held open".into(),
                path: top.map(|f| f.path.clone()),
                observed_bytes: Some(quick.open_deleted.total_bytes),
                reclaimable: format!("up to {} when the processes close them", format_size(quick.open_deleted.total_bytes, u)),
                risk: "low".into(),
                confidence: "observed".into(),
                how: format!(
                    "Quit or restart the process holding them{}.",
                    top.map(|f| format!(" (largest: {} pid {})", f.command, f.pid)).unwrap_or_default()
                ),
            },
        ));
    }
    // Recent growth from saved scans.
    if let Some((a, b)) = comparable_pair(settings)? {
        let d = snapshots::diff(settings, &a.id.to_string(), &b.id.to_string(), None, false, DiffOptions { limit: 200, list_files: false, ..Default::default() })?;
        let grew: Vec<&diskdrift_core::diff::DiffItem> =
            d.report.growing_dirs.iter().filter(|x| x.delta_allocated >= 1_000_000_000 && x.path.lossy() != d.new.root).collect();
        let when = a.started_at.get(..16).unwrap_or(&a.started_at).replace('T', " ");
        for dir in growth_targets(&grew).into_iter().take(5) {
            let delta = diskdrift_core::units::format_delta(dir.delta_allocated as i128, u);
            scored.push((
                dir.delta_allocated as f64 * 0.5,
                Candidate {
                    rank: 0,
                    skipped: false,
                    growth: Some((a.id, b.id)),
                    within: None,
                    label: format!("Grew {delta} since scan #{}", a.id),
                    path: Some(dir.path.lossy()),
                    observed_bytes: Some(dir.new_allocated),
                    reclaimable: "depends on what grew; inspect it".into(),
                    risk: "unknown".into(),
                    confidence: "observed".into(),
                    how: format!(
                        "Grew by {delta} between scan #{} ({when}) and #{}. Run `diskdrift diff {} {} --path '{}'` to see which files.",
                        a.id,
                        b.id,
                        a.id,
                        b.id,
                        dir.path.lossy()
                    ),
                },
            ));
        }
    }
    let mut candidates: Vec<Candidate> = scored.iter().map(|(_, c)| c.clone()).collect();
    mark_within(&mut candidates);
    for ((_, c), m) in scored.iter_mut().zip(candidates) {
        c.within = m.within;
    }
    scored.sort_by(|x, y| y.0.partial_cmp(&x.0).unwrap_or(std::cmp::Ordering::Equal));
    let candidates = scored
        .into_iter()
        .enumerate()
        .map(|(i, (_, mut c))| {
            c.rank = i + 1;
            c
        })
        .collect();
    Ok(RecoverReport {
        candidates,
        notes: vec![
            "Ranked by observed size weighted by risk. Sizes overlap between nested locations; never add them up.".into(),
            "\"Up to\" is the most deleting that location could free; shared (cloned/hardlinked) blocks or snapshots may keep space allocated.".into(),
            "DiskDrift does not delete anything. Inspect each location yourself before removing data.".into(),
        ],
        files_modified: false,
    })
}

#[cfg(test)]
mod candidate_tests {
    use super::*;
    use diskdrift_core::diff::{Change, DiffItem};
    use diskdrift_core::record::Kind;

    fn grew(path: &str, delta: i64) -> DiffItem {
        DiffItem {
            path: path.as_bytes().to_vec().into(),
            kind: Kind::Dir,
            change: Change::Modified,
            old_logical: 0,
            new_logical: delta as u64,
            old_allocated: 0,
            new_allocated: delta as u64,
            delta_allocated: delta,
            delta_logical: delta,
            mtime: 0,
            btime: 0,
            is_dir: true,
        }
    }

    #[test]
    fn growth_is_listed_once_where_it_sits() {
        const G: i64 = 1_000_000_000;
        let items = [grew("/r/Caches", 2 * G), grew("/r/Caches/app", 2 * G), grew("/r/Caches/app/a", G + G / 5), grew("/r/Caches/app/b", G), grew("/r/Logs", G)];
        let refs: Vec<&DiffItem> = items.iter().collect();
        let kept: Vec<String> = growth_targets(&refs).iter().map(|d| d.path.lossy()).collect();
        // Caches is all app's growth; app's is split between a and b, so app is
        // where it sits (a and b are seen by browsing it).
        assert_eq!(kept, ["/r/Caches/app", "/r/Logs"]);
    }

    #[test]
    fn nested_candidates_name_the_nearest_container() {
        let c = |label: &str, path: &str| Candidate {
            rank: 0,
            label: label.into(),
            path: Some(path.into()),
            observed_bytes: None,
            reclaimable: String::new(),
            risk: String::new(),
            confidence: String::new(),
            how: String::new(),
            skipped: false,
            growth: None,
            within: None,
        };
        let mut v = [c("Caches", "/h/Caches"), c("pip", "/h/Caches/pip"), c("wheels", "/h/Caches/pip/wheels"), c("Caches2", "/h/Caches2")];
        mark_within(&mut v);
        let within: Vec<Option<&str>> = v.iter().map(|c| c.within.as_deref()).collect();
        assert_eq!(within, [None, Some("Caches"), Some("pip"), None], "a name prefix is not a parent");
    }
}

#[cfg(test)]
#[cfg(unix)]
mod known_sizes_tests {
    use super::*;
    use crate::config::{Overrides, resolve_with};
    use diskdrift_core::testutil::TestDir;
    use std::path::Path;

    #[test]
    #[cfg(not(windows))]
    fn sizes_come_from_a_fresh_scan_that_covers_them() {
        let t = TestDir::new("known-sizes");
        t.file("Library/Caches/app/blob", 300_000);
        t.file("Library/Caches/pip/wheel", 50_000);
        std::fs::write(t.path().join("config.json"), r#"{"schemaVersion":1}"#).unwrap();
        let ov = Overrides { config: Some(t.path().join("config.json")), db: Some(t.path().join("store")), ..Default::default() };
        let s = resolve_with(&ov, t.path(), Path::new("/")).unwrap();
        assert!(known_sizes(&s, 3600, 0).unwrap().is_none(), "no store yet");

        let req = snapshots::ScanRequest::from_settings(&s, Some(t.path().join("Library")), Some("lib".into()), &[]);
        let o = snapshots::scan(&s, &req, &AtomicBool::new(false), None).unwrap();
        let now = chrono::Local::now().timestamp();
        let k = known_sizes(&s, 3600, now).unwrap().expect("fresh scan covers ~/Library");
        let caches = k.sizes.get(&t.path().join("Library/Caches")).expect("user caches");
        assert_eq!(caches.status, MeasureStatus::Complete);
        assert!(caches.allocated_bytes >= 350_000, "{caches:?}");
        assert!(k.sizes.contains_key(&t.path().join("Library/Caches/pip")), "nested locations too");
        assert!(!k.sizes.contains_key(Path::new("/private/var/vm")), "outside the scan: measured live");
        assert!(k.large_files[0].path.ends_with("blob"));
        assert!(k.source.contains(&format!("#{}", o.snapshot.id)));
        assert!(known_sizes(&s, 3600, now + 7200).unwrap().is_none(), "too old");

        // A folder analysis from the same scan, without walking the folder.
        let a = crate::analyze::analyze_from_scan(&s, &t.path().join("Library/Caches"), 10, 3600, now).unwrap().expect("in the scan");
        let names: Vec<_> = a.items.iter().map(|i| (i.name.as_str(), i.status)).collect();
        assert_eq!(names, [("app", MeasureStatus::Complete), ("pip", MeasureStatus::Complete)]);
        assert!(a.sizes_from.as_deref().unwrap().contains(&format!("#{}", o.snapshot.id)));
        assert!(crate::analyze::analyze_from_scan(&s, Path::new("/private/var"), 10, 3600, now).unwrap().is_none(), "outside the scan");
    }
}
