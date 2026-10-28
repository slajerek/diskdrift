use diskdrift_core::native::{NativeName, NativePath};
use diskdrift_core::record::{
    EntryFields, FileId, IdentityEvidence, MoveCertainty, Record, TimestampResolution,
};
use diskdrift_storage::codec::RecordEncoder;
use diskdrift_storage::disks::{
    Alias, DISKS_FORMAT, DISKS_VERSION_V2, Identity, Registry, Sighting, Strength,
};
use diskdrift_storage::meta::{
    AllocationSemantics, FileIdScheme, FileIdScope, MetadataV3, Provenance, VolumeDescriptor,
};

#[test]
fn file_id_round_trips_all_128_bits() {
    let id = FileId {
        low: u64::MAX,
        high: 0x0123_4567_89ab_cdef,
    };
    assert_eq!(FileId::from_le_bytes(id.to_le_bytes()), id);
    let record = Record {
        name: NativeName::unix(b"entry".to_vec()),
        file_id: id,
        available: EntryFields::FILE_ID,
        ..Default::default()
    };
    assert_eq!(record.file_id, id);
}

#[test]
fn unavailable_is_not_observed_zero() {
    let record = Record {
        size: 0,
        available: EntryFields::empty(),
        ..Default::default()
    };
    assert!(!record.available.contains(EntryFields::LOGICAL_SIZE));
    assert!(!record.has_observed_size());
}

#[test]
fn unavailable_payload_does_not_change_content() {
    let mut before = Record {
        size: 11,
        available: EntryFields::KIND,
        ..Default::default()
    };
    let mut after = before.clone();
    after.size = 99;
    assert!(!before.content_differs(&after));
    before.available = EntryFields::KIND.union(EntryFields::LOGICAL_SIZE);
    assert!(
        !before.content_differs(&after),
        "coverage changes are not content equality"
    );
}

#[test]
fn stat_errors_with_unavailable_payload_are_equal_and_do_not_differ() {
    let before = Record {
        flags: diskdrift_core::record::flags::STAT_ERROR,
        size: 11,
        mtime_nanos: 3,
        ..Default::default()
    };
    let after = Record {
        flags: diskdrift_core::record::flags::STAT_ERROR,
        size: 99,
        mtime_nanos: 7,
        ..before.clone()
    };
    assert_eq!(before, after);
    assert!(!before.content_differs(&after));
}

#[test]
fn available_kind_and_timestamp_precision_are_compared() {
    let available = EntryFields::KIND
        .union(EntryFields::MTIME)
        .union(EntryFields::MTIME_NANOS)
        .union(EntryFields::BIRTH_TIME)
        .union(EntryFields::BIRTH_TIME_NANOS);
    let before = Record {
        kind: diskdrift_core::record::Kind::File,
        mtime: 1,
        mtime_nanos: 2,
        btime: 3,
        btime_nanos: 4,
        available,
        ..Default::default()
    };
    let mut after = before.clone();
    after.kind = diskdrift_core::record::Kind::Dir;
    assert!(before.content_differs(&after));
    after.kind = before.kind;
    after.mtime_nanos += 1;
    assert!(before.content_differs(&after));
    after.mtime_nanos = before.mtime_nanos;
    after.btime_nanos += 1;
    assert!(before.content_differs(&after));
}

#[test]
fn hardlinks_require_stable_scoped_identity() {
    assert!(
        !IdentityEvidence::new(7, FileId::ZERO, FileIdScheme::Unavailable, false)
            .supports_hardlinks()
    );
    assert!(
        IdentityEvidence::new(
            7,
            FileId { low: 4, high: 5 },
            FileIdScheme::PosixInode64,
            true
        )
        .supports_hardlinks()
    );
}

#[test]
fn stable_scoped_id_supports_definite_move() {
    let a = IdentityEvidence::new(
        3,
        FileId { low: 42, high: 0 },
        FileIdScheme::PosixInode64,
        true,
    );
    let b = a;
    assert_eq!(a.move_certainty(&b), MoveCertainty::Definite);
}

