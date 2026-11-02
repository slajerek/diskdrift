//! Benchmark jobs. Each job runs in its own child process so that peak RSS
//! (`ru_maxrss`) is attributable to exactly one operation.

use crate::stores::{self, Backend, Store};
use crate::synth::{Gen, Params};
use diskdrift_core::delta::{DeltaBuilder, DeltaOp, OpDecoder, OpEncoder};
use diskdrift_core::diff::{self, DiffOptions};
use diskdrift_core::native::NativeName;
use diskdrift_core::record::Record;
use diskdrift_core::top::{self, TopOptions};
use diskdrift_core::walk::{self, WalkOptions};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(not(unix))]
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    Full,
    /// Delta against the last full checkpoint; a new checkpoint every `k`.
    Cumulative {
        k: u32,
    },
    /// Delta against the previous snapshot; a new checkpoint every `k`.
    Chain {
        k: u32,
    },
    /// Delta against the last checkpoint; a new checkpoint once the latest delta
    /// exceeds `pct`% of the checkpoint's size (the production policy).
    Adaptive {
        pct: u32,
    },
}

impl Strategy {
    pub fn label(self) -> String {
        match self {
            Strategy::Full => "full".into(),
            Strategy::Cumulative { k } => format!("checkpoint+cumulative-delta(k={k})"),
            Strategy::Chain { k } => format!("checkpoint+chained-delta(k={k})"),
            Strategy::Adaptive { pct } => {
                format!("checkpoint+cumulative-delta(new checkpoint at {pct}%)")
            }
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProductionFormat {
    Dds1,
    Dds2,
}

impl ProductionFormat {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Dds1 => "dds1",
            Self::Dds2 => "dds2",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "job", rename_all = "kebab-case")]
pub enum Job {
    Gen {
        params: Params,
        version: u32,
    },
    Import {
        backend: Backend,
        dir: PathBuf,
        params: Params,
        version: u32,
        id: u32,
    },
    Read {
        backend: Backend,
        dir: PathBuf,
        id: u32,
    },
    Top {
        backend: Backend,
        dir: PathBuf,
        id: u32,
    },
    NativeTop {
        backend: Backend,
        dir: PathBuf,
        id: u32,
    },
    Prefix {
        backend: Backend,
        dir: PathBuf,
        id: u32,
        prefix: Vec<NativeName>,
    },
    Delta {
        backend: Backend,
        dir: PathBuf,
        params: Params,
        version: u32,
        id: u32,
        base: u32,
    },
    Diff {
        backend: Backend,
        dir: PathBuf,
        a: u32,
        b: u32,
    },
    List {
        backend: Backend,
        dir: PathBuf,
    },
    History {
        backend: Backend,
        dir: PathBuf,
        params: Params,
        versions: u32,
        strategy: Strategy,
    },
    Scan {
        root: PathBuf,
        lister: String,
        threads: usize,
    },
    ScanStore {
        root: PathBuf,
        lister: String,
        threads: usize,
        backend: Backend,
        dir: PathBuf,
    },
    /// Import under RLIMIT_FSIZE so the write fails part-way (disk-full stand-in).
    LimitedImport {
        backend: Backend,
        dir: PathBuf,
        params: Params,
        id: u32,
        limit_bytes: u64,
    },
    Verify {
        backend: Backend,
        dir: PathBuf,
        id: u32,
        params: Params,
        version: u32,
    },
    /// Writes and reads a full snapshot through the shipping `Store`.
    ProductionStore {
        dir: PathBuf,
        params: Params,
        format: ProductionFormat,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobResult {
    pub elapsed_ms: f64,
    pub items: u64,
    pub footprint: u64,
    pub ok: bool,
    pub error: Option<String>,
    #[serde(default)]
    pub peak_rss: u64,
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
#[cfg(unix)]
pub fn run_child(job: &Job) -> io::Result<Measured> {
    run_child_with(job, None)
}

/// Like `run_child`, but sends SIGKILL after `kill_after_ms` (crash test).
#[cfg(unix)]
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
        error: Some(if kill_after_ms.is_some() {
            "killed".into()
        } else {
            format!("child failed (status {status}): {text}")
        }),
        ..Default::default()
    });
    // macOS reports ru_maxrss in bytes.
    Ok(Measured {
        result,
        max_rss: ru.ru_maxrss as u64,
        wall_ms,
    })
}

#[cfg(not(unix))]
pub fn run_child(job: &Job) -> io::Result<Measured> {
    let exe = std::env::current_exe()?;
    let started = Instant::now();
    let output = Command::new(exe)
        .arg("child")
        .arg(serde_json::to_string(job).unwrap())
        .stdin(Stdio::null())
        .output()?;
    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    let text = String::from_utf8_lossy(&output.stdout);
    let result = serde_json::from_str::<JobResult>(text.trim()).unwrap_or_else(|_| JobResult {
        ok: false,
        error: Some(format!("child failed ({}): {text}", output.status)),
        ..Default::default()
    });
    Ok(Measured {
        max_rss: result.peak_rss,
        result,
        wall_ms,
    })
}

#[cfg(unix)]
fn limit_file_size(limit_bytes: u64) -> io::Result<()> {
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
        let lim = libc::rlimit {
            rlim_cur: limit_bytes,
            rlim_max: limit_bytes,
        };
        if libc::setrlimit(libc::RLIMIT_FSIZE, &lim) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn limit_file_size(_limit_bytes: u64) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "limited benchmark imports require Unix RLIMIT_FSIZE support",
    ))
}

