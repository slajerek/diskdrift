//! DiskDrift snapshot storage (decision: docs/adr/0001-storage-engine.md).
//!
//! One immutable file per snapshot holding columnar, zstd-compressed record blocks
//! (a full checkpoint) or delta ops against a checkpoint, plus a small JSON
//! metadata file. Writes are atomic (temp file, fsync, rename); a single writer is
//! enforced with `flock`; readers never block.

pub mod codec;
pub mod container;
pub mod disks;
pub mod meta;
pub mod store;

pub use disks::{Disk, Identity, Observed, Registry, Sighting, Strength};
pub use meta::{ScanOptions, SnapshotMeta, SpaceSample, Status, StorageInfo, VolumeIdentity};
pub use store::{Compaction, CompactStep, OpStream, Rebase, Removal, RemovalPlan, SnapshotWriter, Store, StoreError, WriteLock};

#[cfg(test)]
mod tests {
    use super::*;
    use diskdrift_core::testutil::{boxed, records};
    use diskdrift_core::record::{Kind, Record};
    use diskdrift_core::testutil::TestDir;
    use diskdrift_core::walk::WalkReport;

    fn template(name: &str) -> SnapshotMeta {
        SnapshotMeta {
            format_version: meta::META_FORMAT_VERSION,
            id: 0,
            name: name.into(),
            root: "/r".into(),
            disk: None,
            root_in_volume: None,
            status: Status::InProgress,
            started_at: "2026-10-21T10:00:00+02:00".into(),
            finished_at: None,
            volume: VolumeIdentity { uuid: Some("U".into()), fs_type: "apfs".into(), mount_point: "/".into(), ..Default::default() },
            options: ScanOptions { stay_on_filesystem: true, ..Default::default() },
            space_before: None,
            space_after: None,
            storage: Default::default(),
            counts: None,
            error: None,
            diskdrift_version: "test".into(),
        }
    }

    fn write(store: &Store, name: &str, recs: &[Record], base: Option<&SnapshotMeta>) -> SnapshotMeta {
        let lock = store.lock().unwrap();
        let mut w = store.begin(lock, template(name), base).unwrap();
        for r in recs {
            w.push(r).unwrap();
        }
        w.commit(WalkReport { entries: recs.len() as u64, ..Default::default() }, None, "now".into()).unwrap()
    }

    fn read(store: &Store, id: u64) -> Vec<Record> {
        store.records(id).unwrap().collect::<std::io::Result<Vec<_>>>().unwrap()
    }

    fn tree(extra: usize, big: u64) -> Vec<Record> {
        let mut spec: Vec<(String, Kind, u64)> = vec![("".into(), Kind::Dir, 0)];
        for d in 0..30 {
            spec.push((format!("d{d:02}"), Kind::Dir, 0));
            for f in 0..200 {
                spec.push((format!("d{d:02}/f{f:03}"), Kind::File, (d * 1000 + f) as u64));
            }
        }
        for e in 0..extra {
            spec.push((format!("d05/new{e}"), Kind::File, big));
        }
        records(&spec)
    }

    fn write_rebase(store: &Store, name: &str, recs: &[Record]) -> (SnapshotMeta, Rebase) {
        let lock = store.lock().unwrap();
        let mut w = store.begin(lock, template(name), None).unwrap();
        for r in recs {
            w.push(r).unwrap();
        }
        let (m, rb) = w.commit_and_rebase(WalkReport { entries: recs.len() as u64, ..Default::default() }, None, "now".into()).unwrap();
        (m, rb.unwrap())
    }

    #[test]
    fn full_and_delta_roundtrip_and_refs() {
        let t = TestDir::new("store-roundtrip");
        let s = Store::create(&t.path().join("store")).unwrap();
        let v0 = tree(0, 0);
        let v1 = tree(5, 1 << 20);
        let m0 = write(&s, "baseline", &v0, None);
        assert_eq!(m0.status, Status::Completed);
        assert_eq!(read(&s, m0.id), v0);
        // Forward delta (supported by the writer, not used by scans).
        let m1 = write(&s, "followup", &v1, Some(&m0));
        assert_eq!(m1.storage.base, Some(m0.id));
        assert_eq!(m1.storage.items, 5, "only the changes are stored");
        assert_eq!(read(&s, m1.id), v1);
        assert!(m1.storage.bytes_on_disk < m0.storage.bytes_on_disk);
        assert_eq!(s.resolve("latest").unwrap().id, m1.id);
        assert_eq!(s.resolve("latest~1").unwrap().id, m0.id);
        assert_eq!(s.resolve("baseline").unwrap().id, m0.id);
        assert_eq!(s.resolve("2").unwrap().id, m1.id);
        assert!(matches!(s.resolve("nope"), Err(StoreError::NotFound(_))));
        assert_eq!(s.verify(m1.id).unwrap(), v1.len() as u64);
        let s2 = Store::open(s.path()).unwrap();
        assert_eq!(read(&s2, m1.id), v1);
    }

