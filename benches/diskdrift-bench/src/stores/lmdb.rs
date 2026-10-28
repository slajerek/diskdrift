//! LMDB candidate: one ordered key/value per entry, key = (snapshot, sequence).

use super::{Ops, SnapInfo, Store};
use diskdrift_core::delta::{DeltaOp, OpDecoder, OpEncoder};
use diskdrift_core::diff::RecordStream;
use diskdrift_core::record::Record;
use heed::types::Bytes;
use heed::{Database, Env, EnvOpenOptions, PutFlags};
use std::io;
use std::path::Path;

pub struct LmdbStore {
    env: Env,
    entries: Database<Bytes, Bytes>,
    ops: Database<Bytes, Bytes>,
    snaps: Database<Bytes, Bytes>,
}

fn err(e: heed::Error) -> io::Error {
    match e {
        heed::Error::Mdb(heed::MdbError::MapFull) => io::Error::new(io::ErrorKind::StorageFull, "LMDB map full"),
        heed::Error::Io(e) => e,
        other => io::Error::other(other.to_string()),
    }
}

fn key(snap: u32, seq: u64) -> [u8; 12] {
    let mut k = [0u8; 12];
    k[..4].copy_from_slice(&snap.to_be_bytes());
    k[4..].copy_from_slice(&seq.to_be_bytes());
    k
}

const CHUNK: usize = 20_000;

impl LmdbStore {
    pub fn open(dir: &Path) -> io::Result<Self> {
        let map_size = if cfg!(windows) { 64 << 20 } else { 256 << 30 };
        let env = unsafe {
            EnvOpenOptions::new().map_size(map_size).max_dbs(4).open(dir).map_err(err)?
        };
        let mut wtxn = env.write_txn().map_err(err)?;
        let entries = env.create_database(&mut wtxn, Some("entries")).map_err(err)?;
        let ops = env.create_database(&mut wtxn, Some("ops")).map_err(err)?;
        let snaps = env.create_database(&mut wtxn, Some("snaps")).map_err(err)?;
        wtxn.commit().map_err(err)?;
        Ok(LmdbStore { env, entries, ops, snaps })
    }

    fn put_snap(&self, wtxn: &mut heed::RwTxn<'_>, id: u32, base: Option<u32>) -> io::Result<()> {
        let v: [u8; 5] = match base {
            Some(b) => {
                let mut v = [1u8; 5];
                v[1..].copy_from_slice(&b.to_be_bytes());
                v
            }
            None => [0u8; 5],
        };
        self.snaps.put(wtxn, &id.to_be_bytes(), &v).map_err(err)
    }

    fn chunk(&self, db: Database<Bytes, Bytes>, snap: u32, from: u64) -> io::Result<Vec<(u64, Vec<u8>)>> {
        let rtxn = self.env.read_txn().map_err(err)?;
        let start = key(snap, from);
        let end = key(snap, u64::MAX);
        let range = (std::ops::Bound::Included(&start[..]), std::ops::Bound::Included(&end[..]));
        let mut out = Vec::with_capacity(CHUNK);
        for item in db.range(&rtxn, &range).map_err(err)? {
            let (k, v) = item.map_err(err)?;
            let seq = u64::from_be_bytes(k[4..12].try_into().unwrap());
            out.push((seq, v.to_vec()));
            if out.len() >= CHUNK {
                break;
            }
        }
        Ok(out)
    }
}

#[allow(clippy::type_complexity)]
struct Chunks<'a, T> {
    store: &'a LmdbStore,
    db: Database<Bytes, Bytes>,
    snap: u32,
    next: u64,
    cur: std::vec::IntoIter<Vec<u8>>,
    decode: Box<dyn FnMut(&[u8]) -> io::Result<T> + 'a>,
    done: bool,
}

impl<T> Iterator for Chunks<'_, T> {
    type Item = io::Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(v) = self.cur.next() {
                return Some((self.decode)(&v));
            }
            if self.done {
                return None;
            }
            match self.store.chunk(self.db, self.snap, self.next) {
                Ok(v) => {
                    if v.len() < CHUNK {
                        self.done = true;
                    }
                    if let Some((last, _)) = v.last() {
                        self.next = last + 1;
                    }
                    self.cur = v.into_iter().map(|(_, b)| b).collect::<Vec<_>>().into_iter();
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

impl Store for LmdbStore {
    fn write_full(&mut self, id: u32, records: &mut dyn Iterator<Item = Record>) -> io::Result<u64> {
        let mut wtxn = self.env.write_txn().map_err(err)?;
        let mut buf = Vec::new();
        let mut n = 0u64;
        for (seq, r) in records.enumerate() {
            buf.clear();
            r.encode_dds1(&mut buf)?;
            self.entries.put_with_flags(&mut wtxn, PutFlags::APPEND, &key(id, seq as u64), &buf).map_err(err)?;
            n += 1;
        }
        self.put_snap(&mut wtxn, id, None)?;
        wtxn.commit().map_err(err)?;
        Ok(n)
    }

    fn write_delta(&mut self, id: u32, base: u32, ops: &mut dyn Iterator<Item = DeltaOp>) -> io::Result<u64> {
        let mut wtxn = self.env.write_txn().map_err(err)?;
        let mut enc = OpEncoder::new();
        let mut buf = Vec::new();
        let mut n = 0u64;
        for (seq, op) in ops.enumerate() {
            buf.clear();
            enc.encode(&op, &mut buf)?;
            self.ops.put_with_flags(&mut wtxn, PutFlags::APPEND, &key(id, seq as u64), &buf).map_err(err)?;
            n += 1;
        }
        self.put_snap(&mut wtxn, id, Some(base))?;
        wtxn.commit().map_err(err)?;
        Ok(n)
    }

    fn read_full(&self, id: u32) -> io::Result<RecordStream<'_>> {
        Ok(Box::new(Chunks {
            store: self,
            db: self.entries,
            snap: id,
            next: 0,
            cur: Vec::new().into_iter(),
            decode: Box::new(|b: &[u8]| Record::decode(&mut &b[..])),
            done: false,
        }))
    }

    fn read_delta(&self, id: u32) -> io::Result<Ops<'_>> {
        let mut dec = OpDecoder::new();
        Ok(Box::new(Chunks {
            store: self,
            db: self.ops,
            snap: id,
            next: 0,
            cur: Vec::new().into_iter(),
            decode: Box::new(move |b: &[u8]| dec.decode(&mut &b[..])),
            done: false,
        }))
    }

    fn list(&self) -> io::Result<Vec<SnapInfo>> {
        let rtxn = self.env.read_txn().map_err(err)?;
        let mut out = Vec::new();
        for item in self.snaps.iter(&rtxn).map_err(err)? {
            let (k, v) = item.map_err(err)?;
            let id = u32::from_be_bytes(k.try_into().unwrap());
            let base = (v[0] == 1).then(|| u32::from_be_bytes(v[1..5].try_into().unwrap()));
            out.push(SnapInfo { id, base });
        }
        Ok(out)
    }
}
