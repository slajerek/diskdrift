//! Block codecs (chosen in ADR 0001).
//!
//! Records are stored column by column inside each block, with value transforms
//! that make typical metadata tiny before zstd: allocated size predicted from the
//! logical size, inode/mtime/depth as deltas from the previous entry, birth time
//! relative to mtime. Every block starts with the path context of its first record,
//! so it can be decoded on its own (seeking, parallel decode).
//!
//! Delta ops are stored row-wise with front-coded parent paths; the coding restarts
//! at every block.

use diskdrift_core::delta::{DeltaOp, OpDecoder, OpEncoder};
use diskdrift_core::native::NativeName;
use diskdrift_core::paths::PathStack;
use diskdrift_core::record::{FileId, Kind, Record, get_varint, put_varint, unzigzag, zigzag};
use std::io;

/// Uncompressed bytes collected before a records block is closed.
pub const BLOCK_RAW: usize = 256 * 1024;
/// Delta ops decode into larger in-memory objects (each carries its parent path)
/// and a reconstruction may stack many delta streams: smaller blocks bound memory.
pub const OPS_BLOCK_RAW: usize = 64 * 1024;
/// zstd level: ~4% larger than level 9-12 but 2-3x faster to encode (benchmarks).
pub const ZSTD_LEVEL: i32 = 3;

const STREAMS: usize = 10;

#[derive(Debug, Clone)]
pub struct Block {
    pub data: Vec<u8>,
    pub raw_len: u32,
    pub count: u32,
    pub crc: u32,
    pub first_path: Vec<Vec<u8>>,
}

fn corrupt(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, m.to_string())
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
    let mut v = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        let len = get_varint(s)? as usize;
        if s.len() < len {
            return Err(corrupt("bad path in block"));
        }
        v.push(s[..len].to_vec());
        *s = &s[len..];
    }
    Ok(v)
}

fn compress(raw: &[u8], count: u32, first_path: Vec<Vec<u8>>) -> io::Result<Block> {
    let data = zstd::bulk::compress(raw, ZSTD_LEVEL)?;
    Ok(Block {
        crc: crc32fast::hash(&data),
        data,
        raw_len: raw.len() as u32,
        count,
        first_path,
    })
}

/// Verifies the checksum and decompresses one block.
pub fn decompress(data: &[u8], raw_len: u32, crc: u32) -> io::Result<Vec<u8>> {
    if crc32fast::hash(data) != crc {
        return Err(corrupt("block checksum mismatch"));
    }
    zstd::bulk::decompress(data, raw_len as usize)
        .map_err(|e| corrupt(&format!("block decompression failed: {e}")))
}

fn expected_alloc(size: u64) -> u64 {
    size.div_ceil(4096) * 4096
}

fn normalize_stat_error(record: &mut Record) {
    record.size = 0;
    record.alloc = 0;
    record.mtime = 0;
    record.mtime_nanos = 0;
    record.btime = 0;
    record.btime_nanos = 0;
    record.ino = 0;
    record.file_id = FileId::ZERO;
    record.nlink = 0;
    record.available = diskdrift_core::record::EntryFields::empty();
}

pub struct RecordEncoder {
    cols: [Vec<u8>; STREAMS],
    count: u32,
    stack: PathStack,
    ctx: Vec<u8>,
    first_path: Vec<Vec<u8>>,
    prev: (u16, i64, u64),
    approx: usize,
}

impl Default for RecordEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordEncoder {
    pub fn new() -> Self {
        RecordEncoder {
            cols: Default::default(),
            count: 0,
            stack: PathStack::new(),
            ctx: Vec::new(),
            first_path: Vec::new(),
            prev: (0, 0, 0),
            approx: 0,
        }
    }

