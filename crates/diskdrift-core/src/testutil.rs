//! Test helpers. Temporary trees live under the workspace `target/test-tmp/`, never
//! in system temp directories, so tests only ever touch the project folder.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Root for all test scratch directories: `<workspace>/target/test-tmp`.
pub fn test_tmp_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-tmp");
    std::fs::create_dir_all(&root).expect("create target/test-tmp");
    root.canonicalize().expect("canonicalize target/test-tmp")
}

/// A scratch directory removed on drop.
pub struct TestDir {
    path: PathBuf,
}

impl TestDir {
    pub fn new(name: &str) -> TestDir {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = test_tmp_root().join(format!("{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create test dir");
        TestDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Creates a file of `len` bytes (and its parent directories).
    pub fn file(&self, rel: &str, len: usize) -> PathBuf {
        let p = self.path.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, vec![b'x'; len]).unwrap();
        p
    }

    pub fn dir(&self, rel: &str) -> PathBuf {
        let p = self.path.join(rel);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Canonical records for `(relative path, kind, allocated size)` specs given in any
/// order, below the root `/r`. Sizes are used as logical and allocated size.
pub fn records<S: AsRef<str>>(spec: &[(S, crate::record::Kind, u64)]) -> Vec<crate::record::Record> {
    let mut v: Vec<(Vec<Vec<u8>>, crate::record::Kind, u64)> = spec
        .iter()
        .map(|(p, k, a)| {
            let mut comps: Vec<Vec<u8>> = vec![b"/r".to_vec()];
            comps.extend(p.as_ref().split('/').filter(|c| !c.is_empty()).map(|c| c.as_bytes().to_vec()));
            (comps, *k, *a)
        })
        .collect();
    v.sort_by(|a, b| crate::paths::compare_paths(&a.0, &b.0));
    v.into_iter()
        .map(|(comps, kind, alloc)| crate::record::Record {
            depth: (comps.len() - 1) as u16,
            // Stable per path, like a real inode across scans.
            ino: comps.iter().flatten().fold(1469598103934665603u64, |h, &b| (h ^ b as u64).wrapping_mul(1099511628211)),
            name: comps.last().unwrap().clone(),
            kind,
            size: alloc,
            alloc,
            mtime: 1000,
            nlink: 1,
            ..Default::default()
        })
        .collect()
}

pub fn boxed(v: Vec<crate::record::Record>) -> crate::diff::RecordStream<'static> {
    Box::new(v.into_iter().map(Ok))
}
