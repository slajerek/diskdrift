//! SQLite candidates: typed rows (optionally indexed), or compressed blocks.

use super::blocks::{self, OpBlockEncoder, RecordBlockEncoder};
use super::{Ops, PrefixFilter, SnapInfo, Store};
use diskdrift_core::delta::{DeltaOp, OpDecoder, OpEncoder};
use diskdrift_core::diff::RecordStream;
use diskdrift_core::paths::PathStack;
use diskdrift_core::record::{Kind, Record};
use rusqlite::{Connection, params};
use std::io;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Rows,
    Indexed,
    Blocks,
}

pub struct SqliteStore {
    conn: Connection,
    mode: Mode,
}

fn err(e: rusqlite::Error) -> io::Error {
    match e {
        rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::DiskFull => {
            io::Error::new(io::ErrorKind::StorageFull, e.to_string())
        }
        _ => io::Error::other(e.to_string()),
    }
}

const CHUNK: i64 = 20_000;

impl SqliteStore {
    pub fn open(dir: &Path, mode: Mode) -> io::Result<Self> {
        let conn = Connection::open(dir.join("store.sqlite")).map_err(err)?;
        conn.execute_batch(
            "PRAGMA page_size=4096;
             PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA cache_size=-65536;
             CREATE TABLE IF NOT EXISTS snaps(id INTEGER PRIMARY KEY, base INTEGER);
             CREATE TABLE IF NOT EXISTS entries(
               snap INTEGER NOT NULL, seq INTEGER NOT NULL, parent INTEGER NOT NULL,
               depth INTEGER NOT NULL, name BLOB NOT NULL, kind INTEGER NOT NULL,
               flags INTEGER NOT NULL, size INTEGER NOT NULL, alloc INTEGER NOT NULL,
               mtime INTEGER NOT NULL, btime INTEGER NOT NULL, ino INTEGER NOT NULL,
               nlink INTEGER NOT NULL, vol INTEGER NOT NULL,
               PRIMARY KEY(snap, seq)) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS ops(snap INTEGER NOT NULL, seq INTEGER NOT NULL, op BLOB NOT NULL,
               PRIMARY KEY(snap, seq)) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS blocks(snap INTEGER NOT NULL, idx INTEGER NOT NULL, kind INTEGER NOT NULL,
               raw_len INTEGER NOT NULL, count INTEGER NOT NULL, crc INTEGER NOT NULL,
               first_path BLOB NOT NULL, data BLOB NOT NULL, PRIMARY KEY(snap, idx));",
        )
        .map_err(err)?;
        if mode == Mode::Indexed {
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS entries_alloc ON entries(snap, alloc);
                 CREATE INDEX IF NOT EXISTS entries_child ON entries(snap, parent, name);",
            )
            .map_err(err)?;
        }
        Ok(SqliteStore { conn, mode })
    }

    fn checkpoint(&self) -> io::Result<()> {
        self.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())).map_err(err)
    }

    #[allow(clippy::type_complexity)]
    fn write_blocks(&mut self, id: u32, base: Option<u32>, blocks_iter: &mut dyn FnMut(&mut dyn FnMut(blocks::Block) -> io::Result<()>) -> io::Result<u64>) -> io::Result<u64> {
        let tx = self.conn.transaction().map_err(err)?;
        let n = {
            let mut st = tx
                .prepare("INSERT INTO blocks(snap, idx, kind, raw_len, count, crc, first_path, data) VALUES (?,?,?,?,?,?,?,?)")
                .map_err(err)?;
            let mut idx = 0i64;
            let kind = if base.is_some() { 1 } else { 0 };
            blocks_iter(&mut |b: blocks::Block| {
                let mut fp = Vec::new();
                blocks::encode_path(&b.first_path, &mut fp);
                st.execute(params![id, idx, kind, b.raw_len, b.count, b.crc, fp, b.data]).map_err(err)?;
                idx += 1;
                Ok(())
            })?
        };
        tx.execute("INSERT INTO snaps(id, base) VALUES (?, ?)", params![id, base]).map_err(err)?;
        tx.commit().map_err(err)?;
        self.checkpoint()?;
        Ok(n)
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<Record> {
    Ok(Record {
        depth: row.get::<_, i64>(0)? as u16,
        name: diskdrift_core::native::NativeName::unix(row.get::<_, Vec<u8>>(1)?),
        kind: match row.get::<_, i64>(2)? {
            0 => Kind::File,
            1 => Kind::Dir,
            2 => Kind::Symlink,
            _ => Kind::Other,
        },
        flags: row.get::<_, i64>(3)? as u8,
        size: row.get::<_, i64>(4)? as u64,
        alloc: row.get::<_, i64>(5)? as u64,
        mtime: row.get(6)?,
        mtime_nanos: 0,
        btime: row.get(7)?,
        btime_nanos: 0,
        ino: row.get::<_, i64>(8)? as u64,
        nlink: row.get::<_, i64>(9)? as u32,
        vol: row.get::<_, i64>(10)? as u16,
        file_id: diskdrift_core::record::FileId { low: row.get::<_, i64>(8)? as u64, high: 0 },
        available: diskdrift_core::record::EntryFields::KIND.union(diskdrift_core::record::EntryFields::LOGICAL_SIZE).union(diskdrift_core::record::EntryFields::ALLOCATED_SIZE).union(diskdrift_core::record::EntryFields::MTIME).union(diskdrift_core::record::EntryFields::FILE_ID).union(diskdrift_core::record::EntryFields::LINK_COUNT),
    })
}

/// Reads rows in primary-key chunks so no statement outlives the iterator call.
struct RowChunks<'a> {
    conn: &'a Connection,
    snap: u32,
    next_seq: i64,
    end_seq: i64,
    buf: std::vec::IntoIter<Record>,
    done: bool,
}

