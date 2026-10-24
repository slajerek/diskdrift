//! Text output for scans, snapshot lists, top and diff.

use super::table::{Align, Table, ellipsize_start};
use crate::core::diff::{Change, DiffItem};
use crate::core::top::TopItem;
use crate::core::units::{SizeUnits, format_count, format_delta, format_size};
use crate::snapshots::{DiffResult, ScanOutcome, SnapshotList, TopResult};
use crate::storage::{SnapshotMeta, Status};
use std::fmt::Write;

fn lossy(p: &[u8]) -> String {
    String::from_utf8_lossy(p).into_owned()
}

fn short_time(rfc: &str) -> String {
    // 2026-10-21T14:03:12+02:00 -> 2026-10-21 14:03
    rfc.get(..16).map(|s| s.replace('T', " ")).unwrap_or_else(|| rfc.to_string())
}

fn coverage(m: &SnapshotMeta) -> String {
    let c = coverage_only(m);
    if m.options.careful && m.status.is_usable() { format!("{c}, careful") } else { c }
}

fn coverage_only(m: &SnapshotMeta) -> String {
    match (m.status, &m.counts) {
        (Status::Completed, _) => "complete".into(),
        (Status::Partial, Some(c)) => format!("partial ({} unreadable dirs)", c.inaccessible_dirs),
        (Status::Partial, None) => "partial".into(),
        (Status::InProgress, _) => "scanning…".into(),
        (_, _) => "-".into(),
    }
}

/// What one snapshot is, for a detail line under a list of snapshots: whether
/// it holds data, why not, what it left out, and how long it took.
pub fn snapshot_detail(m: &SnapshotMeta) -> Vec<String> {
    let mut out = Vec::new();
    let c = m.counts.as_ref();
    let took = c.filter(|c| c.elapsed_ms > 0).map(|c| format!(" in {:.0} s", c.elapsed_ms as f64 / 1000.0)).unwrap_or_default();
    out.push(match m.status {
        Status::Completed => format!("Complete{took}: every folder could be read."),
        Status::Partial => format!(
            "Partial{took}: {} folder(s) could not be read (often missing Full Disk Access); their contents are not in it.",
            c.map(|c| c.inaccessible_dirs).unwrap_or(0)
        ),
        Status::Cancelled => "Cancelled before it finished: it holds no data, so it cannot be browsed or compared.".into(),
        Status::Failed => format!("Failed: {}. It holds no data.", m.error.as_deref().unwrap_or("no reason recorded")),
        Status::InProgress => "Not finished: still running, or interrupted (then it holds no data).".into(),
    });
    let mut scope = Vec::new();
    if m.options.careful {
        scope.push("careful scan (mounted read-only, one thread)".into());
    }
    if !m.options.exclude.is_empty() {
        scope.push(format!("excluded: {}", m.options.exclude.join(", ")));
    }
    if m.options.stay_on_filesystem {
        scope.push("stayed on one filesystem".into());
    }
    if let Some(c) = c.filter(|c| c.skipped_mounts > 0) {
        scope.push(format!("{} other volume(s) skipped", c.skipped_mounts));
    }
    if !scope.is_empty() {
        out.push(format!("Scope: {}.", scope.join(" · ")));
    }
    out
}

fn headline_status(m: &SnapshotMeta) -> String {
    match (m.status, &m.counts) {
        (Status::Partial, Some(c)) => format!("PARTIAL: {} unreadable dirs", c.inaccessible_dirs),
        (s, _) => s.to_string(),
    }
}

/// The answer first, for the TUI after a scan: what was saved, what it left
/// out, and what changed since the previous scan of the same folder.
pub fn scan_summary(o: &ScanOutcome, prev: Option<&SnapshotMeta>, now: i64, u: SizeUnits) -> Vec<String> {
    let m = &o.snapshot;
    if o.cancelled {
        return vec![
            "Scan stopped: nothing of it was kept.".into(),
            format!("The history lists it as #{} CANCELLED, with no data.", m.id),
        ];
    }
    let mut out = Vec::new();
    let Some(c) = m.counts.as_ref() else { return vec![format!("Saved as scan #{} of {}.", m.id, m.root)] };
    let partial = m.status == Status::Partial;
    out.push(format!(
        "Saved as scan #{}: {} holds {}{} in {} entries (scanned in {:.0} s).",
        m.id,
        m.root,
        if partial { "at least " } else { "" },
        format_size(c.allocated_bytes, u),
        format_count(c.entries),
        c.elapsed_ms as f64 / 1000.0
    ));
    if partial {
        out.push(format!(
            "{} folders could not be read (macOS protects them), so sizes are lower bounds. Full Disk Access for your terminal lets DiskDrift read them.",
            format_count(c.inaccessible_dirs)
        ));
    }
    match prev.and_then(|p| p.counts.as_ref().map(|pc| (p, pc))) {
        Some((p, pc)) => {
            let delta = c.allocated_bytes as i128 - pc.allocated_bytes as i128;
            let (from, to) = (format_size(pc.allocated_bytes, u), format_size(c.allocated_bytes, u));
            let range = if from == to { String::new() } else { format!(" ({from} → {to})") };
            out.push(format!("Since {}: {} tracked{range}.", crate::snapshots::scan_label(p, now), format_delta(delta, u)));
        }
        None => out.push(format!("First scan of {} with these settings: scan it again later to see what grew.", m.root)),
    }
    if let Some(s) = &m.space_after {
        out.push(format!("Free space now: {} available.", format_size(s.available_important_bytes.unwrap_or(s.free_bytes), u)));
    }
    out
}

