//! Walker diagnostics on a real tree: peak queue/buffer sizes and RSS.
use diskdrift_core::record::Record;
use diskdrift_core::walk::{self, WalkOptions};
use std::path::Path;
use std::sync::atomic::AtomicBool;

fn main() {
    let root = std::env::args().nth(1).unwrap();
    let threads: usize = std::env::args().nth(2).map(|t| t.parse().unwrap()).unwrap_or(8);
    let budget: usize = std::env::args().nth(3).map(|t| t.parse().unwrap()).unwrap_or(200_000);
    let l = diskdrift_bench::jobs::lister_by_name("bulk");
    let cancel = AtomicBool::new(false);
    let mut sink = |_: &Record, _: &Path| Ok(());
    let opts = WalkOptions { threads, buffer_budget: budget, ..Default::default() };
    let r = walk::walk(Path::new(&root), l.as_ref(), &opts, &mut sink, &cancel, None).unwrap();
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    println!(
        "entries {} dirs {} in {:.1}s | peak pending dirs {} | peak buffered entries {} | hardlink inodes {} | issues {} | RSS {:.0} MB",
        r.entries, r.dirs, r.elapsed_ms as f64 / 1000.0, r.peak_pending_dirs, r.peak_buffered_entries, r.hardlink_inodes, r.issue_count, ru.ru_maxrss as f64 / 1e6
    );
}
