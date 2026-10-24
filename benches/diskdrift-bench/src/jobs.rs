//! Benchmark jobs. Each job runs in its own child process so that peak RSS
//! (`ru_maxrss`) is attributable to exactly one operation.

use crate::stores::{self, Backend, Store};
use crate::synth::{Gen, Params};
use diskdrift_core::delta::{DeltaBuilder, DeltaOp, OpDecoder, OpEncoder};
use diskdrift_core::diff::{self, DiffOptions};
use diskdrift_core::record::Record;
use diskdrift_core::top::{self, TopOptions};
use diskdrift_core::walk::{self, WalkOptions};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    Full,
    /// Delta against the last full checkpoint; a new checkpoint every `k`.
    Cumulative { k: u32 },
    /// Delta against the previous snapshot; a new checkpoint every `k`.
    Chain { k: u32 },
    /// Delta against the last checkpoint; a new checkpoint once the latest delta
    /// exceeds `pct`% of the checkpoint's size (the production policy).
    Adaptive { pct: u32 },
}

impl Strategy {
    pub fn label(self) -> String {
        match self {
            Strategy::Full => "full".into(),
            Strategy::Cumulative { k } => format!("checkpoint+cumulative-delta(k={k})"),
            Strategy::Chain { k } => format!("checkpoint+chained-delta(k={k})"),
            Strategy::Adaptive { pct } => format!("checkpoint+cumulative-delta(new checkpoint at {pct}%)"),
        }
    }