    /// Adds a record (canonical order required); returns a block when one fills up.
    pub fn push(&mut self, r: &Record) -> io::Result<Option<Block>> {
        let done = if self.approx >= BLOCK_RAW {
            Some(self.flush()?)
        } else {
            None
        };
        if r.depth as usize > self.stack.components().len() {
            return Err(corrupt("record stream skips a directory level"));
        }
        if self.count == 0 {
            self.ctx.clear();
            encode_path(&self.stack.components()[..r.depth as usize], &mut self.ctx);
            self.prev = (r.depth, 0, 0);
        }
        let name = r
            .name
            .as_unix_bytes()
            .ok_or_else(|| corrupt("DDS1 only supports Unix names"))?;
        self.stack.set(r.depth, name)?;
        if self.count == 0 {
            self.first_path = self.stack.components().to_vec();
        }
        let c = &mut self.cols;
        let before: usize = c.iter().map(Vec::len).sum();
        put_varint(&mut c[0], zigzag(r.depth as i64 - self.prev.0 as i64));
        c[1].push((r.kind as u8) | (r.flags << 2));
        put_varint(&mut c[2], r.name.len() as u64);
        c[3].extend_from_slice(name);
        put_varint(&mut c[4], r.size);
        put_varint(
            &mut c[5],
            zigzag(r.alloc as i64 - expected_alloc(r.size) as i64),
        );
        put_varint(&mut c[6], zigzag(r.mtime.wrapping_sub(self.prev.1)));
        put_varint(&mut c[7], zigzag(r.mtime.wrapping_sub(r.btime)));
        let legacy_id = if r.file_id == FileId::ZERO {
            r.ino
        } else {
            r.file_id.low
        };
        put_varint(
            &mut c[8],
            zigzag((legacy_id as i64).wrapping_sub(self.prev.2 as i64)),
        );
        put_varint(&mut c[9], ((r.nlink as u64) << 16) | r.vol as u64);
        self.prev = (r.depth, r.mtime, legacy_id);
        self.approx += c.iter().map(Vec::len).sum::<usize>() - before;
        self.count += 1;
        Ok(done)
    }

    pub fn finish(&mut self) -> io::Result<Option<Block>> {
        if self.count == 0 {
            Ok(None)
        } else {
            self.flush().map(Some)
        }
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
        let b = compress(&raw, self.count, std::mem::take(&mut self.first_path))?;
        self.count = 0;
        self.approx = 0;
        Ok(b)
    }
}

/// Path context of a records block without decoding its records.
pub fn records_context(raw: &[u8]) -> io::Result<Vec<Vec<u8>>> {
    let mut s = raw;
    let ctx_len = get_varint(&mut s)? as usize;
    let mut ctx = s
        .get(..ctx_len)
        .ok_or_else(|| corrupt("malformed records block"))?;
    decode_path(&mut ctx)
}

/// Decodes a records block into `out`; returns the block's path context.
pub fn decode_records(raw: &[u8], count: u32, out: &mut Vec<Record>) -> io::Result<Vec<Vec<u8>>> {
    let bad = || corrupt("malformed records block");
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
    out.reserve(count as usize);
    for _ in 0..count {
        let depth = u16::try_from(prev.0 as i64 + unzigzag(get_varint(&mut cols[0])?))
            .map_err(|_| bad())?;
        let kf = *cols[1].first().ok_or_else(bad)?;
        cols[1] = &cols[1][1..];
        let nlen = get_varint(&mut cols[2])? as usize;
        let name = names.get(..nlen).ok_or_else(bad)?.to_vec();
        names = &names[nlen..];
        let size = get_varint(&mut cols[4])?;
        let alloc =
            (expected_alloc(size) as i64).wrapping_add(unzigzag(get_varint(&mut cols[5])?)) as u64;
        let mtime = prev.1.wrapping_add(unzigzag(get_varint(&mut cols[6])?));
        let btime = mtime.wrapping_sub(unzigzag(get_varint(&mut cols[7])?));
        let ino = (prev.2 as i64).wrapping_add(unzigzag(get_varint(&mut cols[8])?)) as u64;
        let nv = get_varint(&mut cols[9])?;
        // Keep encoded state separate from the consumer-facing record. A
        // STAT_ERROR payload is sanitized below, but its raw values still
        // seed the following delta-coded record.
        let next_prev = (depth, mtime, ino);
        let available = diskdrift_core::record::EntryFields::KIND
            .union(diskdrift_core::record::EntryFields::LOGICAL_SIZE)
            .union(diskdrift_core::record::EntryFields::ALLOCATED_SIZE)
            .union(diskdrift_core::record::EntryFields::MTIME)
            .union(diskdrift_core::record::EntryFields::LINK_COUNT);
        let stat_error = kf >> 2 & diskdrift_core::record::flags::STAT_ERROR != 0;
        let mut record = Record {
            depth,
            name: NativeName::unix(name),
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
            mtime_nanos: 0,
            btime,
            btime_nanos: 0,
            ino,
            file_id: FileId { low: ino, high: 0 },
            nlink: (nv >> 16) as u32,
            vol: (nv & 0xffff) as u16,
            available,
        };
        if stat_error {
            normalize_stat_error(&mut record);
        } else if btime != 0 {
            // DDS1 uses zero as the unavailable birth-time sentinel and cannot
            // represent the full portable file identity.
            record.available = record
                .available
                .union(diskdrift_core::record::EntryFields::BIRTH_TIME);
        }
        out.push(record);
        prev = next_prev;
    }
    cols[3] = names;
    if cols.iter().any(|c| !c.is_empty()) {
        return Err(bad());
    }
    Ok(ctx_path)
}

