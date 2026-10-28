//! One directory level of a snapshot (or of a comparison): its immediate
//! children with inclusive sizes, for drilling down a tree level by level.
//!
//! Streams are canonical (depth-first pre-order), so the subtree of the
//! requested directory is contiguous: the pass stops right after it and never
//! decodes the rest of the snapshot.

use crate::diff::{Change, DiffEntry, DiffVisitor, RecordStream, merge};
use crate::paths::{PathStack, join_components};
use crate::pathser::NativePath;
use crate::record::{Kind, Record, flags};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::io;

/// A child of the listed directory, sizes inclusive of everything below it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Child {
    #[serde(flatten)]
    pub path: NativePath,
    pub kind: Kind,
    pub allocated_bytes: u64,
    pub logical_bytes: u64,
    /// Entries at and below this child.
    pub entries: u64,
    /// Newest modification time at or below this child.
    pub newest_mtime: i64,
    /// Contents partly unknown (unreadable, excluded or a mount point below).
    pub incomplete: bool,
}

impl Child {
    pub fn name(&self) -> String {
        last_name(&self.path)
    }

    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }
}

fn last_name(path: &[u8]) -> String {
    let name = path.rsplit(|&b| b == b'/').find(|c| !c.is_empty()).unwrap_or(path);
    String::from_utf8_lossy(name).into_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Listing {
    /// The listed directory itself (sizes include it and everything below).
    /// `None` when the path is not in the snapshot.
    pub dir: Option<Child>,
    /// Largest first.
    pub items: Vec<Child>,
    /// Children beyond the limit, and their total allocated size.
    pub more_items: u64,
    pub more_allocated_bytes: u64,
}

/// Lists the children of `dir` (path components as in [`PathStack`]), largest first.
pub fn children(stream: RecordStream<'_>, dir: &[Vec<u8>], limit: usize) -> io::Result<Listing> {
    let mut stack = PathStack::new();
    let mut this: Option<Child> = None;
    let mut items: Vec<Child> = Vec::new();
    let level = dir.len();
    for rec in stream {
        let rec = rec?;
        let name = rec.name.as_unix_bytes().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "portable browse requires Unix names"))?;
        stack.set(rec.depth, name)?;
        if !stack.starts_with(dir) {
            if this.is_some() {
                break;
            }
            continue;
        }
        let (l, a) = (rec.counted_size(), rec.counted_alloc());
        let incomplete = rec.flags & (flags::CONTENTS_UNKNOWN | flags::STAT_ERROR) != 0;
        if stack.components().len() == level {
            this = Some(Child {
                path: NativePath(stack.path_bytes()),
                kind: rec.kind,
                allocated_bytes: a,
                logical_bytes: l,
                entries: 1,
                newest_mtime: rec.mtime,
                incomplete,
            });
            continue;
        }
        let Some(d) = this.as_mut() else { continue };
        d.allocated_bytes += a;
        d.logical_bytes += l;
        d.entries += 1;
        d.newest_mtime = d.newest_mtime.max(rec.mtime);
        d.incomplete |= incomplete;
        if stack.components().len() == level + 1 {
            items.push(Child {
                path: NativePath(stack.path_bytes()),
                kind: rec.kind,
                allocated_bytes: a,
                logical_bytes: l,
                entries: 1,
                newest_mtime: rec.mtime,
                incomplete,
            });
        } else if let Some(c) = items.last_mut() {
            c.allocated_bytes += a;
            c.logical_bytes += l;
            c.entries += 1;
            c.newest_mtime = c.newest_mtime.max(rec.mtime);
            c.incomplete |= incomplete;
        }
    }
    items.sort_by(|x, y| y.allocated_bytes.cmp(&x.allocated_bytes).then_with(|| x.path.cmp(&y.path)));
    let rest = items.split_off(limit.min(items.len()));
    Ok(Listing {
        dir: this,
        more_items: rest.len() as u64,
        more_allocated_bytes: rest.iter().map(|c| c.allocated_bytes).sum(),
        items,
    })
}

