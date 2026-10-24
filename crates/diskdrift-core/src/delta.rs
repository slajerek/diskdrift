//! Delta snapshots: the exact changes that turn a base snapshot into a newer one.
//!
//! A delta is a stream of [`DeltaOp`]s in canonical path order, produced from a
//! complete comparison (`merge`) and applied by merging it with the base stream.
//! Reconstruction is exact: applying the delta to the base yields a stream equal,
//! record for record, to the snapshot it was created from.

use crate::diff::{Change, DiffEntry, DiffVisitor, RecordStream};
use crate::paths::{PathStack, compare_paths};
use crate::record::{Record, get_varint, put_varint};
use std::cmp::Ordering;
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OpKind {
    Add = 0,
    Remove = 1,
    Replace = 2,
}

/// One change. `parent` holds the path components above the record (including
/// the root); the record's own name and depth complete the path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaOp {
    pub kind: OpKind,
    pub parent: Vec<Vec<u8>>,
    pub record: Record,
}

impl DeltaOp {
    fn path_cmp(&self, comps: &[Vec<u8>]) -> Ordering {
        // Compare parent + [name] against comps without allocating.
        let own_len = self.parent.len() + 1;
        for (i, theirs) in comps.iter().enumerate().take(own_len) {
            let mine: &[u8] = if i < self.parent.len() { &self.parent[i] } else { &self.record.name };
            match mine.cmp(theirs.as_slice()) {
                Ordering::Equal => continue,
                o => return o,
            }
        }
        own_len.cmp(&comps.len())
    }
}

/// Encodes ops compactly; parent paths are front-coded against the previous op.
#[derive(Default)]
pub struct OpEncoder {
    prev_parent: Vec<Vec<u8>>,
}

impl OpEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn encode(&mut self, op: &DeltaOp, out: &mut Vec<u8>) {
        out.push(op.kind as u8);
        let shared = self.prev_parent.iter().zip(op.parent.iter()).take_while(|(a, b)| a == b).count();
        put_varint(out, shared as u64);
        put_varint(out, (op.parent.len() - shared) as u64);
        for c in &op.parent[shared..] {
            put_varint(out, c.len() as u64);
            out.extend_from_slice(c);
        }
        if op.kind == OpKind::Remove {
            put_varint(out, op.record.name.len() as u64);
            out.extend_from_slice(&op.record.name);
        } else {
            op.record.encode(out);
        }
        self.prev_parent.truncate(shared);
        self.prev_parent.extend(op.parent[shared..].iter().cloned());
    }
}

#[derive(Default)]
pub struct OpDecoder {
    prev_parent: Vec<Vec<u8>>,
}

fn corrupt(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("corrupt delta: {what}"))
}

impl OpDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn decode(&mut self, buf: &mut &[u8]) -> io::Result<DeltaOp> {
        let kind = match buf.first().copied() {
            Some(0) => OpKind::Add,
            Some(1) => OpKind::Remove,
            Some(2) => OpKind::Replace,
            _ => return Err(corrupt("op kind")),
        };
        *buf = &buf[1..];
        let shared = get_varint(buf)? as usize;
        let extra = get_varint(buf)? as usize;
        if shared > self.prev_parent.len() {
            return Err(corrupt("shared prefix"));
        }
        self.prev_parent.truncate(shared);
        for _ in 0..extra {
            let len = get_varint(buf)? as usize;
            if buf.len() < len {
                return Err(corrupt("component length"));
            }
            self.prev_parent.push(buf[..len].to_vec());
            *buf = &buf[len..];
        }
        let record = if kind == OpKind::Remove {
            let len = get_varint(buf)? as usize;
            if buf.len() < len {
                return Err(corrupt("name length"));
            }
            let r = Record { depth: self.prev_parent.len() as u16, name: buf[..len].to_vec(), ..Default::default() };
            *buf = &buf[len..];
            r
        } else {
            Record::decode(buf)?
        };
        if record.depth as usize != self.prev_parent.len() {
            return Err(corrupt("depth does not match path"));
        }
        Ok(DeltaOp { kind, parent: self.prev_parent.clone(), record })
    }
}

/// Merge visitor that turns a base-vs-new comparison into delta ops.
///
/// Exact equality (`!=`) decides, not the reporting notion of "changed", so the
/// delta also carries flag, link-count and volume-index differences.
pub struct DeltaBuilder<F: FnMut(&DeltaOp) -> io::Result<()>> {
    emit: F,
    pub ops: u64,
}