pub struct OpEncoderBlocks {
    raw: Vec<u8>,
    count: u32,
    enc: OpEncoder,
    first_path: Vec<Vec<u8>>,
}

impl Default for OpEncoderBlocks {
    fn default() -> Self {
        Self::new()
    }
}

impl OpEncoderBlocks {
    pub fn new() -> Self {
        OpEncoderBlocks {
            raw: Vec::new(),
            count: 0,
            enc: OpEncoder::new(),
            first_path: Vec::new(),
        }
    }

    pub fn push(&mut self, op: &DeltaOp) -> io::Result<Option<Block>> {
        let done = if self.raw.len() >= OPS_BLOCK_RAW {
            Some(self.flush()?)
        } else {
            None
        };
        if self.count == 0 {
            self.first_path = op.parent.clone();
            let name = op
                .record
                .name
                .as_unix_bytes()
                .ok_or_else(|| corrupt("DDS1 only supports Unix names"))?;
            self.first_path.push(name.to_vec());
        }
        self.enc.encode(op, &mut self.raw)?;
        self.count += 1;
        Ok(done)
    }

    pub fn finish(&mut self) -> io::Result<Option<Block>> {
        if self.count == 0 {
            Ok(None)
        } else {
            self.flush().map(Some)
        }
    }

    fn flush(&mut self) -> io::Result<Block> {
        let b = compress(&self.raw, self.count, std::mem::take(&mut self.first_path))?;
        self.raw.clear();
        self.count = 0;
        self.enc = OpEncoder::new();
        Ok(b)
    }
}

