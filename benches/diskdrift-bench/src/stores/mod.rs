//! Storage-engine candidates (spec section 7) behind one minimal interface.

pub mod binary;
pub mod blocks;
pub mod lmdb;
pub mod sqlite;

use diskdrift_core::delta::{Apply, DeltaOp};
use diskdrift_core::diff::RecordStream;
use diskdrift_core::paths::PathStack;
use diskdrift_core::record::Record;
use std::io;
use std::path::Path;

pub type Ops<'a> = Box<dyn Iterator<Item = io::Result<DeltaOp>> + 'a>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapInfo {
    pub id: u32,
    /// `Some(base)` for a delta snapshot.
    pub base: Option<u32>,
}

pub trait Store {
    fn write_full(&mut self, id: u32, records: &mut dyn Iterator<Item = Record>) -> io::Result<u64>;
    fn write_delta(&mut self, id: u32, base: u32, ops: &mut dyn Iterator<Item = DeltaOp>) -> io::Result<u64>;
    fn read_full(&self, id: u32) -> io::Result<RecordStream<'_>>;
    fn read_delta(&self, id: u32) -> io::Result<Ops<'_>>;
    fn list(&self) -> io::Result<Vec<SnapInfo>>;

    /// Records at or below `prefix` (components including the root). The default
    /// streams the whole snapshot and filters.
    fn read_prefix(&self, id: u32, prefix: &[Vec<u8>]) -> io::Result<RecordStream<'_>> {
        Ok(Box::new(PrefixFilter::new(self.read_full(id)?, prefix.to_vec())))
    }

    /// Largest files by a native query, if the engine has one (an index).
    fn native_top_files(&self, _id: u32, _n: usize) -> Option<io::Result<Vec<u64>>> {
        None
    }
}

/// Opens any snapshot, reconstructing deltas (recursively for chains).
pub fn open<'a>(store: &'a dyn Store, infos: &[SnapInfo], id: u32) -> io::Result<RecordStream<'a>> {
    let info = infos
        .iter()
        .find(|i| i.id == id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("snapshot {id} not found")))?;
    match info.base {
        None => store.read_full(id),
        Some(base) => Ok(Box::new(Apply::new(open(store, infos, base)?, store.read_delta(id)?))),
    }
}

/// Keeps the contiguous run of records at or below `prefix`.
pub struct PrefixFilter<'a> {
    inner: RecordStream<'a>,
    prefix: Vec<Vec<u8>>,
    stack: PathStack,
    inside: bool,
    done: bool,
}

impl<'a> PrefixFilter<'a> {
    pub fn new(inner: RecordStream<'a>, prefix: Vec<Vec<u8>>) -> Self {
        PrefixFilter { inner, prefix, stack: PathStack::new(), inside: false, done: false }
    }

    /// Starts with a pre-filled path context (ancestors of the first record).
    pub fn with_context(inner: RecordStream<'a>, prefix: Vec<Vec<u8>>, ctx: &[Vec<u8>]) -> Self {
        let mut f = Self::new(inner, prefix);
        for (d, c) in ctx.iter().enumerate() {
            let _ = f.stack.set(d as u16, c);
        }
        f
    }
}

impl Iterator for PrefixFilter<'_> {
    type Item = io::Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            let rec = match self.inner.next()? {
                Ok(r) => r,
                Err(e) => return Some(Err(e)),
            };
            if let Err(e) = self.stack.set(rec.depth, &rec.name) {
                return Some(Err(e));
            }
            if self.stack.starts_with(&self.prefix) {
                self.inside = true;
                return Some(Ok(rec));
            }
            if self.inside {
                self.done = true;
                return None;
            }
        }
    }
}

/// Allocated bytes of everything under `dir`.
pub fn footprint(dir: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                stack.push(e.path());
            } else {
                total += md.blocks() * 512;
            }
        }
    }
    total
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    /// Versioned sorted records in zstd blocks, one file per snapshot.
    Binary,
    /// Same container, columnar blocks with value transforms.
    BinaryColumnar,
    /// SQLite, one row per entry, typed columns, WITHOUT ROWID primary key.
    SqliteRows,
    /// SQLite rows plus secondary indexes for native top-N and path lookups.
    SqliteIndexed,
    /// SQLite as a container for the same compressed blocks as `binary`.
    SqliteBlocks,
    /// LMDB, one key/value per entry.
    Lmdb,
}

