use diskdrift_core::native::NativePath;
use diskdrift_core::record::{EntryFields, FileIdScheme, Kind, Record, flags};
use diskdrift_storage::container::{DataFile, FileKind};
use diskdrift_storage::disks::RegistryV2;
#[cfg(all(unix, feature = "test-seams"))]
use diskdrift_storage::test_support::fail_atomic_replacement_for;
use diskdrift_storage::{Registry, Store, Strength};
use std::fs;
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn tree_manifest(path: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn visit(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(Result::unwrap).collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            let contents = if metadata.is_file() {
                fs::read(&path).unwrap()
            } else {
                Vec::new()
            };
            out.push((rel, contents));
            if metadata.is_dir() {
                visit(root, &path, out);
            }
        }
    }

    let mut out = Vec::new();
    visit(path, path, &mut out);
    out
}

#[test]
fn dds1_decodes_frozen_records() {
    let path = fixture("dds1/data.dds");
    let data = DataFile::open(&path).unwrap();
    assert_eq!(data.kind, FileKind::Records);
    let records: Vec<Record> = data
        .records_from(0)
        .unwrap()
        .collect::<std::io::Result<_>>()
        .unwrap();
    assert_eq!(
        records,
        vec![
            Record {
                depth: 0,
                name: diskdrift_core::native::NativeName::unix(b"/r".to_vec()),
                kind: Kind::Dir,
                ino: 0,
                ..Default::default()
            },
            Record {
                depth: 1,
                name: diskdrift_core::native::NativeName::unix(b"bad\xff".to_vec()),
                kind: Kind::File,
                size: 7,
                alloc: 4096,
                btime: 0,
                ino: 0,
                ..Default::default()
            },
            Record {
                depth: 1,
                name: diskdrift_core::native::NativeName::unix(b"error".to_vec()),
                kind: Kind::File,
                flags: flags::STAT_ERROR,
                available: diskdrift_core::record::EntryFields::empty(),
                ..Default::default()
            },
            Record {
                depth: 1,
                name: diskdrift_core::native::NativeName::unix(b"hardlink-a".to_vec()),
                kind: Kind::File,
                size: 11,
                alloc: 4096,
                ino: 42,
                nlink: 2,
                ..Default::default()
            },
            Record {
                depth: 1,
                name: diskdrift_core::native::NativeName::unix(b"hardlink-b".to_vec()),
                kind: Kind::File,
                flags: flags::HARDLINK_DUP,
                size: 11,
                alloc: 4096,
                ino: 42,
                nlink: 2,
                ..Default::default()
            },
        ]
    );
    for record in records.iter().filter(|record| record.ino != 0) {
        assert!(record.available.contains(EntryFields::FILE_ID));
        assert_eq!(record.file_id_scheme, FileIdScheme::PosixInode64);
        assert_eq!(record.file_id.high, 0);
    }
    for record in records.iter().filter(|record| record.ino == 0) {
        assert!(!record.available.contains(EntryFields::FILE_ID));
        assert_eq!(record.file_id_scheme, FileIdScheme::Unavailable);
        assert_eq!(record.file_id, diskdrift_core::record::FileId::ZERO);
    }
}

