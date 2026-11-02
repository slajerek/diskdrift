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
use crate::descriptor::StreamDescriptor;
use crate::fsops;
use crate::meta::MetadataV3;
use diskdrift_core::delta::DeltaOp;
use diskdrift_core::native::NativeName;
use diskdrift_core::paths::compare_paths;
use diskdrift_core::record::{Record, get_varint, put_varint};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 4] = b"DDS1";
const END: &[u8; 4] = b"DDSE";
const TRAILER: usize = 28;

#[cfg(test)]
thread_local! {
    static DDS2_BLOCK_VALIDATION_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_dds2_block_validation() {
    DDS2_BLOCK_VALIDATION_COUNT.with(|count| count.set(count.get() + 1));
}

#[cfg(not(test))]
fn note_dds2_block_validation() {}

#[cfg(test)]
fn reset_dds2_block_validation_count() {
    DDS2_BLOCK_VALIDATION_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
fn dds2_block_validation_count() -> usize {
    DDS2_BLOCK_VALIDATION_COUNT.with(std::cell::Cell::get)
}

pub use crate::descriptor::FileKind;

#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub offset: u64,
    pub comp_len: u32,
    pub raw_len: u32,
    pub count: u32,
    pub crc: u32,
    pub first_path: Vec<NativeName>,
}

pub struct DataWriter {
    out: BufWriter<File>,
    tmp: PathBuf,
    descriptor: StreamDescriptor,
    offset: u64,
    index: Vec<IndexEntry>,
    items: u64,
}

impl DataWriter {
    pub fn create(tmp: PathBuf, kind: FileKind) -> io::Result<Self> {
        Self::create_with_descriptor(tmp, StreamDescriptor::dds1(kind))
    }

    fn create_with_descriptor(tmp: PathBuf, descriptor: StreamDescriptor) -> io::Result<Self> {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        let mut out = BufWriter::with_capacity(1 << 20, f);
        if descriptor.is_dds2() {
            out.write_all(&descriptor.header())?;
        } else {
            out.write_all(MAGIC)?;
            out.write_all(&[descriptor.kind as u8, 0, 0, 0])?;
        }
        Ok(DataWriter {
            out,
            tmp,
            descriptor,
            offset: 8,
            index: Vec::new(),
            items: 0,
        })
    }

    pub fn create_dds2(tmp: PathBuf, descriptor: StreamDescriptor) -> io::Result<Self> {
        descriptor.validate_dds2()?;
        Self::create_with_descriptor(tmp, descriptor)
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
            if self.descriptor.is_dds2() {
                codec::encode_path_for(&e.first_path, self.descriptor.path_encoding, &mut idx)?;
            } else {
                codec::encode_path(&e.first_path, &mut idx)?;
            }
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
        fsops::flush_file(&f)?;
        drop(f);
        fsops::replace_atomic(&self.tmp, dest)?;
        fsops::sync_parent(dest)?;
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
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: {m}", path.display()),
    )
}

/// An opened, validated data file.
pub struct DataFile {
    pub path: PathBuf,
    pub kind: FileKind,
    pub descriptor: StreamDescriptor,
    pub index: Vec<IndexEntry>,
    pub items: u64,
    volumes: Option<Vec<crate::meta::VolumeDescriptor>>,
    file: File,
}

impl DataFile {
    pub fn open(path: &Path) -> io::Result<DataFile> {
        Self::open_internal(path, None, true)
    }

    pub fn open_with_volumes(
        path: &Path,
        volumes: &[crate::meta::VolumeDescriptor],
    ) -> io::Result<DataFile> {
        Self::open_internal(path, Some(volumes.to_vec()), true)
    }

    pub fn open_with_metadata(path: &Path, metadata: &MetadataV3) -> io::Result<DataFile> {
        metadata
            .validate()
            .map_err(|error| corrupt(path, &error.to_string()))?;
        let data = Self::open_with_volumes(path, &metadata.volumes)?;
        data.descriptor
            .validate_metadata(metadata)
            .map_err(|error| corrupt(path, &error.to_string()))?;
        Ok(data)
    }

    pub(crate) fn open_with_metadata_unverified(
        path: &Path,
        metadata: &MetadataV3,
    ) -> io::Result<DataFile> {
        metadata
            .validate()
            .map_err(|error| corrupt(path, &error.to_string()))?;
        let data = Self::open_internal(path, Some(metadata.volumes.clone()), false)?;
        data.descriptor
            .validate_metadata(metadata)
            .map_err(|error| corrupt(path, &error.to_string()))?;
        Ok(data)
    }

    pub fn descriptor(&self) -> StreamDescriptor {
        self.descriptor
    }

    fn open_internal(
        path: &Path,
        volumes: Option<Vec<crate::meta::VolumeDescriptor>>,
        verify_blocks: bool,
    ) -> io::Result<DataFile> {
        if let Some(volumes) = &volumes {
            for (expected, volume) in volumes.iter().enumerate() {
                if volume.index as usize != expected {
                    return Err(corrupt(path, "volume descriptors are not ordered"));
                }
                volume
                    .validate()
                    .map_err(|error| corrupt(path, &error.to_string()))?;
            }
        }
        let file = fsops::open_read_shared(path)?;
        let len = file.metadata()?.len();
        if len < (8 + TRAILER) as u64 {
            return Err(corrupt(path, "file too short"));
        }
        let mut h = [0u8; 8];
        fsops::read_exact_at(&file, &mut h, 0)?;
        let descriptor = if &h[..4] == MAGIC {
            let kind = match h[4] {
                2 => FileKind::Records,
                1 => FileKind::Ops,
                k => return Err(corrupt(path, &format!("unsupported data kind {k}"))),
            };
            StreamDescriptor::dds1(kind)
        } else if &h[..4] == b"DDS2" {
            StreamDescriptor::from_header(&h).map_err(|error| corrupt(path, &error.to_string()))?
        } else {
            return Err(corrupt(path, "not a DiskDrift snapshot file"));
        };
        let kind = descriptor.kind;
        let mut t = [0u8; TRAILER];
        fsops::read_exact_at(&file, &mut t, len - TRAILER as u64)?;
        if &t[TRAILER - 4..] != END {
            let bytes = std::fs::read(path)?;
            let has_complete_trailer = bytes
                .windows(TRAILER)
                .any(|window| window[TRAILER - 4..] == END[..]);
            return Err(corrupt(
                path,
                if has_complete_trailer {
                    "trailing bytes after trailer"
                } else {
                    "missing trailer (incomplete write?)"
                },
            ));
        }
        let idx_off = u64::from_le_bytes(t[0..8].try_into().unwrap());
        let idx_len = u32::from_le_bytes(t[8..12].try_into().unwrap()) as usize;
        let idx_crc = u32::from_le_bytes(t[12..16].try_into().unwrap());
        let items = u64::from_le_bytes(t[16..24].try_into().unwrap());
        let trailer_start = len - TRAILER as u64;
        if idx_off < 8 || idx_off.saturating_add(idx_len as u64) > trailer_start {
            return Err(corrupt(path, "index out of bounds"));
        }
        if idx_off.saturating_add(idx_len as u64) != trailer_start {
            return Err(corrupt(path, "trailing bytes in index"));
        }
        let mut idx = vec![0u8; idx_len];
        fsops::read_exact_at(&file, &mut idx, idx_off)?;
        if crc32fast::hash(&idx) != idx_crc {
            return Err(corrupt(path, "index checksum mismatch"));
        }
        let mut s = idx.as_slice();
        let n = if descriptor.is_dds2() {
            usize::try_from(codec::get_dds2_varint(&mut s)?)
                .map_err(|_| corrupt(path, "index entry count overflow"))?
        } else {
            get_varint(&mut s)? as usize
        };
        let mut index: Vec<IndexEntry> = Vec::with_capacity(n.min(1 << 20));
        let mut total = 0u64;
        for _ in 0..n {
            let read = |s: &mut &[u8]| {
                if descriptor.is_dds2() {
                    codec::get_dds2_varint(s)
                } else {
                    get_varint(s)
                }
            };
            let offset = read(&mut s)?;
            let comp_len =
                u32::try_from(read(&mut s)?).map_err(|_| corrupt(path, "index field overflow"))?;
            let raw_len =
                u32::try_from(read(&mut s)?).map_err(|_| corrupt(path, "index field overflow"))?;
            let count =
                u32::try_from(read(&mut s)?).map_err(|_| corrupt(path, "index field overflow"))?;
            if s.len() < 4 {
                return Err(corrupt(path, "truncated index"));
            }
            let crc = u32::from_le_bytes(s[..4].try_into().unwrap());
            s = &s[4..];
            let first_path = if descriptor.is_dds2() {
                codec::decode_path_for(&mut s, descriptor.path_encoding, true).map_err(|error| {
                    if descriptor.path_encoding
                        == diskdrift_core::native::PathEncoding::WindowsUtf16le
                        && error.to_string() == "malformed UTF-16 path"
                    {
                        corrupt(path, "descriptor mismatch in index path")
                    } else {
                        corrupt(path, &error.to_string())
                    }
                })?
            } else {
                codec::decode_path(&mut s)?
            };
            if offset < 8 || offset.saturating_add(comp_len as u64) > idx_off {
                return Err(corrupt(path, "block out of bounds"));
            }
            if let Some(previous) = index.last()
                && previous.offset.saturating_add(previous.comp_len as u64) != offset
            {
                return Err(corrupt(path, "non-contiguous block index"));
            }
            if descriptor.is_dds2()
                && first_path
                    .iter()
                    .any(|component| component.encoding() != descriptor.path_encoding)
            {
                return Err(corrupt(path, "descriptor mismatch in index path"));
            }
            if descriptor.is_dds2()
                && let Some(previous) = index.last()
                && compare_paths(&previous.first_path, &first_path)
                    .map_err(|error| corrupt(path, &error.to_string()))?
                    != std::cmp::Ordering::Less
            {
                return Err(corrupt(path, "non-canonical index paths"));
            }
            total += count as u64;
            index.push(IndexEntry {
                offset,
                comp_len,
                raw_len,
                count,
                crc,
                first_path,
            });
        }
        if !s.is_empty() {
            return Err(corrupt(path, "trailing bytes in index"));
        }
        if n == 0 && items != 0 {
            return Err(corrupt(path, "empty stream item count"));
        }
        if total != items {
            return Err(corrupt(path, "item count mismatch"));
        }
        if n > 0 {
            let last = index.last().expect("non-empty index");
            if last.offset.saturating_add(last.comp_len as u64) != idx_off {
                return Err(corrupt(path, "index block bounds"));
            }
        } else if idx_off != 8 {
            return Err(corrupt(path, "empty stream index bounds"));
        }
        let data = DataFile {
            path: path.to_path_buf(),
            kind,
            descriptor,
            index,
            items,
            volumes,
            file,
        };
        if verify_blocks && data.volumes.is_some() && !data.descriptor.is_dds2() {
            data.validate_volume_indexes()?;
        }
        if verify_blocks && data.descriptor.is_dds2() {
            data.validate_dds2_blocks()?;
        }
        Ok(data)
    }

    fn validate_volume_indexes(&self) -> io::Result<()> {
        let Some(volumes) = self.volumes.as_deref() else {
            return Ok(());
        };
        for entry in &self.index {
            let raw = self.read_block(entry)?;
            match self.kind {
                FileKind::Records => {
                    let mut records = Vec::with_capacity(entry.count as usize);
                    if self.descriptor.is_dds2() {
                        codec::decode_dds2_records(
                            &raw,
                            entry.count,
                            self.descriptor,
                            Some(volumes),
                            &mut records,
                        )?;
                    } else {
                        codec::decode_records(&raw, entry.count, &mut records)?;
                    }
                    for record in records {
                        if volumes.get(record.vol as usize).is_none() {
                            return Err(corrupt(&self.path, "invalid volume index"));
                        }
                    }
                }
                FileKind::Ops => {
                    let mut ops = Vec::with_capacity(entry.count as usize);
                    if self.descriptor.is_dds2() {
                        codec::decode_dds2_ops(
                            &raw,
                            entry.count,
                            self.descriptor,
                            Some(volumes),
                            &mut ops,
                        )?;
                    } else {
                        codec::decode_ops(&raw, entry.count, &mut ops)?;
                    }
                    for op in ops {
                        if volumes.get(op.record.vol as usize).is_none() {
                            return Err(corrupt(&self.path, "invalid volume index"));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_dds2_blocks(&self) -> io::Result<()> {
        note_dds2_block_validation();
        let mut previous_path = None;
        for entry in &self.index {
            let raw = self.read_block(entry)?;
            let (actual_path, last_path) = match self.kind {
                FileKind::Records => {
                    let decoded = codec::validate_dds2_records(
                        &raw,
                        entry.count,
                        self.descriptor,
                        self.volumes.as_deref(),
                        previous_path.as_deref(),
                    )
                    .map_err(|error| corrupt(&self.path, &error.to_string()))?;
                    let mut path = decoded.context;
                    path.push(decoded.first_name);
                    (path, decoded.last_path)
                }
                FileKind::Ops => {
                    let mut ops = Vec::with_capacity(entry.count as usize);
                    let last_path = codec::decode_dds2_ops_with_previous(
                        &raw,
                        entry.count,
                        self.descriptor,
                        self.volumes.as_deref(),
                        previous_path.as_deref(),
                        &mut ops,
                    )
                    .map_err(|error| corrupt(&self.path, &error.to_string()))?;
                    let Some(first) = ops.first() else {
                        return Err(corrupt(&self.path, "malformed delta block"));
                    };
                    let mut path = first.parent.clone();
                    path.push(first.record.name.clone());
                    (path, last_path)
                }
            };
            if actual_path != entry.first_path {
                return Err(corrupt(&self.path, "descriptor mismatch in index path"));
            }
            previous_path = Some(last_path);
        }
        Ok(())
    }

    fn read_block(&self, e: &IndexEntry) -> io::Result<Vec<u8>> {
        let mut comp = vec![0u8; e.comp_len as usize];
        fsops::read_exact_at(&self.file, &mut comp, e.offset)?;
        codec::decompress(&comp, e.raw_len, e.crc)
            .map_err(|err| corrupt(&self.path, &err.to_string()))
    }

    /// Path context (ancestors of the first record) of block `i`.
    pub fn block_context(&self, i: usize) -> io::Result<Vec<NativeName>> {
        let raw = self.read_block(&self.index[i])?;
        if self.descriptor.is_dds2() {
            let mut records = Vec::new();
            codec::decode_dds2_records(
                &raw,
                self.index[i].count,
                self.descriptor,
                self.volumes.as_deref(),
                &mut records,
            )
        } else {
            codec::records_context(&raw)
        }
    }

    /// Streams records from block `start` on, decoding on a background thread.
    pub fn records_from(self, start: usize) -> io::Result<Pipelined<Record>> {
        if self.kind != FileKind::Records {
            return Err(corrupt(&self.path, "expected a records file"));
        }
        let descriptor = self.descriptor;
        let volumes = self.volumes.clone();
        Ok(Pipelined::spawn(self, start, 3, move |raw, count, out| {
            if descriptor.is_dds2() {
                codec::decode_dds2_records_verified(
                    raw,
                    count,
                    descriptor,
                    volumes.as_deref(),
                    out,
                )
            } else {
                codec::decode_records(raw, count, out).map(|_| ())
            }
        }))
    }

    /// Delta ops, decoded on the calling thread: delta files are small, and a
    /// long reconstruction chain must not cost one thread (and heap) per level.
    pub fn ops(self) -> io::Result<InlineOps> {
        if self.kind != FileKind::Ops {
            return Err(corrupt(&self.path, "expected a delta file"));
        }
        Ok(InlineOps {
            descriptor: self.descriptor,
            volumes: self.volumes.clone(),
            df: self,
            next_block: 0,
            cur: Vec::new().into_iter(),
            failed: false,
        })
    }

    /// Reads and checks every block (integrity verification).
    pub fn verify(&self) -> io::Result<u64> {
        if self.descriptor.is_dds2() {
            self.validate_dds2_blocks()?;
            return Ok(self.items);
        }
        let mut n = 0;
        for e in &self.index {
            let raw = self.read_block(e)?;
            match self.kind {
                FileKind::Records => {
                    let mut v = Vec::new();
                    if self.descriptor.is_dds2() {
                        codec::decode_dds2_records(
                            &raw,
                            e.count,
                            self.descriptor,
                            self.volumes.as_deref(),
                            &mut v,
                        )
                        .map(|_| ())
                    } else {
                        codec::decode_records(&raw, e.count, &mut v).map(|_| ())
                    }
                    .map_err(|err| corrupt(&self.path, &err.to_string()))?;
                    n += v.len() as u64;
                }
                FileKind::Ops => {
                    let mut v = Vec::new();
                    if self.descriptor.is_dds2() {
                        codec::decode_dds2_ops(
                            &raw,
                            e.count,
                            self.descriptor,
                            self.volumes.as_deref(),
                            &mut v,
                        )
                    } else {
                        codec::decode_ops(&raw, e.count, &mut v).map(|_| ())
                    }
                    .map_err(|err| corrupt(&self.path, &err.to_string()))?;
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
    fn spawn<F>(df: DataFile, start: usize, ahead: usize, decode: F) -> Self
    where
        F: Fn(&[u8], u32, &mut Vec<T>) -> io::Result<()> + Send + 'static,
    {
        let (tx, rx) = crossbeam_channel::bounded(ahead);
        std::thread::spawn(move || {
            for e in df.index.iter().skip(start) {
                let r = df.read_block(e).and_then(|raw| {
                    let mut out = Vec::with_capacity(e.count as usize);
                    decode(&raw, e.count, &mut out)
                        .map_err(|err| corrupt(&df.path, &err.to_string()))?;
                    Ok(out)
                });
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

/// Ops of a delta file, one block decoded at a time on the caller's thread.
pub struct InlineOps {
    descriptor: StreamDescriptor,
    volumes: Option<Vec<crate::meta::VolumeDescriptor>>,
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
                if self.descriptor.is_dds2() {
                    codec::decode_dds2_ops(
                        &raw,
                        e.count,
                        self.descriptor,
                        self.volumes.as_deref(),
                        &mut out,
                    )
                } else {
                    codec::decode_ops(&raw, e.count, &mut out).map(|_| ())
                }
                .map_err(|err| corrupt(&self.df.path, &err.to_string()))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::RecordEncoder;
    use diskdrift_core::native::{NativeName, PathEncoding};
    use diskdrift_core::record::{Kind, Record};
    use diskdrift_core::testutil::TestDir;

    #[test]
    fn dds2_records_from_reuses_open_validation() {
        let dir = TestDir::new("dds2-records-validation");
        let descriptor = StreamDescriptor::dds2(FileKind::Records, PathEncoding::UnixBytes)
            .unwrap();
        let tmp = dir.path().join("records.dds.tmp");
        let path = dir.path().join("records.dds");
        let mut writer = DataWriter::create_dds2(tmp, descriptor).unwrap();
        let mut encoder = RecordEncoder::new_dds2(descriptor).unwrap();
        for record in [
            Record {
                depth: 0,
                name: NativeName::unix(b"root".to_vec()),
                kind: Kind::Dir,
                ..Default::default()
            },
            Record {
                depth: 1,
                name: NativeName::unix(b"child".to_vec()),
                ..Default::default()
            },
        ] {
            if let Some(block) = encoder.push(&record).unwrap() {
                writer.block(block).unwrap();
            }
        }
        if let Some(block) = encoder.finish().unwrap() {
            writer.block(block).unwrap();
        }
        writer.commit(&path).unwrap();

        reset_dds2_block_validation_count();
        let records: Vec<_> = DataFile::open(&path)
            .unwrap()
            .records_from(0)
            .unwrap()
            .collect::<io::Result<_>>()
            .unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(
            dds2_block_validation_count(),
            1,
            "streaming must reuse the validation performed while opening the file"
        );
    }
}