    #[test]
    fn reverse_deltas_keep_newest_full() {
        let t = TestDir::new("store-reverse");
        let s = Store::create(&t.path().join("store")).unwrap();
        let versions: Vec<Vec<Record>> = (0..5).map(|i| tree(i * 3, 1000 + i as u64)).collect();
        let mut metas: Vec<SnapshotMeta> = Vec::new();
        for (i, v) in versions.iter().enumerate() {
            let (m, rb) = write_rebase(&s, &format!("s{i}"), v);
            if i == 0 {
                assert_eq!(rb, Rebase::Nothing);
            } else {
                let prev = metas.last().unwrap().id;
                assert!(matches!(rb, Rebase::Converted { id, .. } if id == prev), "{rb:?}");
            }
            metas.push(m);
        }
        let listed = s.list().unwrap();
        assert!(listed.last().unwrap().storage.base.is_none(), "newest is full");
        for (i, m) in listed.iter().enumerate().take(4) {
            assert_eq!(m.storage.base, Some(listed[i + 1].id), "older ones are deltas against the next");
        }
        for (m, v) in metas.iter().zip(&versions) {
            assert_eq!(&read(&s, m.id), v, "snapshot {} reconstructs exactly", m.id);
        }
        let full = listed.last().unwrap().storage.bytes_on_disk;
        let deltas: u64 = listed.iter().take(4).map(|m| m.storage.bytes_on_disk).sum();
        assert!(deltas < full, "deltas {deltas} vs full {full}");
        // Leftover files from an interrupted rebase are cleaned up.
        std::fs::write(s.path().join("snapshots/261021-100000.r261021-999999.dds"), b"orphan").unwrap();
        drop(s.lock().unwrap());
        assert!(!s.path().join("snapshots/261021-100000.r261021-999999.dds").exists());
        for (m, v) in metas.iter().zip(&versions) {
            assert_eq!(&read(&s, m.id), v);
        }
    }

    #[test]
    fn chain_length_is_bounded_by_keyframes() {
        let t = TestDir::new("store-keyframe");
        let s = Store::create(&t.path().join("store")).unwrap();
        let mut kept = 0;
        for i in 0..(store::MAX_CHAIN + 3) {
            let (_, rb) = write_rebase(&s, &format!("s{i}"), &tree(i % 7, i as u64));
            if matches!(rb, Rebase::KeptFull { .. }) {
                kept += 1;
            }
        }
        assert_eq!(kept, 1, "one keyframe after MAX_CHAIN deltas");
        let all = s.list().unwrap();
        let chains = Store::chains_for_test(&all);
        assert!(chains.values().all(|(d, _)| *d <= store::MAX_CHAIN));
        let oldest = all.first().unwrap();
        assert_eq!(read(&s, oldest.id), tree(0, 0));
    }

    #[test]
    fn incompatible_roots_do_not_rebase() {
        let t = TestDir::new("store-incompat");
        let s = Store::create(&t.path().join("store")).unwrap();
        write_rebase(&s, "a", &tree(0, 0));
        let lock = s.lock().unwrap();
        let mut other = template("b");
        other.root = "/elsewhere".into();
        let mut w = s.begin(lock, other, None).unwrap();
        w.push(&Record { name: b"/elsewhere".to_vec(), kind: Kind::Dir, ..Default::default() }).unwrap();
        let (_, rb) = w.commit_and_rebase(WalkReport::default(), None, "now".into()).unwrap();
        assert_eq!(rb.unwrap(), Rebase::Nothing);
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let t = TestDir::new("store-dup");
        let s = Store::create(&t.path().join("store")).unwrap();
        write(&s, "a", &tree(0, 0), None);
        let lock = s.lock().unwrap();
        assert!(matches!(s.begin(lock, template("a"), None), Err(StoreError::DuplicateName(_))));
    }

    #[test]
    fn single_writer_lock() {
        let t = TestDir::new("store-lock");
        let s = Store::create(&t.path().join("store")).unwrap();
        let _held = s.lock().unwrap();
        let s2 = Store::open(s.path()).unwrap();
        assert!(matches!(s2.lock(), Err(StoreError::Locked { .. })));
        assert!(s2.is_busy());
    }

    #[test]
    fn abort_and_interrupted_scans_never_look_complete() {
        let t = TestDir::new("store-abort");
        let s = Store::create(&t.path().join("store")).unwrap();
        let lock = s.lock().unwrap();
        let mut w = s.begin(lock, template("cancelled"), None).unwrap();
        for r in &tree(0, 0) {
            w.push(r).unwrap();
        }
        let m = w.abort(Status::Cancelled, None, None, "now".into());
        assert_eq!(m.status, Status::Cancelled);
        assert!(matches!(s.records(m.id), Err(StoreError::NoData { .. })));

        // Simulate a crash: metadata says IN_PROGRESS, a temp file exists, no lock.
        let mut crashed = template("crashed");
        crashed.id = 99;
        std::fs::write(s.path().join("snapshots/00000099.json"), serde_json::to_vec(&crashed).unwrap()).unwrap();
        std::fs::write(s.path().join("snapshots/00000099.dds.tmp"), b"partial").unwrap();
        let listed = s.list().unwrap();
        assert_eq!(listed.iter().find(|m| m.id == 99).unwrap().status, Status::Failed);
        drop(s.lock().unwrap());
        assert!(!s.path().join("snapshots/00000099.dds.tmp").exists(), "recovery removes temp data");
        assert!(matches!(s.resolve("latest"), Err(StoreError::NotFound(_))));
    }

    #[test]
    fn corruption_is_detected() {
        let t = TestDir::new("store-corrupt");
        let s = Store::create(&t.path().join("store")).unwrap();
        let m = write(&s, "a", &tree(0, 0), None);
        let p = s.path().join("snapshots").join(m.storage.data_file.as_ref().unwrap());
        let mut bytes = std::fs::read(&p).unwrap();
        bytes[100] ^= 0xff;
        std::fs::write(&p, &bytes).unwrap();
        assert!(matches!(s.verify(m.id), Err(StoreError::Corrupt(_))));
        let r: std::io::Result<Vec<Record>> = s.records(m.id).unwrap().collect();
        assert!(r.is_err());
        // Truncation (e.g. a partially copied store) is caught by the trailer check.
        std::fs::write(&p, &bytes[..bytes.len() / 2]).unwrap();
        assert!(matches!(s.records(m.id), Err(StoreError::Corrupt(_))));
    }