#[test]
fn store_v1_v2_v3_open_without_writes() {
    for version in ["store-v1", "store-v2", "store-v3"] {
        let path = fixture(version);
        let before = tree_manifest(&path);
        let marker: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path.join("store.json")).unwrap()).unwrap();
        let snapshots_dir = path.join("snapshots");
        let mut names: Vec<_> = fs::read_dir(&snapshots_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        match version {
            "store-v1" => {
                assert_eq!(marker["version"], 1);
                assert!(!path.join("disks.json").exists());
                assert_eq!(
                    names,
                    [
                        "00000001.json",
                        "00000001.r00000002.dds",
                        "00000002.dds",
                        "00000002.json"
                    ]
                );
            }
            "store-v2" => {
                assert_eq!(marker["version"], 2);
                assert!(!path.join("disks.json").exists());
                assert_eq!(
                    names,
                    [
                        "261021-100000-2.dds",
                        "261021-100000-2.json",
                        "261021-100000.json",
                        "261021-100000.r261021-100000-2.dds"
                    ]
                );
            }
            "store-v3" => {
                assert_eq!(marker["version"], 3);
                assert!(path.join("disks.json").exists());
                assert_eq!(
                    names,
                    [
                        "dc4396bd8-261021-100000-2.dds",
                        "dc4396bd8-261021-100000-2.json",
                        "dc4396bd8-261021-100000.json",
                        "dc4396bd8-261021-100000.rdc4396bd8-261021-100000-2.dds"
                    ]
                );
            }
            _ => unreachable!(),
        }
        let store = Store::open(&path).unwrap();
        let snapshots = store.list().unwrap();
        assert_eq!(
            snapshots.len(),
            2,
            "{version} lost the reverse-delta history"
        );
        assert!(
            snapshots
                .iter()
                .any(|snapshot| snapshot.storage.base.is_some()),
            "{version} has no reverse delta"
        );
        for id in [1, 2] {
            assert_eq!(
                store.records(id).unwrap().count(),
                5,
                "{version} snapshot {id} did not decode"
            );
            assert_eq!(
                store.verify(id).unwrap(),
                5,
                "{version} snapshot {id} did not verify"
            );
        }
        let after = tree_manifest(&path);
        assert_eq!(before, after, "reading {version} wrote or changed a file");
    }
}

#[test]
fn registry_v1_preserves_weak_identity() {
    let path = fixture("registry-v1");
    let before = tree_manifest(&path);
    let registry = Registry::load(&path).unwrap();
    let disk = &registry.disks[0];
    assert_eq!(disk.identity.strength, Strength::Weak);
    assert_eq!(disk.identity.source, "exfat|ARCHIVE|1234");
    assert_eq!(disk.identity.platform, "unknown");
    assert_eq!(disk.identity.scheme, "legacy_string");
    assert_eq!(disk.id, "dlegacy01");
    assert_eq!(tree_manifest(&path), before);
}

#[test]
fn registry_v2_fixture_round_trips_full_model_and_exact_fields() {
    let dir = diskdrift_core::testutil::TestDir::new("registry-v2-full-fixture");
    let document: RegistryV2 =
        serde_json::from_slice(&fs::read(fixture("registry-v2/full.json")).unwrap()).unwrap();
    let model = Registry::from_v2(document).unwrap();
    let serialized = serde_json::to_vec_pretty(&model.as_v2()).unwrap();
    fs::write(dir.path().join("disks.json"), serialized).unwrap();

    let registry = Registry::load(dir.path()).unwrap();
    let disk = &registry.disks[0];
    assert_eq!(disk.id, "dwindows01");
    assert_eq!(disk.identity.platform, "windows");
    assert_eq!(disk.identity.scheme, "volume_guid");
    assert_eq!(disk.identity.strength, Strength::Uuid);
    assert_eq!(
        disk.identity.source,
        r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}"
    );
    assert_eq!(disk.label.as_deref(), Some("archive"));
    assert_eq!(disk.description.as_deref(), Some("offline Windows archive"));
    assert_eq!(disk.sets, ["archives", "usb"]);
    assert_eq!(disk.volume.name.as_deref(), Some("ARCHIVE"));
    assert_eq!(disk.volume.fs_type, "ntfs");
    assert_eq!(disk.volume.capacity_bytes, Some(500_000_000_000));
    let device = disk
        .device
        .as_ref()
        .expect("full registry fixture preserves device facts");
    assert_eq!(device.model.as_deref(), Some("Portable SSD"));
    assert_eq!(device.vendor.as_deref(), Some("DiskDrift"));
    assert_eq!(device.bus.as_deref(), Some("USB"));
    assert_eq!(device.serial.as_deref(), Some("SERIAL-01"));
    assert_eq!(device.media_bytes, Some(512_000_000_000));
    assert_eq!(device.internal, Some(false));
    assert_eq!(disk.first_seen, "2026-01-01T00:00:00Z");
    assert_eq!(disk.last_seen, "2026-10-28T12:00:00Z");
    assert_eq!(disk.last_mount_point.as_deref(), Some(r"E:\Archive"));
    assert_eq!(disk.aliases.len(), 2);
    assert_eq!(
        disk.aliases[0],
        NativePath::windows(r"E:\Archive".encode_utf16().collect())
    );
    assert_eq!(
        disk.aliases[1],
        NativePath::windows(r"E:\Archive\".encode_utf16().chain([0xd800]).collect(),)
    );
    assert_eq!(
        disk.last_mount_path,
        Some(NativePath::windows(r"E:\Archive".encode_utf16().collect()))
    );
}