impl<F: FnMut(&DeltaOp) -> io::Result<()>> DeltaBuilder<F> {
    pub fn new(emit: F) -> Self {
        DeltaBuilder { emit, ops: 0 }
    }
}

impl<F: FnMut(&DeltaOp) -> io::Result<()>> DiffVisitor for DeltaBuilder<F> {
    fn entry(&mut self, e: &DiffEntry<'_>) -> io::Result<()> {
        let (kind, record) = match (e.old, e.new) {
            (Some(o), Some(n)) if o == n => return Ok(()),
            (Some(_), Some(n)) => (OpKind::Replace, n.clone()),
            (None, Some(n)) => (OpKind::Add, n.clone()),
            (Some(o), None) => (OpKind::Remove, Record { depth: o.depth, name: o.name.clone(), ..Default::default() }),
            (None, None) => return Ok(()),
        };
        debug_assert!(!matches!(e.change, Change::Unchanged) || kind == OpKind::Replace);
        let op = DeltaOp { kind, parent: e.path[..e.path.len() - 1].to_vec(), record };
        self.ops += 1;
        (self.emit)(&op)
    }
}

/// Applies `ops` to `base`, yielding the reconstructed canonical stream.
pub struct Apply<'a> {
    base: RecordStream<'a>,
    ops: Box<dyn Iterator<Item = io::Result<DeltaOp>> + 'a>,
    base_cur: Option<Record>,
    base_stack: PathStack,
    op_cur: Option<DeltaOp>,
    started: bool,
}

impl<'a> Apply<'a> {
    pub fn new(base: RecordStream<'a>, ops: Box<dyn Iterator<Item = io::Result<DeltaOp>> + 'a>) -> Self {
        Apply { base, ops, base_cur: None, base_stack: PathStack::new(), op_cur: None, started: false }
    }

    fn advance_base(&mut self) -> io::Result<()> {
        self.base_cur = self.base.next().transpose()?;
        if let Some(r) = &self.base_cur {
            self.base_stack.set(r.depth, &r.name)?;
        }
        Ok(())
    }

    fn advance_op(&mut self) -> io::Result<()> {
        self.op_cur = self.ops.next().transpose()?;
        Ok(())
    }

    fn step(&mut self) -> io::Result<Option<Record>> {
        if !self.started {
            self.started = true;
            self.advance_base()?;
            self.advance_op()?;
        }
        loop {
            let order = match (&self.base_cur, &self.op_cur) {
                (None, None) => return Ok(None),
                (Some(_), None) => Ordering::Greater,
                (None, Some(_)) => Ordering::Less,
                (Some(_), Some(op)) => op.path_cmp(self.base_stack.components()),
            };
            match order {
                Ordering::Greater => {
                    let r = self.base_cur.take().unwrap();
                    self.advance_base()?;
                    return Ok(Some(r));
                }
                Ordering::Less => {
                    let op = self.op_cur.take().unwrap();
                    self.advance_op()?;
                    if op.kind != OpKind::Add {
                        return Err(corrupt("change for a path missing from the base"));
                    }
                    return Ok(Some(op.record));
                }
                Ordering::Equal => {
                    let op = self.op_cur.take().unwrap();
                    self.advance_op()?;
                    self.advance_base()?;
                    match op.kind {
                        OpKind::Remove => continue,
                        OpKind::Replace => return Ok(Some(op.record)),
                        OpKind::Add => return Err(corrupt("added path already in the base")),
                    }
                }
            }
        }
    }
}

impl Iterator for Apply<'_> {
    type Item = io::Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        self.step().transpose()
    }
}

/// Push-style delta construction: the new snapshot's records are fed one at a time
/// in canonical order (as a scan produces them) and the base stream is pulled as
/// needed. Produces exactly the ops [`DeltaBuilder`] would for the same streams.
pub struct PushDelta<'a> {
    base: RecordStream<'a>,
    base_cur: Option<Record>,
    base_stack: PathStack,
    new_stack: PathStack,
    started: bool,
    pub ops: u64,
}

pub type OpSink<'s> = dyn FnMut(&DeltaOp) -> io::Result<()> + 's;

impl<'a> PushDelta<'a> {
    pub fn new(base: RecordStream<'a>) -> Self {
        PushDelta { base, base_cur: None, base_stack: PathStack::new(), new_stack: PathStack::new(), started: false, ops: 0 }
    }