    #[test]
    fn prefix_query_seeks() {
        let t = TestDir::new("store-prefix");
        let s = Store::create(&t.path().join("store")).unwrap();
        let v = tree(0, 0);
        let m = write(&s, "a", &v, None);
        let prefix = vec![b"/r".to_vec(), b"d17".to_vec()];
        let got: Vec<Record> = s.records_under(m.id, &prefix).unwrap().map(Result::unwrap).collect();
        assert_eq!(got.len(), 201);
        assert_eq!(got[0].name, b"d17");
        let none: Vec<Record> = s.records_under(m.id, &[b"/r".to_vec(), b"zz".to_vec()]).unwrap().map(Result::unwrap).collect();
        assert!(none.is_empty());
        let _ = boxed(Vec::new());
    }

    #[test]
    fn remove_folds_dependents() {
        let t = TestDir::new("store-remove");
        let s = Store::create(&t.path().join("store")).unwrap();
        let v: Vec<Vec<Record>> = (0..3).map(|i| tree(i * 4, 7 + i as u64)).collect();
        let ids: Vec<u64> = v.iter().enumerate().map(|(i, r)| write_rebase(&s, &format!("s{i}"), r).0.id).collect();
        // Chain: s0 -> s1 -> s2 (full). Removing s1 rewrites s0 against s2.
        let lock = s.lock().unwrap();
        let (freed, folded) = s.remove(&lock, ids[1]).unwrap();
        assert_eq!(folded, vec![ids[0]]);
        assert!(freed > 0);
        assert_eq!(read(&s, ids[0]), v[0]);
        assert_eq!(s.get(ids[0]).unwrap().storage.base, Some(ids[2]));
        // Removing the full newest makes its dependent full.
        let (_, folded) = s.remove(&lock, ids[2]).unwrap();
        assert_eq!(folded, vec![ids[0]]);
        assert!(s.get(ids[0]).unwrap().storage.base.is_none());
        assert_eq!(read(&s, ids[0]), v[0]);
        s.remove(&lock, ids[0]).unwrap();
        assert!(s.list().unwrap().is_empty());
        let left: Vec<_> = std::fs::read_dir(s.path().join("snapshots")).unwrap().flatten().collect();
        assert!(left.is_empty(), "no files left: {left:?}");
    }

    #[test]
    fn removing_a_range_rewrites_each_kept_snapshot_once() {
        let t = TestDir::new("store-remove-many");
        let s = Store::create(&t.path().join("store")).unwrap();
        let v: Vec<Vec<Record>> = (0..6).map(|i| tree(i * 4, 7 + i as u64)).collect();
        let ids: Vec<u64> = v.iter().enumerate().map(|(i, r)| write_rebase(&s, &format!("s{i}"), r).0.id).collect();
        // s0 -> s1 -> ... -> s5 (full). Removing s1..s3 rewrites only s0, against s4.
        let plan = s.plan_removal(&ids[1..4]).unwrap();
        assert_eq!(plan.removed, ids[1..4].to_vec());
        assert_eq!(plan.rewrites, vec![(ids[0], Some(ids[4]))]);
        assert!(plan.bytes > 0);
        assert_eq!(s.list().unwrap().len(), 6, "planning changes nothing");
        let lock = s.lock().unwrap();
        let r = s.remove_many(&lock, &ids[1..4], &std::sync::atomic::AtomicBool::new(false)).unwrap();
        assert_eq!((r.rewritten.clone(), r.cancelled), (vec![ids[0]], false));
        assert!(r.freed_bytes > 0);
        assert_eq!(read(&s, ids[0]), v[0]);
        assert_eq!(s.get(ids[0]).unwrap().storage.base, Some(ids[4]));
        // The newest goes too: s4 becomes the full one, s0 still reads.
        let r = s.remove_many(&lock, &[ids[5]], &std::sync::atomic::AtomicBool::new(false)).unwrap();
        assert_eq!(r.rewritten, vec![ids[4]]);
        assert!(s.get(ids[4]).unwrap().storage.base.is_none());
        assert_eq!(read(&s, ids[4]), v[4]);
        assert_eq!(read(&s, ids[0]), v[0]);
        // Everything before s4 (no rewrite: nothing kept depends on s0).
        assert_eq!(s.plan_removal(&[ids[0]]).unwrap().rewrites, vec![]);
        assert!(matches!(s.plan_removal(&[999]), Err(StoreError::NotFound(_))));
        let files = std::fs::read_dir(s.path().join("snapshots")).unwrap().count();
        assert_eq!(files, 2 * 2, "data and metadata of s0 and s4 only");
    }

    #[test]
    fn a_stopped_removal_removes_nothing() {
        let t = TestDir::new("store-remove-stop");
        let s = Store::create(&t.path().join("store")).unwrap();
        let v: Vec<Vec<Record>> = (0..3).map(|i| tree(i * 4, 7 + i as u64)).collect();
        let ids: Vec<u64> = v.iter().enumerate().map(|(i, r)| write_rebase(&s, &format!("s{i}"), r).0.id).collect();
        let lock = s.lock().unwrap();
        let r = s.remove_many(&lock, &[ids[1]], &std::sync::atomic::AtomicBool::new(true)).unwrap();
        assert!(r.cancelled && r.removed.is_empty());
        assert_eq!(s.list().unwrap().len(), 3);
        for (id, want) in ids.iter().zip(&v) {
            assert_eq!(&read(&s, *id), want);
        }
    }

