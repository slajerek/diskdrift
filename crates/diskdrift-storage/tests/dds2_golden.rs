//! Frozen DDS2 fixtures for the always-available reader and writer.

use diskdrift_core::native::{NativeName, NativePath, PathEncoding};
use diskdrift_core::paths::compare_paths;
use diskdrift_core::record::{EntryFields, FileId, Kind, Record, TimestampResolution};
use diskdrift_core::testutil::TestDir;
use diskdrift_storage::codec::{OpEncoderBlocks, RecordEncoder};
use diskdrift_storage::container::{DataFile, DataWriter, FileKind};
use diskdrift_storage::descriptor::StreamDescriptor as StorageDescriptor;
use diskdrift_storage::meta::{
    AllocationSemantics, FileIdScheme, FileIdScope, Provenance, VolumeDescriptor,
};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const DDS2_MAGIC: &[u8; 4] = b"DDS2";
const DDS2_TRAILER_LEN: usize = 28;
const RECORD_COLUMNS: usize = 14;

#[test]
fn default_build_exposes_the_dds2_writer_api() {
    let dir = TestDir::new("dds2-writer-api");
    let tmp = dir.path().join("records.dds.tmp");
    let writer: DataWriter = DataWriter::create(tmp, FileKind::Records).unwrap();
    assert_eq!(writer.blocks(), 0);
    writer.abort();
}

#[test]
fn default_build_writers_round_trip_records_and_ops() {
    let dir = TestDir::new("dds2-writer-roundtrip");
    let records_descriptor =
        StorageDescriptor::dds2(FileKind::Records, PathEncoding::UnixBytes).unwrap();
    let records_tmp = dir.path().join("records.dds.tmp");
    let records_dest = dir.path().join("records.dds");
    let mut records_writer = DataWriter::create_dds2(records_tmp, records_descriptor).unwrap();
    let record = Record {
        depth: 0,
        name: NativeName::unix(b"record".to_vec()),
        kind: Kind::File,
        available: EntryFields::KIND
            .union(EntryFields::MTIME)
            .union(EntryFields::MTIME_NANOS)
            .union(EntryFields::FILE_ID),
        mtime: -2,
        mtime_nanos: 123,
        file_id: FileId { low: 7, high: 9 },
        ..Default::default()
    };
    let mut record_encoder = RecordEncoder::new_dds2(records_descriptor).unwrap();
    if let Some(block) = record_encoder.push(&record).unwrap() {
        records_writer.block(block).unwrap();
    }
    if let Some(block) = record_encoder.finish().unwrap() {
        records_writer.block(block).unwrap();
    }
    records_writer.commit(&records_dest).unwrap();
    let decoded: Vec<_> = DataFile::open(&records_dest)
        .unwrap()
        .records_from(0)
        .unwrap()
        .collect::<io::Result<_>>()
        .unwrap();
    assert_eq!(decoded, vec![record.clone()]);

    let ops_descriptor = StorageDescriptor::dds2(FileKind::Ops, PathEncoding::UnixBytes).unwrap();
    let ops_tmp = dir.path().join("ops.dds.tmp");
    let ops_dest = dir.path().join("ops.dds");
    let mut ops_writer = DataWriter::create_dds2(ops_tmp, ops_descriptor).unwrap();
    let op = diskdrift_core::delta::DeltaOp {
        kind: diskdrift_core::delta::OpKind::Replace,
        parent: vec![NativeName::unix(b"root".to_vec())],
        record: Record {
            depth: 1,
            name: NativeName::unix(b"child".to_vec()),
            ..record
        },
    };
    let mut op_encoder = OpEncoderBlocks::new_dds2(ops_descriptor).unwrap();
    if let Some(block) = op_encoder.push(&op).unwrap() {
        ops_writer.block(block).unwrap();
    }
    if let Some(block) = op_encoder.finish().unwrap() {
        ops_writer.block(block).unwrap();
    }
    ops_writer.commit(&ops_dest).unwrap();
    let decoded = DataFile::open(&ops_dest)
        .unwrap()
        .ops()
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(decoded, op);
}

#[derive(Clone)]
struct FixtureRecord {
    depth: u16,
    name: NativeName,
    kind: Kind,
    flags: u8,
    available: EntryFields,
    size: u64,
    alloc: u64,
    mtime: i64,
    mtime_nanos: u32,
    btime: i64,
    btime_nanos: u32,
    file_id: FileId,
    nlink: u32,
    vol: u16,
}

#[derive(Clone)]
struct BlockSpec {
    raw: Vec<u8>,
    count: u32,
    first_path: Vec<NativeName>,
}

#[derive(Debug, PartialEq, Eq)]
struct IndexSpec {
    offset: u64,
    comp_len: u32,
    raw_len: u32,
    count: u32,
    crc: u32,
    first_path: Vec<NativeName>,
}

#[derive(Debug, PartialEq, Eq)]
struct DecodedPaths {
    context: Vec<NativeName>,
    depths: Vec<u16>,
    names: Vec<NativeName>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamDescriptor {
    path_encoding: u8,
    record_schema: u8,
    stream_flags: u8,
}

impl StreamDescriptor {
    const DDS1: Self = Self {
        path_encoding: 1,
        record_schema: 0,
        stream_flags: 0,
    };

    fn dds2(path_encoding: u8) -> Self {
        Self {
            path_encoding,
            record_schema: 1,
            stream_flags: 0,
        }
    }

    fn lineage_compatible(self, other: Self) -> bool {
        self.path_encoding == other.path_encoding
            && self.stream_flags == 0
            && other.stream_flags == 0
            && matches!(
                (self.record_schema, other.record_schema),
                (0, 1) | (1, 0) | (1, 1)
            )
    }
}

fn put_uvarint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_svarint(out: &mut Vec<u8>, value: i64) {
    put_uvarint(out, diskdrift_core::record::zigzag(value));
}

fn read_uvarint(input: &mut &[u8]) -> u64 {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let byte = *input.first().expect("fixture varint is truncated");
        *input = &input[1..];
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
    }
    panic!("fixture varint is too wide");
}

fn read_svarint(input: &mut &[u8]) -> i64 {
    diskdrift_core::record::unzigzag(read_uvarint(input))
}

fn take_bytes(input: &mut &[u8], len: usize) -> Vec<u8> {
    let bytes = input
        .get(..len)
        .expect("fixture payload is shorter than its declared length")
        .to_vec();
    *input = &input[len..];
    bytes
}

fn decode_utf16_payload(bytes: Vec<u8>) -> Vec<u16> {
    let (pairs, remainder) = bytes.as_chunks::<2>();
    assert!(remainder.is_empty());
    pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect()
}

fn decode_fixture_path(input: &mut &[u8], encoding: u8) -> Vec<NativeName> {
    let count = read_uvarint(input) as usize;
    (0..count)
        .map(|_| {
            let units = read_uvarint(input) as usize;
            match encoding {
                1 => NativeName::unix(take_bytes(input, units)),
                2 => NativeName::windows(decode_utf16_payload(take_bytes(input, units * 2))),
                _ => panic!("unknown fixture path encoding {encoding}"),
            }
        })
        .collect()
}

