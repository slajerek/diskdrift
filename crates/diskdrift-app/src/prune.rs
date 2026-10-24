//! Removing many snapshots at once, retention rules and compacting the store
//! (spec 3.4a). Only DiskDrift's own store changes; scanned files never do.
//! Nothing is removed without the caller asking for exactly these snapshots:
//! [`propose`] says what a selection would remove, [`remove`] removes it.

use crate::config::Effective;
use crate::snapshots::{AppError, Result, SnapshotRow, require_store};
use chrono::{DateTime, Datelike, FixedOffset, NaiveDate};
use diskdrift_storage::{Compaction, CompactStep, SnapshotMeta, Status};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicBool;

/// Which scans a retention run keeps, per disk and folder. A scan is kept when
/// any rule keeps it; the newest usable scan is always kept. All zero: off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Retention {
    /// The newest N scans.
    pub keep_last: usize,
    /// The newest scan of each of the last N calendar weeks (this one included).
    pub keep_weekly: usize,
    /// The newest scan of each of the last N calendar months (this one included).
    pub keep_monthly: usize,
}

impl Retention {
    pub fn is_off(&self) -> bool {
        *self == Retention::default()
    }

    /// "the last 5, one a week for 12 weeks, one a month for 24 months".
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.keep_last > 0 {
            parts.push(format!("the last {}", self.keep_last));
        }
        if self.keep_weekly > 0 {
            parts.push(format!("one a week for {} weeks", self.keep_weekly));
        }
        if self.keep_monthly > 0 {
            parts.push(format!("one a month for {} months", self.keep_monthly));
        }
        if parts.is_empty() { "no rules".into() } else { parts.join(", ") }
    }
}

/// What to remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// These snapshots (references as `snapshots` accepts them).
    Refs(Vec<String>),
    /// These snapshots, by id.
    Ids(Vec<u64>),
    /// Every cancelled and failed scan: they hold no data.
    Failed,
    /// Every scan started before this day (local time).
    Before(NaiveDate),
    /// What the retention rules do not keep.
    Retention(Retention),
}

/// What a selection would remove.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Proposal {
    pub snapshots: Vec<SnapshotRow>,
    /// Allocated bytes of their files: about what removing them frees.
    pub bytes: u64,
    /// Kept snapshots rewritten so they stay readable.
    pub rewritten: usize,
    /// Of every scan the selection looked at (one disk's, or all).
    pub kept: usize,
}

/// What removal did.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Removed {
    /// Ids of the removed snapshots.
    pub removed: Vec<u64>,
    pub rewritten: Vec<u64>,
    pub freed_bytes: u64,
    pub cancelled: bool,
    /// Snapshots compaction would now store tighter.
    pub uncompacted: usize,
}

/// `YYYY-MM-DD`.
pub fn parse_day(s: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").map_err(|_| AppError::Usage(format!("'{s}' is not a date (YYYY-MM-DD)")))
}

fn started(m: &SnapshotMeta) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(&m.started_at).ok()
}

/// The scans of one disk and folder are compared, so they are pruned together.
pub fn scan_group(m: &SnapshotMeta) -> (String, String) {
    (m.disk.clone().unwrap_or_default(), m.root_in_volume.clone().unwrap_or_else(|| m.root.clone()))
}

/// The ids of the usable scans `rules` do not keep. Scans without data are
/// left to [`Selection::Failed`]; a scan whose start cannot be read is kept.
pub fn retention_removals(metas: &[SnapshotMeta], rules: Retention, now: DateTime<FixedOffset>) -> Vec<u64> {
    if rules.is_off() {
        return Vec::new();
    }
    let mut groups: BTreeMap<(String, String), Vec<&SnapshotMeta>> = BTreeMap::new();
    for m in metas.iter().filter(|m| m.status.is_usable()) {
        groups.entry(scan_group(m)).or_default().push(m);
    }
    let today = now.date_naive();
    let monday = |d: NaiveDate| d - chrono::Days::new(d.weekday().num_days_from_monday() as u64);
    let month = |d: NaiveDate| d.year() as i64 * 12 + d.month0() as i64;
    let mut out = Vec::new();
    for (_, mut scans) in groups {
        // Newest first: in each bucket the first one seen is kept.
        scans.sort_by_key(|m| std::cmp::Reverse((started(m), m.id)));
        let mut keep = BTreeSet::new();
        let mut weeks = BTreeSet::new();
        let mut months = BTreeSet::new();
        for (i, m) in scans.iter().enumerate() {
            let Some(at) = started(m) else {
                keep.insert(m.id);
                continue;
            };
            let day = at.with_timezone(now.offset()).date_naive();
            if i == 0 || i < rules.keep_last {
                keep.insert(m.id);
            }
            let w = (monday(today) - monday(day)).num_days() / 7;
            if w >= 0 && (w as usize) < rules.keep_weekly && weeks.insert(w) {
                keep.insert(m.id);
            }
            let mo = month(today) - month(day);
            if mo >= 0 && (mo as usize) < rules.keep_monthly && months.insert(mo) {
                keep.insert(m.id);
            }
        }
        out.extend(scans.iter().filter(|m| !keep.contains(&m.id)).map(|m| m.id));
    }
    out.sort_unstable();
    out
}

