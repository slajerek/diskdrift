#![cfg(unix)]

use diskdrift_app::config::{Overrides, resolve_with};
use diskdrift_app::snapshots::{ScanRequest, scan};
use diskdrift_core::testutil::TestDir;
use diskdrift_storage::Store;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

#[test]
fn scan_persists_descriptors_for_real_mount_devices_and_writes_their_records() {
    let mounts = diskdrift_platform::mounts().unwrap().mounts;
    let root = diskdrift_platform::statfs_path(Path::new("/")).unwrap();
    let mounted_child = mounts
        .iter()
        .find(|mount| {
            mount.mount_point != Path::new("/")
                && mount.mount_point.parent() == Some(Path::new("/"))
                && mount.dev != 0
                && mount.dev != root.dev
        })
        .unwrap_or_else(|| panic!("required nested filesystem mount is unavailable"));

    let dir = TestDir::new("multi-volume-scan");
    let settings = resolve_with(
        &Overrides {
            db: Some(dir.path().join("store")),
            ..Default::default()
        },
        dir.path(),
        dir.path(),
    )
    .unwrap();
    let excluded = fs::read_dir("/")
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path != &mounted_child.mount_point)
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let mut request = ScanRequest::from_settings_with_native_root(
        &settings,
        Some(PathBuf::from("/")),
        None,
        Some("real-multi-volume".into()),
        &excluded,
    );
    request.stay_on_filesystem = false;
    request.threads = 1;

    let outcome = scan(&settings, &request, &AtomicBool::new(false), None).unwrap();
    let report = outcome.snapshot.counts.as_ref().unwrap();
    assert!(
        report.devices.len() > 1,
        "expected multiple devices: {:?}",
        report.devices
    );
    assert_eq!(outcome.snapshot.volumes.len(), report.devices.len());
    assert!(report.devices.contains(&mounted_child.dev));
    for (index, descriptor) in outcome.snapshot.volumes.iter().enumerate() {
        assert_eq!(descriptor.index as usize, index);
        assert!(!descriptor.identity.display().is_empty());
    }

    let store = Store::open(&outcome.store_path).unwrap();
    let records = store
        .records(outcome.snapshot.id)
        .unwrap()
        .collect::<std::io::Result<Vec<_>>>()
        .unwrap();
    let child = records
        .iter()
        .find(|record| record.vol != records[0].vol)
        .unwrap_or_else(|| {
            panic!(
                "missing record from {}",
                mounted_child.mount_point.display()
            )
        });
    assert_ne!(child.vol, records[0].vol);
    assert!(outcome.snapshot.volumes.get(child.vol as usize).is_some());
}
