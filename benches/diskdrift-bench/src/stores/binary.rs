//! Binary candidate: one file per snapshot, zstd blocks, block index at the end.
//!
//! Layout: `DDS1` + kind byte + 3 reserved, compressed blocks, index (per block:
//! offset, sizes, count, crc, first path), trailer (index offset/len/crc, total
//! count, `DDSE`). Written to `<id>.tmp`, fsynced, renamed: a crash never leaves
//! a half snapshot that looks complete. A small `<id>.meta` (written after the
//! rename) makes it visible to `list`.

use super::blocks::{self, Block, OpBlockEncoder, RecordBlockEncoder};
use super::{Ops, PrefixFilter, SnapInfo, Store};
use diskdrift_core::delta::DeltaOp;
use diskdrift_core::diff::RecordStream;
use diskdrift_core::paths::compare_paths;
use diskdrift_core::record::{Record, get_varint, put_varint};
use std::fs::File;
use std::io::{self, BufWriter, Write};
#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 4] = b"DDS1";
const END: &[u8; 4] = b"DDSE";
const TRAILER: usize = 8 + 4 + 4 + 8 + 4;
const KIND_RECORDS: u8 = 0;
const KIND_OPS: u8 = 1;
const KIND_COLUMNAR: u8 = 2;

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    file.read_exact_at(buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = file.seek_read(buf, offset)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short file read",
            ));
        }
        offset += n as u64;
        buf = &mut buf[n..];
    }
    Ok(())
}

pub struct BinaryStore {
    dir: PathBuf,
    level: i32,
    columnar: bool,
}

#[derive(Debug, Clone)]
struct IndexEntry {
    offset: u64,
    comp_len: u32,
    raw_len: u32,
    count: u32,
    crc: u32,
    first_path: Vec<Vec<u8>>,
}

struct FileWriter {
    out: BufWriter<File>,
    tmp: PathBuf,
    offset: u64,
    index: Vec<IndexEntry>,
    total: u64,
}

impl FileWriter {
    fn create(tmp: PathBuf, kind: u8) -> io::Result<Self> {
        let f = File::create(&tmp)?;
        let mut out = BufWriter::with_capacity(1 << 20, f);
        out.write_all(MAGIC)?;
        out.write_all(&[kind, 0, 0, 0])?;
        Ok(FileWriter {
            out,
            tmp,
            offset: 8,
            index: Vec::new(),
            total: 0,
        })
    }

    fn block(&mut self, b: Block) -> io::Result<()> {
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
        self.total += b.count as u64;
        Ok(())
    }

    fn finish(mut self, dest: &Path) -> io::Result<u64> {
        let mut idx = Vec::new();
        put_varint(&mut idx, self.index.len() as u64);
        for e in &self.index {
            put_varint(&mut idx, e.offset);
            put_varint(&mut idx, e.comp_len as u64);
            put_varint(&mut idx, e.raw_len as u64);
            put_varint(&mut idx, e.count as u64);
            idx.extend_from_slice(&e.crc.to_le_bytes());
            blocks::encode_path(&e.first_path, &mut idx);
        }
        self.out.write_all(&idx)?;
        let mut t = Vec::with_capacity(TRAILER);
        t.extend_from_slice(&self.offset.to_le_bytes());
        t.extend_from_slice(&(idx.len() as u32).to_le_bytes());
        t.extend_from_slice(&crc32fast::hash(&idx).to_le_bytes());
        t.extend_from_slice(&self.total.to_le_bytes());
        t.extend_from_slice(END);
        self.out.write_all(&t)?;
        let f = self.out.into_inner().map_err(|e| e.into_error())?;
        f.sync_all()?;
        std::fs::rename(&self.tmp, dest)?;
        Ok(self.total)
    }
}

