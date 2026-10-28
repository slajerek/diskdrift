//! Lister cost without the walker: plain single-threaded recursion.
use diskdrift_core::record::Kind;
use diskdrift_core::walk::DirLister;
use std::path::Path;
use std::time::Instant;

fn rec(l: &dyn DirLister, p: &Path, dev: u64, n: &mut u64, buf: &mut Vec<diskdrift_core::walk::RawEntry>) {
    let start = buf.len();
    if l.list(p, buf).is_err() {
        return;
    }
    let children: Vec<_> = buf.drain(start..).collect();
    *n += children.len() as u64;
    for c in children {
        if c.kind == Kind::Dir && c.dev == dev && !c.is_mount_point {
            rec(l, &p.join(std::ffi::OsStr::new(std::str::from_utf8(c.name.as_unix_bytes().unwrap_or_default()).unwrap_or("?"))), dev, n, buf);
        }
    }
}

fn main() {
    let root = std::env::args().nth(1).unwrap();
    let which = std::env::args().nth(2).unwrap_or("bulk".into());
    let l = diskdrift_bench::jobs::lister_by_name(&which);
    let dev = l.stat(Path::new(&root)).unwrap().dev;
    let t = Instant::now();
    let mut n = 0;
    let mut buf = Vec::new();
    rec(l.as_ref(), Path::new(&root), dev, &mut n, &mut buf);
    println!("{which}: {n} entries in {:.2}s ({:.0} k/s)", t.elapsed().as_secs_f64(), n as f64 / t.elapsed().as_secs_f64() / 1000.0);
}