fn timed<F: FnOnce() -> io::Result<(u64, serde_json::Value)>>(f: F) -> JobResult {
    let t = Instant::now();
    match f() {
        Ok((items, extra)) => JobResult {
            elapsed_ms: t.elapsed().as_secs_f64() * 1000.0,
            items,
            ok: true,
            extra,
            ..Default::default()
        },
        Err(e) => JobResult {
            elapsed_ms: t.elapsed().as_secs_f64() * 1000.0,
            ok: false,
            error: Some(e.to_string()),
            ..Default::default()
        },
    }
}

fn count(s: diskdrift_core::diff::RecordStream<'_>) -> io::Result<(u64, u64)> {
    let mut n = 0;
    let mut h = 0u64;
    for r in s {
        let r = r?;
        n += 1;
        h = h
            .wrapping_mul(31)
            .wrapping_add(r.alloc ^ r.ino ^ r.name.len() as u64);
    }
    Ok((n, h))
}

/// Encodes delta ops for `version` against `base` into memory, then writes them.
fn write_delta(
    store: &mut dyn Store,
    params: Params,
    version: u32,
    id: u32,
    base: u32,
) -> io::Result<u64> {
    let mut buf = Vec::new();
    let mut n = 0u64;
    {
        let infos = store.list()?;
        let base_stream = stores::open(store, &infos, base)?;
        let new_stream = Box::new(Gen::new(params, version).map(Ok));
        let mut enc = OpEncoder::new();
        let mut builder = DeltaBuilder::new(|op: &DeltaOp| {
            enc.encode(op, &mut buf)?;
            n += 1;
            Ok(())
        });
        diff::merge(base_stream, new_stream, &mut builder)?;
    }
    let mut dec = OpDecoder::new();
    let mut s = buf.as_slice();
    let mut ops = std::iter::from_fn(|| {
        if s.is_empty() {
            None
        } else {
            dec.decode(&mut s).ok()
        }
    });
    store.write_delta(id, base, &mut ops)?;
    Ok(n)
}