/// The ids `sel` picks among `metas`.
fn select(store: &diskdrift_storage::Store, metas: &[SnapshotMeta], sel: &Selection) -> Result<Vec<u64>> {
    let mut ids: Vec<u64> = match sel {
        Selection::Refs(refs) => refs.iter().map(|r| store.resolve(r).map(|m| m.id)).collect::<std::result::Result<_, _>>()?,
        Selection::Ids(ids) => ids.clone(),
        Selection::Failed => metas.iter().filter(|m| matches!(m.status, Status::Failed | Status::Cancelled)).map(|m| m.id).collect(),
        Selection::Before(day) => metas
            .iter()
            .filter(|m| m.status != Status::InProgress && started(m).is_some_and(|at| at.date_naive() < *day))
            .map(|m| m.id)
            .collect(),
        Selection::Retention(rules) => {
            if rules.is_off() {
                return Err(AppError::Usage(
                    "no retention rules: pass --keep-last, --keep-weekly or --keep-monthly, or set storage.keepLast, storage.keepWeekly or storage.keepMonthly".into(),
                ));
            }
            retention_removals(metas, *rules, chrono::Local::now().fixed_offset())
        }
    };
    ids.sort_unstable();
    ids.dedup();
    if let Some(m) = metas.iter().find(|m| ids.contains(&m.id) && m.status == Status::InProgress) {
        return Err(AppError::Usage(format!("snapshot {} is being written by a scan now; it cannot be removed", m.id)));
    }
    Ok(ids)
}

/// What `sel` would remove, among one disk's scans or all (nothing changes).
pub fn propose(settings: &Effective, sel: &Selection, disk: Option<&str>) -> Result<Proposal> {
    let store = require_store(settings)?;
    let reg = store.registry()?;
    let filter = disk.map(|d| crate::disks::resolve(&store, d)).transpose()?;
    let metas: Vec<SnapshotMeta> = store
        .list()?
        .into_iter()
        .filter(|m| filter.as_ref().is_none_or(|d| m.disk.as_deref() == Some(d.id.as_str())))
        .collect();
    let ids = select(&store, &metas, sel)?;
    if let Selection::Refs(_) = sel
        && let Some(d) = &filter
        && let Some(id) = ids.iter().find(|id| !metas.iter().any(|m| m.id == **id))
    {
        return Err(AppError::Usage(format!("snapshot {id} is not a scan of disk {}", d.display_name())));
    }
    let plan = store.plan_removal(&ids)?;
    let all = store.list()?;
    let snapshots = all
        .into_iter()
        .filter(|m| ids.contains(&m.id))
        .map(|m| SnapshotRow { size_on_disk: store.snapshot_size(&m), disk_name: crate::disks::disk_name(&reg, &m), meta: m })
        .collect();
    Ok(Proposal { snapshots, bytes: plan.bytes, rewritten: plan.rewrites.len(), kept: metas.len().saturating_sub(ids.len()) })
}

/// Removes the snapshots `ids` (as [`propose`] listed them). `cancel` stops it
/// before anything is removed.
pub fn remove(settings: &Effective, ids: &[u64], cancel: &AtomicBool) -> Result<Removed> {
    let store = require_store(settings)?;
    let lock = store.lock()?;
    let r = store.remove_many(&lock, ids, cancel)?;
    Ok(Removed { removed: r.removed, rewritten: r.rewritten, freed_bytes: r.freed_bytes, cancelled: r.cancelled, uncompacted: store.uncompacted()? })
}