fn read_index(f: &File) -> io::Result<Vec<IndexEntry>> {
    let len = f.metadata()?.len();
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    if len < (8 + TRAILER) as u64 {
        return Err(bad("snapshot file too short"));
    }
    let mut t = [0u8; TRAILER];
    read_exact_at(f, &mut t, len - TRAILER as u64)?;
    if &t[TRAILER - 4..] != END {
        return Err(bad("snapshot file has no valid trailer"));
    }
    let idx_off = u64::from_le_bytes(t[0..8].try_into().unwrap());
    let idx_len = u32::from_le_bytes(t[8..12].try_into().unwrap()) as usize;
    let idx_crc = u32::from_le_bytes(t[12..16].try_into().unwrap());
    let mut idx = vec![0u8; idx_len];
    read_exact_at(f, &mut idx, idx_off)?;
    if crc32fast::hash(&idx) != idx_crc {
        return Err(bad("snapshot index checksum mismatch"));
    }
    let mut s = idx.as_slice();
    let n = get_varint(&mut s)? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let offset = get_varint(&mut s)?;
        let comp_len = get_varint(&mut s)? as u32;
        let raw_len = get_varint(&mut s)? as u32;
        let count = get_varint(&mut s)? as u32;
        if s.len() < 4 {
            return Err(bad("truncated index"));
        }
        let crc = u32::from_le_bytes(s[..4].try_into().unwrap());
        s = &s[4..];
        let first_path = blocks::decode_path(&mut s)?;
        out.push(IndexEntry {
            offset,
            comp_len,
            raw_len,
            count,
            crc,
            first_path,
        });
    }
    Ok(out)
}

/// Streams decoded items, decompressing blocks on a background thread.
struct Pipelined<T: Send + 'static> {
    rx: crossbeam_channel::Receiver<io::Result<Vec<T>>>,
    cur: std::vec::IntoIter<T>,
    failed: bool,
}

impl<T: Send + 'static> Pipelined<T> {
    fn spawn(
        f: File,
        index: Vec<IndexEntry>,
        decode: fn(&[u8], u32, &mut Vec<T>) -> io::Result<()>,
    ) -> Self {
        let (tx, rx) = crossbeam_channel::bounded(4);
        std::thread::spawn(move || {
            let mut comp = Vec::new();
            for e in index {
                let r = (|| {
                    comp.resize(e.comp_len as usize, 0);
                    read_exact_at(&f, &mut comp, e.offset)?;
                    let raw = blocks::decompress(&comp, e.raw_len, e.crc)?;
                    let mut out = Vec::with_capacity(e.count as usize);
                    decode(&raw, e.count, &mut out)?;
                    Ok(out)
                })();
                let failed = r.is_err();
                if tx.send(r).is_err() || failed {
                    return;
                }
            }
        });
        Pipelined {
            rx,
            cur: Vec::new().into_iter(),
            failed: false,
        }
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

fn decode_records(raw: &[u8], count: u32, out: &mut Vec<Record>) -> io::Result<()> {
    blocks::decode_records(raw, count, out).map(|_| ())
}

fn file_kind(f: &File) -> io::Result<u8> {
    let mut h = [0u8; 8];
    read_exact_at(f, &mut h, 0)?;
    if &h[..4] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a snapshot file",
        ));
    }
    Ok(h[4])
}

impl BinaryStore {
    pub fn open(dir: &Path, level: i32, columnar: bool) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(BinaryStore {
            dir: dir.to_path_buf(),
            level,
            columnar,
        })
    }

    fn data_path(&self, id: u32) -> PathBuf {
        self.dir.join(format!("{id:08}.dds"))
    }

    fn write_meta(&self, info: SnapInfo) -> io::Result<()> {
        let tmp = self.dir.join(format!("{:08}.meta.tmp", info.id));
        let body = match info.base {
            Some(b) => format!("{{\"id\":{},\"base\":{b}}}", info.id),
            None => format!("{{\"id\":{},\"base\":null}}", info.id),
        };
        let mut f = File::create(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, self.dir.join(format!("{:08}.meta", info.id)))?;
        File::open(&self.dir)?.sync_all()
    }
}