/// The change in tracked size of `m` since `prev`, for a one-line message:
/// "+1.2 GB tracked since scan #4", "no change since scan #4", "first scan of it".
pub fn tracked_change(m: &SnapshotMeta, prev: Option<&SnapshotMeta>, u: SizeUnits) -> String {
    let lower = if m.status == Status::Partial { " (some folders unreadable: a lower bound)" } else { "" };
    match (m.counts.as_ref(), prev.and_then(|p| p.counts.as_ref().map(|pc| (p, pc)))) {
        (Some(c), Some((p, pc))) => {
            let delta = c.allocated_bytes as i128 - pc.allocated_bytes as i128;
            if delta == 0 {
                format!("no change in tracked size since scan #{}{lower}", p.id)
            } else {
                format!("{} tracked since scan #{}{lower}", format_delta(delta, u), p.id)
            }
        }
        (Some(c), None) => format!("first scan of {}: {} tracked{lower}", m.root, format_size(c.allocated_bytes, u)),
        (None, _) => format!("scan #{} saved", m.id),
    }
}

pub fn scan_outcome(o: &ScanOutcome, u: SizeUnits) -> String {
    let m = &o.snapshot;
    let mut out = String::new();
    let c = m.counts.as_ref();
    if o.cancelled {
        let _ = writeln!(out, "Scan cancelled after {} entries; snapshot {} recorded as CANCELLED (no data kept).", format_count(c.map(|c| c.entries).unwrap_or(0)), m.id);
        return out;
    }
    let careful = if m.options.careful { ", careful scan (one thread)" } else { "" };
    let _ = writeln!(out, "Snapshot {} '{}' — {}{careful}", m.id, m.name, headline_status(m));
    let _ = writeln!(out, "  Root:        {} on {} ({})", m.root, m.volume.name.as_deref().unwrap_or("?"), m.volume.fs_type);
    if let Some(d) = &o.disk {
        let first = if d.new { ", seen for the first time" } else { "" };
        let _ = writeln!(out, "  Disk:        {} ({}){first}", d.disk.display_name(), d.disk.id);
    }
    if let Some(c) = c {
        let _ = writeln!(
            out,
            "  Entries:     {} ({} files, {} directories, {} symlinks, {} other) in {:.1}s",
            format_count(c.entries),
            format_count(c.files),
            format_count(c.dirs),
            format_count(c.symlinks),
            format_count(c.others),
            c.elapsed_ms as f64 / 1000.0
        );
        let _ = writeln!(out, "  Tracked:     {} allocated, {} logical", format_size(c.allocated_bytes, u), format_size(c.logical_bytes, u));
        if c.hardlink_duplicates > 0 {
            let _ = writeln!(out, "  Hardlinks:   {} extra paths counted once", format_count(c.hardlink_duplicates));
        }
        if c.skipped_mounts > 0 || c.excluded > 0 {
            let _ = writeln!(out, "  Skipped:     {} other filesystems, {} excluded directories", c.skipped_mounts, c.excluded);
        }
        if c.issue_count > 0 {
            let _ = writeln!(out, "  Unreadable:  {} directories, {} entries ({} issues; first ones below)", c.inaccessible_dirs, c.stat_errors, c.issue_count);
            for i in c.issues.iter().take(5) {
                let _ = writeln!(out, "               {} — {}", i.path, i.message);
            }
        }
    }
    if let Some(s) = &m.space_after {
        let _ = writeln!(out, "  OS free now: {} (available {})", format_size(s.free_bytes, u), s.available_important_bytes.map(|b| format_size(b, u)).unwrap_or_else(|| "-".into()));
    }
    let _ = writeln!(
        out,
        "  Stored:      {} on disk; store total {} at {}",
        format_size(m.storage.bytes_on_disk, u),
        format_size(o.store_bytes, u),
        o.store_path.display()
    );
    match &o.rebase {
        crate::storage::Rebase::Converted { id, freed_bytes } => {
            let _ = writeln!(out, "  History:     snapshot {id} compacted to a delta against this one ({} freed)", format_size(*freed_bytes, u));
        }
        crate::storage::Rebase::KeptFull { id, reason } => {
            let _ = writeln!(out, "  History:     snapshot {id} kept as a full snapshot: {reason}");
        }
        crate::storage::Rebase::Nothing => {}
    }
    if let Some(e) = &o.rebase_error {
        let _ = writeln!(out, "  Note:        previous snapshot left uncompacted ({e})");
    }
    out
}

