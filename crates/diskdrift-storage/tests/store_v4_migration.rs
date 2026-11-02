use diskdrift_core::native::{NativePath, NativeRoot, RootKind};
use diskdrift_core::testutil::TestDir;
use diskdrift_storage::MigrationOutcome;
use diskdrift_storage::meta::complete_volume_table;
use diskdrift_storage::meta::{META_FORMAT_VERSION, SnapshotMeta, Status};
use diskdrift_storage::{
    Registry, ScanOptions, SpaceSample, StorageInfo, Store, StoreError, VolumeIdentity,
};
use std::fs;
use std::path::Path;

use diskdrift_core::native::NativeName;
use diskdrift_core::record::{EntryFields, FileIdScheme, Kind, Record, TimestampResolution};
use diskdrift_storage::meta::{AllocationSemantics, FileIdScope, Provenance, VolumeDescriptor};

fn write_marker(root: &Path, value: serde_json::Value) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("store.json"), serde_json::to_vec(&value).unwrap()).unwrap();
}

fn v4_marker(metadata_dir: &str, registry: &str) -> serde_json::Value {
    serde_json::json!({
        "format": "diskdrift-snapshots",
        "version": 4,
        "metadataDir": metadata_dir,
        "registry": registry,
    })
}

fn metadata(id: u64, status: Status) -> SnapshotMeta {
    SnapshotMeta {
        format_version: META_FORMAT_VERSION,
        id,
        name: format!("snapshot-{id}"),
        root: "/".into(),
        root_native: Some(
            NativeRoot::new(NativePath::unix(b"/".to_vec()), RootKind::Posix, false).unwrap(),
        ),
        disk: None,
        root_in_volume: Some("/".into()),
        root_in_volume_native: Some(NativePath::unix(b"/".to_vec())),
        status,
        started_at: "2026-10-30T10:00:00Z".into(),
        finished_at: None,
        volume: VolumeIdentity {
            uuid: Some("test-volume".into()),
            fs_type: "apfs".into(),
            mount_point: "/".into(),
            ..Default::default()
        },
        volumes: Vec::new(),
        options: ScanOptions::default(),
        space_before: Some(SpaceSample::default()),
        space_after: None,
        storage: StorageInfo::default(),
        counts: None,
        error: None,
        diskdrift_version: "test".into(),
    }
}

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let source = entry.path();
        let target = to.join(entry.file_name());
        if source.is_dir() {
            copy_tree(&source, &target);
        } else {
            fs::copy(source, target).unwrap();
        }
    }
}

fn legacy_fixture(name: &str) -> TestDir {
    legacy_fixture_from(name, "store-v3")
}

fn legacy_fixture_from(name: &str, source: &str) -> TestDir {
    let dir = TestDir::new(name);
    copy_tree(&fixture(source), dir.path());
    dir
}

fn legacy_view_files(root: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    fn visit(root: &Path, path: &Path, out: &mut Vec<(std::path::PathBuf, Vec<u8>)>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == "snapshots-v4") {
                continue;
            }
            if path.is_dir() {
                visit(root, &path, out);
            } else if path == root.join("disks.json") || path.starts_with(root.join("snapshots")) {
                out.push((
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                ));
            }
        }
    }

    let mut out = Vec::new();
    visit(root, root, &mut out);
    out.sort_by(|left, right| left.0.cmp(&right.0));
    out
}

fn volume(index: u16, identity: &[u8], mount: &[u8]) -> VolumeDescriptor {
    let mut descriptor = VolumeDescriptor::new(
        index,
        "linux",
        FileIdScheme::PosixInode64,
        FileIdScope::Volume,
        AllocationSemantics::FilesystemReported,
        TimestampResolution::Seconds,
        Provenance::Observed("test adapter".into()),
    );
    descriptor.identity = NativePath::unix(identity.to_vec());
    descriptor.mount_key = NativePath::unix(mount.to_vec());
    descriptor.identity_strength = "uuid".into();
    descriptor
}

fn record(name: &[u8], depth: u16, vol: u16, size: u64) -> Record {
    Record {
        depth,
        name: NativeName::unix(name.to_vec()),
        kind: if depth == 0 { Kind::Dir } else { Kind::File },
        available: EntryFields::KIND
            .union(EntryFields::LOGICAL_SIZE)
            .union(EntryFields::ALLOCATED_SIZE),
        size,
        alloc: size,
        vol,
        ..Default::default()
    }
}

fn native_record(name: NativeName, depth: u16, vol: u16, size: u64) -> Record {
    Record {
        depth,
        name,
        kind: if depth == 0 { Kind::Dir } else { Kind::File },
        available: EntryFields::KIND
            .union(EntryFields::LOGICAL_SIZE)
            .union(EntryFields::ALLOCATED_SIZE),
        size,
        alloc: size,
        vol,
        ..Default::default()
    }
}

fn windows_volume(index: u16, identity: &str, mount: &str) -> VolumeDescriptor {
    let mut descriptor = VolumeDescriptor::new(
        index,
        "windows",
        FileIdScheme::WindowsFileId128,
        FileIdScope::Volume,
        AllocationSemantics::FilesystemReported,
        TimestampResolution::Seconds,
        Provenance::Observed("test adapter".into()),
    );
    descriptor.identity = NativePath::windows(identity.encode_utf16().collect());
    descriptor.mount_key = NativePath::windows(mount.encode_utf16().collect());
    descriptor.identity_strength = "uuid".into();
    descriptor
}