impl Backend {
    pub const ALL: [Backend; 6] = [
        Backend::Binary,
        Backend::BinaryColumnar,
        Backend::SqliteRows,
        Backend::SqliteIndexed,
        Backend::SqliteBlocks,
        Backend::Lmdb,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Backend::Binary => "binary",
            Backend::BinaryColumnar => "binary-columnar",
            Backend::SqliteRows => "sqlite-rows",
            Backend::SqliteIndexed => "sqlite-indexed",
            Backend::SqliteBlocks => "sqlite-blocks",
            Backend::Lmdb => "lmdb",
        }
    }

    pub fn open(self, dir: &Path) -> io::Result<Box<dyn Store>> {
        std::fs::create_dir_all(dir)?;
        Ok(match self {
            Backend::Binary => Box::new(binary::BinaryStore::open(dir, 3, false)?),
            Backend::BinaryColumnar => Box::new(binary::BinaryStore::open(dir, 3, true)?),
            Backend::SqliteRows => Box::new(sqlite::SqliteStore::open(dir, sqlite::Mode::Rows)?),
            Backend::SqliteIndexed => Box::new(sqlite::SqliteStore::open(dir, sqlite::Mode::Indexed)?),
            Backend::SqliteBlocks => Box::new(sqlite::SqliteStore::open(dir, sqlite::Mode::Blocks)?),
            Backend::Lmdb => Box::new(lmdb::LmdbStore::open(dir)?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{Gen, Params};
    use diskdrift_core::testutil::TestDir;

    fn collect(s: RecordStream<'_>) -> Vec<Record> {
        s.collect::<io::Result<Vec<_>>>().unwrap()
    }

    #[test]
    fn every_backend_roundtrips_full_delta_and_prefix() {
        let p = Params { entries: 30_000, seed: 9, change: 0.05, horizon: 3 };
        let v0: Vec<Record> = Gen::new(p, 0).collect();
        let v1: Vec<Record> = Gen::new(p, 1).collect();
        let v2: Vec<Record> = Gen::new(p, 2).collect();
        let ops1 = diskdrift_core::delta::compute(Box::new(v0.clone().into_iter().map(Ok)), Box::new(v1.clone().into_iter().map(Ok))).unwrap();
        let ops2 = diskdrift_core::delta::compute(Box::new(v1.clone().into_iter().map(Ok)), Box::new(v2.clone().into_iter().map(Ok))).unwrap();
        // A prefix: path of the 100th record if it is a directory, else its parent.
        let mut stack = PathStack::new();
        let mut prefix = Vec::new();
        for (i, r) in v0.iter().enumerate() {
            stack.set(r.depth, &r.name).unwrap();
            if i >= 100 && r.is_dir() && r.depth >= 2 {
                prefix = stack.components().to_vec();
                break;
            }
        }
        let expected_prefix: Vec<Record> =
            PrefixFilter::new(Box::new(v0.clone().into_iter().map(Ok)), prefix.clone()).map(Result::unwrap).collect();
        assert!(expected_prefix.len() > 1);

        for b in Backend::ALL {
            let t = TestDir::new(&format!("store-{}", b.name()));
            let mut s = b.open(t.path()).unwrap();
            s.write_full(0, &mut v0.clone().into_iter()).unwrap();
            s.write_delta(1, 0, &mut ops1.clone().into_iter()).unwrap();
            s.write_delta(2, 1, &mut ops2.clone().into_iter()).unwrap();
            let infos = s.list().unwrap();
            assert_eq!(infos.len(), 3, "{}", b.name());
            assert_eq!(collect(open(s.as_ref(), &infos, 0).unwrap()), v0, "{}", b.name());
            assert_eq!(collect(open(s.as_ref(), &infos, 1).unwrap()), v1, "{}", b.name());
            assert_eq!(collect(open(s.as_ref(), &infos, 2).unwrap()), v2, "{} chain", b.name());
            assert_eq!(collect(s.read_prefix(0, &prefix).unwrap()), expected_prefix, "{} prefix", b.name());
            drop(s);
            // Reopen: data persisted.
            let s = b.open(t.path()).unwrap();
            let infos = s.list().unwrap();
            assert_eq!(collect(open(s.as_ref(), &infos, 2).unwrap()).len(), v2.len());
        }
    }
}