#[test]
fn legacy_string_registry_fixture_is_display_only_identity() {
    let dir = diskdrift_core::testutil::TestDir::new("registry-legacy-string-fixture");
    fs::write(
        dir.path().join("disks.json"),
        fs::read(fixture("registry-legacy-string/disks.json")).unwrap(),
    )
    .unwrap();

    let registry = Registry::load(dir.path()).unwrap();
    let disk = &registry.disks[0];
    assert_eq!(disk.identity.platform, "unknown");
    assert_eq!(disk.identity.scheme, "legacy_string");
    assert_eq!(disk.identity.strength, Strength::Weak);
    assert_eq!(disk.identity.source, "/Volumes/Archive");
}

#[cfg(all(unix, feature = "test-seams"))]
#[test]
#[ignore = "requires a locally built standalone frozen v3 reader"]
fn frozen_v3_reader_observes_commit_boundary() {
    let reader = std::env::var_os("DISKDRIFT_V3_READER").expect("DISKDRIFT_V3_READER is required");
    let reader_home = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/v3-reader-home");
    fs::create_dir_all(&reader_home).unwrap();
    for source in ["store-v1", "store-v2", "store-v3"] {
        let migrated = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("v3-reader-{source}"));
        let _ = fs::remove_dir_all(&migrated);
        copy_tree(&fixture(source), &migrated);
        let store = Store::open(&migrated).unwrap();

        // The staging namespace exists while the legacy marker is still live.
        // The frozen reader must continue to read the old view at this point.
        let marker = migrated.join("store.json");
        let guard = fail_atomic_replacement_for(&marker);
        let result = store.lock();
        drop(guard);
        assert!(
            result.is_err(),
            "migration did not stop before the marker commit"
        );
        assert_eq!(
            store.layout().version(),
            match source {
                "store-v1" => 1,
                "store-v2" => 2,
                "store-v3" => 3,
                _ => unreachable!(),
            }
        );
        assert!(migrated.join("snapshots-v4").is_dir());

        let status = std::process::Command::new(&reader)
            .arg("--db")
            .arg(&migrated)
            .arg("snapshots")
            .arg("--json")
            .env("HOME", &reader_home)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "frozen reader rejected the pre-commit {source} view"
        );
        for id in ["1", "2"] {
            let status = std::process::Command::new(&reader)
                .arg("--db")
                .arg(&migrated)
                .arg("snapshots")
                .arg("verify")
                .arg(id)
                .env("HOME", &reader_home)
                .status()
                .unwrap();
            assert!(
                status.success(),
                "frozen reader failed to verify {source} snapshot {id}"
            );
        }

        let (lock, outcome) = store.lock().unwrap();
        assert_eq!(outcome, diskdrift_storage::MigrationOutcome::V4Active);
        drop(lock);
        let status = std::process::Command::new(&reader)
            .arg("--db")
            .arg(&migrated)
            .arg("snapshots")
            .arg("--json")
            .env("HOME", &reader_home)
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "frozen reader accepted the committed v4 marker"
        );
    }
}

#[cfg(all(unix, feature = "test-seams"))]
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
