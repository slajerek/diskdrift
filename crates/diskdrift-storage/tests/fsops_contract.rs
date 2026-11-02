use diskdrift_core::native::{NativePath, NativeRoot};
use diskdrift_core::record::{FileIdScheme, TimestampResolution};
use diskdrift_core::testutil::TestDir;
use diskdrift_storage::meta::{AllocationSemantics, FileIdScope, Provenance, VolumeDescriptor};
use diskdrift_storage::meta::{ScanOptions, SnapshotMeta, Status, StorageInfo, VolumeIdentity};
use diskdrift_storage::{Store, StoreError};
#[cfg(feature = "test-seams")]
use std::collections::BTreeMap;
use std::fs;
#[cfg(feature = "test-seams")]
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> TestDir {
    TestDir::new(&format!("fsops-contract-{name}"))
}

fn volume_descriptor() -> VolumeDescriptor {
    let mut descriptor = VolumeDescriptor::new(
        0,
        "linux",
        FileIdScheme::PosixInode64,
        FileIdScope::Volume,
        AllocationSemantics::FilesystemReported,
        TimestampResolution::Seconds,
        Provenance::Observed("test adapter".into()),
    );
    descriptor.identity = NativePath::unix(b"test-volume".to_vec());
    descriptor.mount_key = NativePath::unix(b"/".to_vec());
    descriptor.identity_strength = "uuid".into();
    descriptor
}

#[cfg(feature = "test-seams")]
fn store_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn collect(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let entry_path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                collect(root, &entry_path, files);
            } else {
                files.insert(
                    entry_path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(entry_path).unwrap(),
                );
            }
        }
    }

    let mut files = BTreeMap::new();
    collect(root, root, &mut files);
    files
}

#[test]
fn read_only_store_probes_never_create_files() {
    let dir = fixture("read-only");
    let missing = dir.path().join("missing-store");
    assert!(matches!(
        Store::open(&missing),
        Err(StoreError::NotAStore(_))
    ));
    assert!(!missing.exists());

    let store = Store::create(&dir.path().join("store")).unwrap();
    let lock = store.path().join("lock");
    let registry_lock = store.path().join("disks.lock");
    assert_eq!(store.registry().unwrap().disks.len(), 0);
    assert!(!registry_lock.exists());
    assert!(!store.is_busy());
    assert!(!lock.exists());
}

#[test]
fn public_store_lock_is_exclusive_and_released() {
    let dir = fixture("locks");
    let store = Store::create(&dir.path().join("store")).unwrap();
    let (first, _) = store.lock().unwrap();
    assert!(store.is_busy());
    let second = Store::open(store.path()).unwrap();
    assert!(matches!(second.lock(), Err(StoreError::Locked { .. })));

    drop(first);
    assert!(!store.is_busy());
    drop(store.lock().unwrap());
}

#[test]
fn created_store_directories_are_private_backend_objects() {
    let dir = fixture("directory");
    let store_path = dir.path().join("nested").join("store");
    let store = Store::create(&store_path).unwrap();
    assert!(store.path().is_dir());
    let metadata_dir = store.layout().metadata_dir();
    assert!(metadata_dir.is_dir());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [store.path(), metadata_dir.as_path()] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
}

