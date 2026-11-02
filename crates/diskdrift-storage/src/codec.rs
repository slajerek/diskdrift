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
use diskdrift_core::native::{NativeName, PathEncoding};
use diskdrift_core::paths::{PathStack, compare_paths};
use diskdrift_core::record::{
    EntryFields, FileId, FileIdScheme, FileIdScope, Kind, Record, TimestampResolution, get_varint,
    put_varint, unzigzag, zigzag,
};
use std::io;

use crate::descriptor::StreamDescriptor;
use crate::meta::VolumeDescriptor;

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
    pub first_path: Vec<NativeName>,
}

fn corrupt(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, m.to_string())
}

/// DDS2 uses canonical, bounded LEB128. DDS1 keeps the historical decoder in
/// `diskdrift_core::record` for byte compatibility.
pub(crate) fn get_dds2_varint(input: &mut &[u8]) -> io::Result<u64> {
    let mut value = 0u64;
    for index in 0..10 {
        let byte = *input.first().ok_or_else(|| corrupt("truncated varint"))?;
        *input = &input[1..];
        if index == 9 && (byte & 0x7f) > 1 {
            return Err(corrupt("varint overflow"));
        }
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            if index > 0 && value < (1u64 << (index * 7)) {
                return Err(corrupt("non-canonical varint"));
            }
            return Ok(value);
        }
    }
    Err(corrupt("varint overflow"))
}

fn get_dds2_zigzag(input: &mut &[u8]) -> io::Result<i64> {
    Ok(unzigzag(get_dds2_varint(input)?))
}

pub fn encode_path(p: &[NativeName], out: &mut Vec<u8>) -> io::Result<()> {
    put_varint(out, p.len() as u64);
    for c in p {
        put_varint(out, c.len() as u64);
        let NativeName::Unix(bytes) = c else {
            return Err(corrupt("DDS1 only supports Unix-native path components"));
        };
        out.extend_from_slice(bytes);
    }
    Ok(())
}

pub fn encode_path_for(
    p: &[NativeName],
    encoding: PathEncoding,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    put_varint(out, p.len() as u64);
    for component in p {
        if component.encoding() != encoding {
            return Err(corrupt("descriptor mismatch in native path"));
        }
        put_varint(out, component.len() as u64);
        match component {
            NativeName::Unix(bytes) => out.extend_from_slice(bytes),
            NativeName::Windows(units) => {
                for unit in units {
                    out.extend_from_slice(&unit.to_le_bytes());
                }
            }
        }
    }
    Ok(())
}

pub fn decode_path(s: &mut &[u8]) -> io::Result<Vec<NativeName>> {
    let n = get_varint(s)? as usize;
    let mut v = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        let len = get_varint(s)? as usize;
        if s.len() < len {
            return Err(corrupt("bad path in block"));
        }
        v.push(NativeName::unix(s[..len].to_vec()));
        *s = &s[len..];
    }
    Ok(v)
}

pub(crate) fn decode_path_for(
    s: &mut &[u8],
    encoding: PathEncoding,
    strict: bool,
) -> io::Result<Vec<NativeName>> {
    let get = |s: &mut &[u8]| {
        if strict {
            get_dds2_varint(s)
        } else {
            get_varint(s)
        }
    };
    let n = usize::try_from(get(s)?).map_err(|_| corrupt("path component count overflow"))?;
    let mut path = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        let units =
            usize::try_from(get(s)?).map_err(|_| corrupt("path component length overflow"))?;
        match encoding {
            PathEncoding::UnixBytes => {
                let bytes = s.get(..units).ok_or_else(|| corrupt("malformed path"))?;
                path.push(NativeName::unix(bytes.to_vec()));
                *s = &s[units..];
            }
            PathEncoding::WindowsUtf16le => {
                let byte_len = units
                    .checked_mul(2)
                    .ok_or_else(|| corrupt("path component length overflow"))?;
                let bytes = s
                    .get(..byte_len)
                    .ok_or_else(|| corrupt("malformed UTF-16 path"))?;
                let units = bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u16::from_le_bytes(*pair))
                    .collect();
                path.push(NativeName::windows(units));
                *s = &s[byte_len..];
            }
        }
    }
    Ok(path)
}

fn compress(raw: &[u8], count: u32, first_path: Vec<NativeName>) -> io::Result<Block> {
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
    record.timestamp_resolution = TimestampResolution::Unknown;
    record.file_id_scheme = FileIdScheme::Unavailable;
    record.file_id_scope = FileIdScope::Volume;
    record.available = EntryFields::empty();
}

