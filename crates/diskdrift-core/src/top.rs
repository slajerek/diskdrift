//! Largest files and directories of one snapshot, computed in a single stream pass.

use crate::diff::RecordStream;
use crate::paths::{PathStack, join_components};
use crate::pathser::NativePath;
use crate::record::{Kind, flags};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SizeMetric {
    #[default]
    Allocated,
    Logical,
}

#[derive(Debug, Clone)]
pub struct TopOptions {
    pub limit: usize,
    pub metric: SizeMetric,
    pub prefix: Option<Vec<Vec<u8>>>,
    pub files: bool,
    pub dirs: bool,
    /// List only the deepest large directories: a directory is left out when a
    /// directory below it is listed (its size mostly repeats that child's).
    pub deepest: bool,
}

impl Default for TopOptions {
    fn default() -> Self {
        TopOptions { limit: 30, metric: SizeMetric::Allocated, prefix: None, files: true, dirs: true, deepest: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TopItem {
    #[serde(flatten)]
    pub path: NativePath,
    pub kind: Kind,
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
    /// Directories: number of entries below (inclusive of itself).
    pub entries: u64,
    pub mtime: i64,
    /// Directory contents are partly unknown (inaccessible/excluded/mount below).
    pub incomplete: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopReport {
    pub files: Vec<TopItem>,
    pub dirs: Vec<TopItem>,
    pub total_logical: u64,
    pub total_allocated: u64,
    pub entries: u64,
}

struct Acc {
    depth: u16,
    path: Vec<u8>,
    logical: u64,
    alloc: u64,
    entries: u64,
    mtime: i64,
    incomplete: bool,
}

#[allow(clippy::type_complexity)]
struct Heap {
    limit: usize,
    heap: BinaryHeap<Reverse<(u64, Reverse<Vec<u8>>, TopItemOrd)>>,
}

#[derive(PartialEq, Eq)]
struct TopItemOrd(TopItem);
impl PartialOrd for TopItemOrd {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for TopItemOrd {
    fn cmp(&self, _: &Self) -> std::cmp::Ordering {
        std::cmp::Ordering::Equal
    }
}

impl Heap {
    fn accepts(&self, key: u64) -> bool {
        self.limit > 0 && (self.heap.len() < self.limit || self.heap.peek().is_some_and(|Reverse((k, _, _))| key > *k))
    }

    fn offer(&mut self, key: u64, item: TopItem) {
        if !self.accepts(key) {
            return;
        }
        self.heap.push(Reverse((key, Reverse(item.path.0.clone()), TopItemOrd(item))));
        if self.heap.len() > self.limit {
            self.heap.pop();
        }
    }

    fn sorted(self) -> Vec<TopItem> {
        let mut v: Vec<_> = self.heap.into_iter().map(|Reverse(x)| x).collect();
        v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        v.into_iter().map(|(_, _, i)| i.0).collect()
    }
}

pub fn top(stream: RecordStream<'_>, opts: &TopOptions) -> io::Result<TopReport> {
    let mut stack = PathStack::new();
    let mut files = Heap { limit: if opts.files { opts.limit } else { 0 }, heap: BinaryHeap::new() };
    // Dropping ancestors thins the list, so collect more candidates first.
    let dir_candidates = if opts.deepest { opts.limit.saturating_mul(4).saturating_add(64) } else { opts.limit };
    let mut dirs = Heap { limit: if opts.dirs { dir_candidates } else { 0 }, heap: BinaryHeap::new() };
    let mut accs: Vec<Acc> = Vec::new();
    let mut report = TopReport::default();
    let key = |logical: u64, alloc: u64| match opts.metric {
        SizeMetric::Allocated => alloc,
        SizeMetric::Logical => logical,
    };

    let close = |accs: &mut Vec<Acc>, depth: u16, dirs: &mut Heap| {
        while accs.last().is_some_and(|a| a.depth >= depth) {
            let a = accs.pop().unwrap();
            if let Some(p) = accs.last_mut() {
                p.logical += a.logical;
                p.alloc += a.alloc;
                p.entries += a.entries;
                p.incomplete |= a.incomplete;
            }
            let k = key(a.logical, a.alloc);
            if dirs.accepts(k) {
                dirs.offer(
                    k,
                    TopItem {
                        path: NativePath(a.path),
                        kind: Kind::Dir,
                        logical_bytes: a.logical,
                        allocated_bytes: a.alloc,
                        entries: a.entries,
                        mtime: a.mtime,
                        incomplete: a.incomplete,
                    },
                );
            }
        }
    };

    for rec in stream {
        let rec = rec?;
        stack.set(rec.depth, &rec.name)?;
        if let Some(p) = &opts.prefix
            && !stack.starts_with(p) {
                continue;
            }
        close(&mut accs, rec.depth, &mut dirs);
        let (l, a) = (rec.counted_size(), rec.counted_alloc());
        report.entries += 1;
        report.total_logical += l;
        report.total_allocated += a;
        if rec.is_dir() {
            accs.push(Acc {
                depth: rec.depth,
                path: if dirs.limit > 0 { stack.path_bytes() } else { Vec::new() },
                logical: l,
                alloc: a,
                entries: 1,
                mtime: rec.mtime,
                incomplete: rec.flags & (flags::CONTENTS_UNKNOWN | flags::STAT_ERROR) != 0,
            });
            continue;
        }
        if let Some(p) = accs.last_mut() {
            p.logical += l;
            p.alloc += a;
            p.entries += 1;
            p.incomplete |= rec.has(flags::STAT_ERROR);
        }
        let k = key(l, a);
        if rec.kind != Kind::Dir && files.accepts(k) {
            files.offer(
                k,
                TopItem {
                    path: NativePath(join_components(stack.components())),
                    kind: rec.kind,
                    logical_bytes: l,
                    allocated_bytes: a,
                    entries: 1,
                    mtime: rec.mtime,
                    incomplete: rec.has(flags::STAT_ERROR),
                },
            );
        }
    }
    close(&mut accs, 0, &mut dirs);
    report.files = files.sorted();
    report.dirs = dirs.sorted();
    if opts.deepest {
        report.dirs = without_ancestors(report.dirs);
        report.dirs.truncate(opts.limit);
    }
    Ok(report)
}

/// Drops every directory that has another listed directory below it, keeping the order.
fn without_ancestors(dirs: Vec<TopItem>) -> Vec<TopItem> {
    // In canonical (component-wise) order a directory's descendants follow it directly.
    let comps = |p: &[u8]| p.split(|&b| b == b'/').map(<[u8]>::to_vec).collect::<Vec<_>>();
    let mut order: Vec<(Vec<Vec<u8>>, usize)> = dirs.iter().enumerate().map(|(i, d)| (comps(&d.path), i)).collect();
    order.sort_by(|a, b| crate::paths::compare_paths(&a.0, &b.0));
    let mut ancestor = vec![false; dirs.len()];
    for w in order.windows(2) {
        let (a, b) = (&w[0].0, &w[1].0);
        if b.len() > a.len() && b[..a.len()] == a[..] {
            ancestor[w[0].1] = true;
        }
    }
    dirs.into_iter().zip(ancestor).filter(|(_, a)| !a).map(|(d, _)| d).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::tests::{boxed, stream};
    use crate::record::Kind::{Dir as D, File as F};

    #[test]
    fn largest_files_and_inclusive_dirs() {
        let s = stream(&[("", D, 0), ("a", D, 0), ("a/b", D, 0), ("a/b/big", F, 500), ("a/small", F, 5), ("c", F, 300), ("d", D, 0), ("d/x", F, 100)]);
        let r = top(boxed(s.clone()), &TopOptions { limit: 2, ..Default::default() }).unwrap();
        assert_eq!(r.total_allocated, 905);
        assert_eq!(r.files.iter().map(|f| f.path.0.as_slice()).collect::<Vec<_>>(), [b"/r/a/b/big".as_slice(), b"/r/c"]);
        assert_eq!(r.dirs[0].path.0, b"/r");
        assert_eq!(r.dirs[0].allocated_bytes, 905);
        assert_eq!(r.dirs[1].path.0, b"/r/a");
        assert_eq!(r.dirs[1].allocated_bytes, 505);
        assert_eq!(r.dirs[1].entries, 4);

        let r = top(boxed(s.clone()), &TopOptions { limit: 2, deepest: true, ..Default::default() }).unwrap();
        let deepest: Vec<_> = r.dirs.iter().map(|d| d.path.lossy()).collect();
        assert_eq!(deepest, ["/r/a/b", "/r/d"], "/r and /r/a only repeat /r/a/b");

        let p = TopOptions { prefix: Some(vec![b"/r".to_vec(), b"d".to_vec()]), ..Default::default() };
        let r = top(boxed(s), &p).unwrap();
        assert_eq!(r.total_allocated, 100);
        assert_eq!(r.dirs.len(), 1);
    }
}