#[test]
fn incompatible_identity_only_supports_probable_move() {
    let a = IdentityEvidence::new(
        3,
        FileId { low: 42, high: 0 },
        FileIdScheme::PosixInode64,
        true,
    );
    let b = IdentityEvidence::new(
        3,
        FileId { low: 42, high: 0 },
        FileIdScheme::WindowsFileId128,
        true,
    );
    assert_eq!(a.move_certainty(&b), MoveCertainty::Probable);
}

#[test]
fn timestamps_use_coarser_common_resolution() {
    let coarse = TimestampResolution::Seconds;
    let fine = TimestampResolution::Nanoseconds;
    assert_eq!(coarse.common(fine), TimestampResolution::Seconds);
    assert_eq!(
        TimestampResolution::Nanoseconds.quantize(12_345_678, fine),
        12_345_678
    );
    assert_eq!(
        TimestampResolution::Seconds.quantize(12_345_678_000, fine),
        12_000_000_000
    );
}

#[test]
fn timestamps_round_trip_with_availability() {
    let record = Record {
        mtime: 10,
        mtime_nanos: 123,
        btime: 9,
        btime_nanos: 456,
        available: EntryFields::MTIME
            .union(EntryFields::MTIME_NANOS)
            .union(EntryFields::BIRTH_TIME)
            .union(EntryFields::BIRTH_TIME_NANOS),
        ..Default::default()
    };
    assert_eq!(record.mtime_nanos, 123);
    assert_eq!(record.btime_nanos, 456);
    assert!(record.available.contains(EntryFields::MTIME_NANOS));
    assert!(record.available.contains(EntryFields::BIRTH_TIME_NANOS));
}

#[test]
fn dds1_maps_into_portable_record() {
    let legacy = Record {
        name: NativeName::unix(b"legacy".to_vec()),
        size: 0,
        alloc: 0,
        mtime: 0,
        btime: 0,
        ino: 0,
        file_id: FileId::ZERO,
        ..Default::default()
    };
    let mut bytes = Vec::new();
    legacy.encode_dds1(&mut bytes).unwrap();
    let mut input = bytes.as_slice();
    let portable = Record::decode(&mut input).expect("DDS1 record decodes");
    assert!(!portable.available.contains(EntryFields::FILE_ID));
    assert!(!portable.available.contains(EntryFields::BIRTH_TIME));
    let errored = Record {
        flags: diskdrift_core::record::flags::STAT_ERROR,
        ..legacy
    };
    let mut bytes = Vec::new();
    errored.encode_dds1(&mut bytes).unwrap();
    let mut input = bytes.as_slice();
    assert!(Record::decode(&mut input).unwrap().available == EntryFields::empty());
}

#[test]
fn dds1_does_not_advertise_unrepresentable_precision_or_identity() {
    let legacy = Record {
        name: NativeName::unix(b"legacy".to_vec()),
        mtime: 10,
        mtime_nanos: 123,
        btime: 9,
        btime_nanos: 456,
        file_id: FileId { low: 7, high: 8 },
        ino: 7,
        available: EntryFields::MTIME
            .union(EntryFields::MTIME_NANOS)
            .union(EntryFields::BIRTH_TIME)
            .union(EntryFields::BIRTH_TIME_NANOS)
            .union(EntryFields::FILE_ID),
        ..Default::default()
    };
    let mut bytes = Vec::new();
    legacy.encode_dds1(&mut bytes).unwrap();
    let mut input = bytes.as_slice();
    let decoded = Record::decode(&mut input).unwrap();

    assert_eq!(decoded.mtime, legacy.mtime);
    assert_eq!(decoded.btime, legacy.btime);
    assert_eq!(decoded.mtime_nanos, 0);
    assert_eq!(decoded.btime_nanos, 0);
    assert!(!decoded.available.contains(EntryFields::MTIME_NANOS));
    assert!(!decoded.available.contains(EntryFields::BIRTH_TIME_NANOS));
    assert!(!decoded.available.contains(EntryFields::FILE_ID));
    assert_eq!(decoded.file_id, FileId { low: legacy.ino, high: 0 });
    assert_eq!(decoded.ino, legacy.ino);
}

#[test]
fn dds1_rejects_windows_names_without_loss() {
    let record = Record {
        name: NativeName::windows(vec![0x0061, 0xd800]),
        ..Default::default()
    };
    assert!(record.encode_dds1(&mut Vec::new()).is_err());
    assert!(RecordEncoder::new().push(&record).is_err());
}

