//! Snapshot comparison by merge-joining two canonical record streams.
//!
//! Both streams are depth-first pre-order with children sorted by name, so a
//! single forward pass over each matches entries by path in O(n + m) time with
//! memory bounded by tree depth plus the requested result limits.

use crate::measurement::{Envelope, Quality};
use crate::native::{NativeName, NativePath};
use crate::paths::{PathStack, compare_paths, join_components};
use crate::record::{
    CoverageChange, EntryFields, FileId, FileIdScheme, FileIdScope, Kind, MoveCertainty, Record,
    TimestampResolution, flags,
};
use serde::{Deserialize, Serialize, Serializer};
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::io;

pub(crate) const UNKNOWN_DELTA: i64 = i64::MIN;

pub type RecordStream<'a> = Box<dyn Iterator<Item = io::Result<Record>> + 'a>;

/// `stream` with its root record (depth 0, whose name is the full root path)
/// renamed to `root`. Two scans of one disk mounted at different places then
/// compare as the same tree (ADR 0002, section 2.6).
pub fn with_root_name<'a>(stream: RecordStream<'a>, root: NativePath) -> RecordStream<'a> {
    Box::new(stream.map(move |r| {
        r.map(|mut rec| {
            if rec.depth == 0 {
                rec.name = match &root {
                    NativePath::Unix(bytes) => NativeName::unix(bytes.clone()),
                    NativePath::Windows(units) => NativeName::windows(units.clone()),
                };
            }
            rec
        })
    }))
}

/// How an entry changed between the older (A) and newer (B) snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Change {
    Unchanged,
    Added,
    Deleted,
    /// Present in both, metadata differs.
    Modified,
    /// Below a directory that could not be read in A but could in B.
    NewlyVisible,
    /// Below a directory that could be read in A but not in B.
    NoLongerVisible,
    /// The set of observed metadata fields changed without a content change.
    CoverageChanged,
}

/// One matched position of the merge.
pub struct DiffEntry<'a> {
    /// Path components (`[root, name, ...]`).
    pub path: &'a [NativeName],
    pub change: Change,
    pub coverage: CoverageChange,
    pub old: Option<&'a Record>,
    pub new: Option<&'a Record>,
}

pub trait DiffVisitor {
    fn entry(&mut self, e: &DiffEntry<'_>) -> io::Result<()>;
}

struct Side<'a> {
    iter: RecordStream<'a>,
    cur: Option<Record>,
    stack: PathStack,
}

impl<'a> Side<'a> {
    fn new(iter: RecordStream<'a>) -> io::Result<Self> {
        let mut s = Side {
            iter,
            cur: None,
            stack: PathStack::new(),
        };
        s.advance()?;
        Ok(s)
    }

    fn advance(&mut self) -> io::Result<()> {
        self.cur = self.iter.next().transpose()?;
        if let Some(r) = &self.cur {
            self.stack.set(r.depth, &r.name)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct NodeState {
    a_flags: Option<u8>,
    b_flags: Option<u8>,
    change: Change,
}

/// Merges two canonical streams and reports every position to `visitor`.
pub fn merge(
    a: RecordStream<'_>,
    b: RecordStream<'_>,
    visitor: &mut dyn DiffVisitor,
) -> io::Result<()> {
    let mut a = Side::new(a)?;
    let mut b = Side::new(b)?;
    let mut states: Vec<NodeState> = Vec::new();
    loop {
        let order = match (&a.cur, &b.cur) {
            (None, None) => return Ok(()),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), Some(_)) => compare_paths(a.stack.components(), b.stack.components())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
        };
        match order {
            Ordering::Less => {
                let rec = a.cur.as_ref().unwrap();
                let depth = rec.depth as usize;
                states.truncate(depth);
                let change = match depth.checked_sub(1).and_then(|p| states.get(p)) {
                    Some(p) if matches!(p.change, Change::Deleted | Change::NoLongerVisible) => {
                        p.change
                    }
                    Some(p) if p.b_flags.is_some_and(|f| f & flags::CONTENTS_UNKNOWN != 0) => {
                        Change::NoLongerVisible
                    }
                    _ => Change::Deleted,
                };
                states.push(NodeState {
                    a_flags: Some(rec.flags),
                    b_flags: None,
                    change,
                });
                visitor.entry(&DiffEntry {
                    path: a.stack.components(),
                    change,
                    coverage: CoverageChange::default(),
                    old: Some(rec),
                    new: None,
                })?;
                a.advance()?;
            }
            Ordering::Greater => {
                let rec = b.cur.as_ref().unwrap();
                let depth = rec.depth as usize;
                states.truncate(depth);
                let change = match depth.checked_sub(1).and_then(|p| states.get(p)) {
                    Some(p) if matches!(p.change, Change::Added | Change::NewlyVisible) => p.change,
                    Some(p) if p.a_flags.is_some_and(|f| f & flags::CONTENTS_UNKNOWN != 0) => {
                        Change::NewlyVisible
                    }
                    _ => Change::Added,
                };
                states.push(NodeState {
                    a_flags: None,
                    b_flags: Some(rec.flags),
                    change,
                });
                visitor.entry(&DiffEntry {
                    path: b.stack.components(),
                    change,
                    coverage: CoverageChange::default(),
                    old: None,
                    new: Some(rec),
                })?;
                b.advance()?;
            }
            Ordering::Equal => {
                let (ra, rb) = (a.cur.as_ref().unwrap(), b.cur.as_ref().unwrap());
                let depth = rb.depth as usize;
                states.truncate(depth);
                let coverage = ra.coverage_change(rb);
                let change = if ra.content_differs(rb) {
                    Change::Modified
                } else if !coverage.is_empty() {
                    Change::CoverageChanged
                } else {
                    Change::Unchanged
                };
                states.push(NodeState {
                    a_flags: Some(ra.flags),
                    b_flags: Some(rb.flags),
                    change,
                });
                visitor.entry(&DiffEntry {
                    path: b.stack.components(),
                    change,
                    coverage,
                    old: Some(ra),
                    new: Some(rb),
                })?;
                a.advance()?;
                b.advance()?;
            }
        }
    }
}

/// Sort order for listed entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum DiffSort {
    /// Largest allocated-size increase first.
    #[default]
    Growth,
    /// Largest allocated size in the newer snapshot first.
    Size,
    /// Most recently modified (newer snapshot) first.
    Modified,
    /// Most recently created (newer snapshot) first.
    Created,
    /// Canonical path order.
    Path,
}

#[derive(Debug, Clone)]
pub struct DiffOptions {
    /// Restrict to this path (components including the root) and below.
    pub prefix: Option<Vec<NativeName>>,
    /// Only list entries whose |allocated delta| is at least this.
    pub min_size: u64,
    pub limit: usize,
    pub sort: DiffSort,
    pub list_files: bool,
    pub list_dirs: bool,
    /// Pair deleted and added files with the same inode and size as moves.
    pub detect_moves: bool,
    /// Whether file identity has the same scheme and scope in both snapshots.
    /// When it does not, matching IDs can only provide probable move evidence.
    pub file_id_comparable: bool,
    /// Whether allocated-size values have the same measurement semantics in
    /// both snapshots. Stored deltas remain lossless when this is false, but
    /// comparison output must not present allocated-byte changes as facts.
    pub allocated_deltas: bool,
    /// Cap on files remembered for move detection.
    pub move_candidates_cap: usize,
}

fn known_metric(record: &Record, field: EntryFields) -> bool {
    !record.has(flags::STAT_ERROR) && record.available.contains(field)
}

fn metric_value(record: Option<&Record>, field: EntryFields) -> Option<u64> {
    let record = record?;
    known_metric(record, field).then_some(if field == EntryFields::LOGICAL_SIZE {
        record.counted_size()
    } else {
        record.counted_alloc()
    })
}

fn comparison_measurement(quality: Quality) -> Envelope {
    if quality == Quality::Unknown {
        Envelope::default()
    } else {
        Envelope::with_quality(
            "filesystem",
            "snapshot comparison",
            quality,
            "canonical snapshot records",
        )
    }
}