fn decode_fixture_block(raw: &[u8], count: u32, encoding: u8) -> DecodedPaths {
    let mut input = raw;
    let context_len = read_uvarint(&mut input) as usize;
    let context_bytes = take_bytes(&mut input, context_len);
    let mut context_input = context_bytes.as_slice();
    let context = decode_fixture_path(&mut context_input, encoding);
    assert!(context_input.is_empty());
    let mut lengths = [0usize; RECORD_COLUMNS];
    for length in &mut lengths {
        *length = read_uvarint(&mut input) as usize;
    }
    let mut columns = Vec::with_capacity(RECORD_COLUMNS);
    for length in lengths {
        columns.push(take_bytes(&mut input, length));
    }

    let mut depths = Vec::with_capacity(count as usize);
    let mut names = Vec::with_capacity(count as usize);
    let mut previous_depth = context.len() as i64;
    let mut depth_column = columns[0].as_slice();
    let mut name_lengths = columns[3].as_slice();
    let mut name_payload = columns[4].as_slice();
    for _ in 0..count {
        let depth = previous_depth + read_svarint(&mut depth_column);
        depths.push(u16::try_from(depth).expect("fixture depth fits u16"));
        previous_depth = depth;
        let name_len = read_uvarint(&mut name_lengths) as usize;
        let name_bytes = name_len * if encoding == 2 { 2 } else { 1 };
        let mut encoded_name = &name_payload[..name_bytes];
        let name = match encoding {
            1 => NativeName::unix(take_bytes(&mut encoded_name, name_len)),
            2 => NativeName::windows(decode_utf16_payload(take_bytes(
                &mut encoded_name,
                name_len * 2,
            ))),
            _ => panic!("unknown fixture path encoding {encoding}"),
        };
        assert!(encoded_name.is_empty());
        name_payload = &name_payload[name_len * if encoding == 2 { 2 } else { 1 }..];
        names.push(name);
    }
    assert!(depth_column.is_empty());
    assert!(name_lengths.is_empty());
    assert!(name_payload.is_empty());
    assert!(input.is_empty());
    DecodedPaths {
        context,
        depths,
        names,
    }
}

fn replace_record_column(raw: &[u8], column: usize, replacement: &[u8]) -> Vec<u8> {
    let mut input = raw;
    let context_len = read_uvarint(&mut input) as usize;
    let context = take_bytes(&mut input, context_len);
    let mut columns = Vec::with_capacity(RECORD_COLUMNS);
    for _ in 0..RECORD_COLUMNS {
        columns.push(read_uvarint(&mut input) as usize);
    }
    let payloads: Vec<Vec<u8>> = columns
        .into_iter()
        .map(|length| take_bytes(&mut input, length))
        .collect();
    assert!(input.is_empty());

    let mut out = Vec::new();
    put_uvarint(&mut out, context_len as u64);
    out.extend_from_slice(&context);
    for (index, payload) in payloads.iter().enumerate() {
        put_uvarint(
            &mut out,
            if index == column {
                replacement.len() as u64
            } else {
                payload.len() as u64
            },
        );
    }
    for (index, payload) in payloads.iter().enumerate() {
        if index == column {
            out.extend_from_slice(replacement);
        } else {
            out.extend_from_slice(payload);
        }
    }
    out
}

fn record_column(raw: &[u8], column: usize) -> Vec<u8> {
    let mut input = raw;
    let context_len = read_uvarint(&mut input) as usize;
    take_bytes(&mut input, context_len);
    let mut lengths = [0usize; RECORD_COLUMNS];
    for length in &mut lengths {
        *length = read_uvarint(&mut input) as usize;
    }
    for (index, length) in lengths.into_iter().enumerate() {
        let payload = take_bytes(&mut input, length);
        if index == column {
            return payload;
        }
    }
    panic!("record column {column} is out of range");
}

fn parse_index(bytes: &[u8], encoding: u8) -> (u64, u32, u32, u64, Vec<IndexSpec>) {
    let trailer_start = bytes.len() - DDS2_TRAILER_LEN;
    let index_offset =
        u64::from_le_bytes(bytes[trailer_start..trailer_start + 8].try_into().unwrap());
    let index_len = u32::from_le_bytes(
        bytes[trailer_start + 8..trailer_start + 12]
            .try_into()
            .unwrap(),
    );
    let index_crc = u32::from_le_bytes(
        bytes[trailer_start + 12..trailer_start + 16]
            .try_into()
            .unwrap(),
    );
    let items = u64::from_le_bytes(
        bytes[trailer_start + 16..trailer_start + 24]
            .try_into()
            .unwrap(),
    );
    assert_eq!(&bytes[trailer_start + 24..], b"DDSE");
    let mut input = &bytes[index_offset as usize..trailer_start];
    let count = read_uvarint(&mut input) as usize;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let offset = read_uvarint(&mut input);
        let comp_len = read_uvarint(&mut input) as u32;
        let raw_len = read_uvarint(&mut input) as u32;
        let count = read_uvarint(&mut input) as u32;
        let crc = u32::from_le_bytes(take_bytes(&mut input, 4).try_into().unwrap());
        let first_path = decode_fixture_path(&mut input, encoding);
        entries.push(IndexSpec {
            offset,
            comp_len,
            raw_len,
            count,
            crc,
            first_path,
        });
    }
    assert!(input.is_empty());
    (index_offset, index_len, index_crc, items, entries)
}

fn wrapping_delta(current: u64, previous: u64) -> i64 {
    current.wrapping_sub(previous) as i64
}

fn encode_path(path: &[NativeName], out: &mut Vec<u8>) {
    put_uvarint(out, path.len() as u64);
    for component in path {
        match component {
            NativeName::Unix(bytes) => {
                put_uvarint(out, bytes.len() as u64);
                out.extend_from_slice(bytes);
            }
            NativeName::Windows(units) => {
                put_uvarint(out, units.len() as u64);
                for unit in units {
                    out.extend_from_slice(&unit.to_le_bytes());
                }
            }
        }
    }
}

fn encode_name(name: &NativeName, out: &mut Vec<u8>) {
    match name {
        NativeName::Unix(bytes) => out.extend_from_slice(bytes),
        NativeName::Windows(units) => {
            for unit in units {
                out.extend_from_slice(&unit.to_le_bytes());
            }
        }
    }
}