pub fn execute(job: Job) -> JobResult {
    let mut footprint_dir: Option<PathBuf> = None;
    let mut r = match job {
        Job::Gen { params, version } => timed(|| {
            Ok((
                Gen::new(params, version).count() as u64,
                serde_json::Value::Null,
            ))
        }),
        Job::Import {
            backend,
            dir,
            params,
            version,
            id,
        } => {
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
            let r = top::top(
                stores::open(s.as_ref(), &infos, id)?,
                &TopOptions {
                    limit: 100,
                    ..Default::default()
                },
            )?;
            Ok((
                r.entries,
                serde_json::json!({ "largestFile": r.files.first().map(|f| f.allocated_bytes) }),
            ))
        }),
        Job::NativeTop { backend, dir, id } => timed(|| {
            let s = backend.open(&dir)?;
            match s.native_top_files(id, 100) {
                Some(v) => {
                    let v = v?;
                    Ok((
                        v.len() as u64,
                        serde_json::json!({ "largestFile": v.first() }),
                    ))
                }
                None => Err(io::Error::new(io::ErrorKind::Unsupported, "no native top")),
            }
        }),
        Job::Prefix {
            backend,
            dir,
            id,
            prefix,
        } => timed(|| {
            let s = backend.open(&dir)?;
            let (n, h) = count(s.read_prefix(id, &prefix)?)?;
            Ok((n, serde_json::json!({ "hash": h })))
        }),
        Job::Delta {
            backend,
            dir,
            params,
            version,
            id,
            base,
        } => {
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
            let r = diff::diff(
                stores::open(s.as_ref(), &infos, a)?,
                stores::open(s.as_ref(), &infos, b)?,
                DiffOptions::default(),
            )?;
            let sm = &r.summary;
            let total = sm.added.count
                + sm.deleted.count
                + sm.grown.count
                + sm.shrunk.count
                + sm.modified_same_size
                + sm.unchanged;
            Ok((total, serde_json::to_value(sm).unwrap()))
        }),
        Job::List { backend, dir } => timed(|| {
            let s = backend.open(&dir)?;
            Ok((s.list()?.len() as u64, serde_json::Value::Null))
        }),
        Job::History {
            backend,
            dir,
            params,
            versions,
            strategy,
        } => {
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
                        Strategy::Adaptive { pct }
                            if v > 0 && last_delta * 100 <= cp_bytes * pct as u64 =>
                        {
                            Some(checkpoint)
                        }
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
                Ok((
                    versions as u64,
                    serde_json::json!({ "stepMs": steps, "items": sizes.iter().map(|x| x.0).collect::<Vec<_>>(), "footprintAfter": sizes.iter().map(|x| x.1).collect::<Vec<_>>() }),
                ))
            })
        }
        Job::Scan {
            root,
            lister,
            threads,
        } => timed(|| {
            let l = lister_by_name(&lister);
            let layout = diskdrift_platform::scan_layout(&root)?;
            let opts = WalkOptions {
                threads,
                same_filesystem_devs: layout.same_filesystem_devs,
                alias_paths: layout.alias_paths,
                ..Default::default()
            };
            let cancel = AtomicBool::new(false);
            let mut sink = |_: &Record, _: &Path| Ok(());
            let rep = walk::walk(&root, l.as_ref(), &opts, &mut sink, &cancel, None)
                .map_err(|e| io::Error::other(e.to_string()))?;
            Ok((
                rep.entries,
                serde_json::json!({ "inaccessibleDirs": rep.inaccessible_dirs, "allocated": rep.allocated_bytes }),
            ))
        }),
        Job::ScanStore {
            root,
            lister,
            threads,
            backend,
            dir,
        } => {
            footprint_dir = Some(dir.clone());
            timed(|| {
                let l = lister_by_name(&lister);
                let layout = diskdrift_platform::scan_layout(&root)?;
                let opts = WalkOptions {
                    threads,
                    same_filesystem_devs: layout.same_filesystem_devs,
                    alias_paths: layout.alias_paths,
                    ..Default::default()
                };
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
                            tx.send(std::mem::replace(&mut batch, Vec::with_capacity(4096)))
                                .map_err(|_| io::Error::other("writer gone"))?;
                        }
                        Ok(())
                    };
                    let rep = walk::walk(&root2, l.as_ref(), &opts, &mut sink, &cancel, None);
                    if !batch.is_empty() {
                        let _ = tx.send(batch);
                    }
                    rep.map(|r| r.entries)
                        .map_err(|e| io::Error::other(e.to_string()))
                });
                let mut it = rx.into_iter().flatten();
                let n = s.write_full(0, &mut it)?;
                let walked = walker
                    .join()
                    .map_err(|_| io::Error::other("walker panicked"))??;
                Ok((n.max(walked), serde_json::Value::Null))
            })
        }
        Job::LimitedImport {
            backend,
            dir,
            params,
            id,
            limit_bytes,
        } => timed(|| {
            limit_file_size(limit_bytes)?;
            let mut s = backend.open(&dir)?;
            let n = s.write_full(id, &mut Gen::new(params, 1))?;
            Ok((n, serde_json::Value::Null))
        }),
        Job::Verify {
            backend,
            dir,
            id,
            params,
            version,
        } => timed(|| {
            let expected = count(Box::new(Gen::new(params, version).map(Ok)))?;
            let s = backend.open(&dir)?;
            let infos = s.list()?;
            let got = count(stores::open(s.as_ref(), &infos, id)?)?;
            if got != expected {
                return Err(io::Error::other(format!(
                    "snapshot {id}: expected {expected:?}, got {got:?}"
                )));
            }
            let ids: Vec<u32> = infos.iter().map(|i| i.id).collect();
            Ok((got.0, serde_json::json!({ "snapshots": ids })))
        }),
        Job::ProductionStore {
            dir,
            params,
            format,
        } => {
            footprint_dir = Some(dir.clone());
            timed(|| {
                use diskdrift_core::native::{NativePath, NativeRoot, RootKind};
                use diskdrift_core::record::{FileIdScheme, TimestampResolution};
                use diskdrift_core::walk::WalkReport;
                use diskdrift_storage::{
                    ScanOptions, SnapshotMeta, Status, Store, VolumeIdentity, meta,
                };

                let windows = cfg!(windows);
                let root_units: Vec<u16> = r"C:\synthetic".encode_utf16().collect();
                let root_native = if windows {
                    NativeRoot::new(
                        NativePath::windows(root_units.clone()),
                        RootKind::Drive,
                        false,
                    )
                    .map_err(|error| io::Error::other(error.to_string()))?
                } else {
                    NativeRoot::posix(b"/synthetic".to_vec())
                };
                let root_in_volume_native = if windows {
                    NativePath::windows(vec![b'\\' as u16])
                } else {
                    NativePath::unix(b"/synthetic".to_vec())
                };
                let mut descriptor = meta::VolumeDescriptor::new(
                    0,
                    if windows { "windows" } else { "linux" },
                    if windows {
                        FileIdScheme::WindowsFileId128
                    } else {
                        FileIdScheme::PosixInode64
                    },
                    meta::FileIdScope::Volume,
                    meta::AllocationSemantics::FilesystemReported,
                    TimestampResolution::Seconds,
                    meta::Provenance::Observed("synthetic benchmark".into()),
                );
                descriptor.identity = if windows {
                    NativePath::windows(r"\\?\Volume{benchmark}".encode_utf16().collect())
                } else {
                    NativePath::unix(b"benchmark".to_vec())
                };
                descriptor.mount_key = if windows {
                    NativePath::windows(r"C:\".encode_utf16().collect())
                } else {
                    NativePath::unix(b"/synthetic".to_vec())
                };
                descriptor.identity_strength = "uuid".into();
                let meta = SnapshotMeta {
                    format_version: meta::META_FORMAT_VERSION,
                    id: 0,
                    name: "benchmark".into(),
                    root: if windows {
                        r"C:\synthetic"
                    } else {
                        "/synthetic"
                    }
                    .into(),
                    root_native: Some(root_native.clone()),
                    disk: None,
                    root_in_volume: None,
                    root_in_volume_native: Some(root_in_volume_native),
                    status: Status::InProgress,
                    started_at: String::new(),
                    finished_at: None,
                    volume: VolumeIdentity {
                        uuid: Some("benchmark".into()),
                        fs_type: if windows { "ntfs" } else { "synthetic" }.into(),
                        mount_point: if windows { r"C:\" } else { "/synthetic" }.into(),
                        descriptor: Some(descriptor.clone()),
                        ..Default::default()
                    },
                    volumes: meta::complete_volume_table(&descriptor),
                    options: ScanOptions {
                        stay_on_filesystem: true,
                        ..Default::default()
                    },
                    space_before: None,
                    space_after: None,
                    storage: Default::default(),
                    counts: None,
                    error: None,
                    diskdrift_version: "bench".into(),
                };
                if format == ProductionFormat::Dds1 {
                    if windows {
                        return Err(io::Error::other(
                            "DDS1 production benchmark cannot encode Windows-native names",
                        ));
                    }
                    use diskdrift_storage::codec::RecordEncoder;
                    use diskdrift_storage::container::DataWriter;
                    use diskdrift_storage::{FileKind, StorageInfo};

                    let snapshots = dir.join("snapshots");
                    std::fs::create_dir_all(&snapshots)?;
                    let data_path = snapshots.join("00000001.dds");
                    let mut writer =
                        DataWriter::create(snapshots.join("00000001.dds.tmp"), FileKind::Records)?;
                    let import_started = Instant::now();
                    let mut encoder = RecordEncoder::new();
                    let mut items = 0;
                    for record in Gen::new(params, 0) {
                        if let Some(block) = encoder.push(&record)? {
                            writer.block(block)?;
                        }
                        items += 1;
                    }
                    if let Some(block) = encoder.finish()? {
                        writer.block(block)?;
                    }
                    let blocks = writer.blocks();
                    writer.commit(&data_path)?;
                    let mut legacy_meta = meta;
                    legacy_meta.id = 1;
                    legacy_meta.status = Status::Completed;
                    legacy_meta.finished_at = Some(String::new());
                    legacy_meta.storage = StorageInfo {
                        data_file: Some("00000001.dds".into()),
                        bytes_on_disk: stores::footprint(&data_path),
                        blocks,
                        items,
                        ..Default::default()
                    };
                    std::fs::write(
                        dir.join("store.json"),
                        r#"{"format":"diskdrift-snapshots","version":3}"#,
                    )?;
                    std::fs::write(
                        snapshots.join("00000001.json"),
                        serde_json::to_vec(&legacy_meta)?,
                    )?;
                    let import_ms = import_started.elapsed().as_secs_f64() * 1000.0;
                    let footprint = stores::footprint(&dir);
                    let read_started = Instant::now();
                    let legacy_store =
                        Store::open(&dir).map_err(|error| io::Error::other(error.to_string()))?;
                    let read_items = legacy_store
                        .records(1)
                        .map_err(|error| io::Error::other(error.to_string()))?
                        .try_fold(0u64, |count, record| record.map(|_| count + 1))?;
                    let read_ms = read_started.elapsed().as_secs_f64() * 1000.0;
                    return Ok((
                        items,
                        serde_json::json!({
                            "format": format.label(),
                            "importMs": import_ms,
                            "readMs": read_ms,
                            "readItems": read_items,
                            "footprint": footprint,
                            "pathEncoding": "unix-bytes",
                        }),
                    ));
                }
                let store =
                    Store::create(&dir).map_err(|error| io::Error::other(error.to_string()))?;
                let import_started = Instant::now();
                let (lock, _) = store
                    .lock()
                    .map_err(|error| io::Error::other(error.to_string()))?;
                let mut writer = store
                    .begin(lock, meta, None)
                    .map_err(|error| io::Error::other(error.to_string()))?;
                let mut items = 0;
                for mut record in Gen::new(params, 0) {
                    if windows {
                        record.name = if record.depth == 0 {
                            NativeName::windows(root_units.clone())
                        } else {
                            match &record.name {
                                NativeName::Unix(bytes) => NativeName::windows(
                                    bytes.iter().map(|byte| u16::from(*byte)).collect(),
                                ),
                                NativeName::Windows(_) => {
                                    unreachable!("synthetic generator is Unix-byte based")
                                }
                            }
                        };
                    }
                    writer
                        .push(&record)
                        .map_err(|error| io::Error::other(error.to_string()))?;
                    items += 1;
                }
                let committed = writer
                    .commit(WalkReport::default(), None, String::new())
                    .map_err(|error| io::Error::other(error.to_string()))?;
                let import_ms = import_started.elapsed().as_secs_f64() * 1000.0;
                let footprint = store.size_on_disk();
                let read_started = Instant::now();
                let read_items = store
                    .records(committed.id)
                    .map_err(|error| io::Error::other(error.to_string()))?
                    .try_fold(0u64, |count, record| record.map(|_| count + 1))?;
                let read_ms = read_started.elapsed().as_secs_f64() * 1000.0;
                Ok((
                    items,
                    serde_json::json!({
                        "format": format.label(),
                        "importMs": import_ms,
                        "readMs": read_ms,
                        "readItems": read_items,
                        "footprint": footprint,
                        "pathEncoding": if windows { "windows-utf16" } else { "unix-bytes" },
                    }),
                ))
            })
        }
    };
    if let Some(d) = footprint_dir {
        r.footprint = stores::footprint(&d);
    }
    r.peak_rss = peak_rss_bytes();
    r
}

#[cfg(windows)]
fn peak_rss_bytes() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    if unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    } == 0
    {
        0
    } else {
        counters.PeakWorkingSetSize as u64
    }
}

#[cfg(not(windows))]
fn peak_rss_bytes() -> u64 {
    0
}

pub fn lister_by_name(name: &str) -> Box<dyn diskdrift_core::walk::DirLister> {
    use diskdrift_platform::ListerKind;
    diskdrift_platform::lister(match name {
        "readdir" => ListerKind::ReadDir,
        _ => ListerKind::Bulk,
    })
}
