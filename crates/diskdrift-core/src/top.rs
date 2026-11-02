//! Largest files and directories of one snapshot, computed in a single stream pass.

use crate::diff::RecordStream;
use crate::measurement::{Envelope, Quality};
use crate::native::{NativeName, NativePath};
use crate::paths::{PathStack, compare_paths};
use crate::record::{EntryFields, Kind, Record, flags};
use serde::{Deserialize, Serialize, Serializer};
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
    pub prefix: Option<Vec<NativeName>>,
    pub files: bool,
    pub dirs: bool,
    /// List only the deepest large directories: a directory is left out when a
    /// directory below it is listed (its size mostly repeats that child's).
    pub deepest: bool,
}

impl Default for TopOptions {
    fn default() -> Self {
        TopOptions {
            limit: 30,
            metric: SizeMetric::Allocated,
            prefix: None,
            files: true,
            dirs: true,
            deepest: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TopItem {
    #[serde(flatten)]
    pub path: NativePath,
    pub kind: Kind,
    #[serde(default)]
    pub logical_bytes: u64,
    #[serde(default)]
    pub allocated_bytes: u64,
    /// Directories: number of entries below (inclusive of itself).
    #[serde(default)]
    pub entries: u64,
    #[serde(default)]
    pub mtime: i64,
    /// Directory contents are partly unknown (inaccessible/excluded/mount below).
    pub incomplete: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TopItemWire<'a> {
    #[serde(flatten)]
    path: &'a NativePath,
    kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    logical_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allocated_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entries: Option<u64>,
    #[serde(skip_serializing_if = "is_zero_i64")]
    mtime: i64,
    incomplete: bool,
    #[serde(flatten)]
    measurement: Envelope,
}

fn is_zero_i64(value: &i64) -> bool {
    *value == 0
}

impl Serialize for TopItem {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let unavailable = self.incomplete && self.entries == 0;
        TopItemWire {
            path: &self.path,
            kind: self.kind,
            logical_bytes: (!unavailable).then_some(self.logical_bytes),
            allocated_bytes: (!unavailable).then_some(self.allocated_bytes),
            entries: (!unavailable).then_some(self.entries),
            mtime: if unavailable { 0 } else { self.mtime },
            incomplete: self.incomplete,
            measurement: if unavailable {
                Envelope::default()
            } else if self.incomplete {
                Envelope::lower_bound(
                    "filesystem",
                    "snapshot subtree",
                    "partial snapshot coverage",
                )
            } else {
                Envelope::observed(
                    "filesystem",
                    "snapshot subtree",
                    "canonical snapshot record",
                )
            },
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopReport {
    pub files: Vec<TopItem>,
    pub dirs: Vec<TopItem>,
    #[serde(default)]
    pub total_logical: u64,
    #[serde(default)]
    pub total_allocated: u64,
    #[serde(default)]
    pub entries: u64,
    #[serde(skip)]
    logical_known: u64,
    #[serde(skip)]
    allocated_known: u64,
    #[serde(skip)]
    incomplete: bool,
}

impl TopReport {
    /// Quality of the aggregate allocated-byte total.
    pub fn allocated_quality(&self) -> Quality {
        if self.entries == 0 || self.allocated_known == 0 {
            Quality::Unknown
        } else if self.incomplete || self.allocated_known < self.entries {
            Quality::LowerBound
        } else {
            Quality::Observed
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TopReportWire<'a> {
    files: &'a [TopItem],
    dirs: &'a [TopItem],
    #[serde(skip_serializing_if = "Option::is_none")]
    total_logical: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_allocated: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entries: Option<u64>,
    #[serde(flatten)]
    measurement: Envelope,
}

impl Serialize for TopReport {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let any_known = self.logical_known > 0 || self.allocated_known > 0;
        let complete = self.entries > 0
            && !self.incomplete
            && self.logical_known == self.entries
            && self.allocated_known == self.entries;
        let quality = if !any_known {
            crate::measurement::Quality::Unknown
        } else if complete {
            crate::measurement::Quality::Observed
        } else {
            crate::measurement::Quality::LowerBound
        };
        let measurement = if quality == crate::measurement::Quality::Unknown {
            Envelope::default()
        } else {
            Envelope::with_quality(
                "filesystem",
                "snapshot",
                quality,
                "canonical snapshot record",
            )
        };
        TopReportWire {
            files: &self.files,
            dirs: &self.dirs,
            total_logical: (self.logical_known > 0).then_some(self.total_logical),
            total_allocated: (self.allocated_known > 0).then_some(self.total_allocated),
            entries: (self.entries > 0).then_some(self.entries),
            measurement,
        }
        .serialize(serializer)
    }
}

struct Acc {
    depth: u16,
    path: Vec<NativeName>,
    logical: u64,
    alloc: u64,
    entries: u64,
    mtime: i64,
    incomplete: bool,
}

fn bytes_known(record: &Record) -> bool {
    !record.has(flags::STAT_ERROR)
        && record.available.contains(EntryFields::LOGICAL_SIZE)
        && record.available.contains(EntryFields::ALLOCATED_SIZE)
}

fn record_uncertain(record: &Record) -> bool {
    !bytes_known(record) || record.flags & flags::CONTENTS_UNKNOWN != 0
}

#[allow(clippy::type_complexity)]
struct Heap {
    limit: usize,
    heap: BinaryHeap<Reverse<(u64, ReversePath, TopItemOrd)>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReversePath(Vec<NativeName>);

impl PartialOrd for ReversePath {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ReversePath {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.0.cmp(&self.0)
    }
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
        self.limit > 0
            && (self.heap.len() < self.limit
                || self.heap.peek().is_some_and(|Reverse((k, _, _))| key > *k))
    }

    fn offer(&mut self, key: u64, path: &[NativeName], item: TopItem) {
        if !self.accepts(key) {
            return;
        }
        self.heap
            .push(Reverse((key, ReversePath(path.to_vec()), TopItemOrd(item))));
        if self.heap.len() > self.limit {
            self.heap.pop();
        }
    }

    fn sorted(self) -> Vec<(Vec<NativeName>, TopItem)> {
        let mut v: Vec<_> = self.heap.into_iter().map(|Reverse(x)| x).collect();
        v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.0.cmp(&b.1.0)));
        v.into_iter().map(|(_, p, i)| (p.0, i.0)).collect()
    }
}

pub fn top(stream: RecordStream<'_>, opts: &TopOptions) -> io::Result<TopReport> {
    let mut stack = PathStack::new();
    let mut files = Heap {
        limit: if opts.files { opts.limit } else { 0 },
        heap: BinaryHeap::new(),
    };
    // Dropping ancestors thins the list, so collect more candidates first.
    let dir_candidates = if opts.deepest {
        opts.limit.saturating_mul(4).saturating_add(64)
    } else {
        opts.limit
    };
    let mut dirs = Heap {
        limit: if opts.dirs { dir_candidates } else { 0 },
        heap: BinaryHeap::new(),
    };
    let mut accs: Vec<Acc> = Vec::new();
    let mut report = TopReport::default();
    let key = |logical: u64, alloc: u64| match opts.metric {
        SizeMetric::Allocated => alloc,
        SizeMetric::Logical => logical,
    };

    let close = |accs: &mut Vec<Acc>, depth: u16, dirs: &mut Heap| -> io::Result<()> {
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
                    &a.path,
                    TopItem {
                        path: crate::paths::join_components(&a.path)
                            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
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
        Ok(())
    };

    for rec in stream {
        let rec = rec?;
        stack.set(rec.depth, &rec.name)?;
        if let Some(p) = &opts.prefix
            && !stack
                .starts_with(p)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?
        {
            continue;
        }
        close(&mut accs, rec.depth, &mut dirs)?;
        let (l, a) = (rec.counted_size(), rec.counted_alloc());
        report.entries += 1;
        report.total_logical += l;
        report.total_allocated += a;
        report.logical_known += u64::from(
            !rec.has(flags::STAT_ERROR) && rec.available.contains(EntryFields::LOGICAL_SIZE),
        );
        report.allocated_known += u64::from(
            !rec.has(flags::STAT_ERROR) && rec.available.contains(EntryFields::ALLOCATED_SIZE),
        );
        report.incomplete |= record_uncertain(&rec);
        if rec.is_dir() {
            let unavailable = !bytes_known(&rec);
            accs.push(Acc {
                depth: rec.depth,
                path: if dirs.limit > 0 {
                    stack.components().to_vec()
                } else {
                    Vec::new()
                },
                logical: l,
                alloc: a,
                entries: u64::from(!unavailable),
                mtime: if unavailable { 0 } else { rec.mtime },
                incomplete: record_uncertain(&rec),
            });
            continue;
        }
        if let Some(p) = accs.last_mut() {
            p.logical += l;
            p.alloc += a;
            p.entries += u64::from(bytes_known(&rec));
            p.incomplete |= record_uncertain(&rec);
        }
        let k = key(l, a);
        if rec.kind != Kind::Dir && files.accepts(k) {
            files.offer(
                k,
                stack.components(),
                TopItem {
                    path: stack
                        .path()
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
                    kind: rec.kind,
                    logical_bytes: l,
                    allocated_bytes: a,
                    entries: u64::from(bytes_known(&rec)),
                    mtime: if record_uncertain(&rec) { 0 } else { rec.mtime },
                    incomplete: record_uncertain(&rec),
                },
            );
        }
    }
    close(&mut accs, 0, &mut dirs)?;
    report.files = files.sorted().into_iter().map(|(_, item)| item).collect();
    let sorted_dirs = dirs.sorted();
    report.dirs = sorted_dirs.iter().map(|(_, item)| item.clone()).collect();
    if opts.deepest {
        report.dirs = without_ancestors(sorted_dirs)?;
        report.dirs.truncate(opts.limit);
    }
    Ok(report)
}

/// Drops every directory that has another listed directory below it, keeping the order.
fn without_ancestors(dirs: Vec<(Vec<NativeName>, TopItem)>) -> io::Result<Vec<TopItem>> {
    // In canonical (component-wise) order a directory's descendants follow it directly.
    let mut order: Vec<(Vec<NativeName>, usize)> = dirs
        .iter()
        .enumerate()
        .map(|(i, d)| (d.0.clone(), i))
        .collect();
    if let Some((first, _)) = order.first() {
        for (path, _) in &order {
            compare_paths(first, path)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        }
    }
    order.sort_by(|a, b| a.0.cmp(&b.0));
    let mut ancestor = vec![false; dirs.len()];
    for w in order.windows(2) {
        let (a, b) = (&w[0].0, &w[1].0);
        if b.len() > a.len() && b[..a.len()] == a[..] {
            ancestor[w[0].1] = true;
        }
    }
    Ok(dirs
        .into_iter()
        .zip(ancestor)
        .filter(|(_, a)| !a)
        .map(|(d, _)| d.1)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::tests::{boxed, stream};
    use crate::record::Kind::{Dir as D, File as F};

    #[test]
    fn largest_files_and_inclusive_dirs() {
        let s = stream(&[
            ("", D, 0),
            ("a", D, 0),
            ("a/b", D, 0),
            ("a/b/big", F, 500),
            ("a/small", F, 5),
            ("c", F, 300),
            ("d", D, 0),
            ("d/x", F, 100),
        ]);
        let r = top(
            boxed(s.clone()),
            &TopOptions {
                limit: 2,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.total_allocated, 905);
        assert_eq!(
            r.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
            [
                NativePath::unix(b"/r/a/b/big".to_vec()),
                NativePath::unix(b"/r/c".to_vec())
            ]
        );
        assert_eq!(r.dirs[0].path, NativePath::unix(b"/r".to_vec()));
        assert_eq!(r.dirs[0].allocated_bytes, 905);
        assert_eq!(r.dirs[1].path, NativePath::unix(b"/r/a".to_vec()));
        assert_eq!(r.dirs[1].allocated_bytes, 505);
        assert_eq!(r.dirs[1].entries, 4);

        let r = top(
            boxed(s.clone()),
            &TopOptions {
                limit: 2,
                deepest: true,
                ..Default::default()
            },
        )
        .unwrap();
        let deepest: Vec<_> = r.dirs.iter().map(|d| d.path.display()).collect();
        assert_eq!(
            deepest,
            ["/r/a/b", "/r/d"],
            "/r and /r/a only repeat /r/a/b"
        );

        let p = TopOptions {
            prefix: Some(vec![
                NativeName::unix(b"/r".to_vec()),
                NativeName::unix(b"d".to_vec()),
            ]),
            ..Default::default()
        };
        let r = top(boxed(s), &p).unwrap();
        assert_eq!(r.total_allocated, 100);
        assert_eq!(r.dirs.len(), 1);
    }
}