fn encode_record_block(context: &[NativeName], records: &[FixtureRecord]) -> BlockSpec {
    let mut columns: [Vec<u8>; RECORD_COLUMNS] = std::array::from_fn(|_| Vec::new());
    let mut previous_depth = context.len() as u16;
    let mut previous_alloc = 0;
    let mut previous_mtime = 0;
    let mut previous_mtime_nanos = 0;
    let mut previous_file_id = FileId::ZERO;
    for record in records {
        put_svarint(&mut columns[0], record.depth as i64 - previous_depth as i64);
        columns[1].push(record.kind as u8 | record.flags << 2);
        put_uvarint(&mut columns[2], record.available.0 as u64);
        put_uvarint(&mut columns[3], record.name.len() as u64);
        encode_name(&record.name, &mut columns[4]);
        put_uvarint(&mut columns[5], record.size);
        put_svarint(
            &mut columns[6],
            wrapping_delta(record.alloc, previous_alloc),
        );
        put_svarint(&mut columns[7], record.mtime.wrapping_sub(previous_mtime));
        put_svarint(
            &mut columns[8],
            record.mtime_nanos as i64 - previous_mtime_nanos as i64,
        );
        put_svarint(&mut columns[9], record.btime.wrapping_sub(record.mtime));
        put_svarint(
            &mut columns[10],
            record.btime_nanos as i64 - record.mtime_nanos as i64,
        );
        put_svarint(
            &mut columns[11],
            wrapping_delta(record.file_id.low, previous_file_id.low),
        );
        put_svarint(
            &mut columns[12],
            wrapping_delta(record.file_id.high, previous_file_id.high),
        );
        put_uvarint(
            &mut columns[13],
            (u64::from(record.nlink) << 16) | u64::from(record.vol),
        );

        previous_depth = record.depth;
        previous_alloc = record.alloc;
        previous_mtime = record.mtime;
        previous_mtime_nanos = record.mtime_nanos;
        previous_file_id = record.file_id;
    }

    let mut context_bytes = Vec::new();
    encode_path(context, &mut context_bytes);
    let mut raw = Vec::new();
    put_uvarint(&mut raw, context_bytes.len() as u64);
    raw.extend_from_slice(&context_bytes);
    for column in &columns {
        put_uvarint(&mut raw, column.len() as u64);
    }
    for column in columns {
        raw.extend_from_slice(&column);
    }
    let mut first_path = context.to_vec();
    if let Some(first) = records.first() {
        first_path.push(first.name.clone());
    }
    BlockSpec {
        raw,
        count: records.len() as u32,
        first_path,
    }
}

fn encode_delta_remove(parent: &[NativeName], name: &NativeName) -> Vec<u8> {
    let mut out = vec![1];
    put_uvarint(&mut out, 0);
    put_uvarint(&mut out, parent.len() as u64);
    for component in parent {
        put_uvarint(&mut out, component.len() as u64);
        encode_name(component, &mut out);
    }
    put_uvarint(&mut out, name.len() as u64);
    encode_name(name, &mut out);
    out
}

fn header(kind: u8, encoding: u8, schema: u8, stream_flags: u8) -> [u8; 8] {
    [
        DDS2_MAGIC[0],
        DDS2_MAGIC[1],
        DDS2_MAGIC[2],
        DDS2_MAGIC[3],
        kind,
        encoding,
        schema,
        stream_flags,
    ]
}

fn build_file(
    header: [u8; 8],
    blocks: &[BlockSpec],
    item_count: Option<u64>,
    index_suffix: &[u8],
) -> Vec<u8> {
    let mut file = header.to_vec();
    let mut entries = Vec::new();
    let mut items = 0;
    for block in blocks {
        let compressed = zstd::bulk::compress(&block.raw, 3).unwrap();
        let offset = file.len() as u64;
        file.extend_from_slice(&compressed);
        entries.push((
            offset,
            compressed.len() as u32,
            block.raw.len() as u32,
            block.count,
            crc32fast::hash(&compressed),
            block.first_path.clone(),
        ));
        items += u64::from(block.count);
    }
    let index_offset = file.len() as u64;
    let mut index = Vec::new();
    put_uvarint(&mut index, entries.len() as u64);
    for (offset, compressed_len, raw_len, count, crc, first_path) in entries {
        put_uvarint(&mut index, offset);
        put_uvarint(&mut index, u64::from(compressed_len));
        put_uvarint(&mut index, u64::from(raw_len));
        put_uvarint(&mut index, u64::from(count));
        index.extend_from_slice(&crc.to_le_bytes());
        encode_path(&first_path, &mut index);
    }
    index.extend_from_slice(index_suffix);
    let index_crc = crc32fast::hash(&index);
    file.extend_from_slice(&index);
    file.extend_from_slice(&index_offset.to_le_bytes());
    file.extend_from_slice(&(index.len() as u32).to_le_bytes());
    file.extend_from_slice(&index_crc.to_le_bytes());
    file.extend_from_slice(&item_count.unwrap_or(items).to_le_bytes());
    file.extend_from_slice(b"DDSE");
    file
}

fn fixture_path(name: &str, bytes: &[u8]) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/dds2-provisional");
    fs::create_dir_all(&root).unwrap();
    let path = root.join(format!("{name}.dds"));
    fs::write(&path, bytes).unwrap();
    path
}

fn simple_record(name: NativeName) -> FixtureRecord {
    FixtureRecord {
        depth: 0,
        name,
        kind: Kind::File,
        flags: 0,
        available: EntryFields::KIND
            .union(EntryFields::LOGICAL_SIZE)
            .union(EntryFields::ALLOCATED_SIZE)
            .union(EntryFields::MTIME)
            .union(EntryFields::MTIME_NANOS)
            .union(EntryFields::BIRTH_TIME)
            .union(EntryFields::BIRTH_TIME_NANOS)
            .union(EntryFields::FILE_ID)
            .union(EntryFields::LINK_COUNT),
        size: 7,
        alloc: 4096,
        mtime: 1,
        mtime_nanos: 0,
        btime: 1,
        btime_nanos: 0,
        file_id: FileId { low: 7, high: 0 },
        nlink: 1,
        vol: 0,
    }
}

fn delta_record(name: &[u8], size: u64) -> Record {
    Record {
        depth: 1,
        name: NativeName::unix(name.to_vec()),
        kind: Kind::File,
        size,
        alloc: 4096,
        mtime: 10 + size as i64,
        file_id: FileId {
            low: 100 + size,
            high: 0,
        },
        nlink: 1,
        ..Default::default()
    }
}

fn valid_record_file(name: &str, encoding: u8, records: Vec<FixtureRecord>) -> PathBuf {
    let block = encode_record_block(&[], &records);
    fixture_path(
        name,
        &build_file(header(2, encoding, 1, 0), &[block], None, &[]),
    )
}

fn valid_volume(timestamp_resolution: TimestampResolution) -> VolumeDescriptor {
    valid_volume_at(0, timestamp_resolution)
}

fn valid_volume_at(index: u16, timestamp_resolution: TimestampResolution) -> VolumeDescriptor {
    let mut volume = VolumeDescriptor::new(
        index,
        "macos",
        FileIdScheme::PosixInode64,
        FileIdScope::Volume,
        AllocationSemantics::FilesystemReported,
        timestamp_resolution,
        Provenance::Observed("test".into()),
    );
    volume.identity = NativePath::unix(b"volume-id".to_vec());
    volume.mount_key = NativePath::unix(b"/".to_vec());
    volume.identity_strength = "uuid".into();
    volume
}