pub struct RecordEncoder {
    cols: [Vec<u8>; STREAMS],
    dds2_cols: [Vec<u8>; DDS2_COLUMNS],
    descriptor: StreamDescriptor,
    count: u32,
    stack: PathStack,
    ctx: Vec<u8>,
    first_path: Vec<NativeName>,
    prev: (u16, i64, u64),
    dds2_prev_alloc: u64,
    dds2_prev_mtime: i64,
    dds2_prev_mtime_nanos: u32,
    dds2_prev_file_id: FileId,
    dds2_volumes: Option<Vec<VolumeDescriptor>>,
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
            dds2_cols: Default::default(),
            descriptor: StreamDescriptor::dds1(crate::descriptor::FileKind::Records),
            count: 0,
            stack: PathStack::new(),
            ctx: Vec::new(),
            first_path: Vec::new(),
            prev: (0, 0, 0),
            dds2_prev_alloc: 0,
            dds2_prev_mtime: 0,
            dds2_prev_mtime_nanos: 0,
            dds2_prev_file_id: FileId::ZERO,
            dds2_volumes: None,
            approx: 0,
        }
    }

    pub fn new_dds2(descriptor: StreamDescriptor) -> io::Result<Self> {
        Self::new_dds2_inner(descriptor, None)
    }

    pub fn new_dds2_with_volumes(
        descriptor: StreamDescriptor,
        volumes: &[VolumeDescriptor],
    ) -> io::Result<Self> {
        Self::new_dds2_inner(descriptor, Some(volumes))
    }

    fn new_dds2_inner(
        descriptor: StreamDescriptor,
        volumes: Option<&[VolumeDescriptor]>,
    ) -> io::Result<Self> {
        descriptor.validate_dds2()?;
        if descriptor.kind != crate::descriptor::FileKind::Records {
            return Err(corrupt("expected a records descriptor"));
        }
        Ok(Self {
            descriptor,
            dds2_volumes: volumes.map(<[VolumeDescriptor]>::to_vec),
            ..Self::new()
        })
    }

    /// Adds a record (canonical order required); returns a block when one fills up.
    pub fn push(&mut self, r: &Record) -> io::Result<Option<Block>> {
        if self.descriptor.is_dds2() {
            return self.push_dds2(r);
        }
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
            encode_path(&self.stack.components()[..r.depth as usize], &mut self.ctx)?;
            self.prev = (r.depth, 0, 0);
        }
        let NativeName::Unix(name) = &r.name else {
            return Err(corrupt("DDS1 only supports Unix-native names"));
        };
        self.stack.set(r.depth, &r.name)?;
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

    fn push_dds2(&mut self, r: &Record) -> io::Result<Option<Block>> {
        let done = if self.approx >= BLOCK_RAW {
            Some(self.flush()?)
        } else {
            None
        };
        if r.depth as usize > self.stack.components().len() {
            return Err(corrupt("record stream skips a directory level"));
        }
        if r.name.encoding() != self.descriptor.path_encoding {
            return Err(corrupt("descriptor mismatch in native record"));
        }
        if r.flags & !0x3f != 0 {
            return Err(corrupt("record flags exceed schema width"));
        }
        validate_dds2_availability(r.available, self.dds2_volumes.as_deref(), r.vol)?;
        if r.available.contains(EntryFields::MTIME_NANOS) && r.mtime_nanos >= 1_000_000_000
            || r.available.contains(EntryFields::BIRTH_TIME_NANOS) && r.btime_nanos >= 1_000_000_000
        {
            return Err(corrupt("invalid nanoseconds"));
        }

        if self.count == 0 {
            self.ctx.clear();
            encode_path_for(
                &self.stack.components()[..r.depth as usize],
                self.descriptor.path_encoding,
                &mut self.ctx,
            )?;
            self.dds2_prev_alloc = 0;
            self.dds2_prev_mtime = 0;
            self.dds2_prev_mtime_nanos = 0;
            self.dds2_prev_file_id = FileId::ZERO;
            self.prev.0 = self.stack.components().len() as u16;
        }
        let previous_depth = if self.count == 0 {
            r.depth
        } else {
            self.prev.0
        };
        self.stack.set(r.depth, &r.name)?;
        if self.count == 0 {
            self.first_path = self.stack.components().to_vec();
        }

        let available = r.available;
        let encoded_alloc = if available.contains(EntryFields::ALLOCATED_SIZE) {
            r.alloc
        } else {
            self.dds2_prev_alloc
        };
        let encoded_mtime = if available.contains(EntryFields::MTIME) {
            r.mtime
        } else {
            self.dds2_prev_mtime
        };
        let encoded_mtime_nanos = if available.contains(EntryFields::MTIME_NANOS) {
            r.mtime_nanos
        } else {
            self.dds2_prev_mtime_nanos
        };
        let encoded_file_id = if available.contains(EntryFields::FILE_ID) {
            r.file_id
        } else {
            self.dds2_prev_file_id
        };
        let c = &mut self.dds2_cols;
        let before: usize = c.iter().map(Vec::len).sum();
        put_varint(
            &mut c[0],
            zigzag(i64::from(r.depth) - i64::from(previous_depth)),
        );
        c[1].push((r.kind as u8) | (r.flags << 2));
        put_varint(&mut c[2], u64::from(available.0));
        put_varint(&mut c[3], r.name.len() as u64);
        match &r.name {
            NativeName::Unix(bytes) => c[4].extend_from_slice(bytes),
            NativeName::Windows(units) => {
                for unit in units {
                    c[4].extend_from_slice(&unit.to_le_bytes());
                }
            }
        }
        put_varint(
            &mut c[5],
            if available.contains(EntryFields::LOGICAL_SIZE) {
                r.size
            } else {
                0
            },
        );
        put_varint(
            &mut c[6],
            zigzag(encoded_alloc.wrapping_sub(self.dds2_prev_alloc) as i64),
        );
        put_varint(
            &mut c[7],
            zigzag(encoded_mtime.wrapping_sub(self.dds2_prev_mtime)),
        );
        put_varint(
            &mut c[8],
            zigzag(i64::from(encoded_mtime_nanos) - i64::from(self.dds2_prev_mtime_nanos)),
        );
        let encoded_btime = if available.contains(EntryFields::BIRTH_TIME) {
            r.btime
        } else {
            encoded_mtime
        };
        put_varint(&mut c[9], zigzag(encoded_btime.wrapping_sub(encoded_mtime)));
        let encoded_btime_nanos = if available.contains(EntryFields::BIRTH_TIME_NANOS) {
            r.btime_nanos
        } else {
            encoded_mtime_nanos
        };
        put_varint(
            &mut c[10],
            zigzag(i64::from(encoded_btime_nanos) - i64::from(encoded_mtime_nanos)),
        );
        put_varint(
            &mut c[11],
            zigzag(encoded_file_id.low.wrapping_sub(self.dds2_prev_file_id.low) as i64),
        );
        put_varint(
            &mut c[12],
            zigzag(
                encoded_file_id
                    .high
                    .wrapping_sub(self.dds2_prev_file_id.high) as i64,
            ),
        );
        put_varint(
            &mut c[13],
            (if available.contains(EntryFields::LINK_COUNT) {
                u64::from(r.nlink)
            } else {
                0
            } << 16)
                | u64::from(r.vol),
        );
        self.dds2_prev_alloc = encoded_alloc;
        self.dds2_prev_mtime = encoded_mtime;
        self.dds2_prev_mtime_nanos = encoded_mtime_nanos;
        self.dds2_prev_file_id = encoded_file_id;
        self.approx += c.iter().map(Vec::len).sum::<usize>() - before;
        self.prev.0 = r.depth;
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
        let columns: &[Vec<u8>] = if self.descriptor.is_dds2() {
            &self.dds2_cols
        } else {
            &self.cols
        };
        for col in columns {
            put_varint(&mut raw, col.len() as u64);
        }
        if self.descriptor.is_dds2() {
            for col in &mut self.dds2_cols {
                raw.extend_from_slice(col);
                col.clear();
            }
        } else {
            for col in &mut self.cols {
                raw.extend_from_slice(col);
                col.clear();
            }
        }
        let b = compress(&raw, self.count, std::mem::take(&mut self.first_path))?;
        self.count = 0;
        self.approx = 0;
        Ok(b)
    }
}

