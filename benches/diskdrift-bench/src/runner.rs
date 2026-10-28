//! Benchmark suites: which jobs run, in what order, how often.

use crate::jobs::{self, Job, Measured, Strategy};
use crate::stores::{self, Backend};
use crate::synth::{Gen, Params};
use diskdrift_core::paths::PathStack;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Row {
    pub group: String,
    pub backend: String,
    pub n: u64,
    pub param: String,
    pub trial: usize,
    pub ms: f64,
    pub wall_ms: f64,
    pub rss: u64,
    pub items: u64,
    pub footprint: u64,
    pub ok: bool,
    pub error: Option<String>,
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Machine {
    pub model: String,
    pub cpu: String,
    pub cpus: usize,
    pub memory_bytes: u64,
    pub os: String,
    pub filesystem: String,
    pub device: String,
    pub rustc: String,
    pub cache_note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Results {
    pub suite: String,
    pub started: String,
    pub finished: String,
    pub machine: Machine,
    pub rows: Vec<Row>,
}

pub struct Suite {
    pub name: String,
    pub sizes: Vec<u64>,
    pub trials: Box<dyn Fn(u64) -> usize>,
    pub backends: Vec<Backend>,
    pub change_n: u64,
    pub change_rates: Vec<f64>,
    pub history_n: u64,
    pub history_versions: u32,
    pub history_strategies: Vec<Strategy>,
    /// Restrict history runs to these backends (empty: all).
    pub history_backends: Vec<Backend>,
    pub robustness_n: u64,
    pub scan_roots: Vec<PathBuf>,
    pub scan_threads: Vec<usize>,
    pub scan_trials: usize,
}

impl Suite {
    pub fn quick() -> Suite {
        Suite {
            name: "quick".into(),
            sizes: vec![100_000],
            trials: Box::new(|_| 2),
            backends: Backend::ALL.to_vec(),
            change_n: 100_000,
            change_rates: vec![0.01],
            history_n: 100_000,
            history_versions: 12,
            history_strategies: vec![Strategy::Full, Strategy::Cumulative { k: 10 }, Strategy::Chain { k: 10 }],
            history_backends: Vec::new(),
            robustness_n: 200_000,
            scan_roots: vec![PathBuf::from("/System/Library/Frameworks")],
            scan_threads: vec![1, 8],
            scan_trials: 2,
        }
    }

    pub fn full() -> Suite {
        let home = std::env::home_dir().unwrap_or_else(|| PathBuf::from("/Users"));
        Suite {
            name: "full".into(),
            sizes: vec![100_000, 1_000_000, 5_000_000, 10_000_000],
            trials: Box::new(|n| if n <= 1_000_000 { 5 } else { 3 }),
            backends: Backend::ALL.to_vec(),
            change_n: 1_000_000,
            change_rates: vec![0.0, 0.001, 0.01, 0.1, 0.5],
            history_n: 1_000_000,
            history_versions: 100,
            history_strategies: vec![
                Strategy::Full,
                Strategy::Cumulative { k: 10 },
                Strategy::Chain { k: 10 },
                Strategy::Cumulative { k: 30 },
                Strategy::Adaptive { pct: 25 },
            ],
            history_backends: Vec::new(),
            robustness_n: 1_000_000,
            scan_roots: vec![PathBuf::from("/System/Library"), PathBuf::from("/Applications"), home.join("Library")],
            scan_threads: vec![1, 2, 4, 6, 8, 12, 16],
            scan_trials: 3,
        }
    }
}

pub struct Runner {
    pub work: PathBuf,
    pub rows: Vec<Row>,
    pub log: bool,
}

impl Runner {
    fn record(&mut self, group: &str, backend: &str, n: u64, param: &str, trial: usize, m: Measured) -> Measured {
        let r = &m.result;
        if self.log {
            eprintln!(
                "  {group:<14} {backend:<15} n={n:<9} {param:<34} #{trial} {:>10.1} ms  rss {:>7.1} MB  fp {:>9.1} MB {}",
                r.elapsed_ms,
                m.max_rss as f64 / 1e6,
                r.footprint as f64 / 1e6,
                if r.ok { "" } else { r.error.as_deref().unwrap_or("FAILED") }
            );
        }
        self.rows.push(Row {
            group: group.into(),
            backend: backend.into(),
            n,
            param: param.into(),
            trial,
            ms: r.elapsed_ms,
            wall_ms: m.wall_ms,
            rss: m.max_rss,
            items: r.items,
            footprint: r.footprint,
            ok: r.ok,
            error: r.error.clone(),
            extra: r.extra.clone(),
        });
        m
    }

    fn run(&mut self, group: &str, backend: &str, n: u64, param: &str, trial: usize, job: Job) -> Measured {
        let m = jobs::run_child(&job).unwrap_or_else(|e| Measured {
            result: jobs::JobResult { ok: false, error: Some(e.to_string()), ..Default::default() },
            max_rss: 0,
            wall_ms: 0.0,
        });
        self.record(group, backend, n, param, trial, m)
    }

    fn fresh(&self, name: &str) -> PathBuf {
        let d = self.work.join(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub fn run_suite(&mut self, s: &Suite, only: &[String]) {
        let want = |g: &str| only.is_empty() || only.iter().any(|o| o == g);
        if want("scan") {
            self.scanner(s);
        }
        if want("ops") {
            self.ops(s);
        }
        if want("change") {
            self.change(s);
        }
        if want("history") {
            self.history(s);
        }
        if want("robustness") {
            self.robustness(s);
        }
        let _ = std::fs::remove_dir_all(&self.work);
    }

    fn ops(&mut self, s: &Suite) {
        for &n in &s.sizes {
            let params = Params { entries: n, seed: 7, change: 0.01, horizon: 100 };
            eprintln!("== ops n={n}");
            let trials = (s.trials)(n);
            for t in 0..trials {
                self.run("gen", "-", n, "generator only", t, Job::Gen { params, version: 0 });
            }
            let prefix = prefix_target(params);
            for &b in &s.backends {
                for t in 0..trials {
                    let dir = self.fresh(&format!("{}-{n}-{t}", b.name()));
                    let m = self.run("import", b.name(), n, "", t, Job::Import { backend: b, dir: dir.clone(), params, version: 0, id: 0 });
                    if !m.result.ok {
                        continue;
                    }
                    self.run("read", b.name(), n, "", t, Job::Read { backend: b, dir: dir.clone(), id: 0 });
                    self.run("top", b.name(), n, "stream top-100 files+dirs", t, Job::Top { backend: b, dir: dir.clone(), id: 0 });
                    if b == Backend::SqliteIndexed {
                        self.run("top-native", b.name(), n, "SQL ORDER BY alloc LIMIT 100", t, Job::NativeTop { backend: b, dir: dir.clone(), id: 0 });
                    }
                    self.run("prefix", b.name(), n, &format!("~{} entries", prefix.1), t, Job::Prefix { backend: b, dir: dir.clone(), id: 0, prefix: prefix.0.clone() });
                    let _ = std::fs::remove_dir_all(&dir);
                }
            }
        }
    }

    fn change(&mut self, s: &Suite) {
        let n = s.change_n;
        for &c in &s.change_rates {
            eprintln!("== change n={n} rate={c}");
            let params = Params { entries: n, seed: 11, change: c, horizon: 1 };
            for &b in &s.backends {
                for t in 0..(s.trials)(n).min(3) {
                    let dir = self.fresh(&format!("chg-{}-{t}", b.name()));
                    let base = self.run("change-import", b.name(), n, &format!("{c}"), t, Job::Import { backend: b, dir: dir.clone(), params, version: 0, id: 0 });
                    if !base.result.ok {
                        continue;
                    }
                    let before = base.result.footprint;
                    let d = self.run("delta-write", b.name(), n, &format!("{c}"), t, Job::Delta { backend: b, dir: dir.clone(), params, version: 1, id: 1, base: 0 });
                    let after_delta = d.result.footprint;
                    if let Some(row) = self.rows.last_mut() {
                        row.extra = serde_json::json!({ "addedBytes": after_delta.saturating_sub(before), "ops": d.result.items });
                    }
                    let f = self.run("full-write", b.name(), n, &format!("{c}"), t, Job::Import { backend: b, dir: dir.clone(), params, version: 1, id: 2 });
                    if let Some(row) = self.rows.last_mut() {
                        row.extra = serde_json::json!({ "addedBytes": f.result.footprint.saturating_sub(after_delta) });
                    }
                    self.run("diff-via-delta", b.name(), n, &format!("{c}"), t, Job::Diff { backend: b, dir: dir.clone(), a: 0, b: 1 });
                    self.run("diff-full-full", b.name(), n, &format!("{c}"), t, Job::Diff { backend: b, dir: dir.clone(), a: 0, b: 2 });
                    let _ = std::fs::remove_dir_all(&dir);
                }
            }
        }
    }

    fn history(&mut self, s: &Suite) {
        let n = s.history_n;
        let params = Params { entries: n, seed: 13, change: 0.01, horizon: s.history_versions };
        let last = s.history_versions - 1;
        for &strategy in &s.history_strategies {
            for &b in &s.backends {
                if !s.history_backends.is_empty() && !s.history_backends.contains(&b) {
                    continue;
                }
                // A full history of row-per-entry engines only multiplies the one-snapshot
                // footprint (measured in `ops`); skip it to save disk and time.
                if strategy == Strategy::Full && !matches!(b, Backend::Binary | Backend::BinaryColumnar | Backend::SqliteBlocks) {
                    continue;
                }
                eprintln!("== history {} {}", strategy.label(), b.name());
                let dir = self.fresh(&format!("hist-{}", b.name()));
                let label = strategy.label();
                let h = self.run("history-write", b.name(), n, &label, 0, Job::History { backend: b, dir: dir.clone(), params, versions: s.history_versions, strategy });
                if !h.result.ok {
                    continue;
                }
                for t in 0..3 {
                    self.run("history-diff-distant", b.name(), n, &label, t, Job::Diff { backend: b, dir: dir.clone(), a: 0, b: last });
                    self.run("history-diff-adjacent", b.name(), n, &label, t, Job::Diff { backend: b, dir: dir.clone(), a: last - 1, b: last });
                    self.run("history-read-latest", b.name(), n, &label, t, Job::Read { backend: b, dir: dir.clone(), id: last });
                    self.run("history-list", b.name(), n, &label, t, Job::List { backend: b, dir: dir.clone() });
                }
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }

    fn robustness(&mut self, s: &Suite) {
        #[cfg(not(unix))]
        {
            let _ = s;
            eprintln!("== robustness unsupported: crash and disk-full jobs require Unix process/resource limits");
        }

        #[cfg(unix)]
        {
        let n = s.robustness_n;
        let params = Params { entries: n, seed: 17, change: 0.01, horizon: 100 };
        for &b in &s.backends {
            eprintln!("== robustness {}", b.name());
            // Crash during ingest.
            let dir = self.fresh(&format!("crash-{}", b.name()));
            let first = self.run("crash-setup", b.name(), n, "import snapshot 0", 0, Job::Import { backend: b, dir: dir.clone(), params, version: 0, id: 0 });
            let kill_at = (first.result.elapsed_ms * 0.5).max(50.0) as u64;
            let m = match jobs::run_child_with(&Job::Import { backend: b, dir: dir.clone(), params, version: 1, id: 1 }, Some(kill_at)) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("  crash-kill unsupported: {e}");
                    let _ = std::fs::remove_dir_all(&dir);
                    continue;
                }
            };
            self.record("crash-kill", b.name(), n, &format!("SIGKILL after {kill_at} ms"), 0, m);
            let v = self.run("crash-verify", b.name(), n, "snapshot 0 intact after kill", 0, Job::Verify { backend: b, dir: dir.clone(), id: 0, params, version: 0 });
            let listed_partial = v.result.extra["snapshots"].as_array().is_some_and(|a| a.iter().any(|x| x == 1));
            if listed_partial {
                // A listed snapshot 1 must then be complete and correct.
                self.run("crash-verify-partial", b.name(), n, "snapshot 1 listed: must be complete", 0, Job::Verify { backend: b, dir: dir.clone(), id: 1, params, version: 1 });
            }
            self.run("crash-reuse", b.name(), n, "write snapshot 2 after crash", 0, Job::Import { backend: b, dir: dir.clone(), params, version: 1, id: 2 });
            self.run("crash-verify-2", b.name(), n, "snapshot 2 correct", 0, Job::Verify { backend: b, dir: dir.clone(), id: 2, params, version: 1 });
            let _ = std::fs::remove_dir_all(&dir);

            // Write failure (file size limit as a disk-full stand-in).
            let dir = self.fresh(&format!("full-{}", b.name()));
            self.run("diskfull-setup", b.name(), n, "import snapshot 0", 0, Job::Import { backend: b, dir: dir.clone(), params, version: 0, id: 0 });
            // The write must fail part-way: below one snapshot's size for a file-per-
            // snapshot store, a quarter of a snapshot's growth for single-file stores.
            let largest = largest_file(&dir);
            let limit = if matches!(b, Backend::Binary | Backend::BinaryColumnar) { largest / 2 } else { largest + stores::footprint(&dir) / 4 };
            self.run("diskfull-write", b.name(), n, &format!("RLIMIT_FSIZE={} MB", limit / 1_000_000), 0, Job::LimitedImport { backend: b, dir: dir.clone(), params, id: 1, limit_bytes: limit });
            self.run("diskfull-verify", b.name(), n, "snapshot 0 intact after failed write", 0, Job::Verify { backend: b, dir: dir.clone(), id: 0, params, version: 0 });
            self.run("diskfull-reuse", b.name(), n, "write snapshot 2 afterwards", 0, Job::Import { backend: b, dir: dir.clone(), params, version: 1, id: 2 });
            let _ = std::fs::remove_dir_all(&dir);
        }
        }
    }

    fn scanner(&mut self, s: &Suite) {
        for root in &s.scan_roots {
            if !root.exists() {
                continue;
            }
            let r = root.display().to_string();
            eprintln!("== scan {r}");
            // First touch in this session; cache state is not controllable without root.
            self.run("scan-first", "getattrlistbulk", 0, &r, 0, Job::Scan { root: root.clone(), lister: "bulk".into(), threads: 8 });
            for lister in ["bulk", "readdir"] {
                for &threads in &s.scan_threads {
                    for t in 0..s.scan_trials {
                        let name = if lister == "bulk" { "getattrlistbulk" } else { "readdir+lstat" };
                        self.run("scan", name, 0, &format!("{r} threads={threads}"), t, Job::Scan { root: root.clone(), lister: lister.into(), threads });
                    }
                }
            }
            for &b in &s.backends {
                if b == Backend::SqliteIndexed {
                    continue;
                }
                for t in 0..s.scan_trials.min(2) {
                    let dir = self.fresh(&format!("e2e-{}", b.name()));
                    self.run("scan-store", b.name(), 0, &format!("{r} threads=8"), t, Job::ScanStore { root: root.clone(), lister: "bulk".into(), threads: 8, backend: b, dir: dir.clone() });
                    let _ = std::fs::remove_dir_all(&dir);
                }
            }
        }
    }
}

#[cfg(unix)]
fn largest_file(dir: &Path) -> u64 {
    let mut max = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if let Ok(md) = e.metadata() {
                if md.is_dir() {
                    stack.push(e.path());
                } else {
                    max = max.max(md.len());
                }
            }
        }
    }
    max
}

/// A directory (depth >= 2) whose subtree holds about 1% of all entries.
pub fn prefix_target(params: Params) -> (Vec<Vec<u8>>, u64) {
    let target = (params.entries / 100).max(10);
    let mut stack = PathStack::new();
    let mut open: Vec<(Vec<Vec<u8>>, u64)> = Vec::new();
    let mut best: Option<(Vec<Vec<u8>>, u64)> = None;
    let consider = |p: Vec<Vec<u8>>, c: u64, best: &mut Option<(Vec<Vec<u8>>, u64)>| {
        if p.len() >= 3 && best.as_ref().is_none_or(|(_, bc)| c.abs_diff(target) < bc.abs_diff(target)) {
            *best = Some((p, c));
        }
    };
    for r in Gen::new(params, 0) {
        stack.set(r.depth, r.name.as_unix_bytes().unwrap_or_default()).unwrap();
        while open.last().is_some_and(|(p, _)| p.len() > r.depth as usize) {
            let (p, c) = open.pop().unwrap();
            if let Some(parent) = open.last_mut() {
                parent.1 += c;
            }
            consider(p, c, &mut best);
        }
        if let Some(parent) = open.last_mut() {
            parent.1 += u64::from(!r.is_dir());
        }
        if r.is_dir() {
            open.push((stack.components().to_vec(), 1));
        }
    }
    while let Some((p, c)) = open.pop() {
        if let Some(parent) = open.last_mut() {
            parent.1 += c;
        }
        consider(p, c, &mut best);
    }
    best.unwrap_or((vec![b"/synthetic".to_vec()], params.entries))
}

fn sysctl(name: &str) -> String {
    std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", name])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

pub fn machine(work: &Path) -> Machine {
    let fs = diskdrift_platform::statfs_path(work.parent().unwrap_or(work)).map(|m| m.fs_type).unwrap_or_default();
    let info = |target: &str| {
        std::process::Command::new("/usr/sbin/diskutil")
            .args(["info", target])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default()
    };
    let get = |t: &str, k: &str| {
        t.lines()
            .find(|l| l.trim_start().starts_with(k))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
            .unwrap_or_default()
    };
    let vol = info("/System/Volumes/Data");
    let store = get(&vol, "APFS Physical Store");
    let phys = info(if store.is_empty() { "disk0" } else { &store });
    let device = format!(
        "{} (solid state: {}, protocol: {})",
        get(&phys, "Device / Media Name"),
        get(&phys, "Solid State"),
        get(&phys, "Protocol")
    );
    let rustc = std::process::Command::new("rustc").arg("--version").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    Machine {
        model: sysctl("hw.model"),
        cpu: sysctl("machdep.cpu.brand_string"),
        cpus: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        memory_bytes: sysctl("hw.memsize").parse().unwrap_or(0),
        os: format!("macOS {} ({})", sysctl("kern.osproductversion"), sysctl("kern.osversion")),
        filesystem: fs,
        device,
        rustc,
        cache_note: "Filesystem cache state cannot be controlled without root (`purge`). Storage reads directly follow their writes (warm). \
                     Scanner 'scan-first' is the first traversal of that tree in this benchmark session; 'scan' rows are repeated warm runs."
            .into(),
    }
}

pub use stores::footprint;