fn expected_error(path: &Path, expected: &str) {
    let error = match DataFile::open(path) {
        Ok(_) => panic!("DDS2 fixture unexpectedly opened: {}", path.display()),
        Err(error) => error,
    };
    let text = error.to_string();
    assert!(
        text.contains(expected),
        "expected {expected:?} in {text:?} for {}",
        path.display()
    );
}

#[test]
fn dds2_header_is_eight_bytes_and_has_the_frozen_descriptor_fields() {
    let bytes = build_file(header(2, 1, 1, 0), &[], Some(0), &[]);
    assert_eq!(&bytes[..8], b"DDS2\x02\x01\x01\x00");
    assert_eq!(
        bytes.len() - 8 - DDS2_TRAILER_LEN,
        1,
        "empty index is one byte"
    );
    let trailer_start = bytes.len() - DDS2_TRAILER_LEN;
    assert_eq!(
        u64::from_le_bytes(bytes[trailer_start..trailer_start + 8].try_into().unwrap()),
        8
    );
    let path = fixture_path("header-records", &bytes);
    let data = DataFile::open(&path).expect("DDS2 readers must accept the frozen header");
    assert_eq!(data.kind, FileKind::Records);
}

#[test]
fn dds2_unix_invalid_bytes_and_fixed_column_order_are_golden() {
    let record = FixtureRecord {
        depth: 0,
        name: NativeName::unix(b"a\xffb".to_vec()),
        kind: Kind::File,
        flags: 0,
        available: EntryFields(0x01ff),
        size: 7,
        alloc: 4096,
        mtime: -1,
        mtime_nanos: 123,
        btime: -2,
        btime_nanos: 456,
        file_id: FileId {
            low: u64::MAX,
            high: 1,
        },
        nlink: 2,
        vol: 1,
    };
    let block = encode_record_block(&[], std::slice::from_ref(&record));
    assert_eq!(
        block.raw,
        vec![
            0x01, 0x00, // empty path context
            0x01, 0x01, 0x02, 0x01, 0x03, 0x01, 0x02, 0x01, 0x02, 0x01, 0x02, 0x01, 0x01,
            0x03, // fixed column lengths
            0x00, 0x00, 0xff, 0x03, 0x03, 0x61, 0xff, 0x62, 0x07, 0x80, 0x40, 0x01, 0xf6, 0x01,
            0x01, 0x9a, 0x05, 0x01, 0x02, 0x81, 0x80, 0x08,
        ]
    );
    let path = fixture_path(
        "records-unix-invalid-bytes",
        &build_file(header(2, 1, 1, 0), &[block], None, &[]),
    );
    let data = DataFile::open(&path).expect("DDS2 Unix reader must decode invalid bytes");
    let records: Vec<Record> = data
        .records_from(0)
        .unwrap()
        .collect::<io::Result<_>>()
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].name, NativeName::unix(b"a\xffb".to_vec()));
    assert_eq!(records[0].file_id, record.file_id);
    assert_eq!(records[0].mtime_nanos, 123);
    assert_eq!(records[0].btime_nanos, 456);
}

#[test]
fn dds2_windows_utf16_keeps_lone_surrogates_and_numeric_order() {
    let root = NativeName::windows(vec![b'R' as u16]);
    let deliberately_noncanonical = vec![
        NativeName::windows(vec![0xd800]),
        NativeName::windows(vec![0x0100]),
        NativeName::windows(vec![0x00ff]),
    ];
    let expected_numeric_order = vec![
        NativeName::windows(vec![0x00ff]),
        NativeName::windows(vec![0x0100]),
        NativeName::windows(vec![0xd800]),
    ];
    assert_ne!(deliberately_noncanonical, expected_numeric_order);
    let mut canonical = deliberately_noncanonical.clone();
    canonical.sort_by(|left, right| {
        compare_paths(
            &[root.clone(), left.clone()],
            &[root.clone(), right.clone()],
        )
        .unwrap()
    });
    assert_eq!(canonical, expected_numeric_order);

    let mut blocks = vec![encode_record_block(
        &[],
        &[FixtureRecord {
            depth: 0,
            name: root.clone(),
            ..simple_record(root.clone())
        }],
    )];
    blocks.extend(canonical.iter().map(|name| {
        encode_record_block(
            std::slice::from_ref(&root),
            &[FixtureRecord {
                depth: 1,
                name: name.clone(),
                ..simple_record(name.clone())
            }],
        )
    }));
    let bytes = build_file(header(2, 2, 1, 0), &blocks, None, &[]);
    let (_, _, _, _, entries) = parse_index(&bytes, 2);
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.first_path.clone())
            .collect::<Vec<_>>(),
        vec![
            vec![root.clone()],
            vec![root.clone(), expected_numeric_order[0].clone()],
            vec![root.clone(), expected_numeric_order[1].clone()],
            vec![root.clone(), expected_numeric_order[2].clone()],
        ]
    );

    let records = std::iter::once(FixtureRecord {
        depth: 0,
        name: root.clone(),
        ..simple_record(root)
    })
    .chain(canonical.into_iter().map(|name| FixtureRecord {
        depth: 1,
        name: name.clone(),
        ..simple_record(name)
    }))
    .collect();
    let path = valid_record_file("records-windows-wide-order", 2, records);
    let data = DataFile::open(&path).expect("DDS2 Windows reader must decode raw UTF-16");
    let decoded: Vec<Record> = data
        .records_from(0)
        .unwrap()
        .collect::<io::Result<_>>()
        .unwrap();
    let names: Vec<_> = decoded.into_iter().map(|record| record.name).collect();
    assert_eq!(
        names,
        std::iter::once(NativeName::windows(vec![b'R' as u16]))
            .chain(expected_numeric_order)
            .collect::<Vec<_>>()
    );
}

#[test]
fn dds2_unsigned_and_zigzag_values_use_canonical_varints() {
    let mut unsigned = Vec::new();
    put_uvarint(&mut unsigned, 0);
    put_uvarint(&mut unsigned, 127);
    put_uvarint(&mut unsigned, 128);
    put_uvarint(&mut unsigned, u64::MAX);
    assert_eq!(
        unsigned,
        vec![
            0, 0x7f, 0x80, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01
        ]
    );
    let mut signed = Vec::new();
    put_svarint(&mut signed, -1);
    put_svarint(&mut signed, 0);
    put_svarint(&mut signed, i64::MIN);
    put_svarint(&mut signed, i64::MAX);
    assert_eq!(
        signed,
        vec![
            1, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01, 0xfe, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01
        ]
    );
    let path = valid_record_file(
        "records-varints",
        1,
        vec![simple_record(NativeName::unix(b"v".to_vec()))],
    );
    let data = DataFile::open(&path).expect("DDS2 reader must decode canonical varints");
    assert_eq!(data.verify().unwrap(), 1);
}

