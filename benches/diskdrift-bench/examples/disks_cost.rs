//! ADR 0002, phase 8: what disks cost. Each step runs in its own process so
//! peak RSS belongs to that step alone.
//!
//!   disks_cost build <entries> <versions> <dir>   a version 2 store with <versions> scans (~1 % change each)
//!   disks_cost migrate <dir>                      take the writer lock: the migration to version 3, timed
//!   disks_cost verify <dir>                       record counts of the oldest and newest snapshot, checksums
//!   disks_cost adapter <entries> <dir>            diff and rebase with and without the root-rename adapter
//!   disks_cost diff <dir>                         the newest pair with the adapter, alone in its process (RSS)
//!   disks_cost search <dir> <query>               name search without an index: the newest snapshot, then every one
use diskdrift_bench::synth::{Gen, Params};
use diskdrift_core::diff::{self, DiffOptions, with_root_name};
use diskdrift_core::walk::WalkReport;
use diskdrift_storage::{ScanOptions, SnapshotMeta, Status, Store, VolumeIdentity, meta};
use std::path::Path;
use std::time::Instant;

// Same allocator as the shipped binary.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn template(name: String, root: &str, disk: Option<&str>, v: u32) -> SnapshotMeta {
    SnapshotMeta {
        format_version: meta::META_FORMAT_VERSION,
        id: 0,
        name,
        root: root.into(),
        disk: disk.map(String::from),
        root_in_volume: disk.map(|_| "/".into()),
        status: Status::InProgress,
        // One scan a day, so every snapshot gets its own time stem.
        started_at: format!("2026-{:02}-{:02}T10:00:00+02:00", 1 + v / 28, 1 + v % 28),
        finished_at: None,
        volume: VolumeIdentity { uuid: Some("bench".into()), fs_type: "apfs".into(), mount_point: root.into(), ..Default::default() },
        options: ScanOptions { stay_on_filesystem: true, ..Default::default() },
        space_before: None,
        space_after: None,
        storage: Default::default(),
        counts: None,
        error: None,
        diskdrift_version: "bench".into(),
    }
}

#[cfg(unix)]
fn rss_mb() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    ru.ru_maxrss as f64 / 1e6
}

#[cfg(not(unix))]
fn rss_mb() -> f64 {
    0.0
}

fn ms<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed().as_secs_f64() * 1000.0)
}

fn write(store: &Store, params: Params, v: u32, m: SnapshotMeta) -> f64 {
    // A scan's root record is named after the root it scanned (the generator always says /synthetic).
    let root = m.root.as_bytes().to_vec();
    ms(|| {
        let lock = store.lock().unwrap();
        let mut w = store.begin(lock, m, None).unwrap();
        for mut r in Gen::new(params, v) {
            if r.depth == 0 {
                r.name = diskdrift_core::native::NativeName::unix(root.clone());
            }
            w.push(&r).unwrap();
        }
        w.commit_and_rebase(WalkReport::default(), None, String::new()).unwrap().1.unwrap()
    })
    .1
}

