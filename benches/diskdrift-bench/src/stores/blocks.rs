//! Compressed, self-describing blocks of records or delta ops.
//!
//! A records block starts with the path context of its first record (its
//! ancestors), so any block can be decoded on its own: that is what allows
//! seeking to a path without reading the snapshot from the start. An ops block
//! restarts the op front-coding. Each block carries a CRC of its compressed bytes.

use diskdrift_core::delta::{DeltaOp, OpDecoder, OpEncoder};
use diskdrift_core::paths::PathStack;
use diskdrift_core::record::{Record, get_varint, put_varint};
use std::io;

pub const BLOCK_RAW: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct Block {
    pub data: Vec<u8>,
    pub raw_len: u32,
    pub count: u32,
    pub crc: u32,
    /// Full path of the first entry (records) or first op.
    pub first_path: Vec<Vec<u8>>,
}

pub struct RecordBlockEncoder {
    level: i32,
    raw: Vec<u8>,
    count: u32,
    stack: PathStack,
    first_path: Vec<Vec<u8>>,
}

impl RecordBlockEncoder {
    pub fn new(level: i32) -> Self {
        RecordBlockEncoder { level, raw: Vec::with_capacity(BLOCK_RAW + 4096), count: 0, stack: PathStack::new(), first_path: Vec::new() }
    }

    /// Adds a record; returns a finished block when the current one is full.
    pub fn push(&mut self, r: &Record) -> io::Result<Option<Block>> {
        let mut done = None;
        if self.raw.len() >= BLOCK_RAW {
            done = Some(self.flush()?);
        }
        if self.count == 0 {
            // Context: ancestors of this record.
            let ctx = &self.stack.components()[..r.depth as usize];
            put_varint(&mut self.raw, ctx.len() as u64);
            for c in ctx {
                put_varint(&mut self.raw, c.len() as u64);
                self.raw.extend_from_slice(c);
            }
        }
        self.stack.set(r.depth, r.name.as_unix_bytes().unwrap_or_default())?;
        if self.count == 0 {
            self.first_path = self.stack.components().to_vec();
        }
        r.encode_dds1(&mut self.raw)?;
        self.count += 1;
        Ok(done)
    }

    pub fn finish(&mut self) -> io::Result<Option<Block>> {
        if self.count == 0 { Ok(None) } else { self.flush().map(Some) }
    }

    fn flush(&mut self) -> io::Result<Block> {
        let data = zstd::bulk::compress(&self.raw, self.level)?;
        let b = Block {
            crc: crc32fast::hash(&data),
            data,
            raw_len: self.raw.len() as u32,
            count: self.count,
            first_path: std::mem::take(&mut self.first_path),
        };
        self.raw.clear();
        self.count = 0;
        Ok(b)
    }
}

pub struct OpBlockEncoder {
    level: i32,
    raw: Vec<u8>,
    count: u32,
    enc: OpEncoder,
    first_path: Vec<Vec<u8>>,
}

impl OpBlockEncoder {
    pub fn new(level: i32) -> Self {
        OpBlockEncoder { level, raw: Vec::new(), count: 0, enc: OpEncoder::new(), first_path: Vec::new() }
    }

    pub fn push(&mut self, op: &DeltaOp) -> io::Result<Option<Block>> {
        let mut done = None;
        if self.raw.len() >= BLOCK_RAW {
            done = Some(self.flush()?);
        }
        if self.count == 0 {
            self.first_path = op.parent.clone();
            self.first_path.push(op.record.name.as_unix_bytes().unwrap_or_default().to_vec());
        }
        self.enc.encode(op, &mut self.raw)?;
        self.count += 1;
        Ok(done)
    }

    pub fn finish(&mut self) -> io::Result<Option<Block>> {
        if self.count == 0 { Ok(None) } else { self.flush().map(Some) }
    }

    fn flush(&mut self) -> io::Result<Block> {
        let data = zstd::bulk::compress(&self.raw, self.level)?;
        let b = Block {
            crc: crc32fast::hash(&data),
            data,
            raw_len: self.raw.len() as u32,
            count: self.count,
            first_path: std::mem::take(&mut self.first_path),
        };
        self.raw.clear();
        self.count = 0;
        self.enc = OpEncoder::new();
        Ok(b)
    }
}