impl Iterator for RowChunks<'_> {
    type Item = io::Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(r) = self.buf.next() {
                return Some(Ok(r));
            }
            if self.done {
                return None;
            }
            let res = (|| {
                let mut st = self
                    .conn
                    .prepare_cached(
                        "SELECT depth, name, kind, flags, size, alloc, mtime, btime, ino, nlink, vol, seq
                         FROM entries WHERE snap = ? AND seq >= ? AND seq < ? ORDER BY seq LIMIT ?",
                    )
                    .map_err(err)?;
                let mut rows = st.query(params![self.snap, self.next_seq, self.end_seq, CHUNK]).map_err(err)?;
                let mut v = Vec::with_capacity(CHUNK as usize);
                let mut last = None;
                while let Some(row) = rows.next().map_err(err)? {
                    v.push(row_to_record(row).map_err(err)?);
                    last = Some(row.get::<_, i64>(11).map_err(err)?);
                }
                Ok::<_, io::Error>((v, last))
            })();
            match res {
                Ok((v, last)) => {
                    if (v.len() as i64) < CHUNK {
                        self.done = true;
                    }
                    if let Some(l) = last {
                        self.next_seq = l + 1;
                    }
                    self.buf = v.into_iter();
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

struct BlockChunks<'a, T> {
    conn: &'a Connection,
    snap: u32,
    next_idx: i64,
    cur: std::vec::IntoIter<T>,
    decode: fn(&[u8], u32, &mut Vec<T>) -> io::Result<()>,
    done: bool,
}

impl<T> Iterator for BlockChunks<'_, T> {
    type Item = io::Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(x) = self.cur.next() {
                return Some(Ok(x));
            }
            if self.done {
                return None;
            }
            let res = (|| {
                let mut st = self
                    .conn
                    .prepare_cached("SELECT raw_len, count, crc, data FROM blocks WHERE snap = ? AND idx = ?")
                    .map_err(err)?;
                let mut rows = st.query(params![self.snap, self.next_idx]).map_err(err)?;
                match rows.next().map_err(err)? {
                    None => Ok(None),
                    Some(row) => {
                        let raw_len: i64 = row.get(0).map_err(err)?;
                        let count: i64 = row.get(1).map_err(err)?;
                        let crc: i64 = row.get(2).map_err(err)?;
                        let data: Vec<u8> = row.get(3).map_err(err)?;
                        let raw = blocks::decompress(&data, raw_len as u32, crc as u32)?;
                        let mut out = Vec::with_capacity(count as usize);
                        (self.decode)(&raw, count as u32, &mut out)?;
                        Ok(Some(out))
                    }
                }
            })();
            match res {
                Ok(Some(v)) => {
                    self.next_idx += 1;
                    self.cur = v.into_iter();
                }
                Ok(None) => self.done = true,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

fn decode_records(raw: &[u8], count: u32, out: &mut Vec<Record>) -> io::Result<()> {
    blocks::decode_records(raw, count, out).map(|_| ())
}

impl Store for SqliteStore {
    fn write_full(&mut self, id: u32, records: &mut dyn Iterator<Item = Record>) -> io::Result<u64> {
        if self.mode == Mode::Blocks {
            return self.write_blocks(id, None, &mut |emit| {
                let mut enc = RecordBlockEncoder::new(3);
                let mut n = 0u64;
                for r in &mut *records {
                    n += 1;
                    if let Some(b) = enc.push(&r)? {
                        emit(b)?;
                    }
                }
                if let Some(b) = enc.finish()? {
                    emit(b)?;
                }
                Ok(n)
            });
        }
        let tx = self.conn.transaction().map_err(err)?;
        let mut n = 0u64;
        {
            let mut st = tx
                .prepare(
                    "INSERT INTO entries(snap, seq, parent, depth, name, kind, flags, size, alloc, mtime, btime, ino, nlink, vol)
                     VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                )
                .map_err(err)?;
            let mut parents: Vec<i64> = Vec::new();
            for (seq, r) in records.enumerate() {
                let seq = seq as i64;
                parents.truncate(r.depth as usize);
                let parent = parents.last().copied().unwrap_or(-1);
                st.execute(params![
                    id,
                    seq,
                    parent,
                    r.depth,
                    r.name.as_unix_bytes().unwrap_or_default(),
                    r.kind as u8,
                    r.flags,
                    r.size as i64,
                    r.alloc as i64,
                    r.mtime,
                    r.btime,
                    r.ino as i64,
                    r.nlink,
                    r.vol
                ])
                .map_err(err)?;
                parents.push(seq);
                n += 1;
            }
        }
        tx.execute("INSERT INTO snaps(id, base) VALUES (?, NULL)", params![id]).map_err(err)?;
        tx.commit().map_err(err)?;
        self.checkpoint()?;
        Ok(n)
    }

    fn write_delta(&mut self, id: u32, base: u32, ops: &mut dyn Iterator<Item = DeltaOp>) -> io::Result<u64> {
        if self.mode == Mode::Blocks {
            return self.write_blocks(id, Some(base), &mut |emit| {
                let mut enc = OpBlockEncoder::new(3);
                let mut n = 0u64;
                for op in &mut *ops {
                    n += 1;
                    if let Some(b) = enc.push(&op)? {
                        emit(b)?;
                    }
                }
                if let Some(b) = enc.finish()? {
                    emit(b)?;
                }
                Ok(n)
            });
        }
        let tx = self.conn.transaction().map_err(err)?;
        let mut n = 0u64;
        {
            let mut st = tx.prepare("INSERT INTO ops(snap, seq, op) VALUES (?,?,?)").map_err(err)?;
            let mut enc = OpEncoder::new();
            let mut buf = Vec::new();
            for (seq, op) in ops.enumerate() {
                buf.clear();
                enc.encode(&op, &mut buf)?;
                st.execute(params![id, seq as i64, buf]).map_err(err)?;
                n += 1;
            }
        }
        tx.execute("INSERT INTO snaps(id, base) VALUES (?, ?)", params![id, base]).map_err(err)?;
        tx.commit().map_err(err)?;
        self.checkpoint()?;
        Ok(n)
    }

    fn read_full(&self, id: u32) -> io::Result<RecordStream<'_>> {
        if self.mode == Mode::Blocks {
            return Ok(Box::new(BlockChunks {
                conn: &self.conn,
                snap: id,
                next_idx: 0,
                cur: Vec::new().into_iter(),
                decode: decode_records,
                done: false,
            }));
        }
        Ok(Box::new(RowChunks {
            conn: &self.conn,
            snap: id,
            next_seq: 0,
            end_seq: i64::MAX,
            buf: Vec::new().into_iter(),
            done: false,
        }))
    }

    fn read_delta(&self, id: u32) -> io::Result<Ops<'_>> {
        if self.mode == Mode::Blocks {
            return Ok(Box::new(BlockChunks {
                conn: &self.conn,
                snap: id,
                next_idx: 0,
                cur: Vec::new().into_iter(),
                decode: blocks::decode_ops,
                done: false,
            }));
        }
        let mut st = self.conn.prepare("SELECT op FROM ops WHERE snap = ? ORDER BY seq").map_err(err)?;
        let blobs: Vec<Vec<u8>> =
            st.query_map(params![id], |r| r.get(0)).map_err(err)?.collect::<Result<_, _>>().map_err(err)?;
        let mut dec = OpDecoder::new();
        Ok(Box::new(blobs.into_iter().map(move |b| dec.decode(&mut b.as_slice()))))
    }

    fn list(&self) -> io::Result<Vec<SnapInfo>> {
        let mut st = self.conn.prepare("SELECT id, base FROM snaps ORDER BY id").map_err(err)?;
        let v = st
            .query_map([], |r| Ok(SnapInfo { id: r.get::<_, i64>(0)? as u32, base: r.get::<_, Option<i64>>(1)?.map(|b| b as u32) }))
            .map_err(err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(err)?;
        Ok(v)
    }

    fn read_prefix(&self, id: u32, prefix: &[Vec<u8>]) -> io::Result<RecordStream<'_>> {
        if self.mode != Mode::Indexed {
            return Ok(Box::new(PrefixFilter::new(self.read_full(id)?, prefix.to_vec())));
        }
        // Walk down from the root by (parent, name), then stream the subtree range.
        let mut seq: i64 = 0;
        let mut depth: i64 = 0;
        for comp in &prefix[1..] {
            let found: Option<i64> = self
                .conn
                .query_row(
                    "SELECT seq FROM entries WHERE snap = ? AND parent = ? AND name = ?",
                    params![id, seq, comp],
                    |r| r.get(0),
                )
                .map(Some)
                .or_else(|e| if matches!(e, rusqlite::Error::QueryReturnedNoRows) { Ok(None) } else { Err(e) })
                .map_err(err)?;
            match found {
                Some(s) => {
                    seq = s;
                    depth += 1;
                }
                None => return Ok(Box::new(std::iter::empty())),
            }
        }
        // End of the subtree: first later entry at the same or a shallower depth.
        let end: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MIN(seq), 9223372036854775807) FROM entries WHERE snap = ? AND seq > ? AND depth <= ?",
                params![id, seq, depth],
                |r| r.get(0),
            )
            .map_err(err)?;
        let _ = PathStack::new();
        Ok(Box::new(RowChunks {
            conn: &self.conn,
            snap: id,
            next_seq: seq,
            end_seq: end,
            buf: Vec::new().into_iter(),
            done: false,
        }))
    }

    fn native_top_files(&self, id: u32, n: usize) -> Option<io::Result<Vec<u64>>> {
        if self.mode != Mode::Indexed {
            return None;
        }
        Some((|| {
            let mut st = self
                .conn
                .prepare("SELECT alloc FROM entries WHERE snap = ? AND kind != 1 ORDER BY alloc DESC LIMIT ?")
                .map_err(err)?;
            let v = st
                .query_map(params![id, n as i64], |r| r.get::<_, i64>(0).map(|x| x as u64))
                .map_err(err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(err)?;
            Ok(v)
        })())
    }
}