fn windows_metadata(name: &str) -> SnapshotMeta {
    let descriptor = windows_volume(
        0,
        r"\\?\Volume{11111111-2222-3333-4444-555555555555}\",
        r"C:\",
    );
    let root = NativeRoot::new(
        NativePath::windows(r"C:\".encode_utf16().collect()),
        RootKind::Drive,
        false,
    )
    .unwrap();
    let mut meta = metadata(0, Status::InProgress);
    meta.name = name.into();
    meta.root = r"C:\".into();
    meta.root_native = Some(root);
    meta.root_in_volume = Some(r"\".into());
    meta.root_in_volume_native = Some(NativePath::windows(vec![92_u16]));
    meta.volume = VolumeIdentity {
        uuid: Some("windows-volume".into()),
        fs_type: "ntfs".into(),
        mount_point: r"C:\".into(),
        descriptor: Some(descriptor.clone()),
        ..Default::default()
    };
    meta.volumes = complete_volume_table(&descriptor);
    meta.format_version = 3;
    meta
}

fn write_snapshot(
    store: &Store,
    mut meta: SnapshotMeta,
    records: &[Record],
    base: Option<&SnapshotMeta>,
) -> SnapshotMeta {
    let (lock, _) = store.lock().unwrap();
    let mut writer = store.begin(lock, meta.clone(), base).unwrap();
    for item in records {
        writer.push(item).unwrap();
    }
    meta = writer
        .commit(
            diskdrift_core::walk::WalkReport {
                entries: records.len() as u64,
                ..Default::default()
            },
            None,
            "2026-10-30T10:01:00Z".into(),
        )
        .unwrap();
    meta
}

#[test]
fn begin_rejects_cross_encoding_and_incompatible_volume_lineage() {
    let dir = TestDir::new("v4-lineage-fences");
    let store = Store::create(&dir.path().join("store")).unwrap();
    let unix = write_snapshot(
        &store,
        {
            let mut meta = metadata(0, Status::InProgress);
            meta.name = "unix".into();
            let descriptor = volume(0, b"unix-volume", b"/");
            meta.volume.descriptor = Some(descriptor.clone());
            meta.volumes = complete_volume_table(&descriptor);
            meta.format_version = 3;
            meta
        },
        &[record(b"/", 0, 0, 0)],
        None,
    );

    let windows = windows_metadata("windows");
    write_snapshot(
        &store,
        windows.clone(),
        &[native_record(
            NativeName::windows(r"C:\".encode_utf16().collect()),
            0,
            0,
            0,
        )],
        None,
    );

    let (lock, _) = store.lock().unwrap();
    let mut cross = windows.clone();
    cross.name = "cross-encoding".into();
    let result = store.begin(lock, cross, Some(&unix));
    let message = match result {
        Err(error) => error.to_string(),
        Ok(_) => "begin unexpectedly succeeded".into(),
    };
    assert!(message.contains("path encoding"));

    let (lock, _) = store.lock().unwrap();
    let mut changed_identity = unix.clone();
    changed_identity.id = 0;
    changed_identity.name = "changed-identity".into();
    changed_identity.status = Status::InProgress;
    changed_identity.storage = StorageInfo::default();
    let descriptor = changed_identity.volume.descriptor.as_mut().unwrap();
    descriptor.identity = NativePath::unix(b"other-volume".to_vec());
    changed_identity.volumes[0] = descriptor.clone();
    assert!(matches!(
        store.begin(lock, changed_identity, Some(&unix)),
        Err(StoreError::Invalid(message)) if message.contains("volume identity")
    ));
}

#[test]
fn begin_rejects_a_volume_table_with_mixed_path_encodings() {
    let dir = TestDir::new("v4-mixed-table-encoding");
    let store = Store::create(&dir.path().join("store")).unwrap();
    let first = volume(0, b"unix-volume", b"/");
    let second = windows_volume(
        1,
        r"\\?\Volume{11111111-2222-3333-4444-555555555555}\",
        r"D:\",
    );
    let mut meta = metadata(0, Status::InProgress);
    meta.name = "mixed-encoding".into();
    meta.format_version = 3;
    meta.volume.descriptor = Some(first.clone());
    meta.volumes = vec![first, second];

    let (lock, _) = store.lock().unwrap();
    assert!(matches!(
        store.begin(lock, meta, None),
        Err(StoreError::Invalid(message)) if message.contains("path encoding")
    ));
}

#[test]
fn begin_rejects_a_delta_with_a_changed_secondary_volume_identity() {
    let dir = TestDir::new("v4-full-table-lineage");
    let store = Store::create(&dir.path().join("store")).unwrap();
    let first = volume(0, b"root-volume", b"/");
    let second = volume(1, b"secondary-volume", b"/other");
    let template = |name: &str, secondary: VolumeDescriptor| {
        let mut meta = metadata(0, Status::InProgress);
        meta.name = name.into();
        meta.format_version = 3;
        meta.volume.descriptor = Some(first.clone());
        meta.volumes = vec![first.clone(), secondary];
        meta
    };
    let base = write_snapshot(
        &store,
        template("base", second.clone()),
        &[record(b"/", 0, 0, 0), record(b"other", 1, 1, 7)],
        None,
    );
    let changed = volume(1, b"different-secondary-volume", b"/other");

    let (lock, _) = store.lock().unwrap();
    assert!(matches!(
        store.begin(lock, template("changed", changed), Some(&base)),
        Err(StoreError::Invalid(message)) if message.contains("volume table")
    ));
}

#[test]
fn secondary_file_id_scope_change_keeps_delta_compatible_but_downgrades_moves() {
    let dir = TestDir::new("v4-secondary-file-id-capability");
    let store = Store::create(&dir.path().join("store")).unwrap();
    let first = volume(0, b"root-volume", b"/");
    let second = volume(1, b"secondary-volume", b"/other");
    let template = |name: &str, secondary: VolumeDescriptor| {
        let mut meta = metadata(0, Status::InProgress);
        meta.name = name.into();
        meta.format_version = 3;
        meta.volume.descriptor = Some(first.clone());
        meta.volumes = vec![first.clone(), secondary];
        meta
    };
    let base = write_snapshot(
        &store,
        template("base", second.clone()),
        &[record(b"/", 0, 0, 0), record(b"other", 1, 1, 7)],
        None,
    );
    let mut changed = second;
    changed.file_id_scope = FileIdScope::Filesystem;

    let (lock, _) = store.lock().unwrap();
    let writer = store
        .begin(lock, template("changed", changed), Some(&base))
        .unwrap();
    assert!(writer.is_delta());
    assert!(!base.lineage_compatibility(writer.meta()).file_id_comparable);
}

#[test]
fn begin_accepts_mount_alias_changes_but_push_requires_root_and_volume_metadata() {
    let dir = TestDir::new("v4-writer-fences");
    let store = Store::create(&dir.path().join("store")).unwrap();
    let descriptor = volume(0, b"stable-volume", b"/");
    let mut base = metadata(0, Status::InProgress);
    base.name = "base".into();
    base.format_version = 3;
    base.volume.descriptor = Some(descriptor.clone());
    base.volumes = complete_volume_table(&descriptor);
    let base = write_snapshot(&store, base, &[record(b"/", 0, 0, 0)], None);

    let mut next = base.clone();
    next.id = 0;
    next.name = "next".into();
    next.status = Status::InProgress;
    next.storage = StorageInfo::default();
    next.finished_at = None;
    next.counts = None;
    next.volume.descriptor.as_mut().unwrap().mount_key =
        NativePath::unix(b"/Volumes/alias".to_vec());
    next.volume.descriptor.as_mut().unwrap().aliases = vec![NativePath::unix(b"/".to_vec())];
    next.volumes[0] = next.volume.descriptor.clone().unwrap();

    let (lock, _) = store.lock().unwrap();
    let mut writer = store.begin(lock, next, Some(&base)).unwrap();
    let wrong_root = record(b"/wrong", 0, 0, 0);
    let error = writer.push(&wrong_root).unwrap_err();
    assert!(error.to_string().contains("root"));
    let failed_meta = writer.meta().clone();
    drop(writer);
    assert_eq!(failed_meta.status, Status::Failed);
    assert!(
        store
            .list()
            .unwrap()
            .iter()
            .any(|meta| { meta.name == "next" && meta.status == Status::Failed })
    );

    let mut field_mismatch = base.clone();
    field_mismatch.id = 0;
    field_mismatch.name = "field-mismatch".into();
    field_mismatch.status = Status::InProgress;
    field_mismatch.storage = StorageInfo::default();
    field_mismatch.finished_at = None;
    field_mismatch.counts = None;
    field_mismatch.volume.descriptor.as_mut().unwrap().mount_key =
        NativePath::unix(b"/Volumes/alias".to_vec());
    field_mismatch.volume.descriptor.as_mut().unwrap().aliases =
        vec![NativePath::unix(b"/".to_vec())];
    field_mismatch.volumes[0] = field_mismatch.volume.descriptor.clone().unwrap();
    let (lock, _) = store.lock().unwrap();
    let mut writer = store.begin(lock, field_mismatch, Some(&base)).unwrap();
    let mut mismatch = record(b"/", 0, 0, 0);
    mismatch.available = mismatch.available.union(EntryFields::MTIME);
    mismatch.timestamp_resolution = TimestampResolution::Nanoseconds;
    let error = writer.push(&mismatch).unwrap_err();
    assert!(error.to_string().contains("timestamp"));
}

#[test]
fn finalization_rejects_a_stream_without_an_exact_root_record() {
    let dir = TestDir::new("v4-root-finalization");
    let store = Store::create(&dir.path().join("store")).unwrap();
    let descriptor = volume(0, b"volume", b"/");
    let mut meta = metadata(0, Status::InProgress);
    meta.name = "missing-root".into();
    meta.format_version = 3;
    meta.volume.descriptor = Some(descriptor.clone());
    meta.volumes = complete_volume_table(&descriptor);
    let (lock, _) = store.lock().unwrap();
    let writer = store.begin(lock, meta, None).unwrap();
    assert!(matches!(
        writer.commit(diskdrift_core::walk::WalkReport::default(), None, "now".into()),
        Err(StoreError::Invalid(message)) if message.contains("root")
    ));
}

#[test]
fn finalization_rejects_invalid_final_metadata_before_publishing_data() {
    let dir = TestDir::new("v4-final-metadata-validation");
    let store = Store::create(&dir.path().join("store")).unwrap();
    let descriptor = volume(0, b"volume", b"/");
    let mut meta = metadata(0, Status::InProgress);
    meta.name = "invalid-final-metadata".into();
    meta.format_version = 3;
    meta.volume.descriptor = Some(descriptor.clone());
    meta.volumes = complete_volume_table(&descriptor);

    let (lock, _) = store.lock().unwrap();
    let mut writer = store.begin(lock, meta, None).unwrap();
    let stem = writer.meta().storage.stem.clone().unwrap();
    writer.set_root_volume_index(2, &[1, 2]);
    writer.push(&record(b"/", 0, 0, 0)).unwrap();

    let error = writer
        .commit(
            diskdrift_core::walk::WalkReport::default(),
            None,
            "now".into(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("volume descriptor index"));
    assert!(
        !store
            .layout()
            .metadata_dir()
            .join(format!("{stem}.dds"))
            .exists()
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn metadata_publication_failure_recovers_the_finalized_data_file() {
    use diskdrift_storage::test_support::fail_next_atomic_replacement_for;

    let dir = TestDir::new("v4-metadata-publication-recovery");
    let store = Store::create(&dir.path().join("store")).unwrap();
    let descriptor = volume(0, b"volume", b"/");
    let mut meta = metadata(0, Status::InProgress);
    meta.name = "metadata-publication-failure".into();
    meta.format_version = 3;
    meta.volume.descriptor = Some(descriptor.clone());
    meta.volumes = complete_volume_table(&descriptor);

    let (lock, _) = store.lock().unwrap();
    let mut writer = store.begin(lock, meta, None).unwrap();
    let stem = writer.meta().storage.stem.clone().unwrap();
    writer.push(&record(b"/", 0, 0, 0)).unwrap();
    let metadata_path = store.layout().metadata_dir().join(format!("{stem}.json"));

    let guard = fail_next_atomic_replacement_for(&metadata_path);
    let error = writer
        .commit(
            diskdrift_core::walk::WalkReport::default(),
            None,
            "now".into(),
        )
        .unwrap_err();
    drop(guard);

    assert!(matches!(error, StoreError::UnsupportedFilesystem { .. }));
    let stored: SnapshotMeta = serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
    assert_eq!(stored.status, Status::Failed);
    assert!(
        stored
            .error
            .as_deref()
            .is_some_and(|message| message.contains("atomic replacement"))
    );
    assert!(
        fs::read_dir(store.layout().metadata_dir())
            .unwrap()
            .all(|entry| entry
                .unwrap()
                .path()
                .extension()
                .is_none_or(|extension| extension != "dds")),
        "recovery removes finalized data"
    );
}

#[test]
fn first_post_migration_rebase_uses_a_dds2_reverse_delta_without_an_extra_full() {
    use diskdrift_storage::container::{DataFile, FileKind};

    let dir = legacy_fixture("v4-post-migration-rebase");
    let store = Store::open(dir.path()).unwrap();
    let (lock, outcome) = store.lock().unwrap();
    assert_eq!(outcome, MigrationOutcome::V4Active);
    drop(lock);

    let migrated = Store::open(dir.path()).unwrap();
    let previous = migrated.get(2).unwrap();
    let records = migrated
        .records(previous.id)
        .unwrap()
        .collect::<std::io::Result<Vec<_>>>()
        .unwrap();
    let mut next = previous.clone();
    next.id = 0;
    next.name = "post-migration".into();
    next.status = Status::InProgress;
    next.storage = StorageInfo::default();
    next.finished_at = None;
    next.counts = None;
    let (lock, _) = migrated.lock().unwrap();
    let mut writer = migrated.begin(lock, next, None).unwrap();
    for item in &records {
        writer.push(item).unwrap();
    }
    let (committed, rebase) = writer
        .commit_and_rebase(
            diskdrift_core::walk::WalkReport {
                entries: records.len() as u64,
                ..Default::default()
            },
            None,
            "2026-10-30T10:01:00Z".into(),
        )
        .unwrap();
    assert!(
        matches!(rebase.unwrap(), diskdrift_storage::Rebase::Converted { id, .. } if id == previous.id)
    );

    let listed = migrated.list().unwrap();
    assert_eq!(listed.len(), 3);
    let old = listed.iter().find(|meta| meta.id == previous.id).unwrap();
    assert_eq!(old.storage.base, Some(committed.id));
    assert!(committed.storage.base.is_none());
    let old_data = migrated
        .layout()
        .data_path(old.storage.data_file.as_deref().unwrap())
        .unwrap();
    assert_eq!(
        DataFile::open_with_volumes(&old_data, &old.volumes)
            .unwrap()
            .kind,
        FileKind::Ops
    );
    assert!(
        DataFile::open_with_volumes(&old_data, &old.volumes)
            .unwrap()
            .descriptor()
            .is_dds2(),
        "a v4 rebase must publish a DDS2 reverse delta"
    );
    assert_eq!(
        DataFile::open_with_volumes(&old_data, &old.volumes)
            .unwrap()
            .descriptor()
            .path_encoding,
        diskdrift_core::native::PathEncoding::UnixBytes
    );
    for meta in listed {
        assert_eq!(migrated.verify(meta.id).unwrap(), 5);
    }
}

#[test]
fn v4_marker_rejects_unknown_fields_and_wrong_types() {
    let dir = TestDir::new("v4-marker-strict");
    let mut unknown = v4_marker("snapshots-v4", "disks-v2.json");
    unknown["extra"] = serde_json::json!(true);
    write_marker(dir.path(), unknown);
    assert!(matches!(
        Store::open(dir.path()),
        Err(StoreError::Invalid(_))
    ));

    write_marker(
        dir.path(),
        serde_json::json!({
            "format": "diskdrift-snapshots",
            "version": "4",
            "metadataDir": "snapshots-v4",
            "registry": "disks-v2.json"
        }),
    );
    assert!(matches!(
        Store::open(dir.path()),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn marker_version_is_an_exact_supported_json_integer() {
    for version in [0_u64, 4_294_967_296_u64] {
        let dir = TestDir::new("v4-marker-version-range");
        write_marker(
            dir.path(),
            serde_json::json!({
                "format": "diskdrift-snapshots",
                "version": version,
                "metadataDir": "snapshots-v4",
                "registry": "disks-v2.json"
            }),
        );
        assert!(
            matches!(
                Store::open(dir.path()),
                Err(StoreError::Invalid(_)) | Err(StoreError::UnsupportedVersion { .. })
            ),
            "accepted unsupported marker version {version}"
        );
    }
}

#[test]
fn v4_marker_rejects_absolute_escaped_and_legacy_namespaces() {
    for (metadata_dir, registry) in [
        ("/tmp/snapshots-v4", "disks-v2.json"),
        ("../snapshots-v4", "disks-v2.json"),
        ("snapshots-v4", "../disks-v2.json"),
        ("snapshots", "disks-v2.json"),
        ("snapshots-v4", "disks.json"),
        ("store.json", "disks-v2.json"),
        ("snapshots-v4", "store.json"),
        ("snapshots-v4", "lock"),
        ("snapshots-v4", "disks.lock"),
    ] {
        let dir = TestDir::new("v4-marker-path");
        write_marker(dir.path(), v4_marker(metadata_dir, registry));
        assert!(
            matches!(Store::open(dir.path()), Err(StoreError::Invalid(_))),
            "accepted metadataDir={metadata_dir:?}, registry={registry:?}"
        );
    }
}

#[test]
fn registry_rejects_versions_that_would_truncate() {
    let dir = TestDir::new("registry-version-range");
    fs::write(
        dir.path().join("disks.json"),
        serde_json::json!({
            "format": "diskdrift-disks",
            "version": 4_294_967_296_u64,
            "disks": []
        })
        .to_string(),
    )
    .unwrap();

    assert!(matches!(
        Registry::load(dir.path()),
        Err(StoreError::UnsupportedDisksVersion { found }) if found == 4_294_967_296
    ));
}

#[test]
fn default_creation_publishes_store_v4_namespaces() {
    let dir = TestDir::new("default-store-layout");
    let store = Store::create(&dir.path().join("store")).unwrap();
    let marker: serde_json::Value =
        serde_json::from_slice(&fs::read(store.path().join("store.json")).unwrap()).unwrap();
    assert_eq!(marker["version"], 4);
    assert!(store.path().join("snapshots-v4").is_dir());
    assert!(!store.path().join("disks-v2.json").exists());
    assert!(!store.path().join("snapshots").exists());
}

#[test]
fn v4_read_only_open_and_list_do_not_publish_or_rewrite() {
    let dir = TestDir::new("v4-read-only");
    write_marker(dir.path(), v4_marker("snapshots-v4", "disks-v2.json"));
    fs::create_dir(dir.path().join("snapshots-v4")).unwrap();
    let before = fs::read(dir.path().join("store.json")).unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert!(store.list().unwrap().is_empty());
    assert_eq!(fs::read(dir.path().join("store.json")).unwrap(), before);
    assert!(!dir.path().join("lock").exists());
    assert!(!dir.path().join("disks-v2.json").exists());
}

#[test]
fn v4_metadata_paths_are_store_relative_and_cannot_escape() {
    let dir = TestDir::new("v4-relative-data");
    write_marker(dir.path(), v4_marker("snapshots-v4", "disks-v2.json"));
    let metadata_dir = dir.path().join("snapshots-v4");
    fs::create_dir(&metadata_dir).unwrap();
    let mut meta = metadata(1, Status::Completed);
    meta.storage.data_file = Some("../snapshots/old.dds".into());
    fs::write(
        metadata_dir.join("one.json"),
        serde_json::to_vec(&meta).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        Store::open(dir.path()),
        Err(StoreError::Corrupt(_)) | Err(StoreError::Invalid(_))
    ));
}

#[test]
fn v4_data_references_are_slash_normalized_and_host_independent() {
    let dir = TestDir::new("v4-slash-data-reference");
    write_marker(dir.path(), v4_marker("snapshots-v4", "disks-v2.json"));
    fs::create_dir(dir.path().join("snapshots-v4")).unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(
        store
            .layout()
            .data_path("snapshots-v4/nested/record.dds")
            .unwrap(),
        dir.path().join("snapshots-v4/nested/record.dds")
    );
    assert!(matches!(
        store.layout().data_path(r"snapshots-v4\nested\record.dds"),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn v4_persists_the_ordered_volume_table_for_all_record_indexes() {
    let dir = TestDir::new("v4-volume-table");
    let store = Store::create(&dir.path().join("store")).unwrap();
    let first = volume(0, b"volume-0", b"/");
    let second = volume(1, b"volume-1", b"/other");
    let mut meta = metadata(0, Status::InProgress);
    meta.name = "volume-table".into();
    meta.format_version = 3;
    meta.volume.descriptor = Some(first.clone());
    meta.volumes = vec![first, second];
    let committed = write_snapshot(
        &store,
        meta,
        &[record(b"/", 0, 1, 0), record(b"file", 1, 1, 7)],
        None,
    );
    assert_eq!(committed.volumes.len(), 2);
    assert_eq!(committed.volumes[1].index, 1);
    assert!(
        committed
            .storage
            .data_file
            .as_deref()
            .is_some_and(|path| path.contains('/') && !path.contains('\\'))
    );

    let reopened = Store::open(store.path()).unwrap();
    let listed = reopened.list().unwrap();
    assert_eq!(listed[0].volumes.len(), 2);
    assert_eq!(reopened.verify(committed.id).unwrap(), 2);
    assert_eq!(
        reopened
            .records(committed.id)
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn v4_writer_rejects_a_record_volume_index_outside_the_table() {
    let dir = TestDir::new("v4-volume-index-fence");
    let store = Store::create(&dir.path().join("store")).unwrap();
    let descriptor = volume(0, b"volume", b"/");
    let mut meta = metadata(0, Status::InProgress);
    meta.name = "volume-index-fence".into();
    meta.format_version = 3;
    meta.volume.descriptor = Some(descriptor.clone());
    meta.volumes = complete_volume_table(&descriptor);

    let (lock, _) = store.lock().unwrap();
    let mut writer = store.begin(lock, meta, None).unwrap();
    writer.push(&record(b"/", 0, 0, 0)).unwrap();

    let error = writer.push(&record(b"other", 1, 1, 7)).unwrap_err();
    assert!(error.to_string().contains("invalid volume index"));
}

#[test]
fn legacy_snapshot_identity_and_root_remain_display_only() {
    let mut meta = metadata(1, Status::Completed);
    meta.root_native = None;
    meta.root_in_volume_native = None;
    meta.volume.descriptor = None;
    let sighting = diskdrift_storage::Sighting::of_snapshot(&meta);
    assert_eq!(sighting.identity.platform, "unknown");
    assert_eq!(sighting.identity.scheme, "legacy_string");
    assert_eq!(
        sighting.identity.strength,
        diskdrift_storage::Strength::Weak
    );
    assert!(matches!(
        meta.root_value(),
        diskdrift_storage::RootValue::LegacyDisplayOnly(_)
    ));
}

#[test]
fn missing_and_corrupt_data_damage_only_dependent_chains() {
    let dir = TestDir::new("v4-damage-view");
    let store = Store::create(&dir.path().join("store")).unwrap();
    let descriptor = volume(0, b"volume", b"/");
    let records = |name: &[u8]| vec![record(b"/", 0, 0, 0), record(name, 1, 0, 7)];
    let template = |name: &str| {
        let mut meta = metadata(0, Status::InProgress);
        meta.name = name.into();
        meta.format_version = 3;
        meta.volume.descriptor = Some(descriptor.clone());
        meta.volumes = complete_volume_table(&descriptor);
        meta
    };
    let first = write_snapshot(&store, template("first"), &records(b"a"), None);
    let second = write_snapshot(&store, template("second"), &records(b"b"), Some(&first));
    let third = write_snapshot(&store, template("third"), &records(b"c"), Some(&second));
    let independent = write_snapshot(&store, template("independent"), &records(b"d"), None);
    let corrupt = write_snapshot(&store, template("corrupt"), &records(b"e"), None);

    let first_path = store
        .layout()
        .data_path(first.storage.data_file.as_deref().unwrap())
        .unwrap();
    fs::remove_file(first_path).unwrap();
    let corrupt_path = store
        .layout()
        .data_path(corrupt.storage.data_file.as_deref().unwrap())
        .unwrap();
    fs::write(corrupt_path, b"not a data file").unwrap();

    let listed = store.list().unwrap();
    let status = |id| listed.iter().find(|m| m.id == id).unwrap().status;
    assert_eq!(status(first.id), Status::Damaged);
    assert_eq!(status(second.id), Status::Damaged);
    assert_eq!(status(third.id), Status::Damaged);
    assert_eq!(status(corrupt.id), Status::Damaged);
    assert_eq!(status(independent.id), Status::Completed);
    assert_eq!(store.verify(independent.id).unwrap(), 2);

    let (lock, _) = store.lock().unwrap();
    let error = store.begin(lock, template("blocked"), Some(&second));
    assert!(matches!(error, Err(StoreError::Invalid(message)) if message.contains("damaged")));
}

#[test]
fn cyclic_reconstruction_chains_are_marked_damaged() {
    let dir = TestDir::new("v4-damage-cycle");
    let store = Store::create(&dir.path().join("store")).unwrap();
    let descriptor = volume(0, b"volume", b"/");
    let template = |name: &str| {
        let mut meta = metadata(0, Status::InProgress);
        meta.name = name.into();
        meta.format_version = 3;
        meta.volume.descriptor = Some(descriptor.clone());
        meta.volumes = complete_volume_table(&descriptor);
        meta
    };

    let first = write_snapshot(
        &store,
        template("first"),
        &[record(b"/", 0, 0, 0), record(b"a", 1, 0, 7)],
        None,
    );
    let second = write_snapshot(
        &store,
        template("second"),
        &[record(b"/", 0, 0, 0), record(b"b", 1, 0, 7)],
        None,
    );

    for (meta, base) in [(&first, second.id), (&second, first.id)] {
        let path = store
            .layout()
            .metadata_dir()
            .join(format!("{}.json", meta.storage.stem.as_deref().unwrap()));
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["storage"]["base"] = serde_json::json!(base);
        fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    }

    let listed = store.list().unwrap();
    assert!(listed.iter().all(|meta| meta.status == Status::Damaged));
}

#[test]
fn lock_reports_v4_outcome_without_discarding_it() {
    let dir = TestDir::new("v4-lock-outcome");
    let store = Store::create(&dir.path().join("store")).unwrap();
    let (lock, outcome) = store.lock().unwrap();
    assert_eq!(outcome, diskdrift_storage::MigrationOutcome::V4Active);
    drop(lock);
}

#[test]
fn writer_lock_revalidates_the_live_v4_marker_before_recovery() {
    let dir = TestDir::new("v4-live-marker");
    let store = Store::create(&dir.path().join("store")).unwrap();
    fs::write(
        store.path().join("store.json"),
        serde_json::to_vec(&v4_marker("store.json", "disks-v2.json")).unwrap(),
    )
    .unwrap();

    assert!(matches!(store.lock(), Err(StoreError::Invalid(_))));
    assert!(!store.is_busy());
}

#[test]
fn nested_registry_namespace_is_created_only_when_publishing() {
    let dir = TestDir::new("v4-nested-registry");
    write_marker(
        dir.path(),
        v4_marker("snapshots-v4", "registry/disks-v2.json"),
    );
    fs::create_dir(dir.path().join("snapshots-v4")).unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert!(!dir.path().join("registry").exists());
    assert_eq!(store.registry().unwrap(), Registry::default());
    assert!(!dir.path().join("registry").exists());

    let (lock, _) = store.lock().unwrap();
    store.update_registry(&lock, |_| Ok(())).unwrap();
    assert!(dir.path().join("registry/disks-v2.json").is_file());
}

#[test]
fn v4_rejects_a_legacy_registry_document_at_the_declared_path() {
    let dir = TestDir::new("v4-registry-schema");
    write_marker(dir.path(), v4_marker("snapshots-v4", "disks-v2.json"));
    fs::create_dir(dir.path().join("snapshots-v4")).unwrap();
    fs::write(
        dir.path().join("disks-v2.json"),
        serde_json::json!({
            "format": "diskdrift-disks",
            "version": 1,
            "disks": []
        })
        .to_string(),
    )
    .unwrap();

    assert!(matches!(
        Store::open(dir.path()),
        Err(StoreError::Corrupt(_))
    ));
}

#[test]
fn migration_stages_recovery_without_mutating_legacy_data() {
    let dir = legacy_fixture("v4-migration-stage");
    let metadata_path = dir.path().join("snapshots/dc4396bd8-261021-100000.json");
    let mut legacy_meta: serde_json::Value =
        serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
    legacy_meta["status"] = serde_json::json!("IN_PROGRESS");
    legacy_meta["error"] = serde_json::Value::Null;
    fs::write(
        &metadata_path,
        serde_json::to_vec_pretty(&legacy_meta).unwrap(),
    )
    .unwrap();

    let store = Store::open(dir.path()).unwrap();
    let before = legacy_view_files(dir.path());
    let (lock, outcome) = store.lock().unwrap();
    assert_eq!(outcome, MigrationOutcome::V4Active);
    drop(lock);

    let marker: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.path().join("store.json")).unwrap()).unwrap();
    assert_eq!(marker, v4_marker("snapshots-v4", "disks-v2.json"));
    let expected_legacy_files: Vec<_> = before
        .into_iter()
        .filter(|(path, _)| {
            !path.starts_with("snapshots")
                || path.extension().is_some_and(|extension| extension == "dds")
        })
        .collect();
    assert_eq!(legacy_view_files(dir.path()), expected_legacy_files);

    let v4 = Store::open(dir.path()).unwrap();
    let migrated = v4.list().unwrap();
    assert_eq!(migrated.len(), 2);
    assert_eq!(migrated[0].status, Status::Failed);
    assert!(migrated.iter().all(|meta| {
        meta.storage
            .data_file
            .as_deref()
            .is_some_and(|path| path.starts_with("snapshots/"))
    }));
    assert_eq!(v4.verify(2).unwrap(), 5);
}

#[test]
fn v1_and_v2_migrations_preserve_history_without_rewriting_legacy_data() {
    for source in ["store-v1", "store-v2"] {
        let dir = legacy_fixture_from(&format!("v4-migration-{source}"), source);
        let before = legacy_view_files(dir.path());
        let store = Store::open(dir.path()).unwrap();

        let (lock, outcome) = store.lock().unwrap();
        assert_eq!(outcome, MigrationOutcome::V4Active);
        drop(lock);

        let migrated = Store::open(dir.path()).unwrap();
        let migrated_snapshots = migrated.list().unwrap();
        assert_eq!(migrated_snapshots.len(), 2, "{source}");
        assert!(migrated_snapshots.iter().all(|meta| {
            meta.root_native.is_some()
                && meta.root_in_volume_native.is_some()
                && meta.volumes.len() == 1
                && meta.volume.descriptor.is_some()
        }));
        for id in [1, 2] {
            assert_eq!(migrated.verify(id).unwrap(), 5, "{source} snapshot {id}");
        }
        assert!(migrated.list().unwrap().iter().all(|meta| {
            meta.storage
                .data_file
                .as_deref()
                .is_some_and(|path| path.starts_with("snapshots/"))
        }));

        let expected_legacy_files: Vec<_> = before
            .into_iter()
            .filter(|(path, _)| {
                !path.starts_with("snapshots")
                    || path.extension().is_some_and(|extension| extension == "dds")
            })
            .collect();
        assert_eq!(
            legacy_view_files(dir.path()),
            expected_legacy_files,
            "{source}"
        );
    }
}

#[test]
fn migration_cleans_only_unreferenced_legacy_snapshot_files_after_commit() {
    let dir = legacy_fixture("v4-migration-cleanup");
    let legacy_dir = dir.path().join("snapshots");
    fs::write(legacy_dir.join("orphan.dds"), b"orphan data").unwrap();
    fs::create_dir_all(legacy_dir.join("orphan-dir")).unwrap();
    fs::write(legacy_dir.join("orphan-dir/orphan.tmp"), b"orphan temp").unwrap();

    let registry_before = fs::read(dir.path().join("disks.json")).unwrap();
    let registry_lock_before = fs::read(dir.path().join("disks.lock")).unwrap();
    let store = Store::open(dir.path()).unwrap();

    let (lock, outcome) = store.lock().unwrap();
    assert_eq!(outcome, MigrationOutcome::V4Active);
    drop(lock);

    assert!(!legacy_dir.join("dc4396bd8-261021-100000.json").exists());
    assert!(!legacy_dir.join("dc4396bd8-261021-100000-2.json").exists());
    assert!(!legacy_dir.join("orphan.dds").exists());
    assert!(!legacy_dir.join("orphan-dir/orphan.tmp").exists());
    assert!(legacy_dir.join("dc4396bd8-261021-100000-2.dds").exists());
    assert!(
        legacy_dir
            .join("dc4396bd8-261021-100000.rdc4396bd8-261021-100000-2.dds")
            .exists()
    );
    assert_eq!(
        fs::read(dir.path().join("disks.json")).unwrap(),
        registry_before
    );
    assert!(dir.path().join("lock").is_file());
    assert_eq!(
        fs::read(dir.path().join("disks.lock")).unwrap(),
        registry_lock_before
    );
    assert!(dir.path().join("store.json").is_file());
}

#[test]
fn migration_keeps_missing_or_corrupt_referenced_legacy_data() {
    for (source, damaged_file) in [
        ("missing", "dc4396bd8-261021-100000-2.dds"),
        ("corrupt", "dc4396bd8-261021-100000-2.dds"),
    ] {
        let dir = legacy_fixture(&format!("v4-migration-{source}-legacy-data"));
        let legacy_dir = dir.path().join("snapshots");
        let damaged_path = legacy_dir.join(damaged_file);
        if source == "missing" {
            fs::remove_file(&damaged_path).unwrap();
        } else {
            fs::write(&damaged_path, b"corrupt legacy data").unwrap();
        }
        let orphan_metadata = fs::read(legacy_dir.join("dc4396bd8-261021-100000.json")).unwrap();
        fs::write(legacy_dir.join("orphan.json"), orphan_metadata).unwrap();
        fs::write(legacy_dir.join("orphan.dds"), b"orphan data").unwrap();
        let root_note = dir.path().join("root-note");
        fs::write(&root_note, b"keep this root file").unwrap();
        let root_note_before = fs::read(&root_note).unwrap();

        let store = Store::open(dir.path()).unwrap();
        let (lock, outcome) = store.lock().unwrap();
        assert_eq!(outcome, MigrationOutcome::V4Active);
        drop(lock);

        let migrated = Store::open(dir.path()).unwrap();
        let snapshots = migrated.list().unwrap();
        assert!(snapshots.iter().all(|meta| meta.status == Status::Damaged));
        assert!(!legacy_dir.join("orphan.json").exists());
        assert!(!legacy_dir.join("orphan.dds").exists());
        if source == "missing" {
            assert!(!damaged_path.exists());
        } else {
            assert_eq!(fs::read(&damaged_path).unwrap(), b"corrupt legacy data");
        }
        assert_eq!(fs::read(&root_note).unwrap(), root_note_before);
    }
}

#[test]
fn cleanup_considers_all_committed_v4_metadata_files_before_deleting_legacy_data() {
    let dir = legacy_fixture("v4-cleanup-all-metadata");
    let store = Store::open(dir.path()).unwrap();
    let (lock, _) = store.lock().unwrap();
    drop(lock);

    let active = Store::open(dir.path()).unwrap();
    let metadata_path = active
        .layout()
        .metadata_dir()
        .join("duplicate-reference.json");
    let original_path = active
        .layout()
        .metadata_dir()
        .join("dc4396bd8-261021-100000.json");
    let mut duplicate: serde_json::Value =
        serde_json::from_slice(&fs::read(original_path).unwrap()).unwrap();
    duplicate["storage"]["stem"] = serde_json::Value::Null;
    duplicate["storage"]["dataFile"] = serde_json::json!("snapshots/retained.dds");
    fs::write(&metadata_path, serde_json::to_vec(&duplicate).unwrap()).unwrap();
    let retained = dir.path().join("snapshots/retained.dds");
    fs::write(&retained, b"retained for a committed metadata reference").unwrap();

    let (lock, _) = active.lock().unwrap();
    drop(lock);
    assert!(retained.exists());
}

#[cfg(feature = "test-seams")]
#[test]
fn migration_failures_before_commit_preserve_the_legacy_view_and_retry_idempotently() {
    use diskdrift_storage::test_support::fail_store_operation_after;

    let mut reached_commit = false;
    for failure in 1..=128 {
        let dir = legacy_fixture(&format!("v4-migration-failure-{failure}"));
        let store = Store::open(dir.path()).unwrap();
        let before = legacy_view_files(dir.path());

        let guard = fail_store_operation_after(failure);
        let result = store.lock();
        drop(guard);

        let marker: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join("store.json")).unwrap()).unwrap();
        if marker["version"] == 3 {
            assert!(result.is_err(), "failure {failure} did not fail");
            assert_eq!(legacy_view_files(dir.path()), before, "failure {failure}");
            let old_view = Store::open(dir.path()).unwrap();
            assert_eq!(old_view.verify(1).unwrap(), 5);

            let (lock, outcome) = old_view.lock().unwrap();
            assert_eq!(outcome, MigrationOutcome::V4Active);
            drop(lock);
            continue;
        }

        assert_eq!(marker, v4_marker("snapshots-v4", "disks-v2.json"));
        reached_commit = true;
        let active = Store::open(dir.path()).unwrap();
        assert_eq!(active.verify(2).unwrap(), 5);
        let expected_legacy_files: Vec<_> = legacy_view_files(dir.path())
            .into_iter()
            .filter(|(path, _)| {
                !path.starts_with("snapshots")
                    || path.extension().is_some_and(|extension| extension == "dds")
            })
            .collect();
        let (lock, outcome) = active.lock().unwrap();
        assert_eq!(outcome, MigrationOutcome::V4Active);
        drop(lock);
        assert_eq!(legacy_view_files(dir.path()), expected_legacy_files);
        break;
    }
    assert!(
        reached_commit,
        "failure seam did not reach the marker boundary"
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn v1_and_v2_migration_failures_preserve_the_legacy_view() {
    use diskdrift_storage::test_support::fail_store_operation_after;

    for source in ["store-v1", "store-v2"] {
        let mut reached_commit = false;
        for failure in 1..=128 {
            let dir = legacy_fixture_from(&format!("v4-{source}-failure-{failure}"), source);
            let store = Store::open(dir.path()).unwrap();
            let before = legacy_view_files(dir.path());

            let guard = fail_store_operation_after(failure);
            let result = store.lock();
            drop(guard);

            let marker: serde_json::Value =
                serde_json::from_slice(&fs::read(dir.path().join("store.json")).unwrap()).unwrap();
            if marker["version"] != 4 {
                assert!(result.is_err(), "{source} failure {failure} did not fail");
                assert_eq!(
                    legacy_view_files(dir.path()),
                    before,
                    "{source} failure {failure}"
                );
                let old_view = Store::open(dir.path()).unwrap();
                assert_eq!(old_view.verify(1).unwrap(), 5, "{source}");
                continue;
            }

            reached_commit = true;
            let active = Store::open(dir.path()).unwrap();
            assert_eq!(active.verify(2).unwrap(), 5, "{source}");
            break;
        }
        assert!(
            reached_commit,
            "{source} failure seam did not reach the marker boundary"
        );
    }
}

#[cfg(feature = "test-seams")]
#[test]
fn same_store_handle_switches_to_v4_when_marker_replacement_commits_before_flush_finishes() {
    use diskdrift_storage::test_support::fail_store_operation_after;

    let mut observed = false;
    for failure in 1..=128 {
        let dir = legacy_fixture_from(&format!("v4-post-marker-handle-{failure}"), "store-v3");
        let store = Store::open(dir.path()).unwrap();
        let guard = fail_store_operation_after(failure);
        let result = store.lock();
        drop(guard);

        let marker: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join("store.json")).unwrap()).unwrap();
        if marker["version"] == 4 && result.is_err() {
            observed = true;
            assert!(store.layout().is_v4());
            assert_eq!(store.verify(2).unwrap(), 5);
            break;
        }
    }
    assert!(
        observed,
        "did not exercise a failure after marker replacement"
    );
}

#[test]
fn migrated_registry_mutations_hold_the_writer_lock_and_report_the_notice() {
    let dir = legacy_fixture("v4-migration-registry");
    let store = Store::open(dir.path()).unwrap();
    let (lock, outcome) = store.lock().unwrap();
    assert_eq!(
        outcome.notice(),
        Some(
            "DiskDrift store format v4 is active; older DiskDrift versions cannot read this store."
        )
    );
    store
        .update_registry(&lock, |registry| {
            registry.set_label("dc4396bd8", Some("archive"))?;
            registry.set_description("dc4396bd8", Some("migrated"))?;
            registry.add_to_set("dc4396bd8", "offline")?;
            registry.remove_from_set("dc4396bd8", "missing")
        })
        .unwrap();
    drop(lock);

    let registry = store.registry().unwrap();
    let disk = registry.get("dc4396bd8").unwrap();
    assert_eq!(disk.label.as_deref(), Some("archive"));
    assert_eq!(disk.description.as_deref(), Some("migrated"));
    assert_eq!(disk.sets, ["offline"]);
}

#[test]
fn damaged_status_is_persisted_and_excluded_from_delta_bases() {
    let mut json = serde_json::to_value(metadata(7, Status::Completed)).unwrap();
    json["status"] = serde_json::json!("DAMAGED");
    let round_trip: Result<SnapshotMeta, _> = serde_json::from_value(json);
    assert!(round_trip.is_ok(), "DAMAGED is a persisted snapshot status");
    assert!(!round_trip.unwrap().status.is_usable());
}

#[test]
fn creation_publishes_complete_v4_layout() {
    let dir = TestDir::new("feature-store-v4");
    let store = Store::create(&dir.path().join("store")).unwrap();
    let marker: serde_json::Value =
        serde_json::from_slice(&fs::read(store.path().join("store.json")).unwrap()).unwrap();
    assert_eq!(marker, v4_marker("snapshots-v4", "disks-v2.json"));
    assert!(store.path().join("snapshots-v4").is_dir());
    assert!(!store.path().join("snapshots").exists());
    assert!(!store.path().join("disks.json").exists());
    assert!(!store.path().join("disks-v2.json").exists());
}

#[test]
fn registry_publication_uses_v2_without_changing_v1_save() {
    let dir = TestDir::new("feature-registry-v2");
    let store = Store::create(&dir.path().join("store")).unwrap();
    let (lock, _) = store.lock().unwrap();
    store
        .update_registry(&lock, |registry| {
            registry.record(&diskdrift_storage::Sighting::portable(
                diskdrift_storage::Identity::new(diskdrift_storage::Strength::Uuid, "v4-volume"),
                "/Volumes/V4",
            ));
            Ok(())
        })
        .unwrap();
    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(store.path().join("disks-v2.json")).unwrap()).unwrap();
    assert_eq!(value["version"], 2);
    assert!(!store.path().join("disks.json").exists());
}