#[test]
fn volume_descriptor_validates_file_id_scope() {
    let mut descriptor = VolumeDescriptor::new(
        0,
        "macos",
        FileIdScheme::PosixInode64,
        FileIdScope::Volume,
        AllocationSemantics::FilesystemReported,
        TimestampResolution::Nanoseconds,
        Provenance::Observed("stat".into()),
    );
    descriptor.identity = NativePath::unix(b"uuid".to_vec());
    descriptor.mount_key = NativePath::unix(b"/".to_vec());
    assert!(descriptor.validate().is_ok());
    let unavailable = VolumeDescriptor::new(
        0,
        "macos",
        FileIdScheme::Unavailable,
        FileIdScope::Global,
        AllocationSemantics::Unknown,
        TimestampResolution::Unknown,
        Provenance::Unknown,
    );
    assert!(unavailable.validate().is_err());
}

#[test]
fn metadata_v3_requires_ordered_volume_descriptors() {
    let mut descriptor = VolumeDescriptor::new(
        0,
        "windows",
        FileIdScheme::WindowsFileId128,
        FileIdScope::Volume,
        AllocationSemantics::FilesystemReported,
        TimestampResolution::Nanoseconds,
        Provenance::Observed("native".into()),
    );
    descriptor.identity = NativePath::windows(vec![1]);
    descriptor.mount_key = NativePath::windows(vec![2]);
    let metadata = MetadataV3 {
        format_version: 3,
        volumes: vec![descriptor.clone()],
    };
    assert!(metadata.validate().is_ok());
    descriptor.index = 1;
    assert!(
        MetadataV3 {
            format_version: 3,
            volumes: vec![descriptor]
        }
        .validate()
        .is_err()
    );
}

#[test]
fn metadata_v3_keeps_exact_native_identity_and_mount_values() {
    let mut descriptor = VolumeDescriptor::new(
        0,
        "windows",
        FileIdScheme::WindowsFileId128,
        FileIdScope::Volume,
        AllocationSemantics::FilesystemReported,
        TimestampResolution::Nanoseconds,
        Provenance::Observed("native".into()),
    );
    descriptor.identity = NativePath::windows(vec![0xd800]);
    descriptor.mount_key =
        NativePath::windows(vec![b'C' as u16, b':'.into(), b'\\'.into(), 0xdfff]);
    assert!(
        MetadataV3 {
            format_version: 3,
            volumes: vec![descriptor]
        }
        .validate()
        .is_ok()
    );
}

#[test]
fn reconnect_preserves_strong_identity() {
    let mut registry = Registry::default();
    let first = Sighting::portable(Identity::new(Strength::Uuid, "volume-1"), "/Volumes/One");
    let id = registry.record(&first);
    let second = Sighting::portable(first.identity.clone(), "/Volumes/Reconnected");
    assert_eq!(
        registry.observe(&second),
        diskdrift_storage::disks::Observed::Known(id)
    );
    assert_eq!(registry.disks.len(), 1);
}

#[test]
fn short_id_is_platform_and_scheme_qualified() {
    let unix = Identity {
        strength: Strength::Uuid,
        source: "same".into(),
        platform: "macos".into(),
        scheme: "disk_arbitration_uuid".into(),
    };
    let windows = Identity {
        strength: Strength::Uuid,
        source: "same".into(),
        platform: "windows".into(),
        scheme: "volume_guid".into(),
    };
    assert_ne!(unix.short_id(8), windows.short_id(8));
}

#[test]
fn alias_path_can_resolve_a_reconnected_sighting() {
    let mut registry = Registry::default();
    let mut first = Sighting::portable(Identity::new(Strength::Uuid, "volume-2"), "/Volumes/One");
    first.aliases.push(Alias::new("/Volumes/OldName"));
    let id = registry.record(&first);
    assert_eq!(
        registry
            .resolve_mount(&Alias::new("/Volumes/OldName").path)
            .map(|d| d.id.as_str()),
        Some(id.as_str())
    );
}

