use diskdrift_core::record::{Kind, Record, flags};
use diskdrift_storage::container::{DataFile, FileKind};
use diskdrift_storage::{Registry, Store, Strength};
use std::fs;
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn tree_manifest(path: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn visit(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(Result::unwrap).collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            let contents = if metadata.is_file() { fs::read(&path).unwrap() } else { Vec::new() };
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
    let records: Vec<Record> = data.records_from(0).unwrap().collect::<std::io::Result<_>>().unwrap();
    assert_eq!(records, vec![
        Record { depth: 0, name: diskdrift_core::native::NativeName::unix(b"/r".to_vec()), kind: Kind::Dir, ino: 0, ..Default::default() },
        Record { depth: 1, name: diskdrift_core::native::NativeName::unix(b"bad\xff".to_vec()), kind: Kind::File, size: 7, alloc: 4096, btime: 0, ino: 0, ..Default::default() },
        Record { depth: 1, name: diskdrift_core::native::NativeName::unix(b"error".to_vec()), kind: Kind::File, flags: flags::STAT_ERROR, available: diskdrift_core::record::EntryFields::empty(), ..Default::default() },
        Record { depth: 1, name: diskdrift_core::native::NativeName::unix(b"hardlink-a".to_vec()), kind: Kind::File, size: 11, alloc: 4096, ino: 42, nlink: 2, ..Default::default() },
        Record { depth: 1, name: diskdrift_core::native::NativeName::unix(b"hardlink-b".to_vec()), kind: Kind::File, flags: flags::HARDLINK_DUP, size: 11, alloc: 4096, ino: 42, nlink: 2, ..Default::default() },
    ]);
}

#[test]
fn store_v1_v2_v3_open_without_writes() {
    for version in ["store-v1", "store-v2", "store-v3"] {
        let path = fixture(version);
        let before = tree_manifest(&path);
        let marker: serde_json::Value = serde_json::from_str(&fs::read_to_string(path.join("store.json")).unwrap()).unwrap();
        let snapshots_dir = path.join("snapshots");
        let mut names: Vec<_> = fs::read_dir(&snapshots_dir).unwrap().map(|entry| entry.unwrap().file_name()).collect();
        names.sort();
        match version {
            "store-v1" => {
                assert_eq!(marker["version"], 1);
                assert!(!path.join("disks.json").exists());
                assert_eq!(names, ["00000001.json", "00000001.r00000002.dds", "00000002.dds", "00000002.json"]);
            }
            "store-v2" => {
                assert_eq!(marker["version"], 2);
                assert!(!path.join("disks.json").exists());
                assert_eq!(names, ["261021-100000-2.dds", "261021-100000-2.json", "261021-100000.json", "261021-100000.r261021-100000-2.dds"]);
            }
            "store-v3" => {
                assert_eq!(marker["version"], 3);
                assert!(path.join("disks.json").exists());
                assert_eq!(names, ["dc4396bd8-261021-100000-2.dds", "dc4396bd8-261021-100000-2.json", "dc4396bd8-261021-100000.json", "dc4396bd8-261021-100000.rdc4396bd8-261021-100000-2.dds"]);
            }
            _ => unreachable!(),
        }
        let store = Store::open(&path).unwrap();
        let snapshots = store.list().unwrap();
        assert_eq!(snapshots.len(), 2, "{version} lost the reverse-delta history");
        assert!(snapshots.iter().any(|snapshot| snapshot.storage.base.is_some()), "{version} has no reverse delta");
        for id in [1, 2] {
            assert_eq!(store.records(id).unwrap().count(), 5, "{version} snapshot {id} did not decode");
            assert_eq!(store.verify(id).unwrap(), 5, "{version} snapshot {id} did not verify");
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
    assert_eq!(disk.id, "dlegacy01");
    assert_eq!(tree_manifest(&path), before);
}

#[cfg(unix)]
#[test]
#[ignore = "requires a locally built standalone frozen v3 reader"]
fn frozen_v3_reader_observes_commit_boundary() {
    let reader = std::env::var_os("DISKDRIFT_V3_READER").expect("DISKDRIFT_V3_READER is required");
    let status = std::process::Command::new(&reader)
        .arg("--db").arg(fixture("store-v3"))
        .arg("snapshots").arg("--json")
        .env("HOME", fixture("v3-reader/home"))
        .status().unwrap();
    assert!(status.success(), "frozen reader rejected a committed v3 store");
    for id in ["1", "2"] {
        let status = std::process::Command::new(&reader)
            .arg("--db").arg(fixture("store-v3"))
            .arg("snapshots").arg("verify").arg(id)
            .env("HOME", fixture("v3-reader/home"))
            .status().unwrap();
        assert!(status.success(), "frozen reader failed to verify v3 snapshot {id}");
    }

    let v4 = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/v3-reader-v4");
    let _ = fs::remove_dir_all(&v4);
    copy_tree(&fixture("store-v3"), &v4);
    fs::write(v4.join("store.json"), br#"{"format":"diskdrift-snapshots","version":4}"#).unwrap();
    let status = std::process::Command::new(reader)
        .arg("--db").arg(&v4)
        .arg("snapshots").arg("--json")
        .env("HOME", fixture("v3-reader/home"))
        .status().unwrap();
    assert!(!status.success(), "frozen reader accepted the v4 marker");
}

#[cfg(unix)]
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