/// A child of the compared directory: inclusive sizes on both sides.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiffChild {
    #[serde(flatten)]
    pub path: NativePath,
    pub kind: Kind,
    /// How the child itself changed (added, deleted, modified, unchanged…).
    pub change: Change,
    pub old_allocated: u64,
    pub new_allocated: u64,
    pub delta_allocated: i64,
    /// Added, deleted or modified entries at and below this child.
    pub changed_entries: u64,
    /// Entries that only became (in)visible because access changed; their sizes
    /// are left out, as in the diff report.
    pub visibility_changes: u64,
    pub mtime: i64,
}

impl DiffChild {
    pub fn name(&self) -> String {
        last_name(&self.path)
    }

    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    fn new(path: &[Vec<u8>], e: &DiffEntry<'_>, rec: &Record) -> DiffChild {
        DiffChild {
            path: NativePath(join_components(path)),
            kind: rec.kind,
            change: e.change,
            old_allocated: 0,
            new_allocated: 0,
            delta_allocated: 0,
            changed_entries: 0,
            visibility_changes: 0,
            mtime: rec.mtime,
        }
    }

    fn add(&mut self, e: &DiffEntry<'_>) {
        match e.change {
            Change::NewlyVisible | Change::NoLongerVisible => {
                self.visibility_changes += 1;
                return;
            }
            Change::Unchanged => {}
            // Directories change their own metadata with every file added or
            // removed; only their contents count as changes.
            Change::Modified if e.new.is_some_and(Record::is_dir) => {}
            _ => self.changed_entries += 1,
        }
        self.old_allocated += e.old.map(Record::counted_alloc).unwrap_or(0);
        self.new_allocated += e.new.map(Record::counted_alloc).unwrap_or(0);
        self.delta_allocated = (self.new_allocated as i128 - self.old_allocated as i128).clamp(i64::MIN as i128, i64::MAX as i128) as i64;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffListing {
    /// The compared directory itself; `None` when it is in neither snapshot.
    pub dir: Option<DiffChild>,
    /// Children that changed, largest absolute growth or shrinkage first.
    pub items: Vec<DiffChild>,
    /// Children beyond the limit, and their net allocated change.
    pub more_items: u64,
    pub more_delta_allocated: i64,
    /// Children with no change below them (not listed).
    pub unchanged_items: u64,
}

struct DiffLevel<'p> {
    dir: &'p [Vec<u8>],
    this: Option<DiffChild>,
    items: Vec<DiffChild>,
}

/// Raised by the visitor to stop the merge after the directory's subtree.
#[derive(Debug)]
struct Done;
impl std::fmt::Display for Done {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("done")
    }
}
impl std::error::Error for Done {}

impl DiffVisitor for DiffLevel<'_> {
    fn entry(&mut self, e: &DiffEntry<'_>) -> io::Result<()> {
        let level = self.dir.len();
        let inside = e.path.len() >= level && e.path[..level] == self.dir[..];
        if !inside {
            return if self.this.is_some() { Err(io::Error::other(Done)) } else { Ok(()) };
        }
        let rec = e.new.or(e.old).expect("merge reports at least one side");
        if e.path.len() == level {
            let mut d = DiffChild::new(e.path, e, rec);
            d.add(e);
            self.this = Some(d);
            return Ok(());
        }
        let Some(d) = self.this.as_mut() else { return Ok(()) };
        d.add(e);
        if e.path.len() == level + 1 {
            let mut c = DiffChild::new(e.path, e, rec);
            c.add(e);
            self.items.push(c);
        } else if let Some(c) = self.items.last_mut() {
            c.add(e);
            c.mtime = c.mtime.max(rec.mtime);
        }
        Ok(())
    }
}