#[test]
fn dds2_wrapping_64_bit_deltas_reconstruct_every_u64_value() {
    let mut first = simple_record(NativeName::unix(b"a".to_vec()));
    first.size = u64::MAX;
    first.alloc = u64::MAX;
    first.file_id = FileId {
        low: u64::MAX,
        high: u64::MAX,
    };
    let mut second = simple_record(NativeName::unix(b"b".to_vec()));
    second.size = 0;
    second.alloc = 0;
    second.file_id = FileId::ZERO;
    let path = valid_record_file("records-wrapping-deltas", 1, vec![first, second]);
    let data = DataFile::open(&path).expect("DDS2 reader must reconstruct wrapping deltas");
    let decoded: Vec<Record> = data
        .records_from(0)
        .unwrap()
        .collect::<io::Result<_>>()
        .unwrap();
    assert_eq!(decoded[0].size, u64::MAX);
    assert_eq!(decoded[1].size, 0);
    assert_eq!(
        decoded[0].file_id,
        FileId {
            low: u64::MAX,
            high: u64::MAX
        }
    );
    assert_eq!(decoded[1].file_id, FileId::ZERO);
}

#[test]
fn dds2_nanoseconds_and_pre_1970_floor_seconds_round_trip() {
    let record = FixtureRecord {
        name: NativeName::unix(b"time".to_vec()),
        mtime: -2,
        mtime_nanos: 123_456_789,
        btime: -3,
        btime_nanos: 987_654_321,
        ..simple_record(NativeName::unix(b"time".to_vec()))
    };
    let path = valid_record_file("records-pre-epoch-nanos", 1, vec![record]);
    let data = DataFile::open(&path).expect("DDS2 reader must preserve nanosecond timestamps");
    let record = data.records_from(0).unwrap().next().unwrap().unwrap();
    assert_eq!(record.mtime, -2);
    assert_eq!(record.mtime_nanos, 123_456_789);
    assert_eq!(record.btime, -3);
    assert_eq!(record.btime_nanos, 987_654_321);
    assert_eq!(
        record.timestamp_resolution,
        TimestampResolution::Nanoseconds
    );
}

#[test]
fn dds2_uses_the_volume_timestamp_resolution_when_a_table_is_supplied() {
    let path = valid_record_file(
        "records-volume-timestamp-resolution",
        1,
        vec![simple_record(NativeName::unix(b"time".to_vec()))],
    );
    let data =
        DataFile::open_with_volumes(&path, &[valid_volume(TimestampResolution::Milliseconds)])
            .expect("DDS2 reader must validate the volume table");
    let record = data.records_from(0).unwrap().next().unwrap().unwrap();
    assert_eq!(
        record.timestamp_resolution,
        TimestampResolution::Milliseconds
    );
}

#[test]
fn dds2_file_identity_has_both_128_bit_halves() {
    let identity = FileId {
        low: 0x0123_4567_89ab_cdef,
        high: 0xfedc_ba98_7654_3210,
    };
    let mut record = simple_record(NativeName::unix(b"id".to_vec()));
    record.file_id = identity;
    let path = valid_record_file("records-file-id-128", 1, vec![record]);
    let data = DataFile::open(&path).expect("DDS2 reader must expose 128-bit IDs");
    let decoded = data.records_from(0).unwrap().next().unwrap().unwrap();
    assert_eq!(decoded.file_id, identity);
}

#[test]
fn dds2_availability_controls_payload_visibility_and_dependencies() {
    let mut record = simple_record(NativeName::unix(b"availability".to_vec()));
    record.available = EntryFields::KIND
        .union(EntryFields::MTIME)
        .union(EntryFields::MTIME_NANOS);
    record.size = 99;
    record.file_id = FileId { low: 99, high: 99 };
    let path = valid_record_file("records-availability", 1, vec![record]);
    let data = DataFile::open(&path).expect("DDS2 reader must apply availability bits");
    let decoded = data.records_from(0).unwrap().next().unwrap().unwrap();
    assert_eq!(
        decoded.available,
        EntryFields::KIND
            .union(EntryFields::MTIME)
            .union(EntryFields::MTIME_NANOS)
    );
    assert_eq!(decoded.size, 0, "unavailable size is not observed zero");
    assert_eq!(decoded.file_id, FileId::ZERO);
}

#[test]
fn dds2_packs_link_count_and_volume_index_in_the_frozen_column() {
    let mut record = simple_record(NativeName::unix(b"packed".to_vec()));
    record.nlink = 0x1234;
    record.vol = 0xabcd;
    let block = encode_record_block(&[], std::slice::from_ref(&record));
    assert!(
        block
            .raw
            .windows(5)
            .any(|window| window == [0xcd, 0xd7, 0xd2, 0x91, 0x01])
    );
    let path = fixture_path(
        "records-packed-link-volume",
        &build_file(header(2, 1, 1, 0), &[block], None, &[]),
    );
    let data = DataFile::open(&path).expect("DDS2 reader must unpack link and volume fields");
    let decoded = data.records_from(0).unwrap().next().unwrap().unwrap();
    assert_eq!(decoded.nlink, 0x1234);
    assert_eq!(decoded.vol, 0xabcd);
}

#[test]
fn dds2_volume_indices_are_validated_with_the_metadata_table() {
    let mut record = simple_record(NativeName::unix(b"volume".to_vec()));
    record.vol = 1;
    let path = valid_record_file("reject-volume-index", 1, vec![record]);
    let mut volume = VolumeDescriptor::new(
        0,
        "macos",
        FileIdScheme::PosixInode64,
        FileIdScope::Volume,
        AllocationSemantics::FilesystemReported,
        TimestampResolution::Seconds,
        Provenance::Observed("test".into()),
    );
    volume.identity = NativePath::unix(b"volume-id".to_vec());
    volume.mount_key = NativePath::unix(b"/".to_vec());
    volume.identity_strength = "uuid".into();
    let error = match DataFile::open_with_volumes(&path, &[volume]) {
        Ok(_) => panic!("DDS2 fixture unexpectedly opened with an invalid volume index"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("invalid volume index"));
}

#[test]
fn dds2_accepts_records_on_non_root_volume_indices() {
    let mut first = simple_record(NativeName::unix(b"first-volume".to_vec()));
    first.vol = 1;
    let mut second = simple_record(NativeName::unix(b"second-volume".to_vec()));
    second.vol = 2;
    let path = valid_record_file("records-non-root-volume-indices", 1, vec![first, second]);
    let volumes = [
        valid_volume(TimestampResolution::Seconds),
        valid_volume_at(1, TimestampResolution::Seconds),
        valid_volume_at(2, TimestampResolution::Seconds),
    ];
    let data = DataFile::open_with_volumes(&path, &volumes)
        .expect("DDS2 readers must accept every index in the complete table");
    let records = data
        .records_from(0)
        .unwrap()
        .collect::<io::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        records.iter().map(|record| record.vol).collect::<Vec<_>>(),
        [1, 2]
    );
}

