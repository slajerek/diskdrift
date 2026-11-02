use diskdrift_core::browse;
use diskdrift_core::delta::compute;
use diskdrift_core::diff::{DiffOptions, diff};
use diskdrift_core::native::NativeName;
use diskdrift_core::paths::PathOrderError;
use diskdrift_core::record::{
    EntryFields, FileId, FileIdScheme, FileIdScope, IdentityEvidence, Kind, MoveCertainty, Record,
    TimestampResolution,
};
use diskdrift_core::top::{self, TopOptions};
use std::io;

fn boxed(records: Vec<Record>) -> diskdrift_core::diff::RecordStream<'static> {
    Box::new(records.into_iter().map(Ok::<Record, io::Error>))
}

fn windows_records(file_size: u64) -> Vec<Record> {
    vec![
        Record {
            depth: 0,
            name: NativeName::windows(r"C:\".encode_utf16().collect()),
            kind: Kind::Dir,
            ..Default::default()
        },
        Record {
            depth: 1,
            name: NativeName::windows(vec![0x00ff]),
            kind: Kind::Dir,
            ..Default::default()
        },
        Record {
            depth: 2,
            name: NativeName::windows(vec![0xd800]),
            kind: Kind::File,
            size: file_size,
            alloc: file_size,
            file_id: FileId { low: 11, high: 22 },
            available: EntryFields::KIND
                .union(EntryFields::LOGICAL_SIZE)
                .union(EntryFields::ALLOCATED_SIZE)
                .union(EntryFields::FILE_ID),
            ..Default::default()
        },
    ]
}

#[test]
fn windows_names_survive_diff_and_delta_without_unicode_conversion() {
    let old = windows_records(7);
    let new = windows_records(11);

    let report = diff(
        boxed(old.clone()),
        boxed(new.clone()),
        DiffOptions::default(),
    )
    .expect("same-encoding Windows records must be comparable");
    assert_eq!(report.summary.grown.count, 1);

    let ops = compute(boxed(old), boxed(new)).expect("same-encoding Windows records must delta");
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].record.name, NativeName::windows(vec![0xd800]));
    assert_eq!(ops[0].record.file_id, FileId { low: 11, high: 22 });
}

#[test]
fn windows_browse_paths_are_exact_native_values() {
    let records = windows_records(7);
    let dir = records[0..1]
        .iter()
        .map(|r| r.name.clone())
        .collect::<Vec<_>>();
    let listing = browse::children(boxed(records), &dir, 10).unwrap();
    let path = serde_json::to_value(&listing.items[0].path).unwrap();
    assert_eq!(path["pathEncoding"], "windows_utf16le");
    assert!(path.get("pathWide").is_some());
}

