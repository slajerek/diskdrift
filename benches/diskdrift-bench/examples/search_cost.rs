//! Roadmap step 5: what `diskdrift find` costs without an index, through the
//! shipped search service (newest scans streamed, history read from the deltas).
//! Runs against a store built by `disks_cost build` (migrated or not).
//!
//!   search_cost <store> <pattern> [history]
//!
//! Prints one JSON line: the current search, then (with `history`) the history
//! search over every pair of adjacent scans.
use diskdrift_app::config::{Overrides, resolve_with};
use diskdrift_app::search::{self, Query};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

// Same allocator as the shipped binary.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn rss_mb() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    ru.ru_maxrss as f64 / 1e6
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let store = std::fs::canonicalize(Path::new(&a[1])).unwrap();
    let pattern = &a[2];
    let with_history = a.get(3).is_some_and(|x| x == "history");
    // No config file: the home is a directory without one.
    let home = store.parent().unwrap();
    let settings = resolve_with(&Overrides { db: Some(store.clone()), ..Default::default() }, home, home).unwrap();
    assert!(settings.storage_path.value.join("store.json").exists(), "no store at {}", store.display());
    let q = Query::new(pattern);
    let cancel = AtomicBool::new(false);
    let t = Instant::now();
    // No mounted volumes: Disk Arbitration is not part of the search cost.
    let cur = search::current_cancellable(&settings, &q, Some(&[]), &cancel).unwrap();
    let current_ms = t.elapsed().as_secs_f64() * 1000.0;
    let mut out = serde_json::json!({
        "step": "find", "pattern": pattern, "scansSearched": cur.searched.len(),
        "entries": cur.searched.iter().map(|s| s.entries).sum::<u64>(), "matches": cur.total, "currentMs": current_ms,
    });
    if with_history {
        let t = Instant::now();
        let h = search::history(&settings, &q, &cur, true, &cancel, &|_, _| {}).unwrap();
        out["historyMs"] = (t.elapsed().as_secs_f64() * 1000.0).into();
        out["pairs"] = h.pairs.into();
        out["gone"] = h.gone_total.into();
        out["currentWithHistory"] = h.current.len().into();
    }
    out["peakRssMb"] = rss_mb().into();
    println!("{out}");
}