fn corrupt(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, m.to_string())
}

pub fn decompress(data: &[u8], raw_len: u32, crc: u32) -> io::Result<Vec<u8>> {
    if crc32fast::hash(data) != crc {
        return Err(corrupt("block checksum mismatch"));
    }
    zstd::bulk::decompress(data, raw_len as usize)
}

/// Decodes a records block; returns the context (ancestors of the first record).
pub fn decode_records(raw: &[u8], count: u32, out: &mut Vec<Record>) -> io::Result<Vec<Vec<u8>>> {
    let mut s = raw;
    let n = get_varint(&mut s)? as usize;
    let mut ctx = Vec::with_capacity(n);
    for _ in 0..n {
        let len = get_varint(&mut s)? as usize;
        if s.len() < len {
            return Err(corrupt("bad block context"));
        }
        ctx.push(s[..len].to_vec());
        s = &s[len..];
    }
    for _ in 0..count {
        out.push(Record::decode(&mut s)?);
    }
    if !s.is_empty() {
        return Err(corrupt("trailing bytes in block"));
    }
    Ok(ctx)
}

pub fn decode_ops(raw: &[u8], count: u32, out: &mut Vec<DeltaOp>) -> io::Result<()> {
    let mut s = raw;
    let mut dec = OpDecoder::new();
    for _ in 0..count {
        out.push(dec.decode(&mut s)?);
    }
    Ok(())
}

pub fn encode_path(p: &[Vec<u8>], out: &mut Vec<u8>) {
    put_varint(out, p.len() as u64);
    for c in p {
        put_varint(out, c.len() as u64);
        out.extend_from_slice(c);
    }
}

pub fn decode_path(s: &mut &[u8]) -> io::Result<Vec<Vec<u8>>> {
    let n = get_varint(s)? as usize;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        let len = get_varint(s)? as usize;
        if s.len() < len {
            return Err(corrupt("bad path"));
        }
        v.push(s[..len].to_vec());
        *s = &s[len..];
    }
    Ok(v)
}

/// Columnar variant: each field in its own stream, with value transforms that
/// make typical metadata small before compression (alloc predicted from size,
/// inode/mtime/depth as deltas from the previous entry, btime relative to mtime).
pub mod columnar {
    use super::*;
    use diskdrift_core::record::{Kind, unzigzag, zigzag};

    const STREAMS: usize = 10;

    fn expected_alloc(size: u64) -> u64 {
        size.div_ceil(4096) * 4096
    }

    pub struct Encoder {
        level: i32,
        cols: [Vec<u8>; STREAMS],
        count: u32,
        stack: PathStack,
        ctx: Vec<u8>,
        first_path: Vec<Vec<u8>>,
        prev: (u16, i64, u64),
        approx: usize,
    }

    impl Encoder {
        pub fn new(level: i32) -> Self {
            Encoder {
                level,
                cols: Default::default(),
                count: 0,
                stack: PathStack::new(),
                ctx: Vec::new(),
                first_path: Vec::new(),
                prev: (0, 0, 0),
                approx: 0,
            }
        }

        pub fn push(&mut self, r: &Record) -> io::Result<Option<Block>> {
            let mut done = None;
            if self.approx >= BLOCK_RAW {
                done = Some(self.flush()?);
            }
            if self.count == 0 {
                self.ctx.clear();
                encode_path(&self.stack.components()[..r.depth as usize], &mut self.ctx);
                self.prev = (r.depth, 0, 0);
            }
            self.stack.set(r.depth, r.name.as_unix_bytes().unwrap_or_default())?;
            if self.count == 0 {
                self.first_path = self.stack.components().to_vec();
            }
            let c = &mut self.cols;
            let before: usize = c.iter().map(Vec::len).sum();
            put_varint(&mut c[0], zigzag(r.depth as i64 - self.prev.0 as i64));
            c[1].push((r.kind as u8) | (r.flags << 2));
            put_varint(&mut c[2], r.name.len() as u64);
            c[3].extend_from_slice(r.name.as_unix_bytes().unwrap_or_default());
            put_varint(&mut c[4], r.size);
            put_varint(&mut c[5], zigzag(r.alloc as i64 - expected_alloc(r.size) as i64));
            put_varint(&mut c[6], zigzag(r.mtime - self.prev.1));
            put_varint(&mut c[7], zigzag(r.mtime - r.btime));
            put_varint(&mut c[8], zigzag(r.ino as i64 - self.prev.2 as i64));
            put_varint(&mut c[9], ((r.nlink as u64) << 16) | r.vol as u64);
            self.prev = (r.depth, r.mtime, r.ino);
            self.approx += c.iter().map(Vec::len).sum::<usize>() - before;
            self.count += 1;
            Ok(done)
        }