#[test]
fn in_progress_list_marks_abandoned_scan_without_creating_lock() {
    let dir = fixture("in-progress");
    let store = Store::create(&dir.path().join("store")).unwrap();
    let template = SnapshotMeta {
        format_version: 3,
        id: 0,
        name: "in-progress".into(),
        root: "/tmp".into(),
        root_native: Some(NativeRoot::posix(b"/tmp".to_vec())),
        disk: None,
        root_in_volume: None,
        root_in_volume_native: Some(NativePath::unix(b"/tmp".to_vec())),
        status: Status::InProgress,
        started_at: "2026-10-29T10:00:00+00:00".into(),
        finished_at: None,
        volume: VolumeIdentity {
            fs_type: "test".into(),
            mount_point: "/".into(),
            descriptor: Some(volume_descriptor()),
            ..Default::default()
        },
        volumes: diskdrift_storage::meta::complete_volume_table(&volume_descriptor()),
        options: ScanOptions::default(),
        space_before: None,
        space_after: None,
        storage: StorageInfo::default(),
        counts: None,
        error: None,
        diskdrift_version: "test".into(),
    };
    let (lock, _) = store.lock().unwrap();
    let writer = store.begin(lock, template.clone(), None).unwrap();
    let stem = writer.meta().storage.stem.clone().unwrap();
    drop(writer);

    let metadata_path = store.layout().metadata_dir().join(format!("{stem}.json"));
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
    value["status"] = serde_json::Value::String("IN_PROGRESS".into());
    fs::write(&metadata_path, serde_json::to_vec(&value).unwrap()).unwrap();
    let lock_path = store.path().join("lock");
    let _ = fs::remove_file(&lock_path);

    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].status, Status::Failed);
    assert!(
        !lock_path.exists(),
        "list must not create a missing writer lock"
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn migration_atomic_replacement_failure_preserves_legacy_view() {
    use diskdrift_core::native::{NativeName, NativePath, NativeRoot};
    use diskdrift_core::record::{Kind, Record};
    use diskdrift_core::walk::WalkReport;
    use diskdrift_storage::test_support::fail_atomic_replacement_for;

    let dir = fixture("migration-replacement-failure");
    let store = Store::create(&dir.path().join("store")).unwrap();
    let template = SnapshotMeta {
        format_version: 3,
        id: 0,
        name: "baseline".into(),
        root: "/tmp".into(),
        root_native: Some(NativeRoot::posix(b"/tmp".to_vec())),
        disk: None,
        root_in_volume: None,
        root_in_volume_native: Some(NativePath::unix(b"/tmp".to_vec())),
        status: Status::InProgress,
        started_at: "2026-10-29T10:00:00+00:00".into(),
        finished_at: None,
        volume: VolumeIdentity {
            fs_type: "test".into(),
            mount_point: "/".into(),
            descriptor: Some(volume_descriptor()),
            ..Default::default()
        },
        volumes: diskdrift_storage::meta::complete_volume_table(&volume_descriptor()),
        options: ScanOptions::default(),
        space_before: None,
        space_after: None,
        storage: StorageInfo::default(),
        counts: None,
        error: None,
        diskdrift_version: "test".into(),
    };
    let record = Record {
        kind: Kind::Dir,
        name: NativeName::unix(b"/tmp".to_vec()),
        ..Default::default()
    };
    let (lock, _) = store.lock().unwrap();
    let mut writer = store.begin(lock, template, None).unwrap();
    writer.push(&record).unwrap();
    let meta = writer
        .commit(
            WalkReport {
                entries: 1,
                ..Default::default()
            },
            None,
            "now".into(),
        )
        .unwrap();

    let data_file_name = {
        let v4_metadata_dir = store.layout().metadata_dir();
        let v4_registry_path = store.layout().registry_path();
        let legacy_dir = store.path().join("snapshots");
        fs::create_dir_all(&legacy_dir).unwrap();
        for entry in fs::read_dir(&v4_metadata_dir).unwrap() {
            let entry = entry.unwrap();
            let source = entry.path();
            let name = entry.file_name();
            let target = legacy_dir.join(&name);
            if source.extension().and_then(|ext| ext.to_str()) == Some("json") {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&source).unwrap()).unwrap();
                let data_file = value["storage"]["dataFile"].as_str().unwrap();
                let data_name = Path::new(data_file).file_name().unwrap().to_string_lossy();
                value["storage"]["dataFile"] = serde_json::Value::String(data_name.into());
                fs::write(&target, serde_json::to_vec(&value).unwrap()).unwrap();
            } else {
                fs::rename(&source, &target).unwrap();
            }
        }
        fs::remove_dir_all(&v4_metadata_dir).unwrap();
        fs::copy(&v4_registry_path, store.path().join("disks.json")).unwrap();
        fs::remove_file(v4_registry_path).unwrap();
        Path::new(meta.storage.data_file.as_deref().unwrap())
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    };

    let marker = store.path().join("store.json");
    fs::write(&marker, br#"{"format":"diskdrift-snapshots","version":2}"#).unwrap();
    let store = Store::open(store.path()).unwrap();
    let marker_before = fs::read(&marker).unwrap();
    let metadata_path = store
        .path()
        .join("snapshots")
        .join(format!("{}.json", meta.storage.stem.as_deref().unwrap()));
    let metadata_before = fs::read(&metadata_path).unwrap();
    let data_path = store.path().join("snapshots").join(&data_file_name);
    let data_before = fs::read(&data_path).unwrap();
    let registry_path = store.path().join("disks.json");
    let registry_before = fs::read(&registry_path).unwrap();
    let view_before = Store::open(store.path()).unwrap();
    let records_before = view_before
        .records(meta.id)
        .unwrap()
        .collect::<std::io::Result<Vec<_>>>()
        .unwrap();
    let list_before = view_before.list().unwrap();
    let registry_view_before = view_before.registry().unwrap();
    let files_before = store_files(store.path());

    let guard = fail_atomic_replacement_for(&marker);
    let result = store.lock();
    drop(guard);

    assert!(matches!(
        result,
        Err(StoreError::UnsupportedFilesystem { path, source })
            if path == marker && source.kind() == std::io::ErrorKind::Unsupported
    ));
    assert_eq!(fs::read(&marker).unwrap(), marker_before);
    assert_eq!(fs::read(&metadata_path).unwrap(), metadata_before);
    assert_eq!(fs::read(&data_path).unwrap(), data_before);
    assert_eq!(fs::read(&registry_path).unwrap(), registry_before);

    let reopened = Store::open(store.path()).unwrap();
    assert_eq!(reopened.list().unwrap(), list_before);
    assert_eq!(reopened.registry().unwrap(), registry_view_before);
    let records = reopened
        .records(meta.id)
        .unwrap()
        .collect::<std::io::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(records, records_before);

    let files_after = store_files(store.path());
    for (path, bytes) in &files_before {
        assert_eq!(
            files_after.get(path),
            Some(bytes),
            "view file changed: {path:?}"
        );
    }
    let staging: Vec<_> = files_after
        .keys()
        .filter(|path| !files_before.contains_key(*path))
        .collect();
    assert!(
        staging
            .iter()
            .any(|path| *path == Path::new("store.json.tmp"))
    );
    assert!(
        staging
            .iter()
            .any(|path| *path == Path::new("disks-v2.json"))
    );
    assert!(staging.iter().any(|path| {
        path.parent() == Some(Path::new("snapshots-v4"))
            && path.extension().and_then(|ext| ext.to_str()) == Some("json")
    }));
}
