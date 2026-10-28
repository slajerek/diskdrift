//! Snapshot comparison by merge-joining two canonical record streams.
//!
//! Both streams are depth-first pre-order with children sorted by name, so a
//! single forward pass over each matches entries by path in O(n + m) time with
//! memory bounded by tree depth plus the requested result limits.

use crate::paths::{PathStack, compare_paths, join_components};
use crate::pathser::{NativePath, serialize_entry};
use crate::record::{Kind, Record, flags};
use serde::{Deserialize, Serialize};
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::io;

pub type RecordStream<'a> = Box<dyn Iterator<Item = io::Result<Record>> + 'a>;

/// `stream` with its root record (depth 0, whose name is the full root path)
/// renamed to `root`. Two scans of one disk mounted at different places then
/// compare as the same tree (ADR 0002, section 2.6).
pub fn with_root_name<'a>(stream: RecordStream<'a>, root: Vec<u8>) -> RecordStream<'a> {
    Box::new(stream.map(move |r| {
        r.map(|mut rec| {
            if rec.depth == 0 {
                rec.name = crate::native::NativeName::unix(root.clone());
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
}

/// One matched position of the merge.
pub struct DiffEntry<'a> {
    /// Path components (`[root, name, ...]`).
    pub path: &'a [Vec<u8>],
    pub change: Change,
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
        let mut s = Side { iter, cur: None, stack: PathStack::new() };
        s.advance()?;
        Ok(s)
    }

    fn advance(&mut self) -> io::Result<()> {
        self.cur = self.iter.next().transpose()?;
        if let Some(r) = &self.cur {
            let name = r.name.as_unix_bytes().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "portable diff requires Unix names"))?;
            self.stack.set(r.depth, name)?;
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
pub fn merge(a: RecordStream<'_>, b: RecordStream<'_>, visitor: &mut dyn DiffVisitor) -> io::Result<()> {
    let mut a = Side::new(a)?;
    let mut b = Side::new(b)?;
    let mut states: Vec<NodeState> = Vec::new();
    loop {
        let order = match (&a.cur, &b.cur) {
            (None, None) => return Ok(()),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), Some(_)) => compare_paths(a.stack.components(), b.stack.components()),
        };
        match order {
            Ordering::Less => {
                let rec = a.cur.as_ref().unwrap();
                let depth = rec.depth as usize;
                states.truncate(depth);
                let change = match depth.checked_sub(1).and_then(|p| states.get(p)) {
                    Some(p) if matches!(p.change, Change::Deleted | Change::NoLongerVisible) => p.change,
                    Some(p) if p.b_flags.is_some_and(|f| f & flags::CONTENTS_UNKNOWN != 0) => Change::NoLongerVisible,
                    _ => Change::Deleted,
                };
                states.push(NodeState { a_flags: Some(rec.flags), b_flags: None, change });
                visitor.entry(&DiffEntry { path: a.stack.components(), change, old: Some(rec), new: None })?;
                a.advance()?;
            }
            Ordering::Greater => {
                let rec = b.cur.as_ref().unwrap();
                let depth = rec.depth as usize;
                states.truncate(depth);
                let change = match depth.checked_sub(1).and_then(|p| states.get(p)) {
                    Some(p) if matches!(p.change, Change::Added | Change::NewlyVisible) => p.change,
                    Some(p) if p.a_flags.is_some_and(|f| f & flags::CONTENTS_UNKNOWN != 0) => Change::NewlyVisible,
                    _ => Change::Added,
                };
                states.push(NodeState { a_flags: None, b_flags: Some(rec.flags), change });
                visitor.entry(&DiffEntry { path: b.stack.components(), change, old: None, new: Some(rec) })?;
                b.advance()?;
            }
            Ordering::Equal => {
                let (ra, rb) = (a.cur.as_ref().unwrap(), b.cur.as_ref().unwrap());
                let depth = rb.depth as usize;
                states.truncate(depth);
                let change = if ra.content_differs(rb) { Change::Modified } else { Change::Unchanged };
                states.push(NodeState { a_flags: Some(ra.flags), b_flags: Some(rb.flags), change });
                visitor.entry(&DiffEntry { path: b.stack.components(), change, old: Some(ra), new: Some(rb) })?;
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
    pub prefix: Option<Vec<Vec<u8>>>,
    /// Only list entries whose |allocated delta| is at least this.
    pub min_size: u64,
    pub limit: usize,
    pub sort: DiffSort,
    pub list_files: bool,
    pub list_dirs: bool,
    /// Pair deleted and added files with the same inode and size as moves.
    pub detect_moves: bool,
    /// Cap on files remembered for move detection.
    pub move_candidates_cap: usize,
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
            move_candidates_cap: 500_000,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Tally {
    pub count: u64,
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
}

impl Tally {
    fn add(&mut self, r: &Record) {
        self.count += 1;
        self.logical_bytes += r.counted_size();
        self.allocated_bytes += r.counted_alloc();
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffSummary {
    pub added: Tally,
    pub deleted: Tally,
    /// Modified entries whose allocation grew / shrank / stayed equal.
    pub grown: Tally,
    pub shrunk: Tally,
    pub modified_same_size: u64,
    pub unchanged: u64,
    pub newly_visible: Tally,
    pub no_longer_visible: Tally,
    /// Net change of tracked sizes, excluding coverage changes.
    pub net_logical_bytes: i128,
    pub net_allocated_bytes: i128,
    pub moved: u64,
    pub moved_bytes: u64,
    /// Total tracked sizes in each snapshot (within the prefix).
    pub old_total_allocated: u64,
    pub new_total_allocated: u64,
    pub old_total_logical: u64,
    pub new_total_logical: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffItem {
    #[serde(flatten)]
    pub path: NativePath,
    pub kind: Kind,
    pub change: Change,
    pub old_logical: u64,
    pub new_logical: u64,
    pub old_allocated: u64,
    pub new_allocated: u64,
    pub delta_allocated: i64,
    pub delta_logical: i64,
    pub mtime: i64,
    pub btime: i64,
    /// For directories: inclusive totals of the subtree.
    pub is_dir: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MoveItem {
    pub from: Vec<u8>,
    pub to: Vec<u8>,
    pub allocated: u64,
}

impl Serialize for MoveItem {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(None)?;
        serialize_entry(&mut m, "from", &self.from)?;
        serialize_entry(&mut m, "to", &self.to)?;
        m.serialize_entry("allocated", &self.allocated)?;
        m.end()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

/// Bounded top-N by key; ties broken by path for determinism.
struct TopN {
    limit: usize,
    heap: BinaryHeap<Reverse<(i128, Vec<u8>, ItemBox)>>,
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
        TopN { limit, heap: BinaryHeap::new() }
    }

    fn would_accept(&self, key: i128) -> bool {
        self.limit > 0 && (self.heap.len() < self.limit || self.heap.peek().is_some_and(|Reverse((k, _, _))| key > *k))
    }

    fn offer(&mut self, key: i128, item: DiffItem) {
        if !self.would_accept(key) {
            return;
        }
        let path = item.path.0.clone();
        self.heap.push(Reverse((key, invert(&path), ItemBox(item))));
        if self.heap.len() > self.limit {
            self.heap.pop();
        }
    }

    fn into_sorted(self) -> Vec<DiffItem> {
        let mut v: Vec<_> = self.heap.into_iter().map(|Reverse(x)| x).collect();
        v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        v.into_iter().map(|(_, _, i)| i.0).collect()
    }
}

/// Byte-inverted path so that on equal keys the lexicographically smaller path wins.
fn invert(p: &[u8]) -> Vec<u8> {
    p.iter().map(|b| !b).collect()
}

struct DirAcc {
    depth: usize,
    path: Vec<u8>,
    old_alloc: u64,
    new_alloc: u64,
    old_logical: u64,
    new_logical: u64,
    mtime: i64,
    btime: i64,
    change: Change,
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
    deleted_by_ino: HashMap<u64, (Vec<u8>, u64, i64)>,
    added_by_ino: HashMap<u64, (Vec<u8>, u64, i64)>,
    move_cap_hit: bool,
}

impl ReportBuilder {
    pub fn new(opts: DiffOptions) -> Self {
        let limit = opts.limit;
        ReportBuilder {
            files: TopN::new(if opts.list_files && opts.sort != DiffSort::Path { limit } else { 0 }),
            shrinking_files: TopN::new(if opts.list_files { limit } else { 0 }),
            growing_dirs: TopN::new(if opts.list_dirs { limit } else { 0 }),
            shrinking_dirs: TopN::new(if opts.list_dirs { limit } else { 0 }),
            path_order_files: Vec::new(),
            opts,
            report: DiffReport::default(),
            dirs: Vec::new(),
            deleted_by_ino: HashMap::new(),
            added_by_ino: HashMap::new(),
            move_cap_hit: false,
        }
    }

    fn pop_dirs_to(&mut self, depth: usize) {
        while self.dirs.last().is_some_and(|d| d.depth >= depth) {
            let d = self.dirs.pop().unwrap();
            if let Some(parent) = self.dirs.last_mut() {
                parent.old_alloc += d.old_alloc;
                parent.new_alloc += d.new_alloc;
                parent.old_logical += d.old_logical;
                parent.new_logical += d.new_logical;
            }
            let delta = d.new_alloc as i128 - d.old_alloc as i128;
            if delta == 0 || delta.unsigned_abs() < self.opts.min_size as u128 {
                continue;
            }
            let item = DiffItem {
                path: NativePath(d.path),
                kind: Kind::Dir,
                change: d.change,
                old_logical: d.old_logical,
                new_logical: d.new_logical,
                old_allocated: d.old_alloc,
                new_allocated: d.new_alloc,
                delta_allocated: clamp(delta),
                delta_logical: clamp(d.new_logical as i128 - d.old_logical as i128),
                mtime: d.mtime,
                btime: d.btime,
                is_dir: true,
            };
            if delta > 0 {
                self.growing_dirs.offer(delta, item);
            } else {
                self.shrinking_dirs.offer(-delta, item);
            }
        }
    }

    pub fn finish(mut self) -> DiffReport {
        self.pop_dirs_to(0);
        let mut moves = Vec::new();
        if self.opts.detect_moves && !self.move_cap_hit {
            for (ino, (from, size, btime)) in &self.deleted_by_ino {
                if let Some((to, size2, btime2)) = self.added_by_ino.get(ino)
                    && size == size2 && btime == btime2 {
                        moves.push(MoveItem { from: from.clone(), to: to.clone(), allocated: *size });
                    }
            }
            moves.sort_by(|a, b| b.allocated.cmp(&a.allocated).then_with(|| a.from.cmp(&b.from)));
            self.report.summary.moved = moves.len() as u64;
            self.report.summary.moved_bytes = moves.iter().map(|m| m.allocated).sum();
            moves.truncate(self.opts.limit);
        }
        self.report.moves = moves;
        self.report.files = if self.opts.sort == DiffSort::Path { self.path_order_files } else { self.files.into_sorted() };
        self.report.shrinking_files = self.shrinking_files.into_sorted();
        self.report.growing_dirs = self.growing_dirs.into_sorted();
        self.report.shrinking_dirs = self.shrinking_dirs.into_sorted();
        self.report
    }
}

fn clamp(v: i128) -> i64 {
    v.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

impl DiffVisitor for ReportBuilder {
    fn entry(&mut self, e: &DiffEntry<'_>) -> io::Result<()> {
        if let Some(prefix) = &self.opts.prefix
            && !(e.path.len() >= prefix.len() && e.path[..prefix.len()] == prefix[..]) {
                return Ok(());
            }
        let depth = e.path.len() - 1;
        self.pop_dirs_to(depth);

        let s = &mut self.report.summary;
        let visibility_change = matches!(e.change, Change::NewlyVisible | Change::NoLongerVisible);
        let (old_a, old_l) = e.old.map(|r| (r.counted_alloc(), r.counted_size())).unwrap_or((0, 0));
        let (new_a, new_l) = e.new.map(|r| (r.counted_alloc(), r.counted_size())).unwrap_or((0, 0));
        if let Some(o) = e.old
            && !visibility_change {
                s.old_total_allocated += o.counted_alloc();
                s.old_total_logical += o.counted_size();
            }
        if let Some(n) = e.new
            && !visibility_change {
                s.new_total_allocated += n.counted_alloc();
                s.new_total_logical += n.counted_size();
            }
        match e.change {
            Change::Unchanged => s.unchanged += 1,
            Change::Added => s.added.add(e.new.unwrap()),
            Change::Deleted => s.deleted.add(e.old.unwrap()),
            Change::Modified => match new_a.cmp(&old_a) {
                Ordering::Greater => {
                    s.grown.count += 1;
                    s.grown.allocated_bytes += new_a - old_a;
                    s.grown.logical_bytes += new_l.saturating_sub(old_l);
                }
                Ordering::Less => {
                    s.shrunk.count += 1;
                    s.shrunk.allocated_bytes += old_a - new_a;
                    s.shrunk.logical_bytes += old_l.saturating_sub(new_l);
                }
                Ordering::Equal => s.modified_same_size += 1,
            },
            Change::NewlyVisible => s.newly_visible.add(e.new.unwrap()),
            Change::NoLongerVisible => s.no_longer_visible.add(e.old.unwrap()),
        }
        let (d_a, d_l) = if visibility_change {
            (0i128, 0i128)
        } else {
            (new_a as i128 - old_a as i128, new_l as i128 - old_l as i128)
        };
        s.net_allocated_bytes += d_a;
        s.net_logical_bytes += d_l;

        // Directory aggregation: a directory opens an accumulator holding its own
        // size, which flows into its parent when the directory is closed; any other
        // entry adds directly to its parent's accumulator.
        let (own_old_a, own_new_a, own_old_l, own_new_l) =
            if visibility_change { (0, 0, 0, 0) } else { (old_a, new_a, old_l, new_l) };
        let rec = e.new.or(e.old).unwrap();
        let is_dir = e.new.is_some_and(|r| r.is_dir()) || e.old.is_some_and(|r| r.is_dir());
        if is_dir {
            self.dirs.push(DirAcc {
                depth,
                path: join_components(e.path),
                old_alloc: own_old_a,
                new_alloc: own_new_a,
                old_logical: own_old_l,
                new_logical: own_new_l,
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

        if matches!(e.change, Change::Unchanged) || visibility_change {
            return Ok(());
        }
        // Move candidates.
        if self.opts.detect_moves && rec.kind == Kind::File && !rec.has(flags::HARDLINK_DUP) {
            let map = match e.change {
                Change::Added => Some(&mut self.added_by_ino),
                Change::Deleted => Some(&mut self.deleted_by_ino),
                _ => None,
            };
            if let Some(map) = map {
                if map.len() < self.opts.move_candidates_cap {
                    map.insert(rec.ino, (join_components(e.path), rec.alloc, rec.btime));
                } else {
                    self.move_cap_hit = true;
                }
            }
        }
        if !self.opts.list_files || d_a.unsigned_abs() < self.opts.min_size as u128 {
            return Ok(());
        }
        // Sorted by growth, a change that moved no bytes is noise.
        if self.opts.sort == DiffSort::Growth && d_a == 0 {
            return Ok(());
        }
        let key = match self.opts.sort {
            DiffSort::Growth => d_a,
            DiffSort::Size => new_a as i128,
            DiffSort::Modified => e.new.map(|r| r.mtime).unwrap_or(i64::MIN) as i128,
            DiffSort::Created => e.new.map(|r| r.btime).unwrap_or(i64::MIN) as i128,
            DiffSort::Path => 0,
        };
        let wants_shrink = d_a < 0 && self.shrinking_files.would_accept(-d_a);
        let wants_main = if self.opts.sort == DiffSort::Path {
            self.path_order_files.len() < self.opts.limit
        } else {
            self.files.would_accept(key)
        };
        if !(wants_shrink || wants_main) {
            return Ok(());
        }
        let item = DiffItem {
            path: NativePath(join_components(e.path)),
            kind: rec.kind,
            change: e.change,
            old_logical: old_l,
            new_logical: new_l,
            old_allocated: old_a,
            new_allocated: new_a,
            delta_allocated: clamp(d_a),
            delta_logical: clamp(d_l),
            mtime: rec.mtime,
            btime: rec.btime,
            is_dir: false,
        };
        if wants_shrink {
            self.shrinking_files.offer(-d_a, item.clone());
        }
        if wants_main {
            if self.opts.sort == DiffSort::Path {
                self.path_order_files.push(item);
            } else {
                self.files.offer(key, item);
            }
        }
        Ok(())
    }
}

/// Convenience: diff two streams into a report.
pub fn diff(a: RecordStream<'_>, b: RecordStream<'_>, opts: DiffOptions) -> io::Result<DiffReport> {
    let mut builder = ReportBuilder::new(opts);
    merge(a, b, &mut builder)?;
    Ok(builder.finish())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a canonical stream from `(path, kind, alloc)` triples given in any order.
    pub fn stream(entries: &[(&str, Kind, u64)]) -> Vec<Record> {
        let mut v: Vec<(Vec<Vec<u8>>, Kind, u64)> = entries
            .iter()
            .map(|(p, k, a)| {
                let mut comps: Vec<Vec<u8>> = vec![b"/r".to_vec()];
                comps.extend(p.split('/').filter(|c| !c.is_empty()).map(|c| c.as_bytes().to_vec()));
                (comps, *k, *a)
            })
            .collect();
        v.sort_by(|a, b| compare_paths(&a.0, &b.0));
        v.into_iter()
            .enumerate()
            .map(|(i, (comps, kind, alloc))| Record {
                depth: (comps.len() - 1) as u16,
                name: crate::native::NativeName::unix(comps.last().unwrap().clone()),
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
        new.iter_mut().find(|r| r.name == crate::native::NativeName::unix(b"y".to_vec())).unwrap().ino = 102; // same as before
        let r = diff(boxed(base()), boxed(new), DiffOptions { detect_moves: false, ..Default::default() }).unwrap();
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
        let paths: Vec<String> = r.growing_dirs.iter().map(|d| d.path.lossy()).collect();
        assert_eq!(paths, ["/r/a", "/r"], "inclusive deltas, largest first");
        assert_eq!(r.growing_dirs[0].delta_allocated, 5000);
        assert_eq!(r.growing_dirs[1].delta_allocated, 4945, "root includes b's shrinkage");
        assert_eq!(r.files[0].path.0, b"/r/a/z");
        assert_eq!(r.shrinking_dirs[0].path.0, b"/r/b");
    }

    #[test]
    fn nested_directory_deltas_are_not_double_counted() {
        let old = stream(&[("", D, 0), ("p", D, 0), ("p/q", D, 0), ("p/q/f", F, 10)]);
        let new = stream(&[("", D, 0), ("p", D, 0), ("p/q", D, 0), ("p/q/f", F, 110)]);
        let r = diff(boxed(old), boxed(new), DiffOptions::default()).unwrap();
        for d in &r.growing_dirs {
            assert_eq!(d.delta_allocated, 100, "{}", d.path.lossy());
        }
        assert_eq!(r.growing_dirs.len(), 3);
    }

    #[test]
    fn coverage_changes_are_not_growth() {
        let mut old = stream(&[("", D, 0), ("locked", D, 0), ("open", D, 0), ("open/f", F, 7)]);
        old[1].flags = flags::INACCESSIBLE;
        let new = stream(&[("", D, 0), ("locked", D, 0), ("locked/secret", F, 999), ("open", D, 0)]);
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
        assert_eq!(r.summary.deleted.count, 0, "inaccessible is never counted as deleted");
    }

    #[test]
    fn detects_moves() {
        let old = stream(&[("", D, 0), ("a", D, 0), ("a/big.iso", F, 5000)]);
        let mut new = stream(&[("", D, 0), ("b", D, 0), ("b/big.iso", F, 5000)]);
        new[2].ino = old[2].ino;
        let r = diff(boxed(old), boxed(new), DiffOptions::default()).unwrap();
        assert_eq!(r.summary.moved, 1);
        assert_eq!(r.moves[0].from, b"/r/a/big.iso");
        assert_eq!(r.moves[0].to, b"/r/b/big.iso");
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
            prefix: Some(vec![b"/r".to_vec(), b"b".to_vec()]),
            min_size: 1000,
            ..Default::default()
        };
        let r = diff(boxed(base()), boxed(new), opts).unwrap();
        assert_eq!(r.summary.added.count, 1);
        assert_eq!(r.summary.old_total_allocated, 50);
        assert_eq!(r.files.len(), 1);
        assert_eq!(r.growing_dirs.len(), 1);
        assert_eq!(r.growing_dirs[0].path.0, b"/r/b");
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
        let new = stream(&[("", D, 0), ("a", D, 0), ("a/x", F, 1), ("a/y", F, 2), ("b", D, 0), ("b/n1", F, 3), ("b/n2", F, 4), ("c", F, 11)]);
        let opts = DiffOptions { sort: DiffSort::Path, limit: 2, ..Default::default() };
        let r = diff(boxed(base()), boxed(new), opts).unwrap();
        let p: Vec<&[u8]> = r.files.iter().map(|f| f.path.0.as_slice()).collect();
        assert_eq!(p, [b"/r/a/x".as_slice(), b"/r/a/y".as_slice()]);
    }

    #[test]
    fn with_root_name_renames_only_the_root() {
        let v = stream(&[("", Kind::Dir, 0), ("a", Kind::Dir, 0), ("a/b", Kind::File, 5)]);
        let out: Vec<Record> = with_root_name(boxed(v.clone()), b"/Volumes/X 1".to_vec()).collect::<io::Result<_>>().unwrap();
        assert_eq!(out[0].name, crate::native::NativeName::unix(b"/Volumes/X 1".to_vec()));
        assert_eq!(out[1..], v[1..]);
        let report = diff(with_root_name(boxed(v.clone()), b"/x".to_vec()), with_root_name(boxed(v.clone()), b"/x".to_vec()), DiffOptions::default()).unwrap();
        assert_eq!(report.summary.unchanged, 3);
    }
}