#[test]
fn dds2_context_indexes_crc_and_trailer_are_golden() {
    let first = encode_record_block(&[], &[simple_record(NativeName::unix(b"first".to_vec()))]);
    let second = encode_record_block(
        &[NativeName::unix(b"first".to_vec())],
        &[FixtureRecord {
            depth: 1,
            name: NativeName::unix(b"second".to_vec()),
            ..simple_record(NativeName::unix(b"second".to_vec()))
        }],
    );
    let bytes = build_file(
        header(2, 1, 1, 0),
        &[first.clone(), second.clone()],
        None,
        &[],
    );
    let (index_offset, index_len, index_crc, items, entries) = parse_index(&bytes, 1);
    let first_compressed = zstd::bulk::compress(&first.raw, 3).unwrap();
    let second_compressed = zstd::bulk::compress(&second.raw, 3).unwrap();
    assert_eq!(
        index_offset,
        8 + first_compressed.len() as u64 + second_compressed.len() as u64
    );
    assert_eq!(
        index_len as usize,
        bytes.len() - DDS2_TRAILER_LEN - index_offset as usize
    );
    assert_eq!(items, 2);
    assert_eq!(
        entries,
        vec![
            IndexSpec {
                offset: 8,
                comp_len: first_compressed.len() as u32,
                raw_len: first.raw.len() as u32,
                count: 1,
                crc: crc32fast::hash(&first_compressed),
                first_path: vec![NativeName::unix(b"first".to_vec())],
            },
            IndexSpec {
                offset: 8 + first_compressed.len() as u64,
                comp_len: second_compressed.len() as u32,
                raw_len: second.raw.len() as u32,
                count: 1,
                crc: crc32fast::hash(&second_compressed),
                first_path: vec![
                    NativeName::unix(b"first".to_vec()),
                    NativeName::unix(b"second".to_vec()),
                ],
            },
        ]
    );
    let trailer_start = bytes.len() - DDS2_TRAILER_LEN;
    let index = &bytes[index_offset as usize..trailer_start];
    assert_eq!(crc32fast::hash(index), index_crc);
    assert_eq!(&bytes[trailer_start + 24..], b"DDSE");
    assert_eq!(
        decode_fixture_block(
            &zstd::bulk::decompress(&bytes[8..8 + first_compressed.len()], first.raw.len())
                .unwrap(),
            1,
            1,
        ),
        DecodedPaths {
            context: vec![],
            depths: vec![0],
            names: vec![NativeName::unix(b"first".to_vec())],
        }
    );
    assert_eq!(
        decode_fixture_block(
            &zstd::bulk::decompress(
                &bytes[8 + first_compressed.len()..index_offset as usize],
                second.raw.len(),
            )
            .unwrap(),
            1,
            1,
        ),
        DecodedPaths {
            context: vec![NativeName::unix(b"first".to_vec())],
            depths: vec![1],
            names: vec![NativeName::unix(b"second".to_vec())],
        }
    );
    let path = fixture_path("records-context-index-crc-trailer", &bytes);
    let data = DataFile::open(&path).expect("DDS2 reader must validate context and index");
    assert_eq!(data.index.len(), 2);
    assert_eq!(data.verify().unwrap(), 2);
}

#[test]
fn dds2_rejects_a_first_record_whose_depth_does_not_match_context() {
    let root = NativeName::unix(b"root".to_vec());
    let child = NativeName::unix(b"child".to_vec());
    let block = encode_record_block(
        std::slice::from_ref(&root),
        &[FixtureRecord {
            depth: 0,
            name: child.clone(),
            ..simple_record(child)
        }],
    );
    let path = fixture_path(
        "reject-record-context-depth",
        &build_file(header(2, 1, 1, 0), &[block], None, &[]),
    );
    expected_error(&path, "first record depth does not match block context");
}

#[test]
fn dds2_rejects_out_of_order_records_within_a_block() {
    let records = [
        FixtureRecord {
            name: NativeName::unix(b"b".to_vec()),
            ..simple_record(NativeName::unix(b"b".to_vec()))
        },
        FixtureRecord {
            name: NativeName::unix(b"a".to_vec()),
            ..simple_record(NativeName::unix(b"a".to_vec()))
        },
    ];
    let block = encode_record_block(&[], &records);
    let path = fixture_path(
        "reject-record-order-within-block",
        &build_file(header(2, 1, 1, 0), &[block], None, &[]),
    );
    expected_error(&path, "non-canonical record paths");
}

#[test]
fn dds2_rejects_records_out_of_order_across_blocks() {
    let first = encode_record_block(
        &[],
        &[
            FixtureRecord {
                name: NativeName::unix(b"a".to_vec()),
                ..simple_record(NativeName::unix(b"a".to_vec()))
            },
            FixtureRecord {
                name: NativeName::unix(b"z".to_vec()),
                ..simple_record(NativeName::unix(b"z".to_vec()))
            },
        ],
    );
    let second = encode_record_block(
        &[],
        &[FixtureRecord {
            name: NativeName::unix(b"b".to_vec()),
            ..simple_record(NativeName::unix(b"b".to_vec()))
        }],
    );
    let path = fixture_path(
        "reject-record-order-across-blocks",
        &build_file(header(2, 1, 1, 0), &[first, second], None, &[]),
    );
    expected_error(&path, "non-canonical record paths");
}

#[test]
fn dds2_block_crc_mismatch_is_rejected() {
    let block = encode_record_block(&[], &[simple_record(NativeName::unix(b"crc".to_vec()))]);
    let mut bytes = build_file(header(2, 1, 1, 0), &[block], None, &[]);
    bytes[8] ^= 0x01;
    let path = fixture_path("reject-block-crc", &bytes);
    expected_error(&path, "block checksum mismatch");
}

#[test]
fn dds2_index_crc_mismatch_is_rejected() {
    let block = encode_record_block(
        &[],
        &[simple_record(NativeName::unix(b"index-crc".to_vec()))],
    );
    let mut bytes = build_file(header(2, 1, 1, 0), &[block], None, &[]);
    let trailer_start = bytes.len() - DDS2_TRAILER_LEN;
    bytes[trailer_start + 12] ^= 0x01;
    let path = fixture_path("reject-index-crc", &bytes);
    expected_error(&path, "index checksum mismatch");
}

#[test]
fn dds2_delta_remove_operation_is_golden() {
    let raw = encode_delta_remove(
        &[NativeName::unix(b"root".to_vec())],
        &NativeName::unix(b"gone".to_vec()),
    );
    assert_eq!(
        raw,
        vec![
            1, 0, 1, 4, b'r', b'o', b'o', b't', 4, b'g', b'o', b'n', b'e'
        ]
    );
    let block = BlockSpec {
        raw,
        count: 1,
        first_path: vec![
            NativeName::unix(b"root".to_vec()),
            NativeName::unix(b"gone".to_vec()),
        ],
    };
    let path = fixture_path(
        "delta-remove-golden",
        &build_file(header(1, 1, 1, 0), &[block], None, &[]),
    );
    let data = DataFile::open(&path).expect("DDS2 reader must decode delta operations");
    let op = data.ops().unwrap().next().unwrap().unwrap();
    assert_eq!(op.record.name, NativeName::unix(b"gone".to_vec()));
}