#[test]
fn reconnect_retains_prior_mount_and_alias_lookup() {
    let mut registry = Registry::default();
    let first = Sighting::portable(
        Identity::new(Strength::Uuid, "volume-3"),
        "/Volumes/Original",
    );
    let id = registry.record(&first);
    let second = Sighting::portable(
        Identity::new(Strength::Uuid, "volume-3"),
        "/Volumes/Reconnected",
    );
    registry.record(&second);
    assert_eq!(
        registry
            .resolve_mount(&first.mount_path)
            .map(|d| d.id.as_str()),
        Some(id.as_str())
    );
    assert_eq!(
        registry
            .resolve_mount(&second.mount_path)
            .map(|d| d.id.as_str()),
        Some(id.as_str())
    );
}

#[test]
fn registry_v2_converts_to_v1_model_without_changing_short_id() {
    let mut registry = Registry::default();
    let sighting = Sighting::portable(
        Identity {
            platform: "windows".into(),
            scheme: "volume_guid".into(),
            ..Identity::new(Strength::Uuid, "same")
        },
        r"C:\Data",
    );
    let id = registry.record(&sighting);
    let v2 = registry.as_v2();
    assert_eq!(v2.format, DISKS_FORMAT);
    assert_eq!(v2.version, DISKS_VERSION_V2);
    assert_eq!(Registry::from_v2(v2).unwrap().get(&id).unwrap().id, id);
}

#[test]
fn registry_load_reads_v2_documents_into_the_active_model() {
    let mut registry = Registry::default();
    registry.record(&Sighting::portable(
        Identity {
            platform: "windows".into(),
            scheme: "volume_guid".into(),
            ..Identity::new(Strength::Uuid, "read-v2")
        },
        r"C:\Data",
    ));
    let dir = diskdrift_core::testutil::TestDir::new("registry-v2-load");
    std::fs::write(
        dir.path().join("disks.json"),
        serde_json::to_vec(&registry.as_v2()).unwrap(),
    )
    .unwrap();
    let loaded = Registry::load(dir.path()).unwrap();
    assert_eq!(loaded.disks.len(), 1);
    assert_eq!(loaded.disks[0].identity.platform, "windows");
}

#[test]
fn registry_v1_serialization_does_not_publish_v2_fields() {
    let mut registry = Registry::default();
    registry.record(&Sighting::portable(
        Identity::new(Strength::Uuid, "legacy"),
        "/Volumes/Legacy",
    ));
    let dir = diskdrift_core::testutil::TestDir::new("registry-v1-projection");
    let lock = diskdrift_storage::disks::lock(dir.path()).unwrap();
    registry.save(dir.path(), &lock).unwrap();
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("disks.json")).unwrap()).unwrap();
    assert_eq!(value["version"], 1);
    let disk = &value["disks"][0];
    assert!(disk["aliases"].is_null());
    assert!(disk["lastMountPath"].is_null());
    assert!(disk["identity"]["platform"].is_null());
}

#[test]
fn aliases_do_not_create_disks() {
    let mut registry = Registry::default();
    let mut first = Sighting::portable(Identity::new(Strength::Uuid, "volume-1"), "/Volumes/One");
    first.aliases.push(Alias::new("/Volumes/Alias"));
    registry.record(&first);
    assert_eq!(registry.disks.len(), 1);
    assert_eq!(
        registry.record(&Sighting::portable(
            first.identity.clone(),
            "/Volumes/Alias"
        )),
        first.identity.short_id(8)
    );
}

#[test]
fn clone_requires_confirmation() {
    let mut registry = Registry::default();
    let first = Sighting::portable(Identity::new(Strength::Uuid, "volume-1"), "/Volumes/One");
    let id = registry.record(&first);
    let clone = Sighting::portable(Identity::new(Strength::Uuid, "volume-1"), "/Volumes/Clone");
    assert!(matches!(
        registry.observe_clone(&id, &clone),
        diskdrift_storage::disks::Observed::Ask { .. }
    ));
}

#[test]
fn weak_identity_is_not_strengthened() {
    let mut registry = Registry::default();
    let weak = Sighting::portable(
        Identity::new(Strength::Weak, "hfs|disk|500"),
        "/Volumes/Disk",
    );
    let id = registry.record(&weak);
    let stronger = Sighting::portable(Identity::new(Strength::Uuid, "new-uuid"), "/Volumes/Disk");
    assert_ne!(registry.record(&stronger), id);
}