/// Compares the children of `dir` between an older and a newer snapshot.
pub fn diff_children(old: RecordStream<'_>, new: RecordStream<'_>, dir: &[Vec<u8>], limit: usize) -> io::Result<DiffListing> {
    let mut v = DiffLevel { dir, this: None, items: Vec::new() };
    match merge(old, new, &mut v) {
        Ok(()) => {}
        Err(e) if e.get_ref().is_some_and(|i| i.is::<Done>()) => {}
        Err(e) => return Err(e),
    }
    let (changed, unchanged): (Vec<DiffChild>, Vec<DiffChild>) =
        v.items.into_iter().partition(|c| c.changed_entries > 0 || c.visibility_changes > 0 || c.delta_allocated != 0);
    let mut items = changed;
    items.sort_by(|x, y| {
        Reverse(x.delta_allocated.unsigned_abs())
            .cmp(&Reverse(y.delta_allocated.unsigned_abs()))
            .then_with(|| Reverse(x.changed_entries).cmp(&Reverse(y.changed_entries)))
            .then_with(|| x.path.cmp(&y.path))
    });
    let rest = items.split_off(limit.min(items.len()));
    Ok(DiffListing {
        dir: v.this,
        more_items: rest.len() as u64,
        more_delta_allocated: rest.iter().map(|c| c.delta_allocated).sum(),
        unchanged_items: unchanged.len() as u64,
        items,
    })
}

/// Inclusive totals of one directory, from a snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Totals {
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
    pub entries: u64,
    /// Directories below that could not be read.
    pub unreadable_dirs: u64,
    /// Contents partly unknown (unreadable, excluded or a mount point below).
    pub incomplete: bool,
}