#[test]
fn dds2_rejects_out_of_order_ops_within_a_block() {
    let mut raw = encode_delta_remove(&[], &NativeName::unix(b"b".to_vec()));
    raw.extend(encode_delta_remove(&[], &NativeName::unix(b"a".to_vec())));
    let block = BlockSpec {
        raw,
        count: 2,
        first_path: vec![NativeName::unix(b"b".to_vec())],
    };
    let path = fixture_path(
        "reject-op-order-within-block",
        &build_file(header(1, 1, 1, 0), &[block], None, &[]),
    );
    expected_error(&path, "non-canonical delta paths");
}

#[test]
fn dds2_rejects_ops_out_of_order_across_blocks() {
    let mut first_raw = encode_delta_remove(&[], &NativeName::unix(b"a".to_vec()));
    first_raw.extend(encode_delta_remove(&[], &NativeName::unix(b"z".to_vec())));
    let first = BlockSpec {
        raw: first_raw,
        count: 2,
        first_path: vec![NativeName::unix(b"a".to_vec())],
    };
    let second = BlockSpec {
        raw: encode_delta_remove(&[], &NativeName::unix(b"b".to_vec())),
        count: 1,
        first_path: vec![NativeName::unix(b"b".to_vec())],
    };
    let path = fixture_path(
        "reject-op-order-across-blocks",
        &build_file(header(1, 1, 1, 0), &[first, second], None, &[]),
    );
    expected_error(&path, "non-canonical delta paths");
}

#[test]
fn dds2_delta_golden_preserves_kinds_parents_payloads_and_reconstruction() {
    use diskdrift_core::delta::{Apply, OpDecoder, OpEncoder, OpKind, compute, op_order};

    let root = Record {
        depth: 0,
        name: NativeName::unix(b"/r".to_vec()),
        kind: Kind::Dir,
        ..Default::default()
    };
    let base = vec![
        root.clone(),
        delta_record(b"a", 1),
        delta_record(b"gone", 2),
    ];
    let replaced = delta_record(b"a", 9);
    let added = delta_record(b"new", 3);
    let new = vec![root, replaced.clone(), added.clone()];
    let stream = |records: Vec<Record>| -> diskdrift_core::diff::RecordStream<'static> {
        Box::new(records.into_iter().map(Ok))
    };
    let ops = compute(stream(base.clone()), stream(new.clone())).unwrap();

    assert_eq!(ops.len(), 3);
    assert_eq!(ops[0].kind, OpKind::Replace);
    assert_eq!(ops[0].parent, vec![NativeName::unix(b"/r".to_vec())]);
    assert_eq!(ops[0].record, replaced);
    assert_eq!(ops[1].kind, OpKind::Remove);
    assert_eq!(ops[1].parent, vec![NativeName::unix(b"/r".to_vec())]);
    assert_eq!(
        ops[1].record,
        Record {
            depth: 1,
            name: NativeName::unix(b"gone".to_vec()),
            ..Default::default()
        }
    );
    assert_eq!(ops[2].kind, OpKind::Add);
    assert_eq!(ops[2].parent, vec![NativeName::unix(b"/r".to_vec())]);
    assert_eq!(ops[2].record, added);
    assert_eq!(ops[0].record.depth, 1);
    assert_eq!(ops[1].record.depth, 1);
    assert_eq!(ops[2].record.depth, 1);
    for pair in ops.windows(2) {
        assert_eq!(
            op_order(&pair[0], &pair[1]).unwrap(),
            std::cmp::Ordering::Less
        );
    }

    let mut encoded = Vec::new();
    let mut encoder = OpEncoder::new();
    for op in &ops {
        encoder.encode(op, &mut encoded).unwrap();
    }
    assert_eq!(
        &encoded[..1],
        &[OpKind::Replace as u8],
        "the golden stream starts with the replacement opcode"
    );
    let mut input = encoded.as_slice();
    let mut decoder = OpDecoder::new();
    let decoded: Vec<_> = (0..ops.len())
        .map(|_| decoder.decode(&mut input).unwrap())
        .collect();
    assert!(input.is_empty());
    assert_eq!(decoded, ops);

    let rebuilt: Vec<Record> = Apply::new(stream(base), Box::new(decoded.into_iter().map(Ok)))
        .collect::<std::io::Result<_>>()
        .unwrap();
    assert_eq!(rebuilt, new);
}

#[test]
fn dds1_and_dds2_lineage_descriptors_accept_only_the_compatible_pairs() {
    let dds1 = StreamDescriptor::DDS1;
    let dds2_unix = StreamDescriptor::dds2(1);
    let dds2_windows = StreamDescriptor::dds2(2);
    let dds2_future = StreamDescriptor {
        record_schema: 2,
        ..dds2_unix
    };
    let dds2_flagged = StreamDescriptor {
        stream_flags: 1,
        ..dds2_unix
    };

    assert!(dds1.lineage_compatible(dds2_unix));
    assert!(dds2_unix.lineage_compatible(dds1));
    assert!(dds2_unix.lineage_compatible(dds2_unix));
    assert!(!dds1.lineage_compatible(dds2_windows));
    assert!(!dds2_unix.lineage_compatible(dds2_windows));
    assert!(!dds2_unix.lineage_compatible(dds2_future));
    assert!(!dds2_unix.lineage_compatible(dds2_flagged));
}

#[test]
fn dds2_unknown_file_kind_is_rejected() {
    let path = fixture_path(
        "reject-unknown-kind",
        &build_file(header(3, 1, 1, 0), &[], Some(0), &[]),
    );
    expected_error(&path, "unsupported data kind");
}

#[test]
fn dds2_unknown_path_encoding_is_rejected() {
    let path = fixture_path(
        "reject-unknown-encoding",
        &build_file(header(2, 3, 1, 0), &[], Some(0), &[]),
    );
    expected_error(&path, "unknown path encoding");
}

#[test]
fn dds2_unknown_schema_is_rejected() {
    let path = fixture_path(
        "reject-unknown-schema",
        &build_file(header(2, 1, 2, 0), &[], Some(0), &[]),
    );
    expected_error(&path, "unsupported record schema");
}

#[test]
fn dds2_unknown_stream_flags_are_rejected() {
    let path = fixture_path(
        "reject-unknown-flags",
        &build_file(header(2, 1, 1, 1), &[], Some(0), &[]),
    );
    expected_error(&path, "unknown stream flags");
}

#[test]
fn dds2_reserved_availability_bits_are_rejected() {
    let mut record = simple_record(NativeName::unix(b"reserved".to_vec()));
    record.available = EntryFields(0xfe00);
    let block = encode_record_block(&[], &[record]);
    let path = fixture_path(
        "reject-reserved-availability",
        &build_file(header(2, 1, 1, 0), &[block], None, &[]),
    );
    expected_error(&path, "reserved availability bits");
}

