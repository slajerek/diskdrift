//! History benchmark of the production store (diskdrift-storage) with its real
//! policy: newest full, older ones rewritten as reverse deltas, keyframes after
//! MAX_CHAIN. Same synthetic workload as the M1 `history` group.
//!
//!   production_history <entries> <versions> <out.json>
use diskdrift_bench::synth::{Gen, Params};
use diskdrift_core::diff::{self, DiffOptions};
use diskdrift_core::walk::WalkReport;
use diskdrift_storage::{Rebase, ScanOptions, SnapshotMeta, Status, Store, VolumeIdentity, meta};
use std::time::Instant;

// Same allocator as the shipped binary.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn template(name: String) -> SnapshotMeta {
    SnapshotMeta {
        format_version: meta::META_FORMAT_VERSION,
        id: 0,
        name,
        root: "/synthetic".into(),
        disk: None,
        root_in_volume: None,
        status: Status::InProgress,
        started_at: String::new(),
        finished_at: None,
        volume: VolumeIdentity { uuid: Some("bench".into()), fs_type: "apfs".into(), ..Default::default() },
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
fn rss() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    ru.ru_maxrss as f64 / 1e6
}

#[cfg(not(unix))]
fn rss() -> f64 {
    0.0
}

fn time<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed().as_secs_f64() * 1000.0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: u64 = args[1].parse().unwrap();
    let versions: u32 = args[2].parse().unwrap();
    let out = &args[3];
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/bench-work/production-history");
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::create(&dir).unwrap();
    let params = Params { entries: n, seed: 13, change: 0.01, horizon: versions };
    let (mut steps, mut sizes, mut kinds) = (Vec::new(), Vec::new(), Vec::new());
    let mut one_full = 0;
    for v in 0..versions {
        let (rb, ms) = time(|| {
            let lock = store.lock().unwrap();
            let mut w = store.begin(lock, template(format!("v{v}")), None).unwrap();
            for r in Gen::new(params, v) {
                w.push(&r).unwrap();
            }
            w.commit_and_rebase(WalkReport::default(), None, String::new()).unwrap().1.unwrap()
        });
        let size = store.size_on_disk();
        if v == 0 {
            one_full = size;
        }
        kinds.push(match rb {
            Rebase::Converted { .. } => "delta",
            Rebase::KeptFull { .. } => "keyframe",
            Rebase::Nothing => "-",
        });
        steps.push(ms);
        sizes.push(size);
        eprintln!("v{v:3} {ms:7.0} ms  store {:7.1} MB  rss {:5.0} MB  {:?}", size as f64 / 1e6, rss(), rb);
    }
    eprintln!("RSS after writes: {:.0} MB", rss());
    let all = store.list().unwrap();
    let (first, last) = (all.first().unwrap().id, all.last().unwrap().id);
    let count = |id: u64| store.records(id).unwrap().count();
    let (_, read_latest) = time(|| count(last));
    let (_, read_prev) = time(|| count(all[all.len() - 2].id));
    eprintln!("RSS after reading latest/previous: {:.0} MB", rss());
    let (_, read_oldest) = time(|| count(first));
    eprintln!("RSS after reading oldest: {:.0} MB", rss());
    let d = |a: u64, b: u64| diff::diff(store.records(a).unwrap(), store.records(b).unwrap(), DiffOptions::default()).unwrap();
    let (_, diff_adjacent) = time(|| d(all[all.len() - 2].id, last));
    let (_, diff_distant) = time(|| d(first, last));
    let (_, list_ms) = time(|| store.list().unwrap().len());
    let ratio = |i: usize| sizes.get(i).map(|s| *s as f64 / one_full as f64);
    let result = serde_json::json!({
        "entries": n, "versions": versions, "oneFullBytes": one_full,
        "ratioAfter10": ratio(9), "ratioAfter30": ratio(29), "ratioAfter100": ratio(99),
        "finalBytes": sizes.last(), "stepMs": steps, "storeBytesAfter": sizes, "kinds": kinds,
        "readLatestMs": read_latest, "readPreviousMs": read_prev, "readOldestMs": read_oldest,
        "diffAdjacentMs": diff_adjacent, "diffDistantMs": diff_distant, "listMs": list_ms, "peakRssMb": rss(),
    });
    std::fs::write(out, serde_json::to_string_pretty(&result).unwrap()).unwrap();
    let mut sorted = steps.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median_step = sorted[sorted.len() / 2];
    println!("{}", serde_json::json!({
        "ratioAfter10": ratio(9), "ratioAfter30": ratio(29), "ratioAfter100": ratio(99),
        "medianStepMs": median_step,
        "readLatestMs": read_latest, "readPreviousMs": read_prev, "readOldestMs": read_oldest,
        "diffAdjacentMs": diff_adjacent, "diffDistantMs": diff_distant, "peakRssMb": rss()
    }));
    let _ = std::fs::remove_dir_all(&dir);
}