/// Rewrites a version 3 store as version 2 wrote it (as the storage tests do).
fn downgrade(store: &Store) {
    let dir = store.path().join("snapshots");
    let metas = store.list().unwrap();
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
    let _ = std::fs::remove_file(store.path().join("disks.json"));
    std::fs::write(store.path().join("store.json"), br#"{"format":"diskdrift-snapshots","version":2}"#).unwrap();
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let out = match a[1].as_str() {
        "build" => {
            let (n, versions, dir) = (a[2].parse().unwrap(), a[3].parse::<u32>().unwrap(), Path::new(&a[4]));
            let _ = std::fs::remove_dir_all(dir);
            let store = Store::create(dir).unwrap();
            let params = Params { entries: n, seed: 13, change: 0.01, horizon: versions };
            let (_, total) = ms(|| {
                for v in 0..versions {
                    write(&store, params, v, template(format!("v{v}"), "/synthetic", None, v));
                }
            });
            downgrade(&store);
            serde_json::json!({ "step": "build", "entries": n, "versions": versions, "ms": total, "storeBytes": store.size_on_disk() })
        }
        "migrate" => {
            let store = Store::open(Path::new(&a[2])).unwrap();
            let (_, t) = ms(|| drop(store.lock().unwrap()));
            let marker = std::fs::read_to_string(store.path().join("store.json")).unwrap();
            serde_json::json!({ "step": "migrate", "ms": t, "peakRssMb": rss_mb(), "marker": marker, "snapshots": store.list().unwrap().len() })
        }
        "verify" => {
            let store = Store::open(Path::new(&a[2])).unwrap();
            let all = store.list().unwrap();
            let (first, last) = (all.first().unwrap(), all.last().unwrap());
            let (n_first, t_first) = ms(|| store.verify(first.id).unwrap());
            let (n_last, t_last) = ms(|| store.verify(last.id).unwrap());
            let named = all.iter().all(|m| m.disk.is_some() && m.storage.stem.as_deref().unwrap().starts_with(m.disk.as_deref().unwrap()));
            serde_json::json!({ "step": "verify", "oldestRecords": n_first, "oldestMs": t_first, "newestRecords": n_last, "newestMs": t_last, "allNamedAfterTheirDisk": named, "peakRssMb": rss_mb() })
        }
        "adapter" => {
            let (n, dir) = (a[2].parse().unwrap(), Path::new(&a[3]));
            let params = Params { entries: n, seed: 17, change: 0.01, horizon: 100 };
            // Rebase: the second scan of one disk at the same mount point, then at another.
            let mut rebase = Vec::new();
            for (case, second_root) in [("sameRoot", "/Volumes/X"), ("otherRoot", "/Volumes/X 1")] {
                let d = dir.join(case);
                let _ = std::fs::remove_dir_all(&d);
                let store = Store::create(&d).unwrap();
                write(&store, params, 0, template("a".into(), "/Volumes/X", Some("dbench"), 0));
                let t = write(&store, params, 1, template("b".into(), second_root, Some("dbench"), 1));
                let delta_items = store.list().unwrap()[0].storage.items;
                rebase.push(serde_json::json!({ "case": case, "commitAndRebaseMs": t, "deltaItems": delta_items }));
                if case == "otherRoot" {
                    // Diff of the adjacent pair: as stored, and with the adapter on both sides.
                    let all = store.list().unwrap();
                    let (old, new) = (all[0].id, all[1].id);
                    let (r1, plain) = ms(|| diff::diff(store.records(old).unwrap(), store.records(new).unwrap(), DiffOptions::default()).unwrap());
                    let (r2, adapted) = ms(|| {
                        let a = store.records_as(old, "/Volumes/X 1").unwrap();
                        let b = with_root_name(store.records(new).unwrap(), b"/Volumes/X 1".to_vec());
                        diff::diff(a, b, DiffOptions::default()).unwrap()
                    });
                    rebase.push(serde_json::json!({
                        "case": "diffAdjacent", "withoutAdapterMs": plain, "withAdapterMs": adapted,
                        "withoutAdapterChanged": r1.summary.added.count + r1.summary.deleted.count,
                        "withAdapterChanged": r2.summary.added.count + r2.summary.deleted.count,
                    }));
                }
                let _ = std::fs::remove_dir_all(&d);
            }
            serde_json::json!({ "step": "adapter", "entries": n, "results": rebase, "peakRssMb": rss_mb() })
        }
        "diff" => {
            let store = Store::open(Path::new(&a[2])).unwrap();
            let all = store.list().unwrap();
            let (old, new) = (&all[all.len() - 2], all.last().unwrap());
            let (r, t) = ms(|| diff::diff(store.records_as(old.id, &new.root).unwrap(), store.records(new.id).unwrap(), DiffOptions::default()).unwrap());
            serde_json::json!({ "step": "diff", "ms": t, "changed": r.summary.added.count + r.summary.deleted.count, "peakRssMb": rss_mb() })
        }
        "search" => {
            // Case-insensitive substring of the entry's own name: what a search without an
            // index costs today (ROADMAP 3.2, search-index.md E1/E7 baseline).
            let store = Store::open(Path::new(&a[2])).unwrap();
            let q = a[3].to_lowercase();
            let all: Vec<_> = store.list().unwrap().into_iter().filter(|m| m.status.is_usable()).collect();
            let hits = |id: u64| {
                let mut n = 0u64;
                let mut entries = 0u64;
                for r in store.records(id).unwrap() {
                    let r = r.unwrap();
                    entries += 1;
                    if String::from_utf8_lossy(r.name.as_unix_bytes().unwrap_or_default()).to_lowercase().contains(&q) {
                        n += 1;
                    }
                }
                (n, entries)
            };
            let newest = all.last().unwrap();
            let ((h, e), t_newest) = ms(|| hits(newest.id));
            let (per, t_all) = ms(|| all.iter().map(|m| hits(m.id)).collect::<Vec<_>>());
            serde_json::json!({
                "step": "search", "query": q, "newestEntries": e, "newestHits": h, "newestMs": t_newest,
                "snapshots": all.len(), "allEntries": per.iter().map(|p| p.1).sum::<u64>(), "allMs": t_all, "peakRssMb": rss_mb(),
            })
        }
        other => panic!("unknown step {other}"),
    };
    println!("{out}");
}