/// Path context of a records block without decoding its records.
pub fn records_context(raw: &[u8]) -> io::Result<Vec<NativeName>> {
    let mut s = raw;
    let ctx_len = get_varint(&mut s)? as usize;
    let mut ctx = s
        .get(..ctx_len)
        .ok_or_else(|| corrupt("malformed records block"))?;
    decode_path(&mut ctx)
}

/// Decodes a records block into `out`; returns the block's path context.
pub fn decode_records(
    raw: &[u8],
    count: u32,
    out: &mut Vec<Record>,
) -> io::Result<Vec<NativeName>> {
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
        let stat_error = kf >> 2 & diskdrift_core::record::flags::STAT_ERROR != 0;
        let mut available = EntryFields::KIND
            .union(EntryFields::LOGICAL_SIZE)
            .union(EntryFields::ALLOCATED_SIZE)
            .union(EntryFields::MTIME)
            .union(EntryFields::LINK_COUNT);
        if ino != 0 {
            available = available.union(EntryFields::FILE_ID);
        }
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
            timestamp_resolution: if stat_error {
                TimestampResolution::Unknown
            } else {
                TimestampResolution::Seconds
            },
            file_id_scheme: if ino == 0 || stat_error {
                FileIdScheme::Unavailable
            } else {
                FileIdScheme::PosixInode64
            },
            file_id_scope: FileIdScope::Volume,
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

const DDS2_COLUMNS: usize = 14;
const DDS2_FIELDS: u16 = (1 << 9) - 1;

#[cfg(test)]
thread_local! {
    static DDS2_PATH_COPY_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_dds2_path_copy() {
    DDS2_PATH_COPY_COUNT.with(|count| count.set(count.get() + 1));
}

#[cfg(not(test))]
fn note_dds2_path_copy() {}

#[cfg(test)]
fn reset_dds2_path_copy_count() {
    DDS2_PATH_COPY_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
fn dds2_path_copy_count() -> usize {
    DDS2_PATH_COPY_COUNT.with(std::cell::Cell::get)
}

fn validate_dds2_availability_bits(available: EntryFields) -> io::Result<()> {
    if available.0 & !DDS2_FIELDS != 0 {
        return Err(corrupt("reserved availability bits"));
    }
    if available.contains(EntryFields::MTIME_NANOS) && !available.contains(EntryFields::MTIME)
        || available.contains(EntryFields::BIRTH_TIME_NANOS)
            && !available.contains(EntryFields::BIRTH_TIME)
    {
        return Err(corrupt("availability dependency"));
    }
    Ok(())
}

fn validate_dds2_volume(
    available: EntryFields,
    volumes: Option<&[VolumeDescriptor]>,
    vol: u16,
) -> io::Result<()> {
    if let Some(volumes) = volumes {
        let descriptor = volumes
            .get(vol as usize)
            .ok_or_else(|| corrupt("invalid volume index"))?;
        if available.contains(EntryFields::FILE_ID)
            && matches!(descriptor.file_id_scheme, FileIdScheme::Unavailable)
        {
            return Err(corrupt("availability dependency for file identity"));
        }
    }
    Ok(())
}

fn validate_dds2_availability(
    available: EntryFields,
    volumes: Option<&[VolumeDescriptor]>,
    vol: u16,
) -> io::Result<()> {
    validate_dds2_availability_bits(available)?;
    validate_dds2_volume(available, volumes, vol)
}

fn decode_record_name(input: &mut &[u8], encoding: PathEncoding) -> io::Result<NativeName> {
    let length =
        usize::try_from(get_dds2_varint(input)?).map_err(|_| corrupt("name length overflow"))?;
    match encoding {
        PathEncoding::UnixBytes => {
            let bytes = input.get(..length).ok_or_else(|| corrupt("name payload"))?;
            *input = &input[length..];
            Ok(NativeName::unix(bytes.to_vec()))
        }
        PathEncoding::WindowsUtf16le => {
            let byte_len = length
                .checked_mul(2)
                .ok_or_else(|| corrupt("name length overflow"))?;
            let bytes = input
                .get(..byte_len)
                .ok_or_else(|| corrupt("UTF-16 name payload"))?;
            let units = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect();
            *input = &input[byte_len..];
            Ok(NativeName::windows(units))
        }
    }
}

fn dds2_record_scheme(
    descriptor: StreamDescriptor,
    volumes: Option<&[VolumeDescriptor]>,
    vol: u16,
) -> (FileIdScheme, FileIdScope) {
    volumes
        .and_then(|table| table.get(vol as usize))
        .map(|volume| (volume.file_id_scheme, volume.file_id_scope))
        .unwrap_or_else(|| {
            (
                match descriptor.path_encoding {
                    PathEncoding::UnixBytes => FileIdScheme::PosixInode64,
                    PathEncoding::WindowsUtf16le => FileIdScheme::WindowsFileId128,
                },
                FileIdScope::Volume,
            )
        })
}

fn dds2_timestamp_resolution(
    volumes: Option<&[VolumeDescriptor]>,
    vol: u16,
) -> TimestampResolution {
    volumes
        .and_then(|table| table.get(vol as usize))
        .map(|volume| volume.timestamp_resolution)
        .unwrap_or(TimestampResolution::Nanoseconds)
}

/// Decodes one DDS2 schema-1 records block. The returned path is the block
/// context (the ancestors of its first record), matching the DDS1 API.
pub fn decode_dds2_records(
    raw: &[u8],
    count: u32,
    descriptor: StreamDescriptor,
    volumes: Option<&[VolumeDescriptor]>,
    out: &mut Vec<Record>,
) -> io::Result<Vec<NativeName>> {
    decode_dds2_records_internal(raw, count, descriptor, volumes, None, true, Some(out))
        .map(|decoded| decoded.context)
}

/// Decodes records from a data file whose canonical order was verified while
/// opening it. It still validates every value needed to construct `Record`.
pub(crate) fn decode_dds2_records_verified(
    raw: &[u8],
    count: u32,
    descriptor: StreamDescriptor,
    volumes: Option<&[VolumeDescriptor]>,
    out: &mut Vec<Record>,
) -> io::Result<()> {
    decode_dds2_records_internal(raw, count, descriptor, volumes, None, false, Some(out))
        .map(|_| ())
}

pub(crate) struct Dds2RecordValidation {
    pub context: Vec<NativeName>,
    pub first_name: NativeName,
    pub last_path: Vec<NativeName>,
}

pub(crate) fn validate_dds2_records(
    raw: &[u8],
    count: u32,
    descriptor: StreamDescriptor,
    volumes: Option<&[VolumeDescriptor]>,
    previous_path: Option<&[NativeName]>,
) -> io::Result<Dds2RecordValidation> {
    decode_dds2_records_internal(raw, count, descriptor, volumes, previous_path, true, None)
}

fn decode_dds2_records_internal(
    raw: &[u8],
    count: u32,
    descriptor: StreamDescriptor,
    volumes: Option<&[VolumeDescriptor]>,
    previous_path: Option<&[NativeName]>,
    validate_order: bool,
    mut out: Option<&mut Vec<Record>>,
) -> io::Result<Dds2RecordValidation> {
    descriptor.validate_dds2()?;
    if descriptor.kind != crate::descriptor::FileKind::Records {
        return Err(corrupt("expected a records descriptor"));
    }
    if count == 0 {
        return Err(corrupt("malformed records columns"));
    }
    let mut input = raw;
    let context_len = usize::try_from(get_dds2_varint(&mut input)?)
        .map_err(|_| corrupt("records context length overflow"))?;
    let context_bytes = input
        .get(..context_len)
        .ok_or_else(|| corrupt("malformed records context"))?;
    input = &input[context_len..];
    let mut context_input = context_bytes;
    let context = decode_path_for(&mut context_input, descriptor.path_encoding, true)?;
    if !context_input.is_empty() {
        return Err(corrupt("malformed records context"));
    }
    if context.len() > usize::from(u16::MAX) {
        return Err(corrupt("records context depth overflow"));
    }

    let mut lengths = [0usize; DDS2_COLUMNS];
    for length in &mut lengths {
        *length = usize::try_from(get_dds2_varint(&mut input)?)
            .map_err(|_| corrupt("records column length overflow"))?;
    }
    let mut columns = Vec::with_capacity(DDS2_COLUMNS);
    for length in lengths {
        let column = input
            .get(..length)
            .ok_or_else(|| corrupt("malformed records columns"))?;
        columns.push(column);
        input = &input[length..];
    }
    if !input.is_empty() {
        return Err(corrupt("malformed records columns"));
    }

    let mut stack = validate_order.then(PathStack::new);
    if let Some(stack) = &mut stack {
        for (depth, component) in context.iter().enumerate() {
            stack.set(depth as u16, component)?;
        }
    }
    let mut depth_state = context.len() as i64;
    let mut alloc_state = 0u64;
    let mut mtime_state = 0i64;
    let mut mtime_nanos_state = 0u32;
    let mut file_id_state = FileId::ZERO;
    let mut names = columns[4];
    let mut previous_block_path = validate_order.then(|| previous_path.map(<[NativeName]>::to_vec)).flatten();
    let mut previous_depth = None;
    if let Some(out) = &mut out {
        out.reserve(count as usize);
    }
    let mut first_name = None;

    for record_index in 0..count {
        let depth = depth_state
            .checked_add(get_dds2_zigzag(&mut columns[0])?)
            .ok_or_else(|| corrupt("depth overflow"))?;
        let depth = u16::try_from(depth).map_err(|_| corrupt("depth overflow"))?;
        if validate_order && record_index == 0 && depth as usize != context.len() {
            return Err(corrupt("first record depth does not match block context"));
        }
        let kind_flags = *columns[1]
            .first()
            .ok_or_else(|| corrupt("malformed records columns"))?;
        columns[1] = &columns[1][1..];
        let available = EntryFields(
            u16::try_from(get_dds2_varint(&mut columns[2])?)
                .map_err(|_| corrupt("availability overflow"))?,
        );
        validate_dds2_availability_bits(available)?;
        let packed = get_dds2_varint(&mut columns[13])?;
        let nlink = packed >> 16;
        if nlink > u64::from(u32::MAX) {
            return Err(corrupt("narrow packed field"));
        }
        let vol = packed as u16;
        validate_dds2_volume(available, volumes, vol)?;
        let name_length = usize::try_from(get_dds2_varint(&mut columns[3])?)
            .map_err(|_| corrupt("name length overflow"))?;
        let name_bytes = match descriptor.path_encoding {
            PathEncoding::UnixBytes => name_length,
            PathEncoding::WindowsUtf16le => name_length
                .checked_mul(2)
                .ok_or_else(|| corrupt("name length overflow"))?,
        };
        let name_payload = names
            .get(..name_bytes)
            .ok_or_else(|| corrupt("name payload"))?;
        let mut name_input = name_payload;
        let name = match descriptor.path_encoding {
            PathEncoding::UnixBytes => {
                let name = NativeName::unix(name_input.to_vec());
                name_input = &[];
                name
            }
            PathEncoding::WindowsUtf16le => {
                let units = name_input
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u16::from_le_bytes(*pair))
                    .collect();
                name_input = &[];
                NativeName::windows(units)
            }
        };
        if !name_input.is_empty() {
            return Err(corrupt("name payload"));
        }
        names = &names[name_bytes..];
        if record_index == 0 {
            first_name = Some(name.clone());
        }

        let size_encoded = get_dds2_varint(&mut columns[5])?;
        let alloc_encoded = alloc_state.wrapping_add(get_dds2_zigzag(&mut columns[6])? as u64);
        let mtime_encoded = mtime_state.wrapping_add(get_dds2_zigzag(&mut columns[7])?);
        let mtime_nanos_i = (mtime_nanos_state as i64)
            .checked_add(get_dds2_zigzag(&mut columns[8])?)
            .ok_or_else(|| corrupt("invalid nanoseconds"))?;
        if !(0..1_000_000_000).contains(&mtime_nanos_i) {
            return Err(corrupt("invalid nanoseconds"));
        }
        let mtime_nanos_encoded = mtime_nanos_i as u32;
        let btime_encoded = mtime_encoded.wrapping_add(get_dds2_zigzag(&mut columns[9])?);
        let btime_nanos_i = (mtime_nanos_encoded as i64)
            .checked_add(get_dds2_zigzag(&mut columns[10])?)
            .ok_or_else(|| corrupt("invalid nanoseconds"))?;
        if !(0..1_000_000_000).contains(&btime_nanos_i) {
            return Err(corrupt("invalid nanoseconds"));
        }
        let file_id = FileId {
            low: file_id_state
                .low
                .wrapping_add(get_dds2_zigzag(&mut columns[11])? as u64),
            high: file_id_state
                .high
                .wrapping_add(get_dds2_zigzag(&mut columns[12])? as u64),
        };
        if validate_order && record_index > 0 {
            let previous_depth = previous_depth.expect("previous record exists");
            if depth <= previous_depth
                && name <= stack.as_ref().expect("path stack is enabled").components()[depth as usize]
            {
                return Err(corrupt("non-canonical record paths"));
            }
        }
        if let Some(stack) = &mut stack {
            stack.set(depth, &name)?;
        }
        if let Some(previous) = previous_block_path.take()
            && compare_paths(
                &previous,
                stack.as_ref().expect("path stack is enabled").components(),
            )
                .map_err(|error| corrupt(&error.to_string()))?
                != std::cmp::Ordering::Less
        {
            return Err(corrupt("non-canonical record paths"));
        }
        if let Some(out) = &mut out {
            let (scheme, scope) = dds2_record_scheme(descriptor, volumes, vol);
            let kind = match kind_flags & 0b11 {
                0 => Kind::File,
                1 => Kind::Dir,
                2 => Kind::Symlink,
                _ => Kind::Other,
            };
            out.push(Record {
                depth,
                name,
                kind: if available.contains(EntryFields::KIND) {
                    kind
                } else {
                    Kind::File
                },
                flags: kind_flags >> 2,
                size: if available.contains(EntryFields::LOGICAL_SIZE) {
                    size_encoded
                } else {
                    0
                },
                alloc: if available.contains(EntryFields::ALLOCATED_SIZE) {
                    alloc_encoded
                } else {
                    0
                },
                mtime: if available.contains(EntryFields::MTIME) {
                    mtime_encoded
                } else {
                    0
                },
                mtime_nanos: if available.contains(EntryFields::MTIME_NANOS) {
                    mtime_nanos_encoded
                } else {
                    0
                },
                btime: if available.contains(EntryFields::BIRTH_TIME) {
                    btime_encoded
                } else {
                    0
                },
                btime_nanos: if available.contains(EntryFields::BIRTH_TIME_NANOS) {
                    btime_nanos_i as u32
                } else {
                    0
                },
                file_id: if available.contains(EntryFields::FILE_ID) {
                    file_id
                } else {
                    FileId::ZERO
                },
                ino: if available.contains(EntryFields::FILE_ID)
                    && matches!(scheme, FileIdScheme::PosixInode64)
                {
                    file_id.low
                } else {
                    0
                },
                nlink: if available.contains(EntryFields::LINK_COUNT) {
                    nlink as u32
                } else {
                    0
                },
                available,
                vol,
                timestamp_resolution: dds2_timestamp_resolution(volumes, vol),
                file_id_scheme: if available.contains(EntryFields::FILE_ID) {
                    scheme
                } else {
                    FileIdScheme::Unavailable
                },
                file_id_scope: scope,
            });
        }
        if validate_order {
            previous_depth = Some(depth);
        }
        depth_state = i64::from(depth);
        alloc_state = alloc_encoded;
        mtime_state = mtime_encoded;
        mtime_nanos_state = mtime_nanos_encoded;
        file_id_state = file_id;
    }

    if !columns[0].is_empty()
        || !columns[1].is_empty()
        || !columns[2].is_empty()
        || !columns[3].is_empty()
        || !names.is_empty()
        || columns[5..].iter().any(|column| !column.is_empty())
    {
        return Err(corrupt("malformed records columns"));
    }
    let last_path = if let Some(stack) = stack {
        note_dds2_path_copy();
        stack.components().to_vec()
    } else {
        Vec::new()
    };
    Ok(Dds2RecordValidation {
        context,
        first_name: first_name.ok_or_else(|| corrupt("malformed records columns"))?,
        last_path,
    })
}

fn decode_dds2_record_payload(
    input: &mut &[u8],
    descriptor: StreamDescriptor,
    volumes: Option<&[VolumeDescriptor]>,
) -> io::Result<Record> {
    let depth = u16::try_from(get_dds2_varint(input)?).map_err(|_| corrupt("depth overflow"))?;
    let name = decode_record_name(input, descriptor.path_encoding)?;
    let kind_flags = *input.first().ok_or_else(|| corrupt("delta record kind"))?;
    *input = &input[1..];
    let available = EntryFields(
        u16::try_from(get_dds2_varint(input)?).map_err(|_| corrupt("availability overflow"))?,
    );
    validate_dds2_availability_bits(available)?;
    let size_encoded = get_dds2_varint(input)?;
    let alloc_encoded = get_dds2_varint(input)?;
    let mtime_encoded = unzigzag(get_dds2_varint(input)?);
    let mtime_nanos_encoded =
        u32::try_from(get_dds2_varint(input)?).map_err(|_| corrupt("invalid nanoseconds"))?;
    let btime_encoded = unzigzag(get_dds2_varint(input)?);
    let btime_nanos_encoded =
        u32::try_from(get_dds2_varint(input)?).map_err(|_| corrupt("invalid nanoseconds"))?;
    if mtime_nanos_encoded >= 1_000_000_000 || btime_nanos_encoded >= 1_000_000_000 {
        return Err(corrupt("invalid nanoseconds"));
    }
    let file_id = FileId {
        low: get_dds2_varint(input)?,
        high: get_dds2_varint(input)?,
    };
    let packed = get_dds2_varint(input)?;
    let nlink = packed >> 16;
    if nlink > u64::from(u32::MAX) {
        return Err(corrupt("narrow packed field"));
    }
    let vol = packed as u16;
    validate_dds2_volume(available, volumes, vol)?;
    let (scheme, scope) = dds2_record_scheme(descriptor, volumes, vol);
    let kind = match kind_flags & 0b11 {
        0 => Kind::File,
        1 => Kind::Dir,
        2 => Kind::Symlink,
        _ => Kind::Other,
    };
    Ok(Record {
        depth,
        name,
        kind: if available.contains(EntryFields::KIND) {
            kind
        } else {
            Kind::File
        },
        flags: kind_flags >> 2,
        size: if available.contains(EntryFields::LOGICAL_SIZE) {
            size_encoded
        } else {
            0
        },
        alloc: if available.contains(EntryFields::ALLOCATED_SIZE) {
            alloc_encoded
        } else {
            0
        },
        mtime: if available.contains(EntryFields::MTIME) {
            mtime_encoded
        } else {
            0
        },
        mtime_nanos: if available.contains(EntryFields::MTIME_NANOS) {
            mtime_nanos_encoded
        } else {
            0
        },
        btime: if available.contains(EntryFields::BIRTH_TIME) {
            btime_encoded
        } else {
            0
        },
        btime_nanos: if available.contains(EntryFields::BIRTH_TIME_NANOS) {
            btime_nanos_encoded
        } else {
            0
        },
        file_id: if available.contains(EntryFields::FILE_ID) {
            file_id
        } else {
            FileId::ZERO
        },
        ino: if available.contains(EntryFields::FILE_ID)
            && matches!(scheme, FileIdScheme::PosixInode64)
        {
            file_id.low
        } else {
            0
        },
        nlink: if available.contains(EntryFields::LINK_COUNT) {
            nlink as u32
        } else {
            0
        },
        available,
        vol,
        timestamp_resolution: dds2_timestamp_resolution(volumes, vol),
        file_id_scheme: if available.contains(EntryFields::FILE_ID) {
            scheme
        } else {
            FileIdScheme::Unavailable
        },
        file_id_scope: scope,
    })
}

fn decode_dds2_component(input: &mut &[u8], encoding: PathEncoding) -> io::Result<NativeName> {
    decode_record_name(input, encoding)
}

pub fn decode_dds2_ops(
    raw: &[u8],
    count: u32,
    descriptor: StreamDescriptor,
    volumes: Option<&[VolumeDescriptor]>,
    out: &mut Vec<DeltaOp>,
) -> io::Result<()> {
    decode_dds2_ops_with_previous(raw, count, descriptor, volumes, None, out).map(|_| ())
}

pub(crate) fn decode_dds2_ops_with_previous(
    raw: &[u8],
    count: u32,
    descriptor: StreamDescriptor,
    volumes: Option<&[VolumeDescriptor]>,
    previous_path: Option<&[NativeName]>,
    out: &mut Vec<DeltaOp>,
) -> io::Result<Vec<NativeName>> {
    descriptor.validate_dds2()?;
    if descriptor.kind != crate::descriptor::FileKind::Ops {
        return Err(corrupt("expected a delta descriptor"));
    }
    if count == 0 {
        return Err(corrupt("malformed delta block"));
    }
    let mut input = raw;
    let mut previous_parent = Vec::new();
    let mut previous_path = previous_path.map(<[NativeName]>::to_vec);
    for _ in 0..count {
        let kind = match input.first().copied() {
            Some(0) => diskdrift_core::delta::OpKind::Add,
            Some(1) => diskdrift_core::delta::OpKind::Remove,
            Some(2) => diskdrift_core::delta::OpKind::Replace,
            _ => return Err(corrupt("delta op kind")),
        };
        input = &input[1..];
        let shared = usize::try_from(get_dds2_varint(&mut input)?)
            .map_err(|_| corrupt("delta shared prefix overflow"))?;
        let extra = usize::try_from(get_dds2_varint(&mut input)?)
            .map_err(|_| corrupt("delta parent length overflow"))?;
        if shared > previous_parent.len() {
            return Err(corrupt("delta shared prefix"));
        }
        previous_parent.truncate(shared);
        for _ in 0..extra {
            previous_parent.push(decode_dds2_component(&mut input, descriptor.path_encoding)?);
        }
        if previous_parent.len() > usize::from(u16::MAX) {
            return Err(corrupt("delta parent depth overflow"));
        }
        let record = if kind == diskdrift_core::delta::OpKind::Remove {
            let name = decode_dds2_component(&mut input, descriptor.path_encoding)?;
            Record {
                depth: previous_parent.len() as u16,
                name,
                ..Default::default()
            }
        } else {
            decode_dds2_record_payload(&mut input, descriptor, volumes)?
        };
        if record.depth as usize != previous_parent.len()
            || record.name.encoding() != descriptor.path_encoding
        {
            return Err(corrupt("delta record path descriptor mismatch"));
        }
        let mut path = previous_parent.clone();
        path.push(record.name.clone());
        if let Some(previous) = previous_path.as_deref()
            && compare_paths(previous, &path).map_err(|error| corrupt(&error.to_string()))?
                != std::cmp::Ordering::Less
        {
            return Err(corrupt("non-canonical delta paths"));
        }
        previous_path = Some(path);
        out.push(DeltaOp {
            kind,
            parent: previous_parent.clone(),
            record,
        });
    }
    if !input.is_empty() {
        return Err(corrupt("trailing bytes in delta block"));
    }
    previous_path.ok_or_else(|| corrupt("malformed delta block"))
}

fn encode_dds2_name(
    name: &NativeName,
    encoding: PathEncoding,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    if name.encoding() != encoding {
        return Err(corrupt("descriptor mismatch in native name"));
    }
    put_varint(out, name.len() as u64);
    match name {
        NativeName::Unix(bytes) => out.extend_from_slice(bytes),
        NativeName::Windows(units) => {
            for unit in units {
                out.extend_from_slice(&unit.to_le_bytes());
            }
        }
    }
    Ok(())
}

fn encode_dds2_record_payload(
    record: &Record,
    descriptor: StreamDescriptor,
    volumes: Option<&[VolumeDescriptor]>,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    if record.name.encoding() != descriptor.path_encoding || record.flags & !0x3f != 0 {
        return Err(corrupt("descriptor mismatch in delta record"));
    }
    validate_dds2_availability(record.available, volumes, record.vol)?;
    if record.available.contains(EntryFields::MTIME_NANOS) && record.mtime_nanos >= 1_000_000_000
        || record.available.contains(EntryFields::BIRTH_TIME_NANOS)
            && record.btime_nanos >= 1_000_000_000
    {
        return Err(corrupt("invalid nanoseconds"));
    }
    put_varint(out, u64::from(record.depth));
    encode_dds2_name(&record.name, descriptor.path_encoding, out)?;
    out.push((record.kind as u8) | (record.flags << 2));
    put_varint(out, u64::from(record.available.0));
    put_varint(
        out,
        if record.available.contains(EntryFields::LOGICAL_SIZE) {
            record.size
        } else {
            0
        },
    );
    put_varint(
        out,
        if record.available.contains(EntryFields::ALLOCATED_SIZE) {
            record.alloc
        } else {
            0
        },
    );
    put_varint(
        out,
        zigzag(if record.available.contains(EntryFields::MTIME) {
            record.mtime
        } else {
            0
        }),
    );
    put_varint(
        out,
        if record.available.contains(EntryFields::MTIME_NANOS) {
            u64::from(record.mtime_nanos)
        } else {
            0
        },
    );
    put_varint(
        out,
        zigzag(if record.available.contains(EntryFields::BIRTH_TIME) {
            record.btime
        } else {
            0
        }),
    );
    put_varint(
        out,
        if record.available.contains(EntryFields::BIRTH_TIME_NANOS) {
            u64::from(record.btime_nanos)
        } else {
            0
        },
    );
    put_varint(
        out,
        if record.available.contains(EntryFields::FILE_ID) {
            record.file_id.low
        } else {
            0
        },
    );
    put_varint(
        out,
        if record.available.contains(EntryFields::FILE_ID) {
            record.file_id.high
        } else {
            0
        },
    );
    put_varint(
        out,
        (if record.available.contains(EntryFields::LINK_COUNT) {
            u64::from(record.nlink)
        } else {
            0
        } << 16)
            | u64::from(record.vol),
    );
    Ok(())
}

pub struct OpEncoderBlocks {
    raw: Vec<u8>,
    count: u32,
    enc: OpEncoder,
    descriptor: StreamDescriptor,
    dds2_volumes: Option<Vec<VolumeDescriptor>>,
    dds2_prev_parent: Vec<NativeName>,
    first_path: Vec<NativeName>,
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
            descriptor: StreamDescriptor::dds1(crate::descriptor::FileKind::Ops),
            dds2_volumes: None,
            dds2_prev_parent: Vec::new(),
            first_path: Vec::new(),
        }
    }

    pub fn new_dds2(descriptor: StreamDescriptor) -> io::Result<Self> {
        Self::new_dds2_inner(descriptor, None)
    }

    pub fn new_dds2_with_volumes(
        descriptor: StreamDescriptor,
        volumes: &[VolumeDescriptor],
    ) -> io::Result<Self> {
        Self::new_dds2_inner(descriptor, Some(volumes))
    }

    fn new_dds2_inner(
        descriptor: StreamDescriptor,
        volumes: Option<&[VolumeDescriptor]>,
    ) -> io::Result<Self> {
        descriptor.validate_dds2()?;
        if descriptor.kind != crate::descriptor::FileKind::Ops {
            return Err(corrupt("expected a delta descriptor"));
        }
        Ok(Self {
            descriptor,
            dds2_volumes: volumes.map(<[VolumeDescriptor]>::to_vec),
            ..Self::new()
        })
    }

    pub fn push(&mut self, op: &DeltaOp) -> io::Result<Option<Block>> {
        if self.descriptor.is_dds2() {
            return self.push_dds2(op);
        }
        let done = if self.raw.len() >= OPS_BLOCK_RAW {
            Some(self.flush()?)
        } else {
            None
        };
        if self.count == 0 {
            self.first_path = op.parent.clone();
            if !matches!(&op.record.name, NativeName::Unix(_)) {
                return Err(corrupt("DDS1 only supports Unix-native names"));
            }
            self.first_path.push(op.record.name.clone());
        }
        self.enc.encode(op, &mut self.raw)?;
        self.count += 1;
        Ok(done)
    }

    fn push_dds2(&mut self, op: &DeltaOp) -> io::Result<Option<Block>> {
        let done = if self.raw.len() >= OPS_BLOCK_RAW {
            Some(self.flush()?)
        } else {
            None
        };
        if op.record.depth as usize != op.parent.len()
            || op.parent.len() > usize::from(u16::MAX)
            || op.record.name.encoding() != self.descriptor.path_encoding
            || op
                .parent
                .iter()
                .any(|component| component.encoding() != self.descriptor.path_encoding)
        {
            return Err(corrupt("descriptor mismatch in delta record"));
        }
        if self.count == 0 {
            self.first_path = op.parent.clone();
            self.first_path.push(op.record.name.clone());
            self.dds2_prev_parent.clear();
        }
        let shared = self
            .dds2_prev_parent
            .iter()
            .zip(&op.parent)
            .take_while(|(left, right)| left == right)
            .count();
        self.raw.push(op.kind as u8);
        put_varint(&mut self.raw, shared as u64);
        put_varint(&mut self.raw, (op.parent.len() - shared) as u64);
        for component in &op.parent[shared..] {
            encode_dds2_name(component, self.descriptor.path_encoding, &mut self.raw)?;
        }
        if op.kind == diskdrift_core::delta::OpKind::Remove {
            encode_dds2_name(
                &op.record.name,
                self.descriptor.path_encoding,
                &mut self.raw,
            )?;
        } else {
            encode_dds2_record_payload(
                &op.record,
                self.descriptor,
                self.dds2_volumes.as_deref(),
                &mut self.raw,
            )?;
        }
        self.dds2_prev_parent = op.parent.clone();
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
        self.dds2_prev_parent.clear();
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
    fn dds2_canonical_validation_copies_only_the_final_path() {
        let descriptor = StreamDescriptor::dds2(
            crate::descriptor::FileKind::Records,
            PathEncoding::UnixBytes,
        )
        .unwrap();
        let mut records = vec![rec(0, "/", Kind::Dir, 0)];
        for i in 0..4_096 {
            records.push(rec(1, &format!("entry-{i:05}"), Kind::File, i));
        }
        let mut encoder = RecordEncoder::new_dds2(descriptor).unwrap();
        for record in &records {
            encoder.push(record).unwrap();
        }
        let block = encoder.finish().unwrap().unwrap();
        let raw = decompress(&block.data, block.raw_len, block.crc).unwrap();

        reset_dds2_path_copy_count();
        let mut decoded = Vec::new();
        decode_dds2_records(&raw, block.count, descriptor, None, &mut decoded).unwrap();

        assert_eq!(decoded.len(), records.len());
        assert!(decoded
            .iter()
            .zip(&records)
            .all(|(decoded, record)| decoded.depth == record.depth && decoded.name == record.name));
        assert_eq!(
            dds2_path_copy_count(),
            1,
            "canonical validation must not clone every record path"
        );
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
        assert_eq!(
            decoded[0].file_id,
            FileId {
                low: legacy.ino,
                high: 0
            }
        );
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
        assert!(
            !decoded[0]
                .available
                .contains(diskdrift_core::record::EntryFields::BIRTH_TIME)
        );
        assert!(
            decoded[1]
                .available
                .contains(diskdrift_core::record::EntryFields::BIRTH_TIME)
        );
        assert!(decoded.iter().all(|record| {
            record
                .available
                .contains(diskdrift_core::record::EntryFields::FILE_ID)
        }));
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