/// Compacts the store (see `Store::compact`).
pub fn compact(settings: &Effective, cancel: &AtomicBool, progress: &mut dyn FnMut(CompactStep)) -> Result<Compaction> {
    let store = require_store(settings)?;
    let lock = store.lock()?;
    Ok(store.compact(&lock, cancel, progress)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Overrides, resolve_with};
    use diskdrift_core::testutil::{TestDir, records};
    use diskdrift_core::record::Kind;
    use diskdrift_core::walk::WalkReport;
    use diskdrift_storage::{ScanOptions, Store, VolumeIdentity};

    fn meta(id: u64, at: &str, disk: &str, status: Status) -> SnapshotMeta {
        SnapshotMeta {
            format_version: diskdrift_storage::meta::META_FORMAT_VERSION,
            id,
            name: format!("s{id}"),
            root: "/r".into(),
            disk: Some(disk.into()),
            root_in_volume: Some("/".into()),
            status,
            started_at: at.into(),
            finished_at: None,
            volume: VolumeIdentity::default(),
            options: ScanOptions::default(),
            space_before: None,
            space_after: None,
            storage: Default::default(),
            counts: None,
            error: None,
            diskdrift_version: "test".into(),
        }
    }

    #[test]
    fn retention_keeps_the_last_weekly_and_monthly() {
        let now = DateTime::parse_from_rfc3339("2026-10-23T12:00:00+02:00").unwrap();
        let days = [0, 1, 2, 8, 9, 16, 40, 41, 75, 400];
        let metas: Vec<SnapshotMeta> = days
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let at = now - chrono::Duration::days(*d);
                meta(100 - i as u64, &at.to_rfc3339(), "d1", Status::Completed)
            })
            .collect();
        let id_of = |d: i64| 100 - days.iter().position(|x| *x == d).unwrap() as u64;
        // Only the last 2.
        let r = retention_removals(&metas, Retention { keep_last: 2, ..Default::default() }, now);
        assert_eq!(r.len(), days.len() - 2);
        assert!(!r.contains(&id_of(0)) && !r.contains(&id_of(1)));
        // Weekly for 3 weeks: the newest of this week (day 0), last week (day 8: Oct 15),
        // and the week before (day 16: Oct 7).
        let r = retention_removals(&metas, Retention { keep_weekly: 3, ..Default::default() }, now);
        let kept: Vec<u64> = metas.iter().map(|m| m.id).filter(|id| !r.contains(id)).collect();
        assert_eq!(kept, vec![id_of(0), id_of(8), id_of(16)]);
        // Monthly for 4 months: October (day 0), September (day 40), August (day 75).
        let r = retention_removals(&metas, Retention { keep_monthly: 4, ..Default::default() }, now);
        let kept: Vec<u64> = metas.iter().map(|m| m.id).filter(|id| !r.contains(id)).collect();
        assert_eq!(kept, vec![id_of(0), id_of(40), id_of(75)]);
        // Off: nothing. Another disk is pruned on its own; failed scans are not touched.
        assert!(retention_removals(&metas, Retention::default(), now).is_empty());
        let mut more = metas.clone();
        more.push(meta(1, "2020-01-01T10:00:00+01:00", "d2", Status::Completed));
        more.push(meta(2, "2020-01-01T10:00:00+01:00", "d1", Status::Failed));
        let r = retention_removals(&more, Retention { keep_last: 1, ..Default::default() }, now);
        assert!(!r.contains(&1) && !r.contains(&2), "the only scan of d2 and a failed scan stay");
    }

    #[test]
    fn propose_then_remove() {
        let t = TestDir::new("app-prune");
        let db = t.path().join("store");
        let settings = resolve_with(&Overrides { db: Some(db.clone()), ..Default::default() }, t.path(), t.path()).unwrap();
        let store = Store::create(&db).unwrap();
        let days = ["2026-09-01", "2026-09-10", "2026-10-01", "2026-10-20"];
        for (i, d) in days.iter().enumerate() {
            let lock = store.lock().unwrap();
            let mut m = meta(0, &format!("{d}T10:00:00+02:00"), "", Status::InProgress);
            m.disk = None;
            m.root_in_volume = None;
            m.name = format!("s{i}");
            m.volume = VolumeIdentity { uuid: Some("U".into()), fs_type: "apfs".into(), mount_point: "/".into(), ..Default::default() };
            let mut w = store.begin(lock, m, None).unwrap();
            for r in records(&[("".into(), Kind::Dir, 0), (format!("f{i}"), Kind::File, i as u64 * 1000)]) {
                w.push(&r).unwrap();
            }
            w.commit_and_rebase(WalkReport::default(), None, "now".into()).unwrap().1.unwrap();
        }
        let lock = store.lock().unwrap();
        let mut w = store.begin(lock, { let mut m = meta(0, "2026-10-21T10:00:00+02:00", "", Status::InProgress); m.disk = None; m.name = "x".into(); m }, None).unwrap();
        w.push(&records(&[(String::new(), Kind::Dir, 0)])[0]).unwrap();
        w.abort(Status::Cancelled, None, None, "now".into());
        let p = propose(&settings, &Selection::Failed, None).unwrap();
        assert_eq!(p.snapshots.iter().map(|r| r.meta.name.as_str()).collect::<Vec<_>>(), vec!["x"]);
        let p = propose(&settings, &Selection::Before(parse_day("2026-10-01").unwrap()), None).unwrap();
        assert_eq!(p.snapshots.len(), 2);
        assert_eq!((p.rewritten, p.kept), (0, 3), "the oldest go: nothing depends on them");
        let p = propose(&settings, &Selection::Refs(vec!["s1".into(), "s2".into()]), None).unwrap();
        assert_eq!(p.rewritten, 1, "s0 is rewritten against s3");
        assert!(propose(&settings, &Selection::Retention(Retention::default()), None).is_err());
        assert!(parse_day("1 Oct").is_err());
        assert_eq!(store.list().unwrap().len(), 5, "proposing changes nothing");
        let ids: Vec<u64> = p.snapshots.iter().map(|r| r.meta.id).collect();
        let r = remove(&settings, &ids, &AtomicBool::new(false)).unwrap();
        assert_eq!(r.removed.len(), 2);
        assert_eq!(r.rewritten.len(), 1);
        let left: Vec<String> = store.list().unwrap().into_iter().map(|m| m.name).collect();
        assert_eq!(left, vec!["s0", "s3", "x"]);
        let c = compact(&settings, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert!(!c.cancelled && c.checked == 2, "{c:?}");
    }
}
