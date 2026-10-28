//! Snapshot data files: header, compressed blocks, block index, trailer.
//!
//! ```text
//! "DDS1" kind:u8 0 0 0          header (8 bytes)
//! block*                        zstd blocks
//! index                         per block: offset, comp_len, raw_len, count, crc32, first path
//! offset:u64 len:u32 crc:u32 items:u64 "DDSE"   trailer (28 bytes)
//! ```
//! Files are written under a temporary name, fsynced and renamed: a data file
//! with a valid trailer is always complete.

use crate::codec::{self, Block};
use diskdrift_core::delta::DeltaOp;
use diskdrift_core::record::{Record, get_varint, put_varint};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = file.seek_read(&mut buf[done..], offset + done as u64)?;
        if n == 0 { return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short positional read")); }
        done += n;
    }
    Ok(())
}

const MAGIC: &[u8; 4] = b"DDS1";
const END: &[u8; 4] = b"DDSE";
const TRAILER: usize = 28;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FileKind {
    Records = 2,
    Ops = 1,
}

#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub offset: u64,
    pub comp_len: u32,
    pub raw_len: u32,
    pub count: u32,
    pub crc: u32,
    pub first_path: Vec<Vec<u8>>,
}

pub struct DataWriter {
    out: BufWriter<File>,
    tmp: PathBuf,
    offset: u64,
    index: Vec<IndexEntry>,
    items: u64,
}

impl DataWriter {
    pub fn create(tmp: PathBuf, kind: FileKind) -> io::Result<Self> {
        let f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        let mut out = BufWriter::with_capacity(1 << 20, f);
        out.write_all(MAGIC)?;
        out.write_all(&[kind as u8, 0, 0, 0])?;
        Ok(DataWriter { out, tmp, offset: 8, index: Vec::new(), items: 0 })
    }

    pub fn block(&mut self, b: Block) -> io::Result<()> {
        self.out.write_all(&b.data)?;
        self.index.push(IndexEntry {
            offset: self.offset,
            comp_len: b.data.len() as u32,
            raw_len: b.raw_len,
            count: b.count,
            crc: b.crc,
            first_path: b.first_path,
        });
        self.offset += b.data.len() as u64;
        self.items += b.count as u64;
        Ok(())
    }

    pub fn blocks(&self) -> u64 {
        self.index.len() as u64
    }

    /// Writes index and trailer, fsyncs and renames to `dest`. Returns items written.
    pub fn commit(mut self, dest: &Path) -> io::Result<u64> {
        let mut idx = Vec::new();
        put_varint(&mut idx, self.index.len() as u64);
        for e in &self.index {
            put_varint(&mut idx, e.offset);
            put_varint(&mut idx, e.comp_len as u64);
            put_varint(&mut idx, e.raw_len as u64);
            put_varint(&mut idx, e.count as u64);
            idx.extend_from_slice(&e.crc.to_le_bytes());
            codec::encode_path(&e.first_path, &mut idx);
        }
        self.out.write_all(&idx)?;
        let mut t = Vec::with_capacity(TRAILER);
        t.extend_from_slice(&self.offset.to_le_bytes());
        t.extend_from_slice(&(idx.len() as u32).to_le_bytes());
        t.extend_from_slice(&crc32fast::hash(&idx).to_le_bytes());
        t.extend_from_slice(&self.items.to_le_bytes());
        t.extend_from_slice(END);
        self.out.write_all(&t)?;
        let f = self.out.into_inner().map_err(|e| e.into_error())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&self.tmp, dest)?;
        Ok(self.items)
    }

    /// Removes the temporary file.
    pub fn abort(self) {
        let tmp = self.tmp.clone();
        drop(self.out);
        let _ = std::fs::remove_file(tmp);
    }
}

fn corrupt(path: &Path, m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{}: {m}", path.display()))
}

/// An opened, validated data file.
pub struct DataFile {
    pub path: PathBuf,
    pub kind: FileKind,
    pub index: Vec<IndexEntry>,
    pub items: u64,
    file: File,
}