pub fn decode_ops(raw: &[u8], count: u32, out: &mut Vec<DeltaOp>) -> io::Result<()> {
    let mut s = raw;
    let mut dec = OpDecoder::new();
    for _ in 0..count {
        out.push(dec.decode(&mut s)?);
    }
    if !s.is_empty() {
        return Err(corrupt("trailing bytes in ops block"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(depth: u16, name: &str, kind: Kind, size: u64) -> Record {
        Record {
            depth,
            name: NativeName::unix(name.as_bytes().to_vec()),
            kind,
            size,
            alloc: expected_alloc(size) + if size == 7 { 8192 } else { 0 },
            mtime: 1_700_000_000 + size as i64,
            btime: 1_600_000_000,
            ino: 1000 + size,
            file_id: FileId {
                low: 1000 + size,
                high: 0,
            },
            nlink: 1,
            ..Default::default()
        }
    }

    #[test]
    fn records_roundtrip_across_blocks() {
        let mut v = vec![rec(0, "/", Kind::Dir, 0)];
        for i in 0..40_000u64 {
            v.push(rec(1, &format!("d{i:06}"), Kind::Dir, 0));
            v.push(rec(2, &format!("f-{i}"), Kind::File, i * 37 % 100_000));
            v.push(rec(2, "odd\u{ff}", Kind::Symlink, 7));
        }
        v[5].flags = diskdrift_core::record::flags::HARDLINK_DUP;
        v[6].btime = 0;
        v[7].ino = u64::MAX;
        v[7].file_id.low = u64::MAX;
        let mut e = RecordEncoder::new();
        let mut blocks = Vec::new();
        for r in &v {
            if let Some(b) = e.push(r).unwrap() {
                blocks.push(b);
            }
        }
        blocks.extend(e.finish().unwrap());
        assert!(blocks.len() > 1, "test must span several blocks");
        let mut out = Vec::new();
        for b in &blocks {
            let raw = decompress(&b.data, b.raw_len, b.crc).unwrap();
            let ctx = decode_records(&raw, b.count, &mut Vec::new()).unwrap();
            assert_eq!(
                ctx.len() + 1,
                b.first_path.len(),
                "context = ancestors of the first record"
            );
            decode_records(&raw, b.count, &mut out).unwrap();
        }
        assert_eq!(out, v);
    }

    #[test]
    fn errored_record_keeps_encoded_state_for_following_record() {
        let mut errored = rec(1, "error", Kind::File, 7);
        errored.flags = diskdrift_core::record::flags::STAT_ERROR;
        errored.mtime = 1_800_000_007;
        errored.btime = 1_700_000_000;
        errored.ino = 77_007;
        errored.file_id.low = errored.ino;
        let normal = rec(1, "normal", Kind::File, 11);

        let mut encoder = RecordEncoder::new();
        encoder.push(&rec(0, "/", Kind::Dir, 0)).unwrap();
        encoder.push(&errored).unwrap();
        encoder.push(&normal).unwrap();
        let block = encoder.finish().unwrap().unwrap();
        let raw = decompress(&block.data, block.raw_len, block.crc).unwrap();
        let mut decoded = Vec::new();
        decode_records(&raw, block.count, &mut decoded).unwrap();

        assert_eq!(
            decoded[1].size, 0,
            "errored records are sanitized for consumers"
        );
        assert_eq!(
            decoded[2], normal,
            "sanitization must not alter the next record"
        );
    }

    #[test]
    fn legacy_inode_is_used_when_file_id_is_zero() {
        let mut legacy = rec(0, "legacy", Kind::File, 7);
        legacy.file_id = FileId::ZERO;
        legacy.ino = 77;

        let mut encoder = RecordEncoder::new();
        encoder.push(&legacy).unwrap();
        let block = encoder.finish().unwrap().unwrap();
        let raw = decompress(&block.data, block.raw_len, block.crc).unwrap();
        let mut decoded = Vec::new();
        decode_records(&raw, block.count, &mut decoded).unwrap();

        assert_eq!(decoded[0].ino, legacy.ino);
        assert_eq!(decoded[0].file_id, FileId { low: legacy.ino, high: 0 });
    }

    #[test]
    fn columnar_stat_error_matches_dds1_sentinels() {
        let mut errored = rec(0, "error", Kind::Dir, 7);
        errored.flags = diskdrift_core::record::flags::STAT_ERROR;
        errored.size = 11;
        errored.alloc = 4096;
        errored.mtime = 12;
        errored.btime = 14;
        errored.ino = 77;
        errored.file_id = FileId { low: 77, high: 9 };
        errored.nlink = 3;

        let mut encoded = Vec::new();
        errored.encode_dds1(&mut encoded).unwrap();
        let mut dds1_input = encoded.as_slice();
        let expected = Record::decode(&mut dds1_input).unwrap();

        let mut encoder = RecordEncoder::new();
        encoder.push(&errored).unwrap();
        let block = encoder.finish().unwrap().unwrap();
        let raw = decompress(&block.data, block.raw_len, block.crc).unwrap();
        let mut decoded = Vec::new();
        decode_records(&raw, block.count, &mut decoded).unwrap();

        assert_eq!(decoded[0], expected);
        assert_eq!(
            decoded[0].available,
            diskdrift_core::record::EntryFields::empty()
        );
        assert_eq!(decoded[0].file_id, FileId::ZERO);
        assert_eq!(decoded[0].ino, 0);
    }

    #[test]
    fn columnar_legacy_availability_matches_dds1_decode() {
        let mut without_birth_time = rec(0, "zero", Kind::File, 7);
        without_birth_time.btime = 0;
        let with_birth_time = rec(0, "observed", Kind::File, 11);
        let records = [without_birth_time, with_birth_time];

        let mut expected = Vec::new();
        for record in &records {
            let mut encoded = Vec::new();
            record.encode_dds1(&mut encoded).unwrap();
            let mut input = encoded.as_slice();
            expected.push(Record::decode(&mut input).unwrap());
        }

        let mut encoder = RecordEncoder::new();
        for record in &records {
            encoder.push(record).unwrap();
        }
        let block = encoder.finish().unwrap().unwrap();
        let raw = decompress(&block.data, block.raw_len, block.crc).unwrap();
        let mut decoded = Vec::new();
        decode_records(&raw, block.count, &mut decoded).unwrap();

        assert_eq!(decoded, expected);
        assert!(!decoded[0]
            .available
            .contains(diskdrift_core::record::EntryFields::BIRTH_TIME));
        assert!(decoded[1]
            .available
            .contains(diskdrift_core::record::EntryFields::BIRTH_TIME));
        assert!(decoded
            .iter()
            .all(|record| !record
                .available
                .contains(diskdrift_core::record::EntryFields::FILE_ID)));
    }

    #[test]
    fn corruption_is_detected() {
        let mut e = RecordEncoder::new();
        e.push(&rec(0, "/", Kind::Dir, 0)).unwrap();
        let mut b = e.finish().unwrap().unwrap();
        b.data[3] ^= 0x40;
        assert!(decompress(&b.data, b.raw_len, b.crc).is_err());
    }
}