        pub fn finish(&mut self) -> io::Result<Option<Block>> {
            if self.count == 0 { Ok(None) } else { self.flush().map(Some) }
        }

        fn flush(&mut self) -> io::Result<Block> {
            let mut raw = Vec::with_capacity(self.approx + 64);
            put_varint(&mut raw, self.ctx.len() as u64);
            raw.extend_from_slice(&self.ctx);
            for col in &self.cols {
                put_varint(&mut raw, col.len() as u64);
            }
            for col in &mut self.cols {
                raw.extend_from_slice(col);
                col.clear();
            }
            let data = zstd::bulk::compress(&raw, self.level)?;
            let b = Block {
                crc: crc32fast::hash(&data),
                data,
                raw_len: raw.len() as u32,
                count: self.count,
                first_path: std::mem::take(&mut self.first_path),
            };
            self.count = 0;
            self.approx = 0;
            Ok(b)
        }
    }

    pub fn decode(raw: &[u8], count: u32, out: &mut Vec<Record>) -> io::Result<()> {
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "bad columnar block");
        let mut s = raw;
        let ctx_len = get_varint(&mut s)? as usize;
        let mut ctx = s.get(..ctx_len).ok_or_else(bad)?;
        let ctx_path = decode_path(&mut ctx)?;
        s = &s[ctx_len..];
        let mut lens = [0usize; STREAMS];
        for l in lens.iter_mut() {
            *l = get_varint(&mut s)? as usize;
        }
        let mut cols: Vec<&[u8]> = Vec::with_capacity(STREAMS);
        for l in lens {
            cols.push(s.get(..l).ok_or_else(bad)?);
            s = &s[l..];
        }
        let mut prev = (ctx_path.len() as u16, 0i64, 0u64);
        let mut names = cols[3];
        for _ in 0..count {
            let depth = (prev.0 as i64 + unzigzag(get_varint(&mut cols[0])?)) as u16;
            let kf = *cols[1].first().ok_or_else(bad)?;
            cols[1] = &cols[1][1..];
            let nlen = get_varint(&mut cols[2])? as usize;
            let name = names.get(..nlen).ok_or_else(bad)?.to_vec();
            names = &names[nlen..];
            let size = get_varint(&mut cols[4])?;
            let alloc = (expected_alloc(size) as i64 + unzigzag(get_varint(&mut cols[5])?)) as u64;
            let mtime = prev.1 + unzigzag(get_varint(&mut cols[6])?);
            let btime = mtime - unzigzag(get_varint(&mut cols[7])?);
            let ino = (prev.2 as i64 + unzigzag(get_varint(&mut cols[8])?)) as u64;
            let nv = get_varint(&mut cols[9])?;
            out.push(Record {
                depth,
                name: diskdrift_core::native::NativeName::unix(name),
                kind: match kf & 3 {
                    0 => Kind::File,
                    1 => Kind::Dir,
                    2 => Kind::Symlink,
                    _ => Kind::Other,
                },
                flags: kf >> 2,
                size,
                alloc,
                mtime,
                btime,
                ino,
                nlink: (nv >> 16) as u32,
                vol: (nv & 0xffff) as u16,
                ..Default::default()
            });
            prev = (depth, mtime, ino);
        }
        Ok(())
    }

    /// Context (ancestors of the first record) of a columnar block.
    pub fn context(raw: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        let mut s = raw;
        let ctx_len = get_varint(&mut s)? as usize;
        let mut ctx = s.get(..ctx_len).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad block"))?;
        decode_path(&mut ctx)
    }
}