impl DataFile {
    pub fn open(path: &Path) -> io::Result<DataFile> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        if len < (8 + TRAILER) as u64 {
            return Err(corrupt(path, "file too short"));
        }
        let mut h = [0u8; 8];
        read_at(&file, &mut h, 0)?;
        if &h[..4] != MAGIC {
            return Err(corrupt(path, "not a DiskDrift snapshot file"));
        }
        let kind = match h[4] {
            2 => FileKind::Records,
            1 => FileKind::Ops,
            k => return Err(corrupt(path, &format!("unsupported data kind {k}"))),
        };
        let mut t = [0u8; TRAILER];
        read_at(&file, &mut t, len - TRAILER as u64)?;
        if &t[TRAILER - 4..] != END {
            return Err(corrupt(path, "missing trailer (incomplete write?)"));
        }
        let idx_off = u64::from_le_bytes(t[0..8].try_into().unwrap());
        let idx_len = u32::from_le_bytes(t[8..12].try_into().unwrap()) as usize;
        let idx_crc = u32::from_le_bytes(t[12..16].try_into().unwrap());
        let items = u64::from_le_bytes(t[16..24].try_into().unwrap());
        if idx_off.saturating_add(idx_len as u64) > len {
            return Err(corrupt(path, "index out of bounds"));
        }
        let mut idx = vec![0u8; idx_len];
        read_at(&file, &mut idx, idx_off)?;
        if crc32fast::hash(&idx) != idx_crc {
            return Err(corrupt(path, "index checksum mismatch"));
        }
        let mut s = idx.as_slice();
        let n = get_varint(&mut s)? as usize;
        let mut index = Vec::with_capacity(n.min(1 << 20));
        let mut total = 0u64;
        for _ in 0..n {
            let offset = get_varint(&mut s)?;
            let comp_len = get_varint(&mut s)? as u32;
            let raw_len = get_varint(&mut s)? as u32;
            let count = get_varint(&mut s)? as u32;
            if s.len() < 4 {
                return Err(corrupt(path, "truncated index"));
            }
            let crc = u32::from_le_bytes(s[..4].try_into().unwrap());
            s = &s[4..];
            let first_path = codec::decode_path(&mut s)?;
            if offset + comp_len as u64 > idx_off {
                return Err(corrupt(path, "block out of bounds"));
            }
            total += count as u64;
            index.push(IndexEntry { offset, comp_len, raw_len, count, crc, first_path });
        }
        if total != items {
            return Err(corrupt(path, "item count mismatch"));
        }
        Ok(DataFile { path: path.to_path_buf(), kind, index, items, file })
    }

    fn read_block(&self, e: &IndexEntry) -> io::Result<Vec<u8>> {
        let mut comp = vec![0u8; e.comp_len as usize];
        read_at(&self.file, &mut comp, e.offset)?;
        codec::decompress(&comp, e.raw_len, e.crc).map_err(|err| corrupt(&self.path, &err.to_string()))
    }

    /// Path context (ancestors of the first record) of block `i`.
    pub fn block_context(&self, i: usize) -> io::Result<Vec<Vec<u8>>> {
        codec::records_context(&self.read_block(&self.index[i])?)
    }

    /// Streams records from block `start` on, decoding on a background thread.
    pub fn records_from(self, start: usize) -> io::Result<Pipelined<Record>> {
        if self.kind != FileKind::Records {
            return Err(corrupt(&self.path, "expected a records file"));
        }
        Ok(Pipelined::spawn(self, start, 3, |raw, count, out| codec::decode_records(raw, count, out).map(|_| ())))
    }

    /// Delta ops, decoded on the calling thread: delta files are small, and a
    /// long reconstruction chain must not cost one thread (and heap) per level.
    pub fn ops(self) -> io::Result<InlineOps> {
        if self.kind != FileKind::Ops {
            return Err(corrupt(&self.path, "expected a delta file"));
        }
        Ok(InlineOps { df: self, next_block: 0, cur: Vec::new().into_iter(), failed: false })
    }

    /// Reads and checks every block (integrity verification).
    pub fn verify(&self) -> io::Result<u64> {
        let mut n = 0;
        for e in &self.index {
            let raw = self.read_block(e)?;
            match self.kind {
                FileKind::Records => {
                    let mut v = Vec::new();
                    codec::decode_records(&raw, e.count, &mut v).map_err(|err| corrupt(&self.path, &err.to_string()))?;
                    n += v.len() as u64;
                }
                FileKind::Ops => {
                    let mut v = Vec::new();
                    codec::decode_ops(&raw, e.count, &mut v).map_err(|err| corrupt(&self.path, &err.to_string()))?;
                    n += v.len() as u64;
                }
            }
        }
        Ok(n)
    }
}

/// Items of a data file, decompressed and decoded ahead on a background thread
/// (bounded: at most `ahead` decoded blocks wait in the channel).
pub struct Pipelined<T: Send + 'static> {
    rx: crossbeam_channel::Receiver<io::Result<Vec<T>>>,
    cur: std::vec::IntoIter<T>,
    failed: bool,
}

impl<T: Send + 'static> Pipelined<T> {
    fn spawn(df: DataFile, start: usize, ahead: usize, decode: fn(&[u8], u32, &mut Vec<T>) -> io::Result<()>) -> Self {
        let (tx, rx) = crossbeam_channel::bounded(ahead);
        std::thread::spawn(move || {
            for e in df.index.iter().skip(start) {
                let r = df.read_block(e).and_then(|raw| {
                    let mut out = Vec::with_capacity(e.count as usize);
                    decode(&raw, e.count, &mut out).map_err(|err| corrupt(&df.path, &err.to_string()))?;
                    Ok(out)
                });
                let failed = r.is_err();
                if tx.send(r).is_err() || failed {
                    return;
                }
            }
        });
        Pipelined { rx, cur: Vec::new().into_iter(), failed: false }
    }
}

impl<T: Send + 'static> Iterator for Pipelined<T> {
    type Item = io::Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(x) = self.cur.next() {
                return Some(Ok(x));
            }
            if self.failed {
                return None;
            }
            match self.rx.recv() {
                Ok(Ok(v)) => self.cur = v.into_iter(),
                Ok(Err(e)) => {
                    self.failed = true;
                    return Some(Err(e));
                }
                Err(_) => return None,
            }
        }
    }
}

/// Ops of a delta file, one block decoded at a time on the caller's thread.
pub struct InlineOps {
    df: DataFile,
    next_block: usize,
    cur: std::vec::IntoIter<DeltaOp>,
    failed: bool,
}

impl Iterator for InlineOps {
    type Item = io::Result<DeltaOp>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(op) = self.cur.next() {
                return Some(Ok(op));
            }
            if self.failed || self.next_block >= self.df.index.len() {
                return None;
            }
            let e = &self.df.index[self.next_block];
            self.next_block += 1;
            let r = self.df.read_block(e).and_then(|raw| {
                let mut out = Vec::with_capacity(e.count as usize);
                codec::decode_ops(&raw, e.count, &mut out).map_err(|err| corrupt(&self.df.path, &err.to_string()))?;
                Ok(out)
            });
            match r {
                Ok(v) => self.cur = v.into_iter(),
                Err(err) => {
                    self.failed = true;
                    return Some(Err(err));
                }
            }
        }
    }
}