    fn advance_base(&mut self) -> io::Result<()> {
        self.base_cur = self.base.next().transpose()?;
        if let Some(r) = &self.base_cur {
            self.base_stack.set(r.depth, &r.name)?;
        }
        Ok(())
    }

    fn emit_remove(&mut self, out: &mut OpSink<'_>) -> io::Result<()> {
        let b = self.base_cur.as_ref().unwrap();
        let comps = self.base_stack.components();
        let op = DeltaOp {
            kind: OpKind::Remove,
            parent: comps[..comps.len() - 1].to_vec(),
            record: Record { depth: b.depth, name: b.name.clone(), ..Default::default() },
        };
        self.ops += 1;
        out(&op)
    }

    fn emit_new(&mut self, kind: OpKind, rec: &Record, out: &mut OpSink<'_>) -> io::Result<()> {
        let comps = self.new_stack.components();
        let op = DeltaOp { kind, parent: comps[..comps.len() - 1].to_vec(), record: rec.clone() };
        self.ops += 1;
        out(&op)
    }

    pub fn push(&mut self, rec: &Record, out: &mut OpSink<'_>) -> io::Result<()> {
        if !self.started {
            self.started = true;
            self.advance_base()?;
        }
        self.new_stack.set(rec.depth, &rec.name)?;
        loop {
            let Some(b) = &self.base_cur else {
                return self.emit_new(OpKind::Add, rec, out);
            };
            match compare_paths(self.base_stack.components(), self.new_stack.components()) {
                Ordering::Less => {
                    self.emit_remove(out)?;
                    self.advance_base()?;
                }
                Ordering::Equal => {
                    if b != rec {
                        self.emit_new(OpKind::Replace, rec, out)?;
                    }
                    return self.advance_base();
                }
                Ordering::Greater => return self.emit_new(OpKind::Add, rec, out),
            }
        }
    }

    /// Emits removals for the rest of the base; returns the number of ops.
    pub fn finish(&mut self, out: &mut OpSink<'_>) -> io::Result<u64> {
        if !self.started {
            self.started = true;
            self.advance_base()?;
        }
        while self.base_cur.is_some() {
            self.emit_remove(out)?;
            self.advance_base()?;
        }
        Ok(self.ops)
    }
}

/// Collects ops for a comparison of `base` against `new` (convenience for tests
/// and small trees).
pub fn compute(base: RecordStream<'_>, new: RecordStream<'_>) -> io::Result<Vec<DeltaOp>> {
    let mut ops = Vec::new();
    let mut b = DeltaBuilder::new(|op: &DeltaOp| {
        ops.push(op.clone());
        Ok(())
    });
    crate::diff::merge(base, new, &mut b)?;
    Ok(ops)
}