    #[test]
    fn compaction_restores_the_written_layout() {
        let t = TestDir::new("store-compact");
        let s = Store::create(&t.path().join("store")).unwrap();
        let v: Vec<Vec<Record>> = (0..5).map(|i| tree(i * 4, 7 + i as u64)).collect();
        let ids: Vec<u64> = v.iter().enumerate().map(|(i, r)| write_rebase(&s, &format!("s{i}"), r).0.id).collect();
        let lock = s.lock().unwrap();
        let no = std::sync::atomic::AtomicBool::new(false);
        // Already as written: nothing to do, every file checked.
        let c = s.compact(&lock, &no, &mut |_| {}).unwrap();
        assert_eq!((c.rewritten.len(), c.checked, c.cancelled), (0, 5, false));
        // Make s1 full by hand, as a removal of a keyframe would.
        let m1 = s.get(ids[1]).unwrap();
        s.rewrite_for_test(&m1, None);
        assert!(s.get(ids[1]).unwrap().storage.base.is_none());
        let bigger = s.size_on_disk();
        let mut steps = Vec::new();
        let c = s.compact(&lock, &no, &mut |st| steps.push(st)).unwrap();
        assert_eq!(c.rewritten, vec![ids[1]]);
        assert_eq!(s.get(ids[1]).unwrap().storage.base, Some(ids[2]));
        assert!(c.bytes_after < bigger && c.bytes_before == bigger, "{c:?}");
        assert!(matches!(steps[0], CompactStep::Rewriting { done: 0, total: 1, .. }));
        for (id, want) in ids.iter().zip(&v) {
            assert_eq!(&read(&s, *id), want);
        }
        // Stopped before it starts: nothing changes.
        s.rewrite_for_test(&s.get(ids[1]).unwrap(), None);
        let c = s.compact(&lock, &std::sync::atomic::AtomicBool::new(true), &mut |_| {}).unwrap();
        assert!(c.cancelled && c.rewritten.is_empty());
        assert!(s.get(ids[1]).unwrap().storage.base.is_none());
    }

    #[test]
    fn compaction_keeps_chains_bounded() {
        let t = TestDir::new("store-compact-chain");
        let s = Store::create(&t.path().join("store")).unwrap();
        let small = |i: usize| records(&[("".into(), Kind::Dir, 0), (format!("f{i}"), Kind::File, i as u64)]);
        let n = store::MAX_CHAIN + 5;
        for i in 0..n {
            write_rebase(&s, &format!("s{i}"), &small(i));
        }
        let lock = s.lock().unwrap();
        let c = s.compact(&lock, &std::sync::atomic::AtomicBool::new(false), &mut |_| {}).unwrap();
        assert!(c.rewritten.is_empty(), "the writer's layout is the compact one: {c:?}");
        let all = s.list().unwrap();
        let deepest = Store::chains_for_test(&all).values().map(|(d, _)| *d).max().unwrap();
        assert!(deepest <= store::MAX_CHAIN);
    }

    #[test]
    fn create_refuses_foreign_directory() {
        let t = TestDir::new("store-foreign");
        std::fs::write(t.path().join("something.txt"), b"user data").unwrap();
        assert!(matches!(Store::create(t.path()), Err(StoreError::NotAStore(_))));
        assert!(t.path().join("something.txt").exists());
    }

    /// The disk ID of `template`'s volume (UUID "U").
    fn disk_u() -> String {
        disks::Identity::new(disks::Strength::Uuid, "U").short_id(8)
    }

