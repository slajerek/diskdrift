use diskdrift_app::disks::{record_scan_disk, sighting_of};
use diskdrift_app::config::{Overrides, resolve_with};
use diskdrift_app::snapshots::{ScanRequest, scan};
use diskdrift_core::record::{FileIdScheme, FileIdScope, TimestampResolution};
use diskdrift_core::testutil::TestDir;
use diskdrift_platform::{disk_info, native_root};
use diskdrift_storage::container::{DataFile, FileKind};
use diskdrift_storage::disks::Sighting;
use diskdrift_storage::meta::{
    AllocationSemantics, Provenance, SnapshotMeta, VolumeDescriptor, VolumeIdentity,
};
use diskdrift_storage::{ScanOptions, Status, StorageInfo, Store};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

fn fixture_root() -> PathBuf {
    let path = std::env::var_os("DISKDRIFT_WINDOWS_FIXTURE").unwrap_or_else(|| {
        panic!(
            "required native Windows fixture unavailable: set DISKDRIFT_WINDOWS_FIXTURE to a disposable directory under target/"
        )
    });
    let path = PathBuf::from(path);
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("diskdrift crate must be in the workspace")
        .join("target");
    let canonical_target = target.canonicalize().unwrap_or(target);
    let canonical_path = path.canonicalize().unwrap_or(path.clone());
    assert!(
        canonical_path.starts_with(&canonical_target),
        "Windows fixtures must be under target/: {}",
        path.display()
    );
    assert!(
        canonical_path.is_dir(),
        "required native Windows fixture unavailable: {}",
        path.display()
    );
    canonical_path
}

fn descriptor(info: &diskdrift_platform::DiskInfo) -> VolumeDescriptor {
    VolumeDescriptor {
        index: 0,
        platform: "windows".into(),
        identity: info
            .native_identity
            .clone()
            .expect("Windows identity unavailable"),
        identity_strength: "uuid".into(),
        file_id_scheme: FileIdScheme::WindowsFileId128,
        file_id_scope: FileIdScope::Volume,
        mount_key: info
            .native_mount_path
            .clone()
            .expect("Windows mount key unavailable"),
        aliases: info.native_aliases.clone(),
        allocation: AllocationSemantics::FilesystemReported,
        allocation_provenance: Provenance::Observed("FILE_STANDARD_INFO.AllocationSize".into()),
        timestamp_resolution: TimestampResolution::Nanoseconds,
        provenance: Provenance::Observed("Windows fixture adapter".into()),
    }
}

#[test]
fn native_metadata_sighting_and_registry_keep_exact_windows_values() {
    let root = fixture_root();
    let info = disk_info(&root).expect("required native Windows disk-info fixture unavailable");
    let exact_root =
        native_root(root.as_os_str()).expect("required native Windows root unavailable");
    let sighting = sighting_of(&info, "fixture-time");
    let aliases = sighting
        .aliases
        .iter()
        .map(|alias| alias.path.clone())
        .collect::<Vec<_>>();
    assert_eq!(sighting.mount_path, info.native_mount_path.clone().unwrap());
    assert_eq!(aliases, info.native_aliases);

    let metadata = SnapshotMeta {
        format_version: 2,
        id: 1,
        name: "fixture".into(),
        root: root.display().to_string(),
        root_native: Some(exact_root.clone()),
        disk: None,
        root_in_volume: None,
        root_in_volume_native: None,
        status: Status::Completed,
        started_at: "fixture-time".into(),
        finished_at: Some("fixture-time".into()),
        volume: VolumeIdentity {
            uuid: info.volume_uuid.clone(),
            name: info.volume_name.clone(),
            fs_type: info.fs_type.clone(),
            mount_point: info.mount_point.display().to_string(),
            volume_group: false,
            descriptor: Some(descriptor(&info)),
        },
        volumes: vec![descriptor(&info)],
        options: ScanOptions::default(),
        space_before: None,
        space_after: None,
        storage: StorageInfo::default(),
        counts: None,
        error: None,
        diskdrift_version: "fixture".into(),
    };
    metadata
        .volume
        .descriptor
        .as_ref()
        .unwrap()
        .validate()
        .unwrap();
    let from_metadata = Sighting::of_snapshot(&metadata);
    assert_eq!(
        from_metadata.mount_path,
        info.native_mount_path.clone().unwrap()
    );
    assert_eq!(
        from_metadata
            .aliases
            .iter()
            .map(|alias| alias.path.clone())
            .collect::<Vec<_>>(),
        info.native_aliases
    );
    assert_eq!(from_metadata.identity.scheme, "native_volume_identity");

    let test_dir = TestDir::new("windows-native-propagation");
    let store = Store::create(&test_dir.path().join("store")).unwrap();
    let recorded = record_scan_disk(&store, &info, "fixture-time", None).unwrap().0;
    assert_eq!(recorded.disk.aliases, info.native_aliases);
    assert_eq!(recorded.disk.last_mount_path, info.native_mount_path);
}

#[test]
fn default_build_fresh_windows_scan_and_delta_write_dds2_v4() {
    let root = fixture_root();
    let dir = TestDir::new("windows-fresh-dds2-scan");
    let settings = resolve_with(
        &Overrides {
            db: Some(dir.path().join("store")),
            ..Default::default()
        },
        dir.path(),
        dir.path(),
    )
    .unwrap();
    let mut request = ScanRequest::from_settings(
        &settings,
        Some(root.clone()),
        Some("windows-fresh".into()),
        &[],
    );
    request.threads = 1;

    let first = scan(&settings, &request, &AtomicBool::new(false), None)
        .expect("default Windows scan must write DDS2 instead of reporting an unsupported feature");
    assert_eq!(first.snapshot.status, Status::Completed);
    assert!(first.snapshot.storage.data_file.as_deref().is_some_and(|path| {
        path.starts_with("snapshots-v4/") && path.ends_with(".dds")
    }));

    request.name = Some("windows-followup".into());
    let second = scan(&settings, &request, &AtomicBool::new(false), None)
        .expect("second Windows scan must create a DDS2 reverse delta");
    let store = Store::open(&second.store_path).unwrap();
    for snapshot in store.list().unwrap() {
        assert_eq!(store.verify(snapshot.id).unwrap(), snapshot.counts.unwrap().entries);
        let path = store
            .layout()
            .data_path(snapshot.storage.data_file.as_deref().unwrap())
            .unwrap();
        let data = DataFile::open_with_volumes(&path, &snapshot.volumes).unwrap();
        assert!(data.descriptor().is_dds2());
    }
    assert!(
        store
            .list()
            .unwrap()
            .iter()
            .any(|snapshot| snapshot.storage.base.is_some()),
        "the first scan must be rebased as a DDS2 reverse delta"
    );
    assert_eq!(
        DataFile::open_with_volumes(
            &store
                .layout()
                .data_path(
                    store
                        .list()
                        .unwrap()
                        .iter()
                        .find(|snapshot| snapshot.storage.base.is_some())
                        .unwrap()
                        .storage
                        .data_file
                        .as_deref()
                        .unwrap(),
                )
                .unwrap(),
            &store
                .list()
                .unwrap()
                .iter()
                .find(|snapshot| snapshot.storage.base.is_some())
                .unwrap()
                .volumes,
        )
        .unwrap()
        .kind,
        FileKind::Ops
    );
}