#[test]
fn dds2_mtime_nanoseconds_require_mtime_availability() {
    let mut record = simple_record(NativeName::unix(b"mtime-nanos".to_vec()));
    record.available = EntryFields::KIND.union(EntryFields::MTIME_NANOS);
    let path = valid_record_file("reject-mtime-nanos-dependency", 1, vec![record]);
    expected_error(&path, "availability dependency");
}

#[test]
fn dds2_birth_nanoseconds_require_birth_time_availability() {
    let mut record = simple_record(NativeName::unix(b"btime-nanos".to_vec()));
    record.available = EntryFields::KIND.union(EntryFields::BIRTH_TIME_NANOS);
    let path = valid_record_file("reject-btime-nanos-dependency", 1, vec![record]);
    expected_error(&path, "availability dependency");
}

#[test]
fn dds2_noncanonical_varints_are_rejected() {
    let mut block = encode_record_block(
        &[],
        &[simple_record(NativeName::unix(b"noncanonical".to_vec()))],
    );
    // Zero encoded as two bytes is non-canonical; it still decodes to the
    // original value, so a decoder must reach canonical-varint validation.
    block.raw.splice(0..1, [0x80, 0x00]);
    let mut noncanonical = block.raw.as_slice();
    assert_eq!(read_uvarint(&mut noncanonical), 0);
    assert_eq!(&block.raw[..2], &[0x80, 0x00]);
    assert_eq!(noncanonical.first(), Some(&0));
    let path = fixture_path(
        "reject-noncanonical-varint",
        &build_file(header(2, 1, 1, 0), &[block], None, &[]),
    );
    expected_error(&path, "non-canonical varint");
}

#[test]
fn dds2_varint_overflow_is_rejected() {
    let mut block = encode_record_block(
        &[],
        &[simple_record(NativeName::unix(b"overflow".to_vec()))],
    );
    block.raw.splice(0..1, [0xff; 11]);
    let path = fixture_path(
        "reject-varint-overflow",
        &build_file(header(2, 1, 1, 0), &[block], None, &[]),
    );
    expected_error(&path, "varint overflow");
}

#[test]
fn dds2_invalid_nanoseconds_are_rejected() {
    let mut record = simple_record(NativeName::unix(b"invalid-nanos".to_vec()));
    record.mtime_nanos = 1_000_000_000;
    let path = valid_record_file("reject-invalid-nanoseconds", 1, vec![record]);
    expected_error(&path, "invalid nanoseconds");
}

#[test]
fn dds2_narrow_packed_fields_are_rejected() {
    let mut record = simple_record(NativeName::unix(b"narrow".to_vec()));
    record.nlink = u32::MAX;
    record.vol = u16::MAX;
    let block = encode_record_block(&[], &[record]);
    let mut packed = Vec::new();
    put_uvarint(
        &mut packed,
        ((u64::from(u32::MAX) + 1) << 16) | u64::from(u16::MAX),
    );
    let raw = replace_record_column(&block.raw, 13, &packed);
    let packed_column = record_column(&raw, 13);
    let mut packed_input = packed_column.as_slice();
    assert_eq!(
        read_uvarint(&mut packed_input),
        ((u64::from(u32::MAX) + 1) << 16) | u64::from(u16::MAX)
    );
    assert!(packed_input.is_empty());
    let block = BlockSpec { raw, ..block };
    let path = fixture_path(
        "reject-narrow-packed-fields",
        &build_file(header(2, 1, 1, 0), &[block], None, &[]),
    );
    expected_error(&path, "narrow packed field");
}

#[test]
fn dds2_malformed_columns_are_rejected() {
    let mut block =
        encode_record_block(&[], &[simple_record(NativeName::unix(b"columns".to_vec()))]);
    block.raw[2] = 0;
    let path = fixture_path(
        "reject-malformed-columns",
        &build_file(header(2, 1, 1, 0), &[block], None, &[]),
    );
    expected_error(&path, "malformed records columns");
}

#[test]
fn dds2_index_bounds_are_rejected() {
    let block = encode_record_block(&[], &[simple_record(NativeName::unix(b"bounds".to_vec()))]);
    let mut bytes = build_file(header(2, 1, 1, 0), &[block], None, &[]);
    let trailer_start = bytes.len() - DDS2_TRAILER_LEN;
    bytes[trailer_start..trailer_start + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    let path = fixture_path("reject-index-bounds", &bytes);
    expected_error(&path, "index out of bounds");
}

#[test]
fn dds2_trailing_index_bytes_are_rejected() {
    let block = encode_record_block(
        &[],
        &[simple_record(NativeName::unix(b"index-tail".to_vec()))],
    );
    let path = fixture_path(
        "reject-trailing-index-bytes",
        &build_file(header(2, 1, 1, 0), &[block], None, &[0xaa]),
    );
    expected_error(&path, "trailing bytes in index");
}

#[test]
fn dds2_trailing_bytes_after_trailer_are_rejected() {
    let block = encode_record_block(
        &[],
        &[simple_record(NativeName::unix(b"trailer-tail".to_vec()))],
    );
    let mut bytes = build_file(header(2, 1, 1, 0), &[block], None, &[]);
    bytes.push(0xaa);
    let path = fixture_path("reject-trailing-after-trailer", &bytes);
    expected_error(&path, "trailing bytes after trailer");
}

#[test]
fn dds2_empty_stream_item_counts_are_consistent() {
    let path = fixture_path(
        "reject-empty-stream-count",
        &build_file(header(2, 1, 1, 0), &[], Some(1), &[]),
    );
    expected_error(&path, "empty stream item count");
}

#[test]
fn dds2_truncated_trailer_is_rejected_at_the_trailer_boundary() {
    let block = encode_record_block(
        &[],
        &[simple_record(NativeName::unix(
            b"truncated-trailer".to_vec(),
        ))],
    );
    let mut bytes = build_file(header(2, 1, 1, 0), &[block], None, &[]);
    bytes.truncate(bytes.len() - 1);
    let path = fixture_path("reject-truncated-trailer", &bytes);
    expected_error(&path, "missing trailer");
}

#[test]
fn dds2_file_shorter_than_header_and_trailer_is_rejected() {
    let mut bytes = build_file(header(2, 1, 1, 0), &[], Some(0), &[]);
    bytes.truncate(8 + DDS2_TRAILER_LEN - 1);
    let path = fixture_path("reject-short-file", &bytes);
    expected_error(&path, "file too short");
}

#[test]
fn dds2_delta_descriptor_mismatch_is_rejected() {
    let block = BlockSpec {
        raw: encode_delta_remove(&[], &NativeName::windows(vec![0xd800])),
        count: 1,
        first_path: vec![NativeName::unix(vec![0xff])],
    };
    let path = fixture_path(
        "reject-delta-descriptor-mismatch",
        &build_file(header(1, 2, 1, 0), &[block], None, &[]),
    );
    expected_error(&path, "descriptor mismatch");
}