pub fn snapshot_list(l: &SnapshotList, u: SizeUnits) -> String {
    if let Some(d) = &l.disk
        && l.snapshots.is_empty()
    {
        return format!("No snapshots of disk {} ({}).\n", d.display_name(), d.id);
    }
    if !l.store_exists || l.snapshots.is_empty() {
        return format!("No snapshots in {}. Run `diskdrift scan` to create a baseline.\n", l.store_path.display());
    }
    // The disk column only when it tells snapshots apart.
    let with_disk = l.disk_count > 1 && l.disk.is_none();
    let mut cols = vec![("ID", Align::Right), ("Name", Align::Left), ("Started", Align::Left)];
    if with_disk {
        cols.push(("Disk", Align::Left));
    }
    cols.extend([
        ("Root", Align::Left),
        ("Status", Align::Left),
        ("Coverage", Align::Left),
        ("Entries", Align::Right),
        ("Allocated", Align::Right),
        ("Stored", Align::Right),
        ("Kind", Align::Left),
    ]);
    let mut t = Table::new(&cols);
    for r in &l.snapshots {
        let m = &r.meta;
        let mut row = vec![m.id.to_string(), m.name.clone(), short_time(&m.started_at)];
        if with_disk {
            row.push(r.disk_name.clone().unwrap_or_else(|| "-".into()));
        }
        row.extend([
            m.root.clone(),
            m.status.to_string(),
            coverage(m),
            m.counts.as_ref().map(|c| c.entries.to_string()).unwrap_or_else(|| "-".into()),
            m.counts.as_ref().map(|c| format_size(c.allocated_bytes, u)).unwrap_or_else(|| "-".into()),
            format_size(r.size_on_disk, u),
            match (m.status.is_usable(), m.storage.base) {
                (false, _) => "-".into(),
                (true, None) => "full".into(),
                (true, Some(b)) => format!("delta→{b}"),
            },
        ]);
        t.row(row);
    }
    let mut out = String::new();
    if let Some(d) = &l.disk {
        let _ = writeln!(out, "Disk {} ({}):", d.display_name(), d.id);
    }
    out.push_str(&t.render());
    let _ = writeln!(out, "Store: {} — {} on disk", l.store_path.display(), format_size(l.store_bytes, u));
    out
}

fn top_rows(t: &mut Table, items: &[TopItem], u: SizeUnits) {
    for i in items {
        t.row([
            format_size(i.allocated_bytes, u),
            format_size(i.logical_bytes, u),
            if i.kind == crate::core::record::Kind::Dir { i.entries.to_string() } else { String::new() },
            format!("{}{}", lossy(&i.path), if i.incomplete { "  (incomplete)" } else { "" }),
        ]);
    }
}

pub fn top(r: &TopResult, u: SizeUnits, files: bool, dirs: bool) -> String {
    let m = &r.snapshot;
    let mut out = String::new();
    let _ = writeln!(out, "Snapshot {} '{}' ({}, {}) — {} entries, {} allocated", m.id, m.name, short_time(&m.started_at), m.status, r.report.entries, format_size(r.report.total_allocated, u));
    if dirs && !r.report.dirs.is_empty() {
        out.push_str(if r.deepest {
            "\nDeepest large directories (inclusive; parents that mostly repeat a listed child left out):\n"
        } else {
            "\nLargest directories (inclusive):\n"
        });
        let mut t = Table::new(&[("Allocated", Align::Right), ("Logical", Align::Right), ("Entries", Align::Right), ("Path", Align::Left)]);
        top_rows(&mut t, &r.report.dirs, u);
        out.push_str(&t.render());
    }
    if files && !r.report.files.is_empty() {
        out.push_str("\nLargest files:\n");
        let mut t = Table::new(&[("Allocated", Align::Right), ("Logical", Align::Right), ("", Align::Right), ("Path", Align::Left)]);
        top_rows(&mut t, &r.report.files, u);
        out.push_str(&t.render());
    }
    out
}

fn change_label(c: Change) -> &'static str {
    match c {
        Change::Unchanged => "same",
        Change::Added => "new",
        Change::Deleted => "deleted",
        Change::Modified => "changed",
        Change::NewlyVisible => "now visible",
        Change::NoLongerVisible => "not visible",
    }
}