#[test]
fn windows_move_paths_are_exact_native_values() {
    let root = NativeName::windows(r"C:\".encode_utf16().collect());
    let old = vec![
        Record {
            depth: 0,
            name: root.clone(),
            kind: Kind::Dir,
            ..Default::default()
        },
        Record {
            depth: 1,
            name: NativeName::windows(vec![0xd800]),
            kind: Kind::File,
            size: 7,
            alloc: 7,
            file_id: FileId { low: 11, high: 0 },
            btime: 12,
            available: EntryFields::KIND
                .union(EntryFields::LOGICAL_SIZE)
                .union(EntryFields::ALLOCATED_SIZE)
                .union(EntryFields::BIRTH_TIME)
                .union(EntryFields::FILE_ID),
            file_id_scheme: FileIdScheme::WindowsFileId128,
            file_id_scope: FileIdScope::Volume,
            ..Default::default()
        },
    ];
    let new = vec![
        Record {
            depth: 0,
            name: root,
            kind: Kind::Dir,
            ..Default::default()
        },
        Record {
            depth: 1,
            name: NativeName::windows(vec![0xdc00]),
            kind: Kind::File,
            size: 7,
            alloc: 7,
            file_id: FileId { low: 11, high: 0 },
            btime: 12,
            available: EntryFields::KIND
                .union(EntryFields::LOGICAL_SIZE)
                .union(EntryFields::ALLOCATED_SIZE)
                .union(EntryFields::BIRTH_TIME)
                .union(EntryFields::FILE_ID),
            file_id_scheme: FileIdScheme::WindowsFileId128,
            file_id_scope: FileIdScope::Volume,
            ..Default::default()
        },
    ];
    let report = diff(boxed(old), boxed(new), DiffOptions::default()).unwrap();
    let move_json = serde_json::to_value(&report.moves[0]).unwrap();
    assert_eq!(move_json["from"]["pathEncoding"], "windows_utf16le");
    assert_eq!(move_json["to"]["pathEncoding"], "windows_utf16le");
    assert_eq!(move_json["certainty"], "definite");
}

#[test]
fn raw_inode_without_file_id_is_not_a_move() {
    let root = NativeName::unix(b"/r".to_vec());
    let old = vec![
        Record {
            depth: 0,
            name: root.clone(),
            kind: Kind::Dir,
            ..Default::default()
        },
        Record {
            depth: 1,
            name: NativeName::unix(b"old".to_vec()),
            kind: Kind::File,
            size: 7,
            alloc: 7,
            ino: 11,
            ..Default::default()
        },
    ];
    let new = vec![
        Record {
            depth: 0,
            name: root,
            kind: Kind::Dir,
            ..Default::default()
        },
        Record {
            depth: 1,
            name: NativeName::unix(b"new".to_vec()),
            kind: Kind::File,
            size: 7,
            alloc: 7,
            ino: 11,
            ..Default::default()
        },
    ];
    let report = diff(boxed(old), boxed(new), DiffOptions::default()).unwrap();
    assert!(report.moves.is_empty());
}

#[test]
fn mixed_prefix_encoding_is_returned_as_an_error() {
    let records = vec![Record {
        depth: 0,
        name: NativeName::unix(b"/".to_vec()),
        kind: Kind::Dir,
        ..Default::default()
    }];
    let opts = TopOptions {
        prefix: Some(vec![NativeName::windows(vec![b'C' as u16])]),
        ..Default::default()
    };
    assert!(top::top(boxed(records), &opts).is_err());
}

#[test]
fn mixed_encoding_in_delta_preserves_path_order_error() {
    let base = vec![Record {
        depth: 0,
        name: NativeName::unix(b"/".to_vec()),
        kind: Kind::Dir,
        ..Default::default()
    }];
    let op = diskdrift_core::delta::DeltaOp {
        kind: diskdrift_core::delta::OpKind::Add,
        parent: vec![NativeName::windows(vec![
            b'C' as u16,
            b':' as u16,
            b'\\' as u16,
        ])],
        record: Record {
            depth: 1,
            name: NativeName::windows(vec![0xd800]),
            kind: Kind::File,
            ..Default::default()
        },
    };
    let error = diskdrift_core::delta::Apply::new(boxed(base), Box::new(std::iter::once(Ok(op))))
        .next()
        .expect("the invalid delta must produce an error")
        .expect_err("mixed encodings must be rejected");
    assert!(
        error
            .get_ref()
            .is_some_and(|cause| cause.is::<PathOrderError>())
    );
}

#[test]
fn numeric_utf16_order_is_not_little_endian_byte_order() {
    let low = NativeName::windows(vec![0x00ff]);
    let high = NativeName::windows(vec![0x0100]);
    assert!(low < high);

    let low_json = serde_json::to_value(&low).unwrap();
    let high_json = serde_json::to_value(&high).unwrap();
    assert_ne!(low_json["pathWide"], high_json["pathWide"]);
    assert_eq!(serde_json::from_value::<NativeName>(low_json).unwrap(), low);
    assert_eq!(
        serde_json::from_value::<NativeName>(high_json).unwrap(),
        high
    );
}

#[test]
fn availability_changes_are_coverage_not_content_changes() {
    let root = Record {
        depth: 0,
        name: NativeName::unix(b"/".to_vec()),
        kind: Kind::Dir,
        ..Default::default()
    };
    let old_file = Record {
        depth: 1,
        name: NativeName::unix(b"entry".to_vec()),
        kind: Kind::File,
        size: 7,
        alloc: 4096,
        mtime: 10,
        available: EntryFields::KIND.union(EntryFields::MTIME),
        ..Default::default()
    };
    let new_file = Record {
        size: 99,
        alloc: 8192,
        available: EntryFields::KIND
            .union(EntryFields::LOGICAL_SIZE)
            .union(EntryFields::ALLOCATED_SIZE)
            .union(EntryFields::MTIME),
        ..old_file.clone()
    };
    assert!(!old_file.content_differs(&new_file));

    let report = diff(
        boxed(vec![root.clone(), old_file]),
        boxed(vec![root, new_file]),
        DiffOptions::default(),
    )
    .unwrap();
    assert_eq!(report.summary.modified_same_size, 0);
    assert_eq!(report.summary.coverage_changed, 1);
    assert_eq!(report.summary.unchanged, 1);
    assert_eq!(report.summary.net_allocated_bytes, 0);
}

#[test]
fn coverage_only_changes_are_preserved_by_delta_without_becoming_content_changes() {
    let root = Record {
        depth: 0,
        name: NativeName::unix(b"/".to_vec()),
        kind: Kind::Dir,
        ..Default::default()
    };
    let old_file = Record {
        depth: 1,
        name: NativeName::unix(b"entry".to_vec()),
        kind: Kind::File,
        size: 7,
        available: EntryFields::KIND,
        ..Default::default()
    };
    let new_file = Record {
        available: EntryFields::KIND.union(EntryFields::LOGICAL_SIZE),
        ..old_file.clone()
    };
    let ops = compute(
        boxed(vec![root.clone(), old_file]),
        boxed(vec![root.clone(), new_file.clone()]),
    )
    .unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].kind, diskdrift_core::delta::OpKind::Replace);
    let rebuilt: Vec<_> = diskdrift_core::delta::Apply::new(
        boxed(vec![
            root.clone(),
            Record {
                available: EntryFields::KIND,
                ..old_file_for_delta()
            },
        ]),
        Box::new(ops.into_iter().map(Ok)),
    )
    .collect::<io::Result<_>>()
    .unwrap();
    assert_eq!(rebuilt[1].available, new_file.available);
}

fn old_file_for_delta() -> Record {
    Record {
        depth: 1,
        name: NativeName::unix(b"entry".to_vec()),
        kind: Kind::File,
        size: 7,
        ..Default::default()
    }
}

#[test]
fn timestamp_comparison_uses_the_coarser_record_resolution() {
    let mut old = old_file_for_delta();
    old.timestamp_resolution = TimestampResolution::Seconds;
    old.available = EntryFields::KIND
        .union(EntryFields::MTIME)
        .union(EntryFields::MTIME_NANOS);
    old.mtime = 10;
    old.mtime_nanos = 1;
    let mut new = old.clone();
    new.timestamp_resolution = TimestampResolution::Nanoseconds;
    new.mtime_nanos = 999_999_999;
    assert!(!old.content_differs(&new));
}

#[test]
fn unavailable_identity_cannot_claim_a_definite_move() {
    let left = IdentityEvidence::new(
        7,
        FileId::ZERO,
        diskdrift_core::record::FileIdScheme::Unavailable,
        true,
    );
    let right = left;
    assert_eq!(left.move_certainty(&right), MoveCertainty::None);
}