impl Store for BinaryStore {
    fn write_full(
        &mut self,
        id: u32,
        records: &mut dyn Iterator<Item = Record>,
    ) -> io::Result<u64> {
        let tmp = self.dir.join(format!("{id:08}.tmp"));
        let result = (|| {
            if self.columnar {
                let mut w = FileWriter::create(tmp.clone(), KIND_COLUMNAR)?;
                let mut enc = blocks::columnar::Encoder::new(self.level);
                for r in records {
                    if let Some(b) = enc.push(&r)? {
                        w.block(b)?;
                    }
                }
                if let Some(b) = enc.finish()? {
                    w.block(b)?;
                }
                return w.finish(&self.data_path(id));
            }
            let mut w = FileWriter::create(tmp.clone(), KIND_RECORDS)?;
            let mut enc = RecordBlockEncoder::new(self.level);
            for r in records {
                if let Some(b) = enc.push(&r)? {
                    w.block(b)?;
                }
            }
            if let Some(b) = enc.finish()? {
                w.block(b)?;
            }
            w.finish(&self.data_path(id))
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        let n = result?;
        self.write_meta(SnapInfo { id, base: None })?;
        Ok(n)
    }

    fn write_delta(
        &mut self,
        id: u32,
        base: u32,
        ops: &mut dyn Iterator<Item = DeltaOp>,
    ) -> io::Result<u64> {
        let tmp = self.dir.join(format!("{id:08}.tmp"));
        let result = (|| {
            let mut w = FileWriter::create(tmp.clone(), KIND_OPS)?;
            let mut enc = OpBlockEncoder::new(self.level);
            for op in ops {
                if let Some(b) = enc.push(&op)? {
                    w.block(b)?;
                }
            }
            if let Some(b) = enc.finish()? {
                w.block(b)?;
            }
            w.finish(&self.data_path(id))
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        let n = result?;
        self.write_meta(SnapInfo {
            id,
            base: Some(base),
        })?;
        Ok(n)
    }

    fn read_full(&self, id: u32) -> io::Result<RecordStream<'_>> {
        let f = File::open(self.data_path(id))?;
        let kind = file_kind(&f)?;
        let index = read_index(&f)?;
        let decode = if kind == KIND_COLUMNAR {
            blocks::columnar::decode
        } else {
            decode_records
        };
        Ok(Box::new(Pipelined::spawn(f, index, decode)))
    }

    fn read_delta(&self, id: u32) -> io::Result<Ops<'_>> {
        let f = File::open(self.data_path(id))?;
        let index = read_index(&f)?;
        Ok(Box::new(Pipelined::spawn(f, index, blocks::decode_ops)))
    }

    fn list(&self) -> io::Result<Vec<SnapInfo>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(&self.dir)? {
            let p = e?.path();
            if p.extension().is_some_and(|x| x == "meta") {
                let text = std::fs::read_to_string(&p)?;
                let v: serde_json::Value = serde_json::from_str(&text)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                let id = v["id"].as_u64().unwrap_or(0) as u32;
                if self.data_path(id).exists() {
                    out.push(SnapInfo {
                        id,
                        base: v["base"].as_u64().map(|b| b as u32),
                    });
                }
            }
        }
        out.sort_by_key(|i| i.id);
        Ok(out)
    }

    fn read_prefix(&self, id: u32, prefix: &[Vec<u8>]) -> io::Result<RecordStream<'_>> {
        let f = File::open(self.data_path(id))?;
        let kind = file_kind(&f)?;
        let index = read_index(&f)?;
        // Last block whose first path is <= prefix: the prefix starts in it (or later).
        let start = index.partition_point(|e| {
            compare_paths(&e.first_path, prefix) != std::cmp::Ordering::Greater
        });
        let start = start.saturating_sub(1);
        let tail: Vec<IndexEntry> = index[start..].to_vec();
        let ctx = match tail.first() {
            Some(e) => {
                let mut comp = vec![0u8; e.comp_len as usize];
                read_exact_at(&f, &mut comp, e.offset)?;
                let raw = blocks::decompress(&comp, e.raw_len, e.crc)?;
                if kind == KIND_COLUMNAR {
                    blocks::columnar::context(&raw)?
                } else {
                    blocks::decode_records(&raw, e.count, &mut Vec::new())?
                }
            }
            None => Vec::new(),
        };
        let decode = if kind == KIND_COLUMNAR {
            blocks::columnar::decode
        } else {
            decode_records
        };
        let stream: RecordStream<'_> = Box::new(Pipelined::spawn(f, tail, decode));
        Ok(Box::new(PrefixFilter::with_context(
            stream,
            prefix.to_vec(),
            &ctx,
        )))
    }
}
