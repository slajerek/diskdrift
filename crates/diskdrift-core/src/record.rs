//! The metadata record stored for every scanned filesystem entry, and its compact
//! binary encoding.
//!
//! A snapshot is a stream of records in canonical order: depth-first pre-order with
//! the children of every directory sorted by raw name bytes. A record stores only its
//! own name and its depth; full paths are reconstructed while streaming (spec 2A:
//! paths are never stored as full strings per entry).

use serde::{Deserialize, Serialize};
use std::io;

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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Record {
    /// Depth below the scan root; the root itself has depth 0.
    pub depth: u16,
    /// Native name bytes. For the root record this is the full root path.
    pub name: Vec<u8>,
    pub kind: Kind,
    pub flags: u8,
    /// Logical size in bytes (file length).
    pub size: u64,
    /// Allocated size in bytes, as reported by the filesystem.
    pub alloc: u64,
    /// Modification time, seconds since the Unix epoch.
    pub mtime: i64,
    /// Creation (birth) time, seconds since the Unix epoch; 0 if unknown.
    pub btime: i64,
    pub ino: u64,
    pub nlink: u32,
    /// Index into the snapshot's volume table.
    pub vol: u16,
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
        if self.has(flags::HARDLINK_DUP) { 0 } else { self.size }
    }

    /// Allocated bytes this record contributes to totals (hardlink duplicates count 0).
    pub fn counted_alloc(&self) -> u64 {
        if self.has(flags::HARDLINK_DUP) { 0 } else { self.alloc }
    }

    /// True if any attribute other than depth/name differs in a way a diff reports.
    pub fn content_differs(&self, other: &Record) -> bool {
        self.kind != other.kind
            || self.size != other.size
            || self.alloc != other.alloc
            || self.mtime != other.mtime
            || self.ino != other.ino
            || (self.flags & !flags::HARDLINK_DUP) != (other.flags & !flags::HARDLINK_DUP)
    }

    /// Appends the compact encoding of this record to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        put_varint(out, self.depth as u64);
        put_varint(out, self.name.len() as u64);
        out.extend_from_slice(&self.name);
        out.push((self.kind as u8) | (self.flags << 2));
        put_varint(out, self.size);
        put_varint(out, self.alloc);
        put_varint(out, zigzag(self.mtime));
        put_varint(out, zigzag(self.btime));
        put_varint(out, self.ino);
        put_varint(out, self.nlink as u64);
        put_varint(out, self.vol as u64);
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
        rec.name.clear();
        rec.name.extend_from_slice(&buf[..len]);
        *buf = &buf[len..];
        let kf = *buf.first().ok_or_else(|| corrupt("kind"))?;
        *buf = &buf[1..];
        rec.kind = Kind::from_bits(kf);
        rec.flags = kf >> 2;
        rec.size = get_varint(buf)?;
        rec.alloc = get_varint(buf)?;
        rec.mtime = unzigzag(get_varint(buf)?);
        rec.btime = unzigzag(get_varint(buf)?);
        rec.ino = get_varint(buf)?;
        rec.nlink = u32::try_from(get_varint(buf)?).map_err(|_| corrupt("nlink"))?;
        rec.vol = u16::try_from(get_varint(buf)?).map_err(|_| corrupt("volume index"))?;
        Ok(())
    }
}

fn corrupt(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("corrupt record: bad {what}"))
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
            name: b"caf\xc3\xa9 \xff not utf8".to_vec(),
            kind: Kind::Symlink,
            flags: flags::HARDLINK_DUP | flags::STAT_ERROR,
            size: 1 << 40,
            alloc: 4096,
            mtime: 1_760_000_000,
            btime: -5,
            ino: 123_456_789,
            nlink: 2,
            vol: 1,
        };
        let mut out = Vec::new();
        rec.encode(&mut out);
        rec.encode(&mut out);
        let mut s = out.as_slice();
        assert_eq!(Record::decode(&mut s).unwrap(), rec);
        assert_eq!(Record::decode(&mut s).unwrap(), rec);
        assert!(s.is_empty());
    }

    #[test]
    fn truncated_input_is_an_error() {
        let rec = Record { name: b"abc".to_vec(), ..Default::default() };
        let mut out = Vec::new();
        rec.encode(&mut out);
        for cut in 0..out.len() {
            let mut s = &out[..cut];
            assert!(Record::decode(&mut s).is_err(), "cut at {cut}");
        }
    }
}
