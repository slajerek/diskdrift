//! The metadata record stored for every scanned filesystem entry, and its compact
//! binary encoding.
//!
//! A snapshot is a stream of records in canonical order: depth-first pre-order with
//! the children of every directory sorted by raw name bytes. A record stores only its
//! own name and its depth; full paths are reconstructed while streaming (spec 2A:
//! paths are never stored as full strings per entry).

use crate::native::NativeName;
use serde::{Deserialize, Serialize};
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct FileId {
    pub low: u64,
    pub high: u64,
}

impl FileId {
    pub const ZERO: Self = Self { low: 0, high: 0 };
    pub fn to_le_bytes(self) -> [u8; 16] {
        let mut out = [0; 16];
        out[..8].copy_from_slice(&self.low.to_le_bytes());
        out[8..].copy_from_slice(&self.high.to_le_bytes());
        out
    }
    pub fn from_le_bytes(bytes: [u8; 16]) -> Self {
        Self {
            low: u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            high: u64::from_le_bytes(bytes[8..].try_into().unwrap()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct EntryFields(pub u16);

impl EntryFields {
    pub const KIND: Self = Self(1 << 0);
    pub const LOGICAL_SIZE: Self = Self(1 << 1);
    pub const ALLOCATED_SIZE: Self = Self(1 << 2);
    pub const MTIME: Self = Self(1 << 3);
    pub const BIRTH_TIME: Self = Self(1 << 4);
    pub const FILE_ID: Self = Self(1 << 5);
    pub const LINK_COUNT: Self = Self(1 << 6);
    pub const MTIME_NANOS: Self = Self(1 << 7);
    pub const BIRTH_TIME_NANOS: Self = Self(1 << 8);

    pub const fn empty() -> Self {
        Self(0)
    }
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileIdScheme {
    PosixInode64,
    WindowsFileId128,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveCertainty {
    Definite,
    Probable,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityEvidence {
    pub volume: u16,
    pub file_id: FileId,
    pub scheme: FileIdScheme,
    pub stable: bool,
}

impl IdentityEvidence {
    pub const fn new(volume: u16, file_id: FileId, scheme: FileIdScheme, stable: bool) -> Self {
        Self {
            volume,
            file_id,
            scheme,
            stable,
        }
    }
    pub const fn supports_hardlinks(self) -> bool {
        self.stable
            && !matches!(self.scheme, FileIdScheme::Unavailable)
            && (self.file_id.low != 0 || self.file_id.high != 0)
    }
    pub fn move_certainty(self, other: &Self) -> MoveCertainty {
        if self.volume == other.volume
            && self.file_id == other.file_id
            && self.scheme == other.scheme
            && self.stable
            && other.stable
        {
            MoveCertainty::Definite
        } else if self.volume == other.volume && self.file_id == other.file_id {
            MoveCertainty::Probable
        } else {
            MoveCertainty::None
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TimestampResolution {
    Unknown,
    Seconds,
    Milliseconds,
    Microseconds,
    Nanoseconds,
}

impl TimestampResolution {
    pub fn common(self, other: Self) -> Self {
        if self <= other { self } else { other }
    }
    pub const fn nanos(self) -> i64 {
        match self {
            Self::Unknown => i64::MAX,
            Self::Seconds => 1_000_000_000,
            Self::Milliseconds => 1_000_000,
            Self::Microseconds => 1_000,
            Self::Nanoseconds => 1,
        }
    }
    pub const fn quantize(self, nanos: i64, _source: Self) -> i64 {
        if self.nanos() == i64::MAX {
            nanos
        } else {
            nanos.div_euclid(self.nanos()) * self.nanos()
        }
    }
}

/// Entry type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum Kind {
    #[default]
    File = 0,
    Dir = 1,
    Symlink = 2,
    Other = 3,
}

impl Kind {
    fn from_bits(bits: u8) -> Kind {
        match bits & 0b11 {
            0 => Kind::File,
            1 => Kind::Dir,
            2 => Kind::Symlink,
            _ => Kind::Other,
        }
    }
}

/// Per-record flags. Six bits are available in the encoding.
pub mod flags {
    /// Directory whose contents could not be listed (permission denied, I/O error).
    pub const INACCESSIBLE: u8 = 1 << 0;
    /// Additional path of a hardlinked file whose allocation is attributed elsewhere.
    pub const HARDLINK_DUP: u8 = 1 << 1;
    /// Directory on another filesystem (mount point) that was not entered.
    pub const SKIPPED_MOUNT: u8 = 1 << 2;
    /// Directory excluded by configuration; contents not scanned.
    pub const EXCLUDED: u8 = 1 << 3;
    /// Metadata of the entry itself could not be read; sizes are unknown (zero).
    pub const STAT_ERROR: u8 = 1 << 4;

    /// Flags meaning "the children of this directory are not in the snapshot".
    pub const CONTENTS_UNKNOWN: u8 = INACCESSIBLE | SKIPPED_MOUNT | EXCLUDED;
}

/// Metadata of one filesystem entry.
#[derive(Debug, Clone)]
pub struct Record {
    /// Depth below the scan root; the root itself has depth 0.
    pub depth: u16,
    /// Native name bytes. For the root record this is the full root path.
    pub name: NativeName,
    pub kind: Kind,
    pub flags: u8,
    /// Logical size in bytes (file length).
    pub size: u64,
    /// Allocated size in bytes, as reported by the filesystem.
    pub alloc: u64,
    /// Modification time, seconds since the Unix epoch.
    pub mtime: i64,
    pub mtime_nanos: u32,
    /// Creation (birth) time, seconds since the Unix epoch; 0 if unknown.
    pub btime: i64,
    pub btime_nanos: u32,
    pub file_id: FileId,
    pub ino: u64,
    pub nlink: u32,
    pub available: EntryFields,
    /// Index into the snapshot's volume table.
    pub vol: u16,
}

impl PartialEq for Record {
    fn eq(&self, other: &Self) -> bool {
        if self.has(flags::STAT_ERROR) && other.has(flags::STAT_ERROR) {
            return self.depth == other.depth
                && self.name == other.name
                && self.kind == other.kind
                && self.flags == other.flags
                && self.vol == other.vol;
        }
        let same_legacy_id = if self.file_id != FileId::ZERO && other.file_id != FileId::ZERO {
            self.file_id == other.file_id
        } else if self.file_id == FileId::ZERO && other.file_id == FileId::ZERO {
            self.ino == other.ino
        } else if self.file_id == FileId::ZERO {
            self.ino == other.file_id.low
        } else {
            other.ino == self.file_id.low
        };
        self.depth == other.depth
            && self.name == other.name
            && self.flags == other.flags
            && (self.available.contains(EntryFields::KIND)
                && other.available.contains(EntryFields::KIND)
                && self.kind == other.kind
                || !self.available.contains(EntryFields::KIND)
                || !other.available.contains(EntryFields::KIND))
            && (!self.available.contains(EntryFields::LOGICAL_SIZE)
                || !other.available.contains(EntryFields::LOGICAL_SIZE)
                || self.size == other.size)
            && (!self.available.contains(EntryFields::ALLOCATED_SIZE)
                || !other.available.contains(EntryFields::ALLOCATED_SIZE)
                || self.alloc == other.alloc)
            && (!self.available.contains(EntryFields::MTIME)
                || !other.available.contains(EntryFields::MTIME)
                || self.mtime == other.mtime)
            && (!self.available.contains(EntryFields::MTIME_NANOS)
                || !other.available.contains(EntryFields::MTIME_NANOS)
                || self.mtime_nanos == other.mtime_nanos)
            && (!self.available.contains(EntryFields::BIRTH_TIME)
                || !other.available.contains(EntryFields::BIRTH_TIME)
                || self.btime == other.btime)
            && (!self.available.contains(EntryFields::BIRTH_TIME_NANOS)
                || !other.available.contains(EntryFields::BIRTH_TIME_NANOS)
                || self.btime_nanos == other.btime_nanos)
            && same_legacy_id
            && (!self.available.contains(EntryFields::LINK_COUNT)
                || !other.available.contains(EntryFields::LINK_COUNT)
                || self.nlink == other.nlink)
            && self.vol == other.vol
    }
}
impl Eq for Record {}

impl Default for Record {
    fn default() -> Self {
        Self {
            depth: 0,
            name: NativeName::default(),
            kind: Kind::default(),
            flags: 0,
            size: 0,
            alloc: 0,
            mtime: 0,
            mtime_nanos: 0,
            btime: 0,
            btime_nanos: 0,
            file_id: FileId::ZERO,
            ino: 0,
            nlink: 0,
            available: EntryFields::KIND
                .union(EntryFields::LOGICAL_SIZE)
                .union(EntryFields::ALLOCATED_SIZE)
                .union(EntryFields::MTIME)
                .union(EntryFields::LINK_COUNT),
            vol: 0,
        }
    }
}

impl Record {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    /// Logical bytes this record contributes to totals (hardlink duplicates count 0).
    pub fn counted_size(&self) -> u64 {
        if self.has(flags::HARDLINK_DUP) || !self.available.contains(EntryFields::LOGICAL_SIZE) {
            0
        } else {
            self.size
        }
    }

    /// Allocated bytes this record contributes to totals (hardlink duplicates count 0).
    pub fn counted_alloc(&self) -> u64 {
        if self.has(flags::HARDLINK_DUP) || !self.available.contains(EntryFields::ALLOCATED_SIZE) {
            0
        } else {
            self.alloc
        }
    }

    pub fn has_observed_size(&self) -> bool {
        self.available.contains(EntryFields::LOGICAL_SIZE)
    }

    /// True if any attribute other than depth/name differs in a way a diff reports.
    pub fn content_differs(&self, other: &Record) -> bool {
        if self.has(flags::STAT_ERROR) && other.has(flags::STAT_ERROR) {
            return self.kind != other.kind
                || (self.flags & !flags::HARDLINK_DUP) != (other.flags & !flags::HARDLINK_DUP);
        }
        (self.available.contains(EntryFields::KIND)
            && other.available.contains(EntryFields::KIND)
            && self.kind != other.kind)
            || (self.available.contains(EntryFields::LOGICAL_SIZE)
                && other.available.contains(EntryFields::LOGICAL_SIZE)
                && self.size != other.size)
            || (self.available.contains(EntryFields::ALLOCATED_SIZE)
                && other.available.contains(EntryFields::ALLOCATED_SIZE)
                && self.alloc != other.alloc)
            || (self.available.contains(EntryFields::MTIME)
                && other.available.contains(EntryFields::MTIME)
                && self.mtime != other.mtime)
            || (self.available.contains(EntryFields::MTIME_NANOS)
                && other.available.contains(EntryFields::MTIME_NANOS)
                && self.mtime_nanos != other.mtime_nanos)
            || (self.available.contains(EntryFields::BIRTH_TIME)
                && other.available.contains(EntryFields::BIRTH_TIME)
                && self.btime != other.btime)
            || (self.available.contains(EntryFields::BIRTH_TIME_NANOS)
                && other.available.contains(EntryFields::BIRTH_TIME_NANOS)
                && self.btime_nanos != other.btime_nanos)
            || (self.available.contains(EntryFields::FILE_ID)
                && other.available.contains(EntryFields::FILE_ID)
                && self.file_id != other.file_id)
            || (self.flags & !flags::HARDLINK_DUP) != (other.flags & !flags::HARDLINK_DUP)
    }

    /// Appends the compact encoding of this record to `out`.
    pub fn encode_dds1(&self, out: &mut Vec<u8>) -> io::Result<()> {
        let NativeName::Unix(name) = &self.name else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "DDS1 only supports Unix-native names",
            ));
        };
        put_varint(out, self.depth as u64);
        put_varint(out, self.name.len() as u64);
        out.extend_from_slice(name);
        out.push((self.kind as u8) | (self.flags << 2));
        put_varint(out, self.size);
        put_varint(out, self.alloc);
        put_varint(out, zigzag(self.mtime));
        put_varint(out, zigzag(self.btime));
        put_varint(
            out,
            if self.file_id == FileId::ZERO {
                self.ino
            } else {
                self.file_id.low
            },
        );
        put_varint(out, self.nlink as u64);
        put_varint(out, self.vol as u64);
        Ok(())
    }

    /// Decodes one record from the front of `buf`, advancing it.
    pub fn decode(buf: &mut &[u8]) -> io::Result<Record> {
        let mut rec = Record::default();
        Self::decode_into(buf, &mut rec)?;
        Ok(rec)
    }

    /// Decodes one record into `rec`, reusing its name allocation.
    pub fn decode_into(buf: &mut &[u8], rec: &mut Record) -> io::Result<()> {
        rec.depth = u16::try_from(get_varint(buf)?).map_err(|_| corrupt("depth"))?;
        let len = get_varint(buf)? as usize;
        if buf.len() < len {
            return Err(corrupt("name length"));
        }
        rec.name = NativeName::unix(buf[..len].to_vec());
        *buf = &buf[len..];
        let kf = *buf.first().ok_or_else(|| corrupt("kind"))?;
        *buf = &buf[1..];
        rec.kind = Kind::from_bits(kf);
        rec.flags = kf >> 2;
        rec.size = get_varint(buf)?;
        rec.alloc = get_varint(buf)?;
        rec.mtime = unzigzag(get_varint(buf)?);
        rec.btime = unzigzag(get_varint(buf)?);
        let _ino = get_varint(buf)?;
        rec.nlink = u32::try_from(get_varint(buf)?).map_err(|_| corrupt("nlink"))?;
        rec.vol = u16::try_from(get_varint(buf)?).map_err(|_| corrupt("volume index"))?;
        rec.mtime_nanos = 0;
        rec.btime_nanos = 0;
        rec.available = if rec.has(flags::STAT_ERROR) {
            rec.size = 0;
            rec.alloc = 0;
            rec.mtime = 0;
            rec.btime = 0;
            rec.ino = 0;
            rec.nlink = 0;
            rec.file_id = FileId::ZERO;
            EntryFields::empty()
        } else {
            // DDS1 has no nanosecond columns and its inode is only the low half
            // of the portable identity, so only the legacy scalar is retained.
            rec.ino = _ino;
            rec.file_id = FileId { low: _ino, high: 0 };
            let mut fields = EntryFields::KIND
                .union(EntryFields::LOGICAL_SIZE)
                .union(EntryFields::ALLOCATED_SIZE)
                .union(EntryFields::MTIME)
                .union(EntryFields::LINK_COUNT);
            if rec.btime != 0 {
                fields = fields.union(EntryFields::BIRTH_TIME);
            }
            fields
        };
        Ok(())
    }
}

fn corrupt(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("corrupt record: bad {what}"),
    )
}

/// LEB128 unsigned varint.
pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn get_varint(buf: &mut &[u8]) -> io::Result<u64> {
    let mut result: u64 = 0;
    let mut shift = 0;
    for (i, &b) in buf.iter().enumerate() {
        if shift >= 64 {
            break;
        }
        result |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            *buf = &buf[i + 1..];
            return Ok(result);
        }
        shift += 7;
    }
    Err(corrupt("varint"))
}

pub fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

pub fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut out = Vec::new();
            put_varint(&mut out, v);
            let mut s = out.as_slice();
            assert_eq!(get_varint(&mut s).unwrap(), v);
            assert!(s.is_empty());
        }
        for v in [0i64, -1, 1, i64::MIN, i64::MAX, -1_700_000_000] {
            assert_eq!(unzigzag(zigzag(v)), v);
        }
    }

    #[test]
    fn record_roundtrip() {
        let rec = Record {
            depth: 3,
            name: NativeName::unix(b"caf\xc3\xa9 \xff not utf8".to_vec()),
            kind: Kind::Symlink,
            flags: flags::HARDLINK_DUP | flags::STAT_ERROR,
            size: 1 << 40,
            alloc: 4096,
            mtime: 1_760_000_000,
            mtime_nanos: 0,
            btime: -5,
            btime_nanos: 0,
            ino: 0,
            file_id: FileId::ZERO,
            nlink: 2,
            vol: 1,
            available: EntryFields::empty(),
        };
        let mut out = Vec::new();
        rec.encode_dds1(&mut out).unwrap();
        rec.encode_dds1(&mut out).unwrap();
        let mut s = out.as_slice();
        assert_eq!(Record::decode(&mut s).unwrap(), rec);
        assert_eq!(Record::decode(&mut s).unwrap(), rec);
        assert!(s.is_empty());
    }

    #[test]
    fn dds1_decode_normalizes_stat_error_payload() {
        let rec = Record {
            name: NativeName::unix(b"error".to_vec()),
            flags: flags::STAT_ERROR,
            size: 11,
            alloc: 4096,
            mtime: 12,
            mtime_nanos: 13,
            btime: 14,
            btime_nanos: 15,
            file_id: FileId { low: 16, high: 17 },
            ino: 16,
            nlink: 18,
            available: EntryFields::KIND
                .union(EntryFields::LOGICAL_SIZE)
                .union(EntryFields::ALLOCATED_SIZE)
                .union(EntryFields::MTIME)
                .union(EntryFields::BIRTH_TIME)
                .union(EntryFields::FILE_ID)
                .union(EntryFields::LINK_COUNT),
            ..Default::default()
        };
        let mut encoded = Vec::new();
        rec.encode_dds1(&mut encoded).unwrap();

        let mut input = encoded.as_slice();
        let decoded = Record::decode(&mut input).unwrap();
        assert_eq!(decoded.size, 0);
        assert_eq!(decoded.alloc, 0);
        assert_eq!(decoded.mtime, 0);
        assert_eq!(decoded.mtime_nanos, 0);
        assert_eq!(decoded.btime, 0);
        assert_eq!(decoded.btime_nanos, 0);
        assert_eq!(decoded.file_id, FileId::ZERO);
        assert_eq!(decoded.ino, 0);
        assert_eq!(decoded.nlink, 0);
        assert_eq!(decoded.available, EntryFields::empty());
    }

    #[test]
    fn dds1_decode_does_not_claim_unrepresentable_precision_or_identity() {
        let rec = Record {
            name: NativeName::unix(b"legacy".to_vec()),
            mtime: 12,
            mtime_nanos: 13,
            btime: 14,
            btime_nanos: 15,
            file_id: FileId { low: 16, high: 17 },
            ino: 16,
            available: EntryFields::MTIME
                .union(EntryFields::MTIME_NANOS)
                .union(EntryFields::BIRTH_TIME)
                .union(EntryFields::BIRTH_TIME_NANOS)
                .union(EntryFields::FILE_ID),
            ..Default::default()
        };
        let mut encoded = Vec::new();
        rec.encode_dds1(&mut encoded).unwrap();

        let mut input = encoded.as_slice();
        let decoded = Record::decode(&mut input).unwrap();
        assert_eq!(decoded.mtime, rec.mtime);
        assert_eq!(decoded.btime, rec.btime);
        assert_eq!(decoded.mtime_nanos, 0);
        assert_eq!(decoded.btime_nanos, 0);
        assert!(!decoded.available.contains(EntryFields::MTIME_NANOS));
        assert!(!decoded.available.contains(EntryFields::BIRTH_TIME_NANOS));
        assert!(!decoded.available.contains(EntryFields::FILE_ID));
        assert_eq!(
            decoded.file_id,
            FileId {
                low: rec.ino,
                high: 0
            }
        );
        assert_eq!(decoded.ino, rec.ino);
    }

    #[test]
    fn truncated_input_is_an_error() {
        let rec = Record {
            name: NativeName::unix(b"abc".to_vec()),
            ..Default::default()
        };
        let mut out = Vec::new();
        rec.encode_dds1(&mut out).unwrap();
        for cut in 0..out.len() {
            let mut s = &out[..cut];
            assert!(Record::decode(&mut s).is_err(), "cut at {cut}");
        }
    }
}