/// Totals of several directories (`None`: not in the snapshot) and the
/// `large_files` largest files inside any of them, in one pass.
///
/// Whether a record is inside a directory can only change at records no deeper
/// than the deepest directory asked for, so the comparison runs only there.
pub fn totals(stream: RecordStream<'_>, dirs: &[Vec<Vec<u8>>], large_files: usize) -> io::Result<(Vec<Option<Totals>>, Vec<Child>)> {
    let mut stack = PathStack::new();
    let mut out: Vec<Option<Totals>> = vec![None; dirs.len()];
    let mut active: Vec<usize> = Vec::new();
    let max_len = dirs.iter().map(Vec::len).max().unwrap_or(0);
    let mut large: Vec<Child> = Vec::new();
    for rec in stream {
        let rec = rec?;
        let name = rec.name.as_unix_bytes().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "portable browse requires Unix names"))?;
        stack.set(rec.depth, name)?;
        if stack.components().len() <= max_len {
            active.clear();
            active.extend((0..dirs.len()).filter(|&i| stack.starts_with(&dirs[i])));
        }
        if active.is_empty() {
            continue;
        }
        let incomplete = rec.flags & (flags::CONTENTS_UNKNOWN | flags::STAT_ERROR) != 0;
        for &i in &active {
            let t = out[i].get_or_insert_with(Totals::default);
            t.logical_bytes += rec.counted_size();
            t.allocated_bytes += rec.counted_alloc();
            t.entries += 1;
            t.unreadable_dirs += u64::from(rec.is_dir() && rec.has(flags::INACCESSIBLE));
            t.incomplete |= incomplete;
        }
        let wanted = large.len() < large_files || large.last().is_some_and(|c| rec.counted_alloc() > c.allocated_bytes);
        if large_files > 0 && rec.kind == Kind::File && wanted {
            large.push(Child {
                path: NativePath(stack.path_bytes()),
                kind: rec.kind,
                allocated_bytes: rec.counted_alloc(),
                logical_bytes: rec.counted_size(),
                entries: 1,
                newest_mtime: rec.mtime,
                incomplete: rec.has(flags::STAT_ERROR),
            });
            large.sort_by(|x, y| y.allocated_bytes.cmp(&x.allocated_bytes).then_with(|| x.path.cmp(&y.path)));
            large.truncate(large_files);
        }
    }
    Ok((out, large))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{boxed, records};
    use Kind::{Dir, File};

    fn comps(p: &str) -> Vec<Vec<u8>> {
        let mut v = vec![b"/r".to_vec()];
        v.extend(p.split('/').filter(|c| !c.is_empty()).map(|c| c.as_bytes().to_vec()));
        v
    }

    fn tree() -> Vec<Record> {
        records(&[
            ("", Dir, 0),
            ("a", Dir, 1),
            ("a/x", File, 10),
            ("a/y", Dir, 1),
            ("a/y/z", File, 100),
            ("b", File, 50),
            ("c", Dir, 1),
            ("c/big", File, 1000),
        ])
    }

    #[test]
    fn lists_children_with_inclusive_sizes() {
        let l = children(boxed(tree()), &comps(""), 10).unwrap();
        let names: Vec<_> = l.items.iter().map(|c| (c.name(), c.allocated_bytes, c.entries)).collect();
        assert_eq!(names, [("c".to_string(), 1001, 2), ("a".into(), 112, 4), ("b".into(), 50, 1)]);
        assert_eq!(l.dir.unwrap().allocated_bytes, 1163);

        let l = children(boxed(tree()), &comps("a"), 1).unwrap();
        assert_eq!(l.items.len(), 1);
        assert_eq!(l.items[0].name(), "y");
        assert_eq!((l.more_items, l.more_allocated_bytes), (1, 10));
        assert_eq!(l.dir.as_ref().unwrap().path.lossy(), "/r/a");
    }

    #[test]
    fn missing_path_and_files() {
        assert!(children(boxed(tree()), &comps("nope"), 10).unwrap().dir.is_none());
        let l = children(boxed(tree()), &comps("b"), 10).unwrap();
        assert!(l.items.is_empty());
        assert_eq!(l.dir.unwrap().kind, File);
    }

    #[test]
    fn stops_after_the_subtree() {
        // Records after "b" (the first one past the subtree of "a") fail if decoded.
        let stream: RecordStream<'static> =
            Box::new(tree().into_iter().enumerate().map(|(i, r)| if i > 5 { Err(io::Error::other("read past the subtree")) } else { Ok(r) }));
        let l = children(stream, &comps("a"), 10).unwrap();
        assert_eq!(l.items.len(), 2);
    }

    #[test]
    fn totals_of_several_dirs_in_one_pass() {
        let (t, large) = totals(boxed(tree()), &[comps("a"), comps("a/y"), comps("nope"), comps("c")], 2).unwrap();
        assert_eq!(t[0].as_ref().map(|t| (t.allocated_bytes, t.entries)), Some((112, 4)));
        assert_eq!(t[1].as_ref().map(|t| t.allocated_bytes), Some(101), "nested directories are counted on their own too");
        assert!(t[2].is_none());
        assert_eq!(t[3].as_ref().map(|t| t.allocated_bytes), Some(1001));
        let names: Vec<_> = large.iter().map(|c| c.path.lossy()).collect();
        assert_eq!(names, ["/r/c/big", "/r/a/y/z"], "b is not inside any of them");
    }

    #[test]
    fn diff_lists_changed_children() {
        let old = tree();
        let new = records(&[
            ("", Dir, 0),
            ("a", Dir, 1),
            ("a/x", File, 10),
            ("a/y", Dir, 1),
            ("a/y/z", File, 400),
            ("b", File, 50),
            ("d", Dir, 1),
            ("d/new", File, 70),
        ]);
        let l = diff_children(boxed(old.clone()), boxed(new.clone()), &comps(""), 10).unwrap();
        let got: Vec<_> = l.items.iter().map(|c| (c.name(), c.change, c.delta_allocated, c.changed_entries)).collect();
        assert_eq!(
            got,
            [("c".to_string(), Change::Deleted, -1001, 2), ("a".into(), Change::Unchanged, 300, 1), ("d".into(), Change::Added, 71, 2)]
        );
        assert_eq!(l.unchanged_items, 1, "b did not change");
        assert_eq!(l.dir.unwrap().delta_allocated, 300 - 1001 + 71);

        let l = diff_children(boxed(old), boxed(new), &comps("a/y"), 10).unwrap();
        assert_eq!(l.items.len(), 1);
        assert_eq!((l.items[0].old_allocated, l.items[0].new_allocated), (100, 400));
    }
}