fn item_rows(t: &mut Table, items: &[DiffItem], u: SizeUnits) {
    for i in items {
        t.row([
            format_delta(i.delta_allocated as i128, u),
            format_size(i.old_allocated, u),
            format_size(i.new_allocated, u),
            if i.is_dir { String::new() } else { change_label(i.change).into() },
            ellipsize_start(&lossy(&i.path), 110),
        ]);
    }
}

fn item_table() -> Table {
    Table::new(&[
        ("Change", Align::Right),
        ("Before", Align::Right),
        ("After", Align::Right),
        ("", Align::Left),
        ("Path", Align::Left),
    ])
}

pub fn diff(r: &DiffResult, u: SizeUnits, files: bool, dirs: bool) -> String {
    let mut out = String::new();
    let (a, b) = (&r.old, &r.new);
    let _ = writeln!(out, "Comparing snapshot {} '{}' ({}) → {} '{}' ({})", a.id, a.name, short_time(&a.started_at), b.id, b.name, short_time(&b.started_at));
    if let Some(p) = &r.path {
        let _ = writeln!(out, "Restricted to {}", p.display());
    }
    if let Some(why) = &r.non_equivalent {
        let _ = writeln!(out, "WARNING: non-equivalent snapshots ({why})");
    }
    let s = &r.report.summary;
    out.push('\n');
    let mut t = Table::new(&[("", Align::Left), ("Value", Align::Right), ("Detail", Align::Left)]);
    if let Some(sp) = &r.space {
        t.row(["1. OS free space change".into(), format_delta(-sp.free_consumed_bytes, u), "statfs, end of each scan".into()]);
        if let Some(av) = sp.available_consumed_bytes {
            t.row(["   OS available change".into(), format_delta(-av, u), "incl. purgeable (as Finder)".into()]);
        }
    } else {
        t.row(["1. OS free space change".into(), "unknown".into(), "not recorded".into()]);
    }
    t.row([
        "2. Tracked logical change".into(),
        format_delta(s.net_logical_bytes, u),
        format!("{} → {}", format_size(s.old_total_logical, u), format_size(s.new_total_logical, u)),
    ]);
    t.row([
        "3. Tracked allocated change".into(),
        format_delta(s.net_allocated_bytes, u),
        format!("{} → {}", format_size(s.old_total_allocated, u), format_size(s.new_total_allocated, u)),
    ]);
    t.row([
        "4. Coverage changes".into(),
        format!("{} / {}", format_size(s.newly_visible.allocated_bytes, u), format_size(s.no_longer_visible.allocated_bytes, u)),
        format!("now visible: {} entries / no longer visible: {} entries", s.newly_visible.count, s.no_longer_visible.count),
    ]);
    match r.unreconciled_bytes {
        Some(x) => t.row([
            "5. Unreconciled difference".into(),
            format_delta(x, u),
            "OS-consumed minus tracked growth (diagnostic, not proof)".into(),
        ]),
        None => t.row(["5. Unreconciled difference".into(), "n/a".into(), String::new()]),
    }
    out.push_str(&t.render());
    let _ = writeln!(
        out,
        "\n  {} new ({}), {} deleted ({}), {} grew (+{}), {} shrank (-{}), {} changed same size, {} unchanged",
        s.added.count,
        format_size(s.added.allocated_bytes, u),
        s.deleted.count,
        format_size(s.deleted.allocated_bytes, u),
        s.grown.count,
        format_size(s.grown.allocated_bytes, u),
        s.shrunk.count,
        format_size(s.shrunk.allocated_bytes, u),
        s.modified_same_size,
        s.unchanged
    );
    if s.moved > 0 {
        let _ = writeln!(out, "  {} files moved or renamed ({}), matched by inode", s.moved, format_size(s.moved_bytes, u));
    }
    if dirs && !r.report.growing_dirs.is_empty() {
        out.push_str("\nDirectories that grew most (inclusive):\n");
        let mut t = item_table();
        item_rows(&mut t, &r.report.growing_dirs, u);
        out.push_str(&t.render());
    }
    if files && !r.report.files.is_empty() {
        out.push_str("\nChanged files:\n");
        let mut t = item_table();
        item_rows(&mut t, &r.report.files, u);
        out.push_str(&t.render());
    }
    if dirs && !r.report.shrinking_dirs.is_empty() {
        out.push_str("\nDirectories that shrank most:\n");
        let mut t = item_table();
        item_rows(&mut t, &r.report.shrinking_dirs, u);
        out.push_str(&t.render());
    }
    if !r.report.moves.is_empty() {
        out.push_str("\nLargest moves:\n");
        for m in r.report.moves.iter().take(10) {
            let _ = writeln!(out, "  {:>10}  {} → {}", format_size(m.allocated, u), lossy(&m.from), lossy(&m.to));
        }
    }
    out.push_str("\nNotes:\n");
    for n in &r.notes {
        let _ = writeln!(out, "  - {n}");
    }
    out
}