/// Ordering helper used by stores to validate op streams.
pub fn op_order(a: &DeltaOp, b: &DeltaOp) -> Ordering {
    let mut pa = a.parent.clone();
    pa.push(a.record.name.clone());
    let mut pb = b.parent.clone();
    pb.push(b.record.name.clone());
    compare_paths(&pa, &pb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::tests::{boxed, stream};
    use crate::record::{Kind::Dir as D, Kind::File as F, flags};

    fn roundtrip(base: Vec<Record>, new: Vec<Record>) {
        let ops = compute(boxed(base.clone()), boxed(new.clone())).unwrap();
        for w in ops.windows(2) {
            assert_eq!(op_order(&w[0], &w[1]), Ordering::Less, "ops are in canonical order");
        }
        let mut enc = OpEncoder::new();
        let mut buf = Vec::new();
        for op in &ops {
            enc.encode(op, &mut buf);
        }
        let mut dec = OpDecoder::new();
        let mut s = buf.as_slice();
        let mut decoded = Vec::new();
        while !s.is_empty() {
            decoded.push(dec.decode(&mut s).unwrap());
        }
        assert_eq!(decoded, ops);
        let rebuilt: Vec<Record> =
            Apply::new(boxed(base.clone()), Box::new(decoded.into_iter().map(Ok))).collect::<io::Result<_>>().unwrap();
        assert_eq!(rebuilt, new);
        let mut pushed = Vec::new();
        let mut collect = |op: &DeltaOp| {
            pushed.push(op.clone());
            Ok(())
        };
        let mut p = PushDelta::new(boxed(base));
        for r in &new {
            p.push(r, &mut collect).unwrap();
        }
        p.finish(&mut collect).unwrap();
        assert_eq!(pushed, ops, "push-style delta equals the merge-based one");
    }

    #[test]
    fn reconstructs_exactly() {
        let base = stream(&[("", D, 0), ("a", D, 0), ("a/x", F, 1), ("a/y", F, 2), ("b", D, 0), ("b/z", F, 3), ("c", F, 4)]);
        let mut new = stream(&[("", D, 0), ("a", D, 0), ("a/w", F, 9), ("a/x", F, 1), ("b2", D, 0), ("b2/q", F, 3), ("c", F, 40), ("d", F, 1)]);
        // Keep identity of unchanged records except where we change them on purpose.
        for r in new.iter_mut() {
            if let Some(b) = base.iter().find(|b| b.name == r.name && b.depth == r.depth) {
                r.ino = b.ino;
            }
        }
        new.iter_mut().find(|r| r.name == b"x").unwrap().flags = flags::HARDLINK_DUP;
        roundtrip(base.clone(), new);
        roundtrip(base.clone(), base.clone());
        roundtrip(base.clone(), stream(&[("", D, 0)]));
        roundtrip(stream(&[("", D, 0)]), base);
    }

    #[test]
    fn rejects_inconsistent_delta() {
        let base = stream(&[("", D, 0), ("a", F, 1)]);
        let bad = DeltaOp {
            kind: OpKind::Add,
            parent: vec![b"/r".to_vec()],
            record: Record { depth: 1, name: b"a".to_vec(), ..Default::default() },
        };
        let r: io::Result<Vec<Record>> = Apply::new(boxed(base), Box::new(std::iter::once(Ok(bad)))).collect();
        assert!(r.is_err());
    }

    /// Random tree as canonical records: names from a small alphabet so that
    /// mutations collide with existing paths.
    fn random_tree(rng: &mut fastrand::Rng, n: usize) -> Vec<(Vec<Vec<u8>>, bool, u64)> {
        let mut set = std::collections::BTreeMap::new();
        for _ in 0..n {
            let depth = rng.usize(1..5);
            let mut comps = vec![b"/r".to_vec()];
            for _ in 0..depth {
                comps.push(vec![b'a' + rng.u8(0..6)]);
            }
            // every prefix is a directory
            for d in 1..comps.len() {
                set.entry(comps[..d].to_vec()).or_insert((true, 0));
            }
            set.entry(comps).or_insert((false, rng.u64(0..10_000)));
        }
        // a path cannot be both a file and an ancestor: ancestors win
        let keys: Vec<Vec<Vec<u8>>> = set.keys().cloned().collect();
        for k in &keys {
            for d in 1..k.len() {
                if let Some(v) = set.get_mut(&k[..d]) {
                    v.0 = true;
                    v.1 = 0;
                }
            }
        }
        let mut v: Vec<_> = set.into_iter().map(|(k, (dir, size))| (k, dir, size)).collect();
        v.sort_by(|a, b| crate::paths::compare_paths(&a.0, &b.0));
        v
    }

    fn records(t: &[(Vec<Vec<u8>>, bool, u64)]) -> Vec<Record> {
        t.iter()
            .map(|(comps, dir, size)| Record {
                depth: (comps.len() - 1) as u16,
                name: comps.last().unwrap().clone(),
                kind: if *dir { D } else { F },
                size: *size,
                alloc: size.div_ceil(4096) * 4096,
                ino: comps.iter().map(|c| c[0] as u64).fold(7, |h, c| h * 31 + c),
                nlink: 1,
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn property_reconstruction_and_net_change() {
        let mut rng = fastrand::Rng::with_seed(42);
        for round in 0..200 {
            let (na, nb) = (rng.usize(1..60), rng.usize(1..60));
            let a = records(&random_tree(&mut rng, na));
            let b = records(&random_tree(&mut rng, nb));
            let mut root = vec![Record { depth: 0, name: b"/r".to_vec(), kind: D, ..Default::default() }];
            let a: Vec<Record> = root.iter().cloned().chain(a).collect();
            root.extend(b);
            let b = root;
            roundtrip(a.clone(), b.clone());
            let r = crate::diff::diff(boxed(a.clone()), boxed(b.clone()), Default::default()).unwrap();
            let total = |v: &[Record]| v.iter().map(|r| r.alloc as i128).sum::<i128>();
            assert_eq!(r.summary.net_allocated_bytes, total(&b) - total(&a), "round {round}");
        }
    }
}