impl Default for DiffOptions {
    fn default() -> Self {
        DiffOptions {
            prefix: None,
            min_size: 0,
            limit: 50,
            sort: DiffSort::Growth,
            list_files: true,
            list_dirs: true,
            detect_moves: true,
            file_id_comparable: true,
            allocated_deltas: true,
            move_candidates_cap: 500_000,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Tally {
    #[serde(default)]
    pub count: u64,
    #[serde(default)]
    pub logical_bytes: u64,
    #[serde(default)]
    pub allocated_bytes: u64,
    #[serde(skip)]
    logical_known: u64,
    #[serde(skip)]
    allocated_known: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TallyWire {
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    logical_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allocated_bytes: Option<u64>,
    #[serde(flatten)]
    measurement: Envelope,
}

impl Serialize for Tally {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let logical_any = self.logical_known > 0;
        let allocated_any = self.allocated_known > 0;
        let any_known = logical_any || allocated_any;
        let complete = self.count > 0
            && self.logical_known == self.count
            && self.allocated_known == self.count;
        TallyWire {
            count: (self.count > 0).then_some(self.count),
            logical_bytes: logical_any.then_some(self.logical_bytes),
            allocated_bytes: allocated_any.then_some(self.allocated_bytes),
            measurement: comparison_measurement(if !any_known {
                Quality::Unknown
            } else if complete {
                Quality::Observed
            } else {
                Quality::LowerBound
            }),
        }
        .serialize(serializer)
    }
}

impl Tally {
    pub fn logical_quality(&self) -> Quality {
        if self.count == 0 || self.logical_known == 0 {
            Quality::Unknown
        } else if self.logical_known < self.count {
            Quality::LowerBound
        } else {
            Quality::Observed
        }
    }

    pub fn allocated_quality(&self) -> Quality {
        if self.count == 0 || self.allocated_known == 0 {
            Quality::Unknown
        } else if self.allocated_known < self.count {
            Quality::LowerBound
        } else {
            Quality::Observed
        }
    }

    fn add_with_allocation(&mut self, r: &Record, allocated_deltas: bool) {
        self.count += 1;
        if known_metric(r, EntryFields::LOGICAL_SIZE) {
            self.logical_bytes += r.counted_size();
            self.logical_known += 1;
        }
        if allocated_deltas && known_metric(r, EntryFields::ALLOCATED_SIZE) {
            self.allocated_bytes += r.counted_alloc();
            self.allocated_known += 1;
        }
    }

    fn add_delta(&mut self, logical: Option<u64>, allocated: Option<u64>) {
        self.count += 1;
        if let Some(bytes) = logical {
            self.logical_bytes += bytes;
            self.logical_known += 1;
        }
        if let Some(bytes) = allocated {
            self.allocated_bytes += bytes;
            self.allocated_known += 1;
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffSummary {
    pub added: Tally,
    pub deleted: Tally,
    /// Modified entries whose allocation grew / shrank / stayed equal.
    pub grown: Tally,
    pub shrunk: Tally,
    #[serde(default)]
    pub modified_same_size: u64,
    #[serde(default)]
    pub unchanged: u64,
    pub newly_visible: Tally,
    pub no_longer_visible: Tally,
    #[serde(default)]
    pub coverage_changed: u64,
    /// Net change of tracked sizes, excluding coverage changes.
    #[serde(default)]
    pub net_logical_bytes: i128,
    #[serde(default)]
    pub net_allocated_bytes: i128,
    #[serde(default)]
    pub moved: u64,
    #[serde(default)]
    pub moved_bytes: u64,
    /// Total tracked sizes in each snapshot (within the prefix).
    #[serde(default)]
    pub old_total_allocated: u64,
    #[serde(default)]
    pub new_total_allocated: u64,
    #[serde(default)]
    pub old_total_logical: u64,
    #[serde(default)]
    pub new_total_logical: u64,
    #[serde(skip)]
    old_total_allocated_records: u64,
    #[serde(skip)]
    old_total_allocated_known: u64,
    #[serde(skip)]
    new_total_allocated_records: u64,
    #[serde(skip)]
    new_total_allocated_known: u64,
    #[serde(skip)]
    old_total_logical_records: u64,
    #[serde(skip)]
    old_total_logical_known: u64,
    #[serde(skip)]
    new_total_logical_records: u64,
    #[serde(skip)]
    new_total_logical_known: u64,
    #[serde(skip)]
    net_allocated_records: u64,
    #[serde(skip)]
    net_allocated_known: u64,
    #[serde(skip)]
    net_allocated_unknown: bool,
    #[serde(skip)]
    net_logical_records: u64,
    #[serde(skip)]
    net_logical_known: u64,
    #[serde(skip)]
    net_logical_unknown: bool,
}

impl DiffSummary {
    /// Quality of the net allocated-byte delta. A partial record stream cannot
    /// produce a precise net change even when some individual values are known.
    pub fn net_allocated_quality(&self) -> Quality {
        if self.net_allocated_records == 0 || self.net_allocated_known == 0 {
            Quality::Unknown
        } else if self.net_allocated_unknown {
            Quality::LowerBound
        } else {
            Quality::Observed
        }
    }

    /// Quality of the net logical-byte delta.
    pub fn net_logical_quality(&self) -> Quality {
        if self.net_logical_records == 0 || self.net_logical_known == 0 {
            Quality::Unknown
        } else if self.net_logical_unknown {
            Quality::LowerBound
        } else {
            Quality::Observed
        }
    }

    pub fn has_measurements(&self) -> bool {
        self.added != Tally::default()
            || self.deleted != Tally::default()
            || self.grown != Tally::default()
            || self.shrunk != Tally::default()
            || self.modified_same_size > 0
            || self.unchanged > 0
            || self.newly_visible != Tally::default()
            || self.no_longer_visible != Tally::default()
            || self.coverage_changed > 0
            || self.net_logical_bytes != 0
            || self.net_allocated_bytes != 0
            || self.moved > 0
            || self.moved_bytes > 0
            || self.old_total_allocated > 0
            || self.new_total_allocated > 0
            || self.old_total_logical > 0
            || self.new_total_logical > 0
    }

    fn quality(&self) -> Quality {
        let mut any_known = false;
        let mut complete = true;
        for (known, records) in [
            (self.added.logical_known, self.added.count),
            (self.added.allocated_known, self.added.count),
            (self.deleted.logical_known, self.deleted.count),
            (self.deleted.allocated_known, self.deleted.count),
            (self.grown.logical_known, self.grown.count),
            (self.grown.allocated_known, self.grown.count),
            (self.shrunk.logical_known, self.shrunk.count),
            (self.shrunk.allocated_known, self.shrunk.count),
            (self.newly_visible.logical_known, self.newly_visible.count),
            (self.newly_visible.allocated_known, self.newly_visible.count),
            (
                self.no_longer_visible.logical_known,
                self.no_longer_visible.count,
            ),
            (
                self.no_longer_visible.allocated_known,
                self.no_longer_visible.count,
            ),
            (
                self.old_total_allocated_known,
                self.old_total_allocated_records,
            ),
            (
                self.new_total_allocated_known,
                self.new_total_allocated_records,
            ),
            (self.old_total_logical_known, self.old_total_logical_records),
            (self.new_total_logical_known, self.new_total_logical_records),
        ] {
            if records > 0 {
                any_known |= known > 0;
                complete &= known == records;
            }
        }
        for (records, unknown) in [
            (self.net_allocated_records, self.net_allocated_unknown),
            (self.net_logical_records, self.net_logical_unknown),
        ] {
            if records > 0 {
                any_known |= !unknown;
                complete &= !unknown;
            }
        }
        if self.moved > 0 {
            any_known = true;
        }
        if !any_known {
            Quality::Unknown
        } else if complete {
            Quality::Observed
        } else {
            Quality::LowerBound
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DiffSummaryWire<'a> {
    added: &'a Tally,
    deleted: &'a Tally,
    grown: &'a Tally,
    shrunk: &'a Tally,
    #[serde(skip_serializing_if = "Option::is_none")]
    modified_same_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unchanged: Option<u64>,
    newly_visible: &'a Tally,
    no_longer_visible: &'a Tally,
    #[serde(skip_serializing_if = "Option::is_none")]
    coverage_changed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    net_logical_bytes: Option<i128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    net_allocated_bytes: Option<i128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    moved: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    moved_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_total_allocated: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_total_allocated: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_total_logical: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_total_logical: Option<u64>,
    #[serde(flatten)]
    measurement: Envelope,
}

impl Serialize for DiffSummary {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let known = self.has_measurements();
        let old_total_allocated =
            (self.old_total_allocated_known > 0).then_some(self.old_total_allocated);
        let new_total_allocated =
            (self.new_total_allocated_known > 0).then_some(self.new_total_allocated);
        let old_total_logical =
            (self.old_total_logical_known > 0).then_some(self.old_total_logical);
        let new_total_logical =
            (self.new_total_logical_known > 0).then_some(self.new_total_logical);
        DiffSummaryWire {
            added: &self.added,
            deleted: &self.deleted,
            grown: &self.grown,
            shrunk: &self.shrunk,
            modified_same_size: known.then_some(self.modified_same_size),
            unchanged: known.then_some(self.unchanged),
            newly_visible: &self.newly_visible,
            no_longer_visible: &self.no_longer_visible,
            coverage_changed: known.then_some(self.coverage_changed),
            net_logical_bytes: (self.net_logical_records > 0 && !self.net_logical_unknown)
                .then_some(self.net_logical_bytes),
            net_allocated_bytes: (self.net_allocated_records > 0 && !self.net_allocated_unknown)
                .then_some(self.net_allocated_bytes),
            moved: known.then_some(self.moved),
            moved_bytes: known.then_some(self.moved_bytes),
            old_total_allocated,
            new_total_allocated,
            old_total_logical,
            new_total_logical,
            measurement: comparison_measurement(self.quality()),
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffItem {
    #[serde(flatten)]
    pub path: NativePath,
    pub kind: Kind,
    pub change: Change,
    #[serde(default)]
    pub old_logical: u64,
    #[serde(default)]
    pub new_logical: u64,
    #[serde(default)]
    pub old_allocated: u64,
    #[serde(default)]
    pub new_allocated: u64,
    #[serde(default)]
    pub delta_allocated: i64,
    #[serde(default)]
    pub delta_logical: i64,
    #[serde(default)]
    pub mtime: i64,
    #[serde(default)]
    pub btime: i64,
    /// For directories: inclusive totals of the subtree.
    pub is_dir: bool,
}

#[derive(Debug, Clone, Copy, Default)]
enum MetricCoverage {
    #[default]
    Unknown,
    Partial,
    Complete,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DiffItemWire<'a> {
    #[serde(flatten)]
    path: &'a NativePath,
    kind: Kind,
    change: Change,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_logical: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_logical: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_allocated: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_allocated: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta_allocated: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta_logical: Option<i64>,
    #[serde(skip_serializing_if = "is_zero_i64")]
    mtime: i64,
    #[serde(skip_serializing_if = "is_zero_i64")]
    btime: i64,
    is_dir: bool,
    #[serde(flatten)]
    measurement: Envelope,
}

fn is_zero_i64(value: &i64) -> bool {
    *value == 0
}

impl Serialize for DiffItem {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let visibility_only = matches!(self.change, Change::NewlyVisible | Change::NoLongerVisible);
        let allocation_unavailable = self.delta_allocated == UNKNOWN_DELTA;
        let logical_unavailable = self.delta_logical == UNKNOWN_DELTA;
        DiffItemWire {
            path: &self.path,
            kind: self.kind,
            change: self.change,
            old_logical: (!logical_unavailable
                && !matches!(self.change, Change::Added)
                && !visibility_only)
                .then_some(self.old_logical),
            new_logical: (!logical_unavailable
                && !matches!(self.change, Change::Deleted)
                && !visibility_only)
                .then_some(self.new_logical),
            old_allocated: (!allocation_unavailable
                && !matches!(self.change, Change::Added)
                && !visibility_only)
                .then_some(self.old_allocated),
            new_allocated: (!allocation_unavailable
                && !matches!(self.change, Change::Deleted)
                && !visibility_only)
                .then_some(self.new_allocated),
            delta_allocated: (!allocation_unavailable && !visibility_only)
                .then_some(self.delta_allocated),
            delta_logical: (!logical_unavailable && !visibility_only).then_some(self.delta_logical),
            mtime: self.mtime,
            btime: self.btime,
            is_dir: self.is_dir,
            measurement: if visibility_only || (allocation_unavailable && logical_unavailable) {
                Envelope::default()
            } else if allocation_unavailable || logical_unavailable {
                comparison_measurement(Quality::LowerBound)
            } else {
                comparison_measurement(Quality::Observed)
            },
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MoveItem {
    pub from: NativePath,
    pub to: NativePath,
    #[serde(default)]
    pub allocated: u64,
    pub certainty: MoveCertainty,
}

#[derive(Serialize)]
struct MoveItemWire<'a> {
    from: &'a NativePath,
    to: &'a NativePath,
    allocated: u64,
    certainty: MoveCertainty,
    #[serde(flatten)]
    measurement: Envelope,
}

impl Serialize for MoveItem {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        MoveItemWire {
            from: &self.from,
            to: &self.to,
            allocated: self.allocated,
            certainty: self.certainty,
            measurement: Envelope::observed(
                "filesystem",
                "snapshot comparison",
                "canonical snapshot records",
            ),
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffReport {
    pub summary: DiffSummary,
    /// Changed files (non-directories), ordered by the requested sort.
    pub files: Vec<DiffItem>,
    /// Directories by inclusive allocated growth (largest increase first).
    pub growing_dirs: Vec<DiffItem>,
    /// Directories by inclusive allocated shrinkage (largest decrease first).
    pub shrinking_dirs: Vec<DiffItem>,
    /// Files by largest allocated decrease (deleted or shrunk).
    pub shrinking_files: Vec<DiffItem>,
    pub moves: Vec<MoveItem>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DiffReportWire<'a> {
    summary: &'a DiffSummary,
    files: &'a [DiffItem],
    growing_dirs: &'a [DiffItem],
    shrinking_dirs: &'a [DiffItem],
    shrinking_files: &'a [DiffItem],
    moves: &'a [MoveItem],
    #[serde(flatten)]
    measurement: Envelope,
}

impl Serialize for DiffReport {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        DiffReportWire {
            summary: &self.summary,
            files: &self.files,
            growing_dirs: &self.growing_dirs,
            shrinking_dirs: &self.shrinking_dirs,
            shrinking_files: &self.shrinking_files,
            moves: &self.moves,
            measurement: self.measurement(),
        }
        .serialize(serializer)
    }
}

impl DiffReport {
    /// Measurement envelope for the report's tracked record values.
    pub fn measurement(&self) -> Envelope {
        let known = self.summary.has_measurements()
            || !self.files.is_empty()
            || !self.growing_dirs.is_empty()
            || !self.shrinking_dirs.is_empty()
            || !self.shrinking_files.is_empty()
            || !self.moves.is_empty();
        if known {
            comparison_measurement(self.summary.quality())
        } else {
            Envelope::default()
        }
    }
}

/// Bounded top-N by key; ties broken by path for determinism.
struct TopN {
    limit: usize,
    heap: BinaryHeap<Reverse<(i128, ReversePath, ItemBox)>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReversePath(Vec<NativeName>);

impl PartialOrd for ReversePath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ReversePath {
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.cmp(&self.0)
    }
}

#[derive(Debug, Clone)]
struct ItemBox(DiffItem);
impl PartialEq for ItemBox {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Eq for ItemBox {}
impl PartialOrd for ItemBox {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ItemBox {
    fn cmp(&self, _: &Self) -> Ordering {
        Ordering::Equal
    }
}

impl TopN {
    fn new(limit: usize) -> Self {
        TopN {
            limit,
            heap: BinaryHeap::new(),
        }
    }

    fn would_accept(&self, key: i128) -> bool {
        self.limit > 0
            && (self.heap.len() < self.limit
                || self.heap.peek().is_some_and(|Reverse((k, _, _))| key > *k))
    }

    fn offer(&mut self, key: i128, path: &[NativeName], item: DiffItem) {
        if !self.would_accept(key) {
            return;
        }
        self.heap
            .push(Reverse((key, ReversePath(path.to_vec()), ItemBox(item))));
        if self.heap.len() > self.limit {
            self.heap.pop();
        }
    }

    fn into_sorted(self) -> Vec<DiffItem> {
        let mut v: Vec<_> = self.heap.into_iter().map(|Reverse(x)| x).collect();
        v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.0.cmp(&b.1.0)));
        v.into_iter().map(|(_, _, i)| i.0).collect()
    }
}

struct DirAcc {
    depth: usize,
    path: Vec<NativeName>,
    old_alloc: u64,
    new_alloc: u64,
    old_logical: u64,
    new_logical: u64,
    old_allocated_records: u64,
    old_allocated_known: u64,
    new_allocated_records: u64,
    new_allocated_known: u64,
    old_logical_records: u64,
    old_logical_known: u64,
    new_logical_records: u64,
    new_logical_known: u64,
    mtime: i64,
    btime: i64,
    change: Change,
}

fn metric_coverage(known: u64, records: u64) -> MetricCoverage {
    if known == 0 {
        MetricCoverage::Unknown
    } else if known == records {
        MetricCoverage::Complete
    } else {
        MetricCoverage::Partial
    }
}

struct MoveCandidate {
    path: Vec<NativeName>,
    allocated: Option<u64>,
    mtime: Option<MoveTimestamp>,
    btime: Option<MoveTimestamp>,
    evidence: crate::record::IdentityEvidence,
}

type MoveKey = (u16, FileId, Option<(FileIdScheme, FileIdScope)>);

fn move_key(evidence: crate::record::IdentityEvidence, file_id_comparable: bool) -> MoveKey {
    (
        evidence.volume,
        evidence.file_id,
        file_id_comparable.then_some((evidence.scheme, evidence.scope)),
    )
}

#[derive(Clone, Copy)]
struct MoveTimestamp {
    seconds: i64,
    nanos: Option<u32>,
    resolution: TimestampResolution,
}

fn move_timestamp(
    record: &Record,
    seconds: EntryFields,
    nanos: EntryFields,
    value: i64,
    subsecond: u32,
) -> Option<MoveTimestamp> {
    if !record.available.contains(seconds) {
        return None;
    }
    let nanos = record.available.contains(nanos).then_some(subsecond);
    Some(MoveTimestamp {
        seconds: value,
        nanos,
        resolution: if nanos.is_some() {
            record.timestamp_resolution
        } else {
            TimestampResolution::Seconds
        },
    })
}

fn move_timestamps_match(left: Option<MoveTimestamp>, right: Option<MoveTimestamp>) -> bool {
    let (Some(left), Some(right)) = (left, right) else {
        return true;
    };
    let resolution = left.resolution.common(right.resolution);
    if resolution == TimestampResolution::Unknown {
        return true;
    }
    let unit = resolution.nanos();
    let quantize = |timestamp: MoveTimestamp| {
        (
            timestamp.seconds,
            timestamp.nanos.unwrap_or(0) as i64 / unit,
        )
    };
    quantize(left) == quantize(right)
}

fn move_candidates_match(left: &MoveCandidate, right: &MoveCandidate) -> bool {
    let Some((left_allocated, right_allocated)) = left.allocated.zip(right.allocated) else {
        return false;
    };
    left_allocated == right_allocated
        && move_timestamps_match(left.mtime, right.mtime)
        && move_timestamps_match(left.btime, right.btime)
}

/// Builds a [`DiffReport`] from merge events.
pub struct ReportBuilder {
    opts: DiffOptions,
    report: DiffReport,
    files: TopN,
    path_order_files: Vec<DiffItem>,
    shrinking_files: TopN,
    growing_dirs: TopN,
    shrinking_dirs: TopN,
    dirs: Vec<DirAcc>,
    deleted_by_id: HashMap<MoveKey, MoveCandidate>,
    added_by_id: HashMap<MoveKey, MoveCandidate>,
    move_cap_hit: bool,
}

impl ReportBuilder {
    pub fn new(opts: DiffOptions) -> Self {
        let limit = opts.limit;
        ReportBuilder {
            files: TopN::new(if opts.list_files && opts.sort != DiffSort::Path {
                limit
            } else {
                0
            }),
            shrinking_files: TopN::new(if opts.list_files { limit } else { 0 }),
            growing_dirs: TopN::new(if opts.list_dirs { limit } else { 0 }),
            shrinking_dirs: TopN::new(if opts.list_dirs { limit } else { 0 }),
            path_order_files: Vec::new(),
            opts,
            report: DiffReport::default(),
            dirs: Vec::new(),
            deleted_by_id: HashMap::new(),
            added_by_id: HashMap::new(),
            move_cap_hit: false,
        }
    }

    fn pop_dirs_to(&mut self, depth: usize) -> io::Result<()> {
        while self.dirs.last().is_some_and(|d| d.depth >= depth) {
            let d = self.dirs.pop().unwrap();
            if let Some(parent) = self.dirs.last_mut() {
                parent.old_alloc += d.old_alloc;
                parent.new_alloc += d.new_alloc;
                parent.old_logical += d.old_logical;
                parent.new_logical += d.new_logical;
                parent.old_allocated_records += d.old_allocated_records;
                parent.old_allocated_known += d.old_allocated_known;
                parent.new_allocated_records += d.new_allocated_records;
                parent.new_allocated_known += d.new_allocated_known;
                parent.old_logical_records += d.old_logical_records;
                parent.old_logical_known += d.old_logical_known;
                parent.new_logical_records += d.new_logical_records;
                parent.new_logical_known += d.new_logical_known;
            }
            let allocated_delta = d.new_alloc as i128 - d.old_alloc as i128;
            let logical_delta = d.new_logical as i128 - d.old_logical as i128;
            let delta = if self.opts.allocated_deltas {
                allocated_delta
            } else {
                logical_delta
            };
            if delta == 0 || delta.unsigned_abs() < self.opts.min_size as u128 {
                continue;
            }
            let item = DiffItem {
                path: join_components(&d.path)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
                kind: Kind::Dir,
                change: d.change,
                old_logical: d.old_logical,
                new_logical: d.new_logical,
                old_allocated: d.old_alloc,
                new_allocated: d.new_alloc,
                delta_allocated: if matches!(
                    (
                        metric_coverage(d.old_allocated_known, d.old_allocated_records),
                        metric_coverage(d.new_allocated_known, d.new_allocated_records)
                    ),
                    (MetricCoverage::Complete, MetricCoverage::Complete)
                ) {
                    clamp(delta)
                } else {
                    UNKNOWN_DELTA
                },
                delta_logical: if matches!(
                    (
                        metric_coverage(d.old_logical_known, d.old_logical_records),
                        metric_coverage(d.new_logical_known, d.new_logical_records)
                    ),
                    (MetricCoverage::Complete, MetricCoverage::Complete)
                ) {
                    clamp(d.new_logical as i128 - d.old_logical as i128)
                } else {
                    UNKNOWN_DELTA
                },
                mtime: d.mtime,
                btime: d.btime,
                is_dir: true,
            };
            if delta > 0 {
                self.growing_dirs.offer(delta, &d.path, item);
            } else {
                self.shrinking_dirs.offer(-delta, &d.path, item);
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<DiffReport> {
        self.pop_dirs_to(0)?;
        let mut moves = Vec::new();
        if self.opts.detect_moves && !self.move_cap_hit {
            for (id, from) in &self.deleted_by_id {
                if let Some(to) = self.added_by_id.get(id)
                    && move_candidates_match(from, to)
                    && !matches!(
                        from.evidence.move_certainty_with_comparability(
                            &to.evidence,
                            self.opts.file_id_comparable
                        ),
                        MoveCertainty::None
                    )
                {
                    moves.push(MoveItem {
                        from: join_components(&from.path)
                            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
                        to: join_components(&to.path)
                            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
                        allocated: from.allocated.unwrap(),
                        certainty: from.evidence.move_certainty_with_comparability(
                            &to.evidence,
                            self.opts.file_id_comparable,
                        ),
                    });
                }
            }
            moves.sort_by(|a, b| {
                b.allocated
                    .cmp(&a.allocated)
                    .then_with(|| a.from.cmp(&b.from))
            });
            self.report.summary.moved = moves.len() as u64;
            self.report.summary.moved_bytes = moves.iter().map(|m| m.allocated).sum();
            moves.truncate(self.opts.limit);
        }
        self.report.moves = moves;
        self.report.files = if self.opts.sort == DiffSort::Path {
            self.path_order_files
        } else {
            self.files.into_sorted()
        };
        self.report.shrinking_files = self.shrinking_files.into_sorted();
        self.report.growing_dirs = self.growing_dirs.into_sorted();
        self.report.shrinking_dirs = self.shrinking_dirs.into_sorted();
        Ok(self.report)
    }
}

fn clamp(v: i128) -> i64 {
    v.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

impl DiffVisitor for ReportBuilder {
    fn entry(&mut self, e: &DiffEntry<'_>) -> io::Result<()> {
        if let Some(prefix) = &self.opts.prefix {
            compare_paths(e.path, prefix)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            if !(e.path.len() >= prefix.len() && e.path[..prefix.len()] == prefix[..]) {
                return Ok(());
            }
        }
        let depth = e.path.len() - 1;
        self.pop_dirs_to(depth)?;

        let s = &mut self.report.summary;
        let visibility_change = matches!(e.change, Change::NewlyVisible | Change::NoLongerVisible);
        if !e.coverage.is_empty() {
            s.coverage_changed += 1;
        }
        let (mut old_a_known, old_l_known, mut new_a_known, new_l_known) = match e.change {
            Change::Added | Change::NewlyVisible => (
                Some(0),
                Some(0),
                metric_value(e.new, EntryFields::ALLOCATED_SIZE),
                metric_value(e.new, EntryFields::LOGICAL_SIZE),
            ),
            Change::Deleted | Change::NoLongerVisible => (
                metric_value(e.old, EntryFields::ALLOCATED_SIZE),
                metric_value(e.old, EntryFields::LOGICAL_SIZE),
                Some(0),
                Some(0),
            ),
            Change::Unchanged | Change::Modified | Change::CoverageChanged => (
                metric_value(e.old, EntryFields::ALLOCATED_SIZE),
                metric_value(e.old, EntryFields::LOGICAL_SIZE),
                metric_value(e.new, EntryFields::ALLOCATED_SIZE),
                metric_value(e.new, EntryFields::LOGICAL_SIZE),
            ),
        };
        if !self.opts.allocated_deltas {
            old_a_known = None;
            new_a_known = None;
        }
        let old_a = old_a_known.unwrap_or(0);
        let old_l = old_l_known.unwrap_or(0);
        let new_a = new_a_known.unwrap_or(0);
        let new_l = new_l_known.unwrap_or(0);
        if let Some(o) = e.old
            && !visibility_change
        {
            s.old_total_allocated_records += 1;
            if let Some(bytes) = old_a_known {
                s.old_total_allocated += bytes;
                s.old_total_allocated_known += 1;
            }
            s.old_total_logical_records += 1;
            if let Some(bytes) = metric_value(Some(o), EntryFields::LOGICAL_SIZE) {
                s.old_total_logical += bytes;
                s.old_total_logical_known += 1;
            }
        }
        if let Some(n) = e.new
            && !visibility_change
        {
            s.new_total_allocated_records += 1;
            if let Some(bytes) = new_a_known {
                s.new_total_allocated += bytes;
                s.new_total_allocated_known += 1;
            }
            s.new_total_logical_records += 1;
            if let Some(bytes) = metric_value(Some(n), EntryFields::LOGICAL_SIZE) {
                s.new_total_logical += bytes;
                s.new_total_logical_known += 1;
            }
        }
        match e.change {
            Change::Unchanged => s.unchanged += 1,
            Change::CoverageChanged => {}
            Change::Added => s
                .added
                .add_with_allocation(e.new.unwrap(), self.opts.allocated_deltas),
            Change::Deleted => s
                .deleted
                .add_with_allocation(e.old.unwrap(), self.opts.allocated_deltas),
            Change::Modified => {
                if self.opts.allocated_deltas
                    && let (Some(new_a), Some(old_a)) = (new_a_known, old_a_known)
                {
                    match new_a.cmp(&old_a) {
                        Ordering::Greater => s.grown.add_delta(
                            new_l_known
                                .zip(old_l_known)
                                .map(|(new, old)| new.saturating_sub(old)),
                            Some(new_a - old_a),
                        ),
                        Ordering::Less => s.shrunk.add_delta(
                            old_l_known
                                .zip(new_l_known)
                                .map(|(old, new)| old.saturating_sub(new)),
                            Some(old_a - new_a),
                        ),
                        Ordering::Equal => s.modified_same_size += 1,
                    }
                }
            }
            Change::NewlyVisible => s
                .newly_visible
                .add_with_allocation(e.new.unwrap(), self.opts.allocated_deltas),
            Change::NoLongerVisible => s
                .no_longer_visible
                .add_with_allocation(e.old.unwrap(), self.opts.allocated_deltas),
        }
        let (d_a, d_l) = if visibility_change {
            (0i128, 0i128)
        } else {
            (new_a as i128 - old_a as i128, new_l as i128 - old_l as i128)
        };
        if !visibility_change {
            s.net_allocated_records += 1;
            match (old_a_known, new_a_known) {
                (Some(old), Some(new)) => {
                    s.net_allocated_known += 1;
                    s.net_allocated_bytes += new as i128 - old as i128;
                }
                _ => s.net_allocated_unknown = true,
            }
            s.net_logical_records += 1;
            match (old_l_known, new_l_known) {
                (Some(old), Some(new)) => {
                    s.net_logical_known += 1;
                    s.net_logical_bytes += new as i128 - old as i128;
                }
                _ => s.net_logical_unknown = true,
            }
        }

        // Directory aggregation: a directory opens an accumulator holding its own
        // size, which flows into its parent when the directory is closed; any other
        // entry adds directly to its parent's accumulator.
        let (own_old_a, own_new_a, own_old_l, own_new_l) = if visibility_change {
            (0, 0, 0, 0)
        } else {
            (old_a, new_a, old_l, new_l)
        };
        let (old_allocated_records, old_allocated_known) = if visibility_change {
            (0, 0)
        } else {
            (1, u64::from(old_a_known.is_some()))
        };
        let (new_allocated_records, new_allocated_known) = if visibility_change {
            (0, 0)
        } else {
            (1, u64::from(new_a_known.is_some()))
        };
        let (old_logical_records, old_logical_known) = if visibility_change {
            (0, 0)
        } else {
            (1, u64::from(old_l_known.is_some()))
        };
        let (new_logical_records, new_logical_known) = if visibility_change {
            (0, 0)
        } else {
            (1, u64::from(new_l_known.is_some()))
        };
        let rec = e.new.or(e.old).unwrap();
        let is_dir = e.new.is_some_and(|r| r.is_dir()) || e.old.is_some_and(|r| r.is_dir());
        if is_dir {
            self.dirs.push(DirAcc {
                depth,
                path: e.path.to_vec(),
                old_alloc: own_old_a,
                new_alloc: own_new_a,
                old_logical: own_old_l,
                new_logical: own_new_l,
                old_allocated_records,
                old_allocated_known,
                new_allocated_records,
                new_allocated_known,
                old_logical_records,
                old_logical_known,
                new_logical_records,
                new_logical_known,
                mtime: rec.mtime,
                btime: rec.btime,
                change: e.change,
            });
            return Ok(());
        }
        if let Some(parent) = self.dirs.last_mut() {
            parent.old_alloc += own_old_a;
            parent.new_alloc += own_new_a;
            parent.old_logical += own_old_l;
            parent.new_logical += own_new_l;
        }

        let explicit_change = matches!(
            e.change,
            Change::NewlyVisible | Change::NoLongerVisible | Change::CoverageChanged
        );
        if matches!(e.change, Change::Unchanged) {
            return Ok(());
        }
        // Move candidates.
        if self.opts.detect_moves && rec.kind == Kind::File && !rec.has(flags::HARDLINK_DUP) {
            let map = match e.change {
                Change::Added => Some(&mut self.added_by_id),
                Change::Deleted => Some(&mut self.deleted_by_id),
                _ => None,
            };
            if let (Some(map), Some(evidence)) = (map, rec.identity_evidence()) {
                if map.len() < self.opts.move_candidates_cap {
                    map.insert(
                        move_key(evidence, self.opts.file_id_comparable),
                        MoveCandidate {
                            path: e.path.to_vec(),
                            allocated: self
                                .opts
                                .allocated_deltas
                                .then_some(rec)
                                .filter(|record| {
                                    record.available.contains(EntryFields::ALLOCATED_SIZE)
                                })
                                .map(|record| record.alloc),
                            mtime: move_timestamp(
                                rec,
                                EntryFields::MTIME,
                                EntryFields::MTIME_NANOS,
                                rec.mtime,
                                rec.mtime_nanos,
                            ),
                            btime: move_timestamp(
                                rec,
                                EntryFields::BIRTH_TIME,
                                EntryFields::BIRTH_TIME_NANOS,
                                rec.btime,
                                rec.btime_nanos,
                            ),
                            evidence,
                        },
                    );
                } else {
                    self.move_cap_hit = true;
                }
            }
        }
        let ranking_delta = if self.opts.allocated_deltas { d_a } else { d_l };
        let ranking_size = if self.opts.allocated_deltas {
            new_a as i128
        } else {
            new_l as i128
        };
        if !self.opts.list_files
            || (!explicit_change && ranking_delta.unsigned_abs() < self.opts.min_size as u128)
        {
            return Ok(());
        }
        // Sorted by growth, a change that moved no bytes is noise.
        if self.opts.sort == DiffSort::Growth && ranking_delta == 0 && !explicit_change {
            return Ok(());
        }
        let key = match self.opts.sort {
            DiffSort::Growth => ranking_delta,
            DiffSort::Size => ranking_size,
            DiffSort::Modified => e.new.map(|r| r.mtime).unwrap_or(i64::MIN) as i128,
            DiffSort::Created => e.new.map(|r| r.btime).unwrap_or(i64::MIN) as i128,
            DiffSort::Path => 0,
        };
        let wants_shrink = ranking_delta < 0 && self.shrinking_files.would_accept(-ranking_delta);
        let wants_main = if self.opts.sort == DiffSort::Path {
            self.path_order_files.len() < self.opts.limit
        } else {
            self.files.would_accept(key)
        };
        if !(wants_shrink || wants_main) {
            return Ok(());
        }
        let item = DiffItem {
            path: join_components(e.path)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
            kind: rec.kind,
            change: e.change,
            old_logical: old_l,
            new_logical: new_l,
            old_allocated: old_a,
            new_allocated: new_a,
            delta_allocated: if old_a_known.is_none() || new_a_known.is_none() {
                UNKNOWN_DELTA
            } else {
                clamp(d_a)
            },
            delta_logical: if old_l_known.is_none() || new_l_known.is_none() {
                UNKNOWN_DELTA
            } else {
                clamp(d_l)
            },
            mtime: if rec.has(flags::STAT_ERROR) {
                0
            } else {
                rec.mtime
            },
            btime: if rec.has(flags::STAT_ERROR) {
                0
            } else {
                rec.btime
            },
            is_dir: false,
        };
        if wants_shrink {
            self.shrinking_files
                .offer(-ranking_delta, e.path, item.clone());
        }
        if wants_main {
            if self.opts.sort == DiffSort::Path {
                self.path_order_files.push(item);
            } else {
                self.files.offer(key, e.path, item);
            }
        }
        Ok(())
    }
}

/// Convenience: diff two streams into a report.
pub fn diff(a: RecordStream<'_>, b: RecordStream<'_>, opts: DiffOptions) -> io::Result<DiffReport> {
    let mut builder = ReportBuilder::new(opts);
    merge(a, b, &mut builder)?;
    builder.finish()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::record::{EntryFields, FileIdScheme, FileIdScope};

    /// Builds a canonical stream from `(path, kind, alloc)` triples given in any order.
    pub fn stream(entries: &[(&str, Kind, u64)]) -> Vec<Record> {
        let mut v: Vec<(Vec<NativeName>, Kind, u64)> = entries
            .iter()
            .map(|(p, k, a)| {
                let mut comps = vec![NativeName::unix(b"/r".to_vec())];
                comps.extend(
                    p.split('/')
                        .filter(|c| !c.is_empty())
                        .map(|c| NativeName::unix(c.as_bytes().to_vec())),
                );
                (comps, *k, *a)
            })
            .collect();
        v.sort_by(|a, b| compare_paths(&a.0, &b.0).unwrap());
        v.into_iter()
            .enumerate()
            .map(|(i, (comps, kind, alloc))| Record {
                depth: (comps.len() - 1) as u16,
                name: comps.last().unwrap().clone(),
                kind,
                size: alloc,
                alloc,
                mtime: 1000,
                ino: 100 + i as u64,
                nlink: 1,
                ..Default::default()
            })
            .collect()
    }

    pub fn boxed(v: Vec<Record>) -> RecordStream<'static> {
        Box::new(v.into_iter().map(Ok))
    }

    use Kind::{Dir as D, File as F};

    fn base() -> Vec<Record> {
        stream(&[
            ("", D, 0),
            ("a", D, 0),
            ("a/x", F, 100),
            ("a/y", F, 200),
            ("b", D, 0),
            ("b/old", F, 50),
            ("c", F, 10),
        ])
    }

    #[test]
    fn classifies_changes_and_aggregates_dirs() {
        let mut new = stream(&[
            ("", D, 0),
            ("a", D, 0),
            ("a/x", F, 1100),
            ("a/y", F, 200),
            ("a/z", F, 4000),
            ("c", F, 5),
        ]);
        new.iter_mut()
            .find(|r| r.name == crate::native::NativeName::unix(b"y".to_vec()))
            .unwrap()
            .ino = 102; // same as before
        let r = diff(
            boxed(base()),
            boxed(new),
            DiffOptions {
                detect_moves: false,
                ..Default::default()
            },
        )
        .unwrap();
        let s = &r.summary;
        assert_eq!(s.added.count, 1);
        assert_eq!(s.added.allocated_bytes, 4000);
        assert_eq!(s.deleted.count, 2, "b and b/old");
        assert_eq!(s.deleted.allocated_bytes, 50);
        assert_eq!(s.grown.count, 1);
        assert_eq!(s.grown.allocated_bytes, 1000);
        assert_eq!(s.shrunk.count, 1);
        assert_eq!(s.net_allocated_bytes, 1000 + 4000 - 50 - 5);
        assert_eq!(s.old_total_allocated, 360);
        assert_eq!(s.new_total_allocated, 5305);
        let paths: Vec<String> = r.growing_dirs.iter().map(|d| d.path.display()).collect();
        assert_eq!(paths, ["/r/a", "/r"], "inclusive deltas, largest first");
        assert_eq!(r.growing_dirs[0].delta_allocated, 5000);
        assert_eq!(
            r.growing_dirs[1].delta_allocated, 4945,
            "root includes b's shrinkage"
        );
        assert_eq!(r.files[0].path, NativePath::unix(b"/r/a/z".to_vec()));
        assert_eq!(r.shrinking_dirs[0].path, NativePath::unix(b"/r/b".to_vec()));
    }

    #[test]
    fn nested_directory_deltas_are_not_double_counted() {
        let old = stream(&[("", D, 0), ("p", D, 0), ("p/q", D, 0), ("p/q/f", F, 10)]);
        let new = stream(&[("", D, 0), ("p", D, 0), ("p/q", D, 0), ("p/q/f", F, 110)]);
        let r = diff(boxed(old), boxed(new), DiffOptions::default()).unwrap();
        for d in &r.growing_dirs {
            assert_eq!(d.delta_allocated, 100, "{}", d.path.display());
        }
        assert_eq!(r.growing_dirs.len(), 3);
    }

    #[test]
    fn coverage_changes_are_not_growth() {
        let mut old = stream(&[
            ("", D, 0),
            ("locked", D, 0),
            ("open", D, 0),
            ("open/f", F, 7),
        ]);
        old[1].flags = flags::INACCESSIBLE;
        let new = stream(&[
            ("", D, 0),
            ("locked", D, 0),
            ("locked/secret", F, 999),
            ("open", D, 0),
        ]);
        let mut new_locked = new.clone();
        new_locked[3].flags = flags::INACCESSIBLE;
        let r = diff(boxed(old.clone()), boxed(new), DiffOptions::default()).unwrap();
        assert_eq!(r.summary.newly_visible.count, 1);
        assert_eq!(r.summary.newly_visible.allocated_bytes, 999);
        assert_eq!(r.summary.added.count, 0);
        assert_eq!(r.summary.deleted.count, 1, "open/f really deleted");
        assert_eq!(r.summary.net_allocated_bytes, -7);
        let r = diff(boxed(old), boxed(new_locked), DiffOptions::default()).unwrap();
        assert_eq!(r.summary.no_longer_visible.count, 1);
        assert_eq!(
            r.summary.deleted.count, 0,
            "inaccessible is never counted as deleted"
        );
    }

    #[test]
    fn allocation_incomparable_comparison_omits_allocated_deltas_and_totals() {
        let old = stream(&[("", D, 10), ("file", F, 10)]);
        let new = stream(&[("", D, 20), ("file", F, 20)]);
        let report = diff(
            boxed(old),
            boxed(new),
            DiffOptions {
                allocated_deltas: false,
                detect_moves: false,
                sort: DiffSort::Path,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(report.summary.net_allocated_quality(), Quality::Unknown);
        assert_eq!(report.summary.old_total_allocated, 0);
        assert_eq!(report.summary.new_total_allocated, 0);
        assert_eq!(report.growing_dirs.len(), 1);
        assert_eq!(report.growing_dirs[0].delta_allocated, UNKNOWN_DELTA);
        assert_eq!(report.growing_dirs[0].delta_logical, 20);
        assert_eq!(report.files[0].delta_allocated, UNKNOWN_DELTA);
    }

    #[test]
    fn allocation_incomparable_comparison_keeps_logical_file_changes() {
        let old = stream(&[("", D, 10), ("file", F, 10)]);
        let mut new = old.clone();
        new[1].size = 20;
        let report = diff(
            boxed(old),
            boxed(new),
            DiffOptions {
                allocated_deltas: false,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].delta_logical, 10);
        assert_eq!(report.files[0].delta_allocated, UNKNOWN_DELTA);
    }

    #[test]
    fn visibility_and_coverage_changes_are_emitted_as_items() {
        let mut old = stream(&[
            ("", D, 0),
            ("coverage", F, 7),
            ("newly", D, 0),
            ("no-longer", D, 0),
            ("no-longer/old", F, 5),
        ]);
        let mut new = stream(&[
            ("", D, 0),
            ("coverage", F, 7),
            ("newly", D, 0),
            ("newly/new", F, 9),
            ("no-longer", D, 0),
        ]);
        let coverage = NativeName::unix(b"coverage".to_vec());
        old.iter_mut()
            .find(|r| r.name == coverage)
            .unwrap()
            .available = EntryFields::KIND
            .union(EntryFields::LOGICAL_SIZE)
            .union(EntryFields::ALLOCATED_SIZE)
            .union(EntryFields::MTIME);
        new.iter_mut()
            .find(|r| r.name == coverage)
            .unwrap()
            .available = old
            .iter()
            .find(|r| r.name == coverage)
            .unwrap()
            .available
            .union(EntryFields::MTIME_NANOS);
        old.iter_mut()
            .find(|r| r.name == NativeName::unix(b"newly".to_vec()))
            .unwrap()
            .flags = flags::INACCESSIBLE;
        new.iter_mut()
            .find(|r| r.name == NativeName::unix(b"no-longer".to_vec()))
            .unwrap()
            .flags = flags::INACCESSIBLE;

        let report = diff(
            boxed(old),
            boxed(new),
            DiffOptions {
                sort: DiffSort::Path,
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();
        let changes: Vec<_> = report
            .files
            .iter()
            .map(|item| (item.path.display(), item.change))
            .collect();
        assert_eq!(changes.len(), 3);
        assert!(changes.contains(&("/r/coverage".into(), Change::CoverageChanged)));
        assert!(changes.contains(&("/r/newly/new".into(), Change::NewlyVisible)));
        assert!(changes.contains(&("/r/no-longer/old".into(), Change::NoLongerVisible)));
    }

    #[test]
    fn detects_moves() {
        let mut old = stream(&[("", D, 0), ("a", D, 0), ("a/big.iso", F, 5000)]);
        old[2].file_id = crate::record::FileId { low: 42, high: 0 };
        old[2].file_id_scheme = crate::record::FileIdScheme::PosixInode64;
        old[2].btime = 12;
        old[2].available = crate::record::EntryFields::ALLOCATED_SIZE
            .union(crate::record::EntryFields::BIRTH_TIME)
            .union(crate::record::EntryFields::FILE_ID);
        let mut new = stream(&[("", D, 0), ("b", D, 0), ("b/big.iso", F, 5000)]);
        new[2].file_id = old[2].file_id;
        new[2].file_id_scheme = old[2].file_id_scheme;
        new[2].btime = old[2].btime;
        new[2].available = old[2].available;
        let r = diff(boxed(old), boxed(new), DiffOptions::default()).unwrap();
        assert_eq!(r.summary.moved, 1);
        assert_eq!(r.moves[0].from, NativePath::unix(b"/r/a/big.iso".to_vec()));
        assert_eq!(r.moves[0].to, NativePath::unix(b"/r/b/big.iso".to_vec()));
    }

    #[test]
    fn unavailable_allocation_is_not_zero_move_evidence() {
        let mut old = stream(&[("", D, 0), ("old", F, 7)]);
        let mut new = stream(&[("", D, 0), ("new", F, 7)]);
        let observed = EntryFields::KIND
            .union(EntryFields::LOGICAL_SIZE)
            .union(EntryFields::FILE_ID);
        for records in [&mut old, &mut new] {
            let record = &mut records[1];
            record.file_id = FileId { low: 42, high: 0 };
            record.available = observed;
        }

        let report = diff(boxed(old), boxed(new), DiffOptions::default()).unwrap();
        assert!(report.moves.is_empty());
        assert_eq!(report.summary.moved, 0);
    }

    #[test]
    fn incomparable_allocation_suppresses_allocated_measurements() {
        let mut old = stream(&[("", D, 0), ("file", F, 10)]);
        let mut new = old.clone();
        old[1].size = 1;
        new[1].size = 2;
        new[1].alloc = 20;

        let report = diff(
            boxed(old),
            boxed(new),
            DiffOptions {
                sort: DiffSort::Path,
                allocated_deltas: false,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(report.summary.net_logical_bytes, 1);
        assert_eq!(report.summary.net_logical_quality(), Quality::Observed);
        assert_eq!(report.summary.net_allocated_quality(), Quality::Unknown);
        let json = serde_json::to_value(&report).unwrap();
        assert!(json["summary"].get("netAllocatedBytes").is_none());
        assert!(json["files"][0].get("deltaAllocated").is_none());
        assert_eq!(json["files"][0]["deltaLogical"], 1);
    }

    #[test]
    fn move_detection_compares_birth_and_mtime_at_common_resolution() {
        let mut old = stream(&[("", D, 0), ("old", F, 7)]);
        let mut new = stream(&[("", D, 0), ("new", F, 7)]);
        let observed = EntryFields::KIND
            .union(EntryFields::LOGICAL_SIZE)
            .union(EntryFields::ALLOCATED_SIZE)
            .union(EntryFields::MTIME)
            .union(EntryFields::MTIME_NANOS)
            .union(EntryFields::BIRTH_TIME)
            .union(EntryFields::BIRTH_TIME_NANOS)
            .union(EntryFields::FILE_ID);
        for records in [&mut old, &mut new] {
            let record = &mut records[1];
            record.file_id = FileId { low: 42, high: 0 };
            record.available = observed;
            record.timestamp_resolution = crate::record::TimestampResolution::Nanoseconds;
            record.mtime = 10;
            record.btime = 20;
        }
        old[1].mtime_nanos = 1;
        new[1].mtime_nanos = 2;

        let report = diff(boxed(old), boxed(new), DiffOptions::default()).unwrap();
        assert!(report.moves.is_empty());
        assert_eq!(report.summary.moved, 0);
    }

    #[test]
    fn unavailable_birth_and_nanoseconds_do_not_block_common_resolution_move() {
        let mut old = stream(&[("", D, 0), ("old", F, 7)]);
        let mut new = stream(&[("", D, 0), ("new", F, 7)]);
        let old_observed = EntryFields::KIND
            .union(EntryFields::LOGICAL_SIZE)
            .union(EntryFields::ALLOCATED_SIZE)
            .union(EntryFields::MTIME)
            .union(EntryFields::BIRTH_TIME)
            .union(EntryFields::FILE_ID);
        let new_observed = old_observed
            .union(EntryFields::MTIME_NANOS)
            .union(EntryFields::BIRTH_TIME_NANOS);
        for records in [&mut old, &mut new] {
            let record = &mut records[1];
            record.file_id = FileId { low: 42, high: 0 };
            record.timestamp_resolution = crate::record::TimestampResolution::Nanoseconds;
            record.mtime = 10;
            record.btime = 20;
        }
        old[1].available = old_observed;
        new[1].available = new_observed;
        old[1].mtime_nanos = 100;
        old[1].btime_nanos = 200;
        new[1].mtime_nanos = 900_000_000;
        new[1].btime_nanos = 900_000_000;

        let report = diff(boxed(old), boxed(new), DiffOptions::default()).unwrap();
        assert_eq!(report.summary.moved, 1);
    }

    #[test]
    fn move_candidates_are_keyed_by_scheme_scope_and_volume() {
        let names = [
            "old-posix",
            "old-windows",
            "old-filesystem",
            "old-other-volume",
        ];
        let mut old = stream(&[
            ("", D, 0),
            (names[0], F, 7),
            (names[1], F, 7),
            (names[2], F, 7),
            (names[3], F, 7),
        ]);
        let mut new = stream(&[
            ("", D, 0),
            ("new-posix", F, 7),
            ("new-windows", F, 7),
            ("new-filesystem", F, 7),
            ("new-other-volume", F, 7),
        ]);
        let identity_fields = EntryFields::KIND
            .union(EntryFields::LOGICAL_SIZE)
            .union(EntryFields::ALLOCATED_SIZE)
            .union(EntryFields::BIRTH_TIME)
            .union(EntryFields::FILE_ID);
        let identities = [
            (FileIdScheme::PosixInode64, FileIdScope::Volume, 1),
            (FileIdScheme::WindowsFileId128, FileIdScope::Volume, 1),
            (FileIdScheme::PosixInode64, FileIdScope::Filesystem, 1),
            (FileIdScheme::PosixInode64, FileIdScope::Volume, 2),
        ];
        for (index, (scheme, scope, volume)) in identities.into_iter().enumerate() {
            let old_record = old
                .iter_mut()
                .find(|r| r.name == NativeName::unix(names[index].as_bytes().to_vec()))
                .unwrap();
            let new_name = format!("new-{}", names[index].strip_prefix("old-").unwrap());
            let new_record = new
                .iter_mut()
                .find(|r| r.name == NativeName::unix(new_name.as_bytes().to_vec()))
                .unwrap();
            for record in [old_record, new_record] {
                record.file_id = crate::record::FileId { low: 42, high: 0 };
                record.file_id_scheme = scheme;
                record.file_id_scope = scope;
                record.vol = volume;
                record.btime = 12;
                record.available = identity_fields;
            }
        }

        let report = diff(boxed(old), boxed(new), DiffOptions::default()).unwrap();
        assert_eq!(report.summary.moved, 4);
        assert_eq!(report.moves.len(), 4);
    }

    #[test]
    fn incomparable_file_ids_only_support_probable_moves() {
        let mut old = stream(&[("", D, 0), ("old", F, 7)]);
        let mut new = stream(&[("", D, 0), ("new", F, 7)]);
        for record in [&mut old[1], &mut new[1]] {
            record.file_id = FileId { low: 42, high: 0 };
            record.btime = 12;
            record.available = EntryFields::KIND
                .union(EntryFields::LOGICAL_SIZE)
                .union(EntryFields::ALLOCATED_SIZE)
                .union(EntryFields::BIRTH_TIME)
                .union(EntryFields::FILE_ID);
        }
        old[1].file_id_scheme = FileIdScheme::PosixInode64;
        new[1].file_id_scheme = FileIdScheme::WindowsFileId128;

        let report = diff(
            boxed(old),
            boxed(new),
            DiffOptions {
                file_id_comparable: false,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(report.summary.moved, 1);
        assert_eq!(report.moves[0].certainty, MoveCertainty::Probable);
    }

    #[test]
    fn prefix_filter_and_min_size() {
        let new = stream(&[
            ("", D, 0),
            ("a", D, 0),
            ("a/x", F, 100),
            ("a/y", F, 200),
            ("b", D, 0),
            ("b/old", F, 50),
            ("b/new", F, 9000),
            ("c", F, 10),
        ]);
        let opts = DiffOptions {
            prefix: Some(vec![
                NativeName::unix(b"/r".to_vec()),
                NativeName::unix(b"b".to_vec()),
            ]),
            min_size: 1000,
            ..Default::default()
        };
        let r = diff(boxed(base()), boxed(new), opts).unwrap();
        assert_eq!(r.summary.added.count, 1);
        assert_eq!(r.summary.old_total_allocated, 50);
        assert_eq!(r.files.len(), 1);
        assert_eq!(r.growing_dirs.len(), 1);
        assert_eq!(r.growing_dirs[0].path, NativePath::unix(b"/r/b".to_vec()));
    }

    #[test]
    fn identical_snapshots_have_no_changes() {
        let r = diff(boxed(base()), boxed(base()), DiffOptions::default()).unwrap();
        assert_eq!(r.summary.unchanged, 7);
        assert_eq!(r.summary.net_allocated_bytes, 0);
        assert!(r.files.is_empty() && r.growing_dirs.is_empty());
    }

    #[test]
    fn path_sort_and_limit() {
        let new = stream(&[
            ("", D, 0),
            ("a", D, 0),
            ("a/x", F, 1),
            ("a/y", F, 2),
            ("b", D, 0),
            ("b/n1", F, 3),
            ("b/n2", F, 4),
            ("c", F, 11),
        ]);
        let opts = DiffOptions {
            sort: DiffSort::Path,
            limit: 2,
            ..Default::default()
        };
        let r = diff(boxed(base()), boxed(new), opts).unwrap();
        let p: Vec<_> = r.files.iter().map(|f| f.path.clone()).collect();
        assert_eq!(
            p,
            [
                NativePath::unix(b"/r/a/x".to_vec()),
                NativePath::unix(b"/r/a/y".to_vec())
            ]
        );
    }

    #[test]
    fn with_root_name_renames_only_the_root() {
        let v = stream(&[
            ("", Kind::Dir, 0),
            ("a", Kind::Dir, 0),
            ("a/b", Kind::File, 5),
        ]);
        let out: Vec<Record> =
            with_root_name(boxed(v.clone()), NativePath::unix(b"/Volumes/X 1".to_vec()))
                .collect::<io::Result<_>>()
                .unwrap();
        assert_eq!(
            out[0].name,
            crate::native::NativeName::unix(b"/Volumes/X 1".to_vec())
        );
        assert_eq!(out[1..], v[1..]);
        let report = diff(
            with_root_name(boxed(v.clone()), NativePath::unix(b"/x".to_vec())),
            with_root_name(boxed(v.clone()), NativePath::unix(b"/x".to_vec())),
            DiffOptions::default(),
        )
        .unwrap();
        assert_eq!(report.summary.unchanged, 3);
    }
}