    /// Base snapshot for version `v`, or `None` for a full snapshot.
    pub fn base(self, v: u32) -> Option<u32> {
        match self {
            Strategy::Full | Strategy::Adaptive { .. } => None,
            Strategy::Cumulative { k } | Strategy::Chain { k } if v.is_multiple_of(k) => None,
            Strategy::Cumulative { k } => Some(v - v % k),
            Strategy::Chain { .. } => Some(v - 1),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "job", rename_all = "kebab-case")]
pub enum Job {
    Gen { params: Params, version: u32 },
    Import { backend: Backend, dir: PathBuf, params: Params, version: u32, id: u32 },
    Read { backend: Backend, dir: PathBuf, id: u32 },
    Top { backend: Backend, dir: PathBuf, id: u32 },
    NativeTop { backend: Backend, dir: PathBuf, id: u32 },
    Prefix { backend: Backend, dir: PathBuf, id: u32, prefix: Vec<Vec<u8>> },
    Delta { backend: Backend, dir: PathBuf, params: Params, version: u32, id: u32, base: u32 },
    Diff { backend: Backend, dir: PathBuf, a: u32, b: u32 },
    List { backend: Backend, dir: PathBuf },
    History { backend: Backend, dir: PathBuf, params: Params, versions: u32, strategy: Strategy },
    Scan { root: PathBuf, lister: String, threads: usize },
    ScanStore { root: PathBuf, lister: String, threads: usize, backend: Backend, dir: PathBuf },
    /// Import under RLIMIT_FSIZE so the write fails part-way (disk-full stand-in).
    LimitedImport { backend: Backend, dir: PathBuf, params: Params, id: u32, limit_bytes: u64 },
    Verify { backend: Backend, dir: PathBuf, id: u32, params: Params, version: u32 },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobResult {
    pub elapsed_ms: f64,
    pub items: u64,
    pub footprint: u64,
    pub ok: bool,
    pub error: Option<String>,
    #[serde(default)]
    pub extra: serde_json::Value,
}

/// Measured outcome of running a job in a child.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Measured {
    pub result: JobResult,
    pub max_rss: u64,
    pub wall_ms: f64,
}

/// Spawns `self child <job>` and waits with `wait4` for its resource usage.
pub fn run_child(job: &Job) -> io::Result<Measured> {
    run_child_with(job, None)
}

/// Like `run_child`, but sends SIGKILL after `kill_after_ms` (crash test).
pub fn run_child_with(job: &Job, kill_after_ms: Option<u64>) -> io::Result<Measured> {
    use std::io::Read;
    let exe = std::env::current_exe()?;
    let mut child = Command::new(exe)
        .arg("child")
        .arg(serde_json::to_string(job).unwrap())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let pid = child.id() as libc::pid_t;
    let mut out = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let started = Instant::now();
    if let Some(ms) = kill_after_ms {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let mut status = 0;
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::wait4(pid, &mut status, 0, &mut ru) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    let text = reader.join().unwrap_or_default();
    let result = serde_json::from_str::<JobResult>(text.trim()).unwrap_or_else(|_| JobResult {
        ok: false,
        error: Some(if kill_after_ms.is_some() { "killed".into() } else { format!("child failed (status {status}): {text}") }),
        ..Default::default()
    });
    // macOS reports ru_maxrss in bytes.
    Ok(Measured { result, max_rss: ru.ru_maxrss as u64, wall_ms })
}

fn timed<F: FnOnce() -> io::Result<(u64, serde_json::Value)>>(f: F) -> JobResult {
    let t = Instant::now();
    match f() {
        Ok((items, extra)) => JobResult { elapsed_ms: t.elapsed().as_secs_f64() * 1000.0, items, ok: true, extra, ..Default::default() },
        Err(e) => JobResult { elapsed_ms: t.elapsed().as_secs_f64() * 1000.0, ok: false, error: Some(e.to_string()), ..Default::default() },
    }
}

fn count(s: diskdrift_core::diff::RecordStream<'_>) -> io::Result<(u64, u64)> {
    let mut n = 0;
    let mut h = 0u64;
    for r in s {
        let r = r?;
        n += 1;
        h = h.wrapping_mul(31).wrapping_add(r.alloc ^ r.ino ^ r.name.len() as u64);
    }
    Ok((n, h))
}

/// Encodes delta ops for `version` against `base` into memory, then writes them.
fn write_delta(store: &mut dyn Store, params: Params, version: u32, id: u32, base: u32) -> io::Result<u64> {
    let mut buf = Vec::new();
    let mut n = 0u64;
    {
        let infos = store.list()?;
        let base_stream = stores::open(store, &infos, base)?;
        let new_stream = Box::new(Gen::new(params, version).map(Ok));
        let mut enc = OpEncoder::new();
        let mut builder = DeltaBuilder::new(|op: &DeltaOp| {
            enc.encode(op, &mut buf);
            n += 1;
            Ok(())
        });
        diff::merge(base_stream, new_stream, &mut builder)?;
    }
    let mut dec = OpDecoder::new();
    let mut s = buf.as_slice();
    let mut ops = std::iter::from_fn(|| if s.is_empty() { None } else { dec.decode(&mut s).ok() });
    store.write_delta(id, base, &mut ops)?;
    Ok(n)
}

pub fn execute(job: Job) -> JobResult {
    let mut footprint_dir: Option<PathBuf> = None;
    let mut r = match job {
        Job::Gen { params, version } => timed(|| Ok((Gen::new(params, version).count() as u64, serde_json::Value::Null))),
        Job::Import { backend, dir, params, version, id } => {
            footprint_dir = Some(dir.clone());
            timed(|| {
                let mut s = backend.open(&dir)?;
                let n = s.write_full(id, &mut Gen::new(params, version))?;
                Ok((n, serde_json::Value::Null))
            })
        }
        Job::Read { backend, dir, id } => timed(|| {
            let s = backend.open(&dir)?;
            let infos = s.list()?;
            let (n, h) = count(stores::open(s.as_ref(), &infos, id)?)?;
            Ok((n, serde_json::json!({ "hash": h })))
        }),
        Job::Top { backend, dir, id } => timed(|| {
            let s = backend.open(&dir)?;
            let infos = s.list()?;
            let r = top::top(stores::open(s.as_ref(), &infos, id)?, &TopOptions { limit: 100, ..Default::default() })?;
            Ok((r.entries, serde_json::json!({ "largestFile": r.files.first().map(|f| f.allocated_bytes) })))
        }),
        Job::NativeTop { backend, dir, id } => timed(|| {
            let s = backend.open(&dir)?;
            match s.native_top_files(id, 100) {
                Some(v) => {
                    let v = v?;
                    Ok((v.len() as u64, serde_json::json!({ "largestFile": v.first() })))
                }
                None => Err(io::Error::new(io::ErrorKind::Unsupported, "no native top")),
            }
        }),
        Job::Prefix { backend, dir, id, prefix } => timed(|| {
            let s = backend.open(&dir)?;
            let (n, h) = count(s.read_prefix(id, &prefix)?)?;
            Ok((n, serde_json::json!({ "hash": h })))
        }),
        Job::Delta { backend, dir, params, version, id, base } => {
            footprint_dir = Some(dir.clone());
            timed(|| {
                let mut s = backend.open(&dir)?;
                let n = write_delta(s.as_mut(), params, version, id, base)?;
                Ok((n, serde_json::Value::Null))
            })
        }
        Job::Diff { backend, dir, a, b } => timed(|| {
            let s = backend.open(&dir)?;
            let infos = s.list()?;
            let r = diff::diff(stores::open(s.as_ref(), &infos, a)?, stores::open(s.as_ref(), &infos, b)?, DiffOptions::default())?;
            let sm = &r.summary;
            let total = sm.added.count + sm.deleted.count + sm.grown.count + sm.shrunk.count + sm.modified_same_size + sm.unchanged;
            Ok((total, serde_json::to_value(sm).unwrap()))
        }),
        Job::List { backend, dir } => timed(|| {
            let s = backend.open(&dir)?;
            Ok((s.list()?.len() as u64, serde_json::Value::Null))
        }),
        Job::History { backend, dir, params, versions, strategy } => {
            footprint_dir = Some(dir.clone());
            timed(|| {
                let mut s = backend.open(&dir)?;
                let mut steps = Vec::new();
                let mut sizes = Vec::new();
                let (mut checkpoint, mut cp_bytes, mut last_delta) = (0u32, 0u64, 0u64);
                for v in 0..versions {
                    let t = Instant::now();
                    let before = stores::footprint(&dir);
                    let base = match strategy {
                        Strategy::Adaptive { pct } if v > 0 && last_delta * 100 <= cp_bytes * pct as u64 => Some(checkpoint),
                        Strategy::Adaptive { .. } => None,
                        other => other.base(v),
                    };
                    let n = match base {
                        None => s.write_full(v, &mut Gen::new(params, v))?,
                        Some(base) => write_delta(s.as_mut(), params, v, v, base)?,
                    };
                    let written = stores::footprint(&dir).saturating_sub(before);
                    if base.is_none() {
                        (checkpoint, cp_bytes, last_delta) = (v, written, 0);
                    } else {
                        last_delta = written;
                    }
                    steps.push(t.elapsed().as_secs_f64() * 1000.0);
                    sizes.push((n, stores::footprint(&dir)));
                }
                Ok((versions as u64, serde_json::json!({ "stepMs": steps, "items": sizes.iter().map(|x| x.0).collect::<Vec<_>>(), "footprintAfter": sizes.iter().map(|x| x.1).collect::<Vec<_>>() })))
            })
        }
        Job::Scan { root, lister, threads } => timed(|| {
            let l = lister_by_name(&lister);
            let layout = diskdrift_platform::scan_layout(&root);
            let opts = WalkOptions { threads, same_filesystem_devs: layout.same_filesystem_devs, alias_paths: layout.alias_paths, ..Default::default() };
            let cancel = AtomicBool::new(false);
            let mut sink = |_: &Record, _: &Path| Ok(());
            let rep = walk::walk(&root, l.as_ref(), &opts, &mut sink, &cancel, None).map_err(|e| io::Error::other(e.to_string()))?;
            Ok((rep.entries, serde_json::json!({ "inaccessibleDirs": rep.inaccessible_dirs, "allocated": rep.allocated_bytes })))
        }),
        Job::ScanStore { root, lister, threads, backend, dir } => {
            footprint_dir = Some(dir.clone());
            timed(|| {
                let l = lister_by_name(&lister);
                let layout = diskdrift_platform::scan_layout(&root);
                let opts = WalkOptions { threads, same_filesystem_devs: layout.same_filesystem_devs, alias_paths: layout.alias_paths, ..Default::default() };
                let mut s = backend.open(&dir)?;
                // Walker on its own thread, store writer on this one, bounded channel between.
                let (tx, rx) = crossbeam_channel::bounded::<Vec<Record>>(64);
                let root2 = root.clone();
                let walker = std::thread::spawn(move || {
                    let cancel = AtomicBool::new(false);
                    let mut batch = Vec::with_capacity(4096);
                    let mut sink = |r: &Record, _: &Path| {
                        batch.push(r.clone());
                        if batch.len() == 4096 {
                            tx.send(std::mem::replace(&mut batch, Vec::with_capacity(4096))).map_err(|_| io::Error::other("writer gone"))?;
                        }
                        Ok(())
                    };
                    let rep = walk::walk(&root2, l.as_ref(), &opts, &mut sink, &cancel, None);
                    if !batch.is_empty() {
                        let _ = tx.send(batch);
                    }
                    rep.map(|r| r.entries).map_err(|e| io::Error::other(e.to_string()))
                });
                let mut it = rx.into_iter().flatten();
                let n = s.write_full(0, &mut it)?;
                let walked = walker.join().map_err(|_| io::Error::other("walker panicked"))??;
                Ok((n.max(walked), serde_json::Value::Null))
            })
        }
        Job::LimitedImport { backend, dir, params, id, limit_bytes } => timed(|| {
            unsafe {
                libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
                let lim = libc::rlimit { rlim_cur: limit_bytes, rlim_max: limit_bytes };
                libc::setrlimit(libc::RLIMIT_FSIZE, &lim);
            }
            let mut s = backend.open(&dir)?;
            let n = s.write_full(id, &mut Gen::new(params, 1))?;
            Ok((n, serde_json::Value::Null))
        }),
        Job::Verify { backend, dir, id, params, version } => timed(|| {
            let expected = count(Box::new(Gen::new(params, version).map(Ok)))?;
            let s = backend.open(&dir)?;
            let infos = s.list()?;
            let got = count(stores::open(s.as_ref(), &infos, id)?)?;
            if got != expected {
                return Err(io::Error::other(format!("snapshot {id}: expected {expected:?}, got {got:?}")));
            }
            let ids: Vec<u32> = infos.iter().map(|i| i.id).collect();
            Ok((got.0, serde_json::json!({ "snapshots": ids })))
        }),
    };
    if let Some(d) = footprint_dir {
        r.footprint = stores::footprint(&d);
    }
    r
}

pub fn lister_by_name(name: &str) -> Box<dyn diskdrift_core::walk::DirLister> {
    use diskdrift_platform::ListerKind;
    diskdrift_platform::lister(match name {
        "readdir" => ListerKind::ReadDir,
        _ => ListerKind::Bulk,
    })
}