    /// The files in `snapshots/`, sorted, with the test disk's ID shown as `D`.
    fn snapshot_files(s: &Store) -> Vec<String> {
        let d = disk_u();
        let mut v: Vec<String> = std::fs::read_dir(s.path().join("snapshots"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().replace(&d, "D"))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn files_are_named_after_the_scan_start() {
        let t = TestDir::new("store-names");
        let s = Store::create(&t.path().join("store")).unwrap();
        let (a, _) = write_rebase(&s, "a", &tree(0, 0));
        let (b, _) = write_rebase(&s, "b", &tree(3, 9));
        let d = disk_u();
        assert_eq!(a.disk.as_deref(), Some(d.as_str()), "the disk comes from the volume");
        assert_eq!(a.storage.stem, Some(format!("{d}-261021-100000")));
        assert_eq!(b.storage.stem, Some(format!("{d}-261021-100000-2")), "same second gets a suffix");
        assert_eq!(
            snapshot_files(&s),
            ["D-261021-100000-2.dds", "D-261021-100000-2.json", "D-261021-100000.json", "D-261021-100000.rD-261021-100000-2.dds"]
        );
        let reg = s.registry().unwrap();
        assert_eq!(reg.disks.len(), 1);
        assert_eq!(reg.disks[0].id, d);
        assert_eq!(read(&s, a.id), tree(0, 0));
    }

    #[test]
    fn version_1_store_is_renamed_on_lock() {
        let t = TestDir::new("store-migrate");
        let s = Store::create(&t.path().join("store")).unwrap();
        let mut a = template("a");
        a.started_at = "2026-10-20T08:15:30+02:00".into();
        let lock = s.lock().unwrap();
        let mut w = s.begin(lock, a, None).unwrap();
        for r in &tree(0, 0) {
            w.push(r).unwrap();
        }
        let a = w.commit(WalkReport::default(), None, "now".into()).unwrap();
        let (b, _) = write_rebase(&s, "b", &tree(3, 9));
        // Rewrite the store as version 1 wrote it: files named after ids, no stems.
        let dir = s.path().join("snapshots");
        std::fs::remove_file(s.path().join("disks.json")).unwrap();
        for m in s.list().unwrap() {
            let stem = m.storage.stem.clone().unwrap();
            let mut legacy = m.clone();
            legacy.storage.stem = None;
            legacy.disk = None;
            legacy.root_in_volume = None;
            let data = match m.storage.base {
                Some(base) => format!("{:08}.r{:08}.dds", m.id, base),
                None => format!("{:08}.dds", m.id),
            };
            std::fs::rename(dir.join(m.storage.data_file.as_ref().unwrap()), dir.join(&data)).unwrap();
            legacy.storage.data_file = Some(data);
            std::fs::remove_file(dir.join(format!("{stem}.json"))).unwrap();
            std::fs::write(dir.join(format!("{:08}.json", m.id)), serde_json::to_vec(&legacy).unwrap()).unwrap();
        }
        std::fs::write(s.path().join("store.json"), br#"{"format":"diskdrift-snapshots","version":1}"#).unwrap();
        assert_eq!(read(&s, a.id), tree(0, 0), "version 1 layout is readable without migrating");

        drop(s.lock().unwrap());
        assert_eq!(
            snapshot_files(&s),
            ["D-261020-081530.json", "D-261020-081530.rD-261021-100000.dds", "D-261021-100000.dds", "D-261021-100000.json"]
        );
        assert_eq!(read(&s, a.id), tree(0, 0));
        assert_eq!(read(&s, b.id), tree(3, 9));
        let marker = std::fs::read_to_string(s.path().join("store.json")).unwrap();
        assert!(marker.contains(&format!("\"version\":{}", store::STORE_VERSION)));
    }

    // ADR 0002: one disk at different mount points, and what is not comparable.

    fn on_disk(name: &str, root: &str, disk: Option<&str>, in_volume: &str) -> SnapshotMeta {
        let mut m = template(name);
        m.root = root.into();
        m.disk = disk.map(String::from);
        m.root_in_volume = Some(in_volume.into());
        m.volume.mount_point = root.into();
        m
    }

    fn rooted(recs: &[Record], root: &str) -> Vec<Record> {
        let mut v = recs.to_vec();
        v[0].name = root.as_bytes().to_vec();
        v
    }

    fn commit_meta(store: &Store, meta: SnapshotMeta, recs: &[Record]) -> (SnapshotMeta, Rebase) {
        let lock = store.lock().unwrap();
        let mut w = store.begin(lock, meta, None).unwrap();
        for r in recs {
            w.push(r).unwrap();
        }
        let (m, rb) = w.commit_and_rebase(WalkReport { entries: recs.len() as u64, ..Default::default() }, None, "now".into()).unwrap();
        (m, rb.unwrap())
    }

    #[test]
    fn one_disk_at_two_mount_points_is_one_lineage() {
        let t = TestDir::new("store-two-mounts");
        let s = Store::create(&t.path().join("store")).unwrap();
        let (r0, r1) = ("/Volumes/Backup", "/Volumes/Backup 1");
        let v0 = rooted(&tree(0, 0), r0);
        let v1 = rooted(&tree(0, 0), r1);
        let (a, _) = commit_meta(&s, on_disk("a", r0, Some("d1"), "/"), &v0);
        let (b, rb) = commit_meta(&s, on_disk("b", r1, Some("d1"), "/"), &v1);
        assert!(a.incompatibility(&b).is_none());
        assert!(matches!(rb, Rebase::Converted { id, .. } if id == a.id), "{rb:?}");
        let a = s.get(a.id).unwrap();
        assert_eq!(a.storage.base, Some(b.id));
        assert_eq!(a.storage.items, 0, "the trees are the same: an empty delta");
        // The older scan still reads under its own root.
        assert_eq!(read(&s, a.id), v0);
        assert_eq!(s.verify(a.id).unwrap(), v0.len() as u64);
        let report = diskdrift_core::diff::diff(s.records_as(a.id, r1).unwrap(), s.records(b.id).unwrap(), Default::default()).unwrap();
        let sum = &report.summary;
        assert_eq!((sum.added.count, sum.deleted.count, sum.grown.count, sum.shrunk.count), (0, 0, 0, 0));
        assert_eq!(sum.unchanged, v0.len() as u64);
    }

    #[test]
    fn a_disk_mounted_in_three_places_survives_removal_and_forward_deltas() {
        let t = TestDir::new("store-three-mounts");
        let s = Store::create(&t.path().join("store")).unwrap();
        let roots = ["/Volumes/X", "/Volumes/X 1", "/Volumes/X 2"];
        let versions: Vec<Vec<Record>> = roots.iter().enumerate().map(|(i, r)| rooted(&tree(i * 2, 77), r)).collect();
        let metas: Vec<SnapshotMeta> =
            roots.iter().zip(&versions).enumerate().map(|(i, (r, v))| commit_meta(&s, on_disk(&format!("s{i}"), r, Some("d1"), "/"), v).0).collect();
        for (m, v) in metas.iter().zip(&versions) {
            assert_eq!(&read(&s, m.id), v);
        }
        // Removing the middle one refolds the oldest against the newest.
        let lock = s.lock().unwrap();
        let (_, folded) = s.remove(&lock, metas[1].id).unwrap();
        drop(lock);
        assert_eq!(folded, [metas[0].id]);
        assert_eq!(s.get(metas[0].id).unwrap().storage.base, Some(metas[2].id));
        assert_eq!(read(&s, metas[0].id), versions[0]);
        // A forward delta against a base at another mount point.
        let v3 = rooted(&tree(9, 5), "/Volumes/X 3");
        let lock = s.lock().unwrap();
        let mut w = s.begin(lock, on_disk("s3", "/Volumes/X 3", Some("d1"), "/"), Some(&metas[2])).unwrap();
        for r in &v3 {
            w.push(r).unwrap();
        }
        let m3 = w.commit(WalkReport { entries: v3.len() as u64, ..Default::default() }, None, "now".into()).unwrap();
        assert_eq!(m3.storage.items, 5 + 4, "5 new files and 4 resized ones, not the whole tree");
        assert_eq!(read(&s, m3.id), v3);
    }

    #[test]
    fn two_disks_at_one_mount_point_are_not_comparable() {
        let t = TestDir::new("store-two-disks");
        let s = Store::create(&t.path().join("store")).unwrap();
        let v = rooted(&tree(0, 0), "/Volumes/USB");
        let (a, _) = commit_meta(&s, on_disk("a", "/Volumes/USB", Some("d1"), "/"), &v);
        let (b, rb) = commit_meta(&s, on_disk("b", "/Volumes/USB", Some("d2"), "/"), &v);
        assert!(a.incompatibility(&b).is_some_and(|w| w.contains("different disks")));
        assert_eq!(rb, Rebase::Nothing);
        assert_eq!(s.get(a.id).unwrap().storage.base, None);
    }

    #[test]
    fn comparability_rules() {
        let a = on_disk("a", "/Volumes/B/Docs", Some("d1"), "/Docs");
        let mut b = on_disk("b", "/Volumes/B 1/Docs", Some("d1"), "/Docs");
        assert!(a.incompatibility(&b).is_none());
        b.root_in_volume = Some("/Photos".into());
        assert!(a.incompatibility(&b).is_some_and(|w| w.contains("different folders")));
        b.root_in_volume = a.root_in_volume.clone();
        b.options.exclude = vec!["*.tmp".into()];
        assert!(a.incompatibility(&b).is_some_and(|w| w.contains("exclusion")));
        // Without disks: the old rule, but an unidentified volume is not "the same".
        let old = template("o1");
        let mut no_uuid = template("o2");
        assert!(old.incompatibility(&no_uuid).is_none());
        no_uuid.volume.uuid = None;
        assert!(old.incompatibility(&no_uuid).is_some_and(|w| w.contains("not identified")));
        assert!(no_uuid.incompatibility(&old).is_some());
        let mut both_none = template("o3");
        both_none.volume.uuid = None;
        assert!(no_uuid.incompatibility(&both_none).is_none());
        // One with a disk, one without: the old rule decides.
        let mut with_disk = template("o4");
        with_disk.disk = Some("d1".into());
        assert!(old.incompatibility(&with_disk).is_none());
        with_disk.root = "/elsewhere".into();
        assert!(old.incompatibility(&with_disk).is_some());
    }

    #[test]
    fn root_in_volume_is_relative_to_the_mount_point() {
        use meta::root_in_volume;
        assert_eq!(root_in_volume("/", "/").as_deref(), Some("/"));
        assert_eq!(root_in_volume("/Users/mars", "/").as_deref(), Some("/Users/mars"));
        assert_eq!(root_in_volume("/Volumes/B", "/Volumes/B").as_deref(), Some("/"));
        assert_eq!(root_in_volume("/Volumes/B/", "/Volumes/B").as_deref(), Some("/"));
        assert_eq!(root_in_volume("/Volumes/B/Docs", "/Volumes/B/").as_deref(), Some("/Docs"));
        assert_eq!(root_in_volume("/Volumes/B 1/Docs", "/Volumes/B"), None);
        assert_eq!(root_in_volume("/Volumes/Other", "/Volumes/B"), None);
    }

    // ADR 0002, phase 5: store version 3 and the migration from version 2.

    /// Rewrites `s` as a version 2 store wrote it: time-only stems, no disk in
    /// the metadata, no registry.
    fn downgrade_to_v2(s: &Store) {
        let dir = s.path().join("snapshots");
        let metas = s.list().unwrap();
        let v2 = |stem: &str| stem.split_once('-').unwrap().1.to_string();
        for m in &metas {
            let stem = m.storage.stem.clone().unwrap();
            let mut old = m.clone();
            old.disk = None;
            old.root_in_volume = None;
            old.storage.stem = Some(v2(&stem));
            if let Some(data) = &m.storage.data_file {
                let name = match m.storage.base.and_then(|b| metas.iter().find(|x| x.id == b)) {
                    Some(b) => format!("{}.r{}.dds", v2(&stem), v2(b.storage.stem.as_ref().unwrap())),
                    None => format!("{}.dds", v2(&stem)),
                };
                std::fs::rename(dir.join(data), dir.join(&name)).unwrap();
                old.storage.data_file = Some(name);
            }
            std::fs::remove_file(dir.join(format!("{stem}.json"))).unwrap();
            std::fs::write(dir.join(format!("{}.json", v2(&stem))), serde_json::to_vec(&old).unwrap()).unwrap();
        }
        let _ = std::fs::remove_file(s.path().join("disks.json"));
        std::fs::write(s.path().join("store.json"), br#"{"format":"diskdrift-snapshots","version":2}"#).unwrap();
    }

    fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                copy_dir(&e.path(), &to.join(e.file_name()));
            } else {
                std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
            }
        }
    }

    /// A version 2 store with three scans of one folder (two reverse deltas),
    /// a failed scan, and a scan of an exFAT stick; and each snapshot's records.
    fn v2_store(t: &TestDir) -> (Store, Vec<(u64, Vec<Record>)>) {
        let s = Store::create(&t.path().join("v2")).unwrap();
        let mut out = Vec::new();
        for i in 0..3 {
            let mut m = template(&format!("s{i}"));
            m.started_at = format!("2026-10-2{i}T10:00:00+02:00");
            let v = tree(i * 2, 50 + i as u64);
            let (m, _) = commit_meta(&s, m, &v);
            out.push((m.id, v));
        }
        let mut failed = template("failed");
        failed.started_at = "2026-10-23T10:00:00+02:00".into();
        let lock = s.lock().unwrap();
        s.begin(lock, failed, None).unwrap().abort(Status::Failed, Some("boom".into()), None, "now".into());
        let mut stick = on_disk("stick", "/Volumes/STICK", None, "/");
        stick.root_in_volume = None;
        stick.volume = VolumeIdentity { uuid: Some("RANDOM-V4".into()), name: Some("STICK".into()), fs_type: "exfat".into(), mount_point: "/Volumes/STICK".into(), volume_group: false };
        stick.started_at = "2026-10-24T10:00:00+02:00".into();
        let v = rooted(&tree(1, 3), "/Volumes/STICK");
        let (m, _) = commit_meta(&s, stick, &v);
        out.push((m.id, v));
        downgrade_to_v2(&s);
        (s, out)
    }

    fn assert_migrated(s: &Store, expected: &[(u64, Vec<Record>)]) {
        for (id, v) in expected {
            assert_eq!(&read(s, *id), v, "snapshot {id}");
            s.verify(*id).unwrap();
        }
        let metas = s.list().unwrap();
        assert!(metas.iter().all(|m| m.disk.is_some() && m.root_in_volume.is_some()));
        let d = disk_u();
        let files = snapshot_files(s);
        assert_eq!(files.len(), 9, "{files:?}");
        let stick = metas.iter().find(|m| m.name == "stick").unwrap().disk.clone().unwrap();
        for f in &files {
            assert!(f.starts_with("D-") || f.starts_with(&stick), "{f} is not named after its disk");
        }
        assert!(files.contains(&"D-261020-100000.rD-261021-100000.dds".to_string()), "{files:?}");
        assert_ne!(stick, d);
        let reg = Registry::load(s.path()).unwrap();
        assert_eq!(reg.disks.len(), 2);
        let main = reg.get(&d).unwrap();
        assert_eq!((main.first_seen.as_str(), main.last_seen.as_str()), ("2026-10-20T10:00:00+02:00", "2026-10-23T10:00:00+02:00"));
        let stick = reg.get(&stick).unwrap();
        assert_eq!(stick.identity.strength, disks::Strength::Weak, "a FAT/exFAT UUID in metadata is not trusted");
        assert_eq!(stick.volume.name.as_deref(), Some("STICK"));
        let marker = std::fs::read_to_string(s.path().join("store.json")).unwrap();
        assert!(marker.contains("\"version\":3"), "{marker}");
    }

    #[test]
    fn a_version_2_store_reads_as_it_is_and_migrates_on_lock() {
        let t = TestDir::new("store-v2-migrate");
        let (s, expected) = v2_store(&t);
        let before = snapshot_files(&s);
        // Read without migrating: disks derived in memory, nothing written.
        let metas = s.list().unwrap();
        assert!(metas.iter().all(|m| m.disk.is_some()));
        assert_eq!(metas[0].disk, metas[1].disk);
        assert_eq!(metas[0].root_in_volume.as_deref(), Some("/r"));
        assert!(metas[0].incompatibility(&metas[1]).is_none());
        assert_eq!(s.registry().unwrap().disks.len(), 2);
        for (id, v) in &expected {
            assert_eq!(&read(&s, *id), v);
        }
        assert_eq!(snapshot_files(&s), before);
        assert!(!s.path().join("disks.json").exists());

        drop(s.lock().unwrap());
        assert_migrated(&s, &expected);
        // Migrating again changes nothing.
        let files = snapshot_files(&s);
        let reg = std::fs::read(s.path().join("disks.json")).unwrap();
        drop(s.lock().unwrap());
        assert_eq!(snapshot_files(&s), files);
        assert_eq!(std::fs::read(s.path().join("disks.json")).unwrap(), reg);
    }

    #[test]
    fn an_interrupted_migration_reads_the_same_and_finishes_later() {
        let t = TestDir::new("store-v2-crash");
        let (s, expected) = v2_store(&t);
        let mut n = 1;
        loop {
            let copy = t.path().join(format!("crash{n}"));
            copy_dir(s.path(), &copy);
            let c = Store::open(&copy).unwrap();
            store::crash::arm(Some(n));
            let r = c.lock();
            store::crash::arm(None);
            match r {
                Ok(_) => break,
                Err(e) => assert!(e.to_string().contains("simulated crash"), "{e}"),
            }
            // In between, everything reads as before.
            for (id, v) in &expected {
                assert_eq!(&read(&c, *id), v, "after crash {n}, snapshot {id}");
            }
            drop(c.lock().unwrap());
            assert_migrated(&c, &expected);
            n += 1;
        }
        assert!(n > 10, "every step was interrupted once ({n} steps)");
    }

    #[test]
    fn the_system_disk_folder_is_the_path_from_the_root() {
        let mut m = template("sys");
        m.root = "/Users/mars/Library".into();
        m.volume.mount_point = "/System/Volumes/Data".into();
        m.volume.volume_group = true;
        assert_eq!(m.derived_root_in_volume(), "/Users/mars/Library");
        m.volume.volume_group = false;
        m.volume.mount_point = "/Volumes/B".into();
        m.root = "/Volumes/B/Docs".into();
        assert_eq!(m.derived_root_in_volume(), "/Docs");
    }

    // ADR 0002, phase 6: references to a disk's snapshots, and merging disks.

    #[test]
    fn references_can_name_a_disk() {
        let t = TestDir::new("store-disk-refs");
        let s = Store::create(&t.path().join("store")).unwrap();
        let v = rooted(&tree(0, 0), "/Volumes/A");
        let (a1, _) = commit_meta(&s, on_disk("a1", "/Volumes/A", Some("daaaaaaaa"), "/"), &v);
        let (b1, _) = commit_meta(&s, on_disk("b1", "/Volumes/A", Some("dbbbbbbbb"), "/"), &v);
        let (a2, _) = commit_meta(&s, on_disk("a2", "/Volumes/A", Some("daaaaaaaa"), "/"), &v);
        // The disks are in the registry under these IDs (as a scan records them).
        s.update_registry(|r| {
            for (id, uuid) in [("daaaaaaaa", "A"), ("dbbbbbbbb", "B")] {
                let mut sg = Sighting::of_snapshot(&template("x"));
                sg.identity = disks::Identity::new(disks::Strength::Uuid, uuid);
                r.add(&sg);
                r.disks.last_mut().unwrap().id = id.into();
            }
            r.set_label("daaaaaaaa", Some("backupy-2014"))
        })
        .unwrap();
        assert_eq!(s.resolve("backupy-2014:latest").unwrap().id, a2.id);
        assert_eq!(s.resolve("BACKUPY-2014:latest~1").unwrap().id, a1.id);
        assert_eq!(s.resolve("dBBB:latest").unwrap().id, b1.id, "a prefix of the ID");
        assert_eq!(s.resolve("DBBBBBBBB:").unwrap().id, b1.id, "any case; nothing after the colon means latest");
        assert_eq!(s.resolve("backupy-2014:a1").unwrap().id, a1.id);
        assert!(matches!(s.resolve("dbbbbbbbb:latest~1"), Err(StoreError::NotFound(r)) if r == "dbbbbbbbb:latest~1"));
        assert!(matches!(s.resolve("nope:latest"), Err(StoreError::UnknownDisk(_))));
        assert_eq!(s.resolve("latest").unwrap().id, a2.id);
    }

    #[test]
    fn merging_disks_joins_their_histories() {
        let t = TestDir::new("store-merge");
        let s = Store::create(&t.path().join("store")).unwrap();
        // An exFAT stick: an old scan with a weak identity, a new one with its serial UUID.
        let mut old = on_disk("old", "/Volumes/STICK", None, "/");
        old.root_in_volume = None;
        old.volume = VolumeIdentity { uuid: Some("RANDOM".into()), name: Some("STICK".into()), fs_type: "exfat".into(), mount_point: "/Volumes/STICK".into(), volume_group: false };
        let v0 = rooted(&tree(0, 0), "/Volumes/STICK");
        let (old, _) = commit_meta(&s, old, &v0);
        let serial = s.update_registry(|r| {
            let mut sg = Sighting::of_snapshot(&template("x"));
            sg.identity = disks::Identity::new(disks::Strength::Serial, "SERIAL-UUID");
            sg.at = "2026-10-25T10:00:00+02:00".into();
            let id = r.add(&sg);
            r.set_label(&id, Some("stick"))?;
            r.add_to_set(&id, "usb")?;
            Ok(id)
        })
        .unwrap();
        let weak = old.disk.clone().unwrap();
        s.update_registry(|r| r.add_to_set(&weak, "old")).unwrap();
        let mut new = on_disk("new", "/Volumes/STICK", Some(&serial), "/");
        new.volume = old.volume.clone();
        let v1 = rooted(&tree(2, 1), "/Volumes/STICK");
        let (new, rb) = commit_meta(&s, new, &v1);
        assert_eq!(rb, Rebase::Nothing, "two disks: no common history yet");
        assert!(old.incompatibility(&new).is_some());

        let lock = s.lock().unwrap();
        assert_eq!(s.merge_disks(&lock, &weak, &serial).unwrap(), [old.id]);
        assert!(s.merge_disks(&lock, &serial, &serial).is_err());
        drop(lock);
        let metas = s.list().unwrap();
        assert!(metas.iter().all(|m| m.disk.as_deref() == Some(serial.as_str())));
        assert!(metas[0].incompatibility(&metas[1]).is_none(), "one lineage now");
        assert_eq!(read(&s, old.id), v0);
        assert_eq!(read(&s, new.id), v1);
        for f in snapshot_files(&s) {
            assert!(f.starts_with(&serial), "{f}");
        }
        let reg = s.registry().unwrap();
        assert_eq!(reg.disks.len(), 1);
        let d = &reg.disks[0];
        assert_eq!((d.label.as_deref(), d.sets.clone()), (Some("stick"), vec!["usb".to_string(), "old".to_string()]));
        assert_eq!(d.first_seen, "2026-10-21T10:00:00+02:00", "the older history's first sighting");
    }
}
