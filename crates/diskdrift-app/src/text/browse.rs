//! Text output for `browse` (one directory level of a snapshot or comparison),
//! plus the per-item labels the TUI shows too.

use super::table::{Align, Table};
use super::{changed_ago, date, entries};
use crate::core::browse::{Child, DiffChild};
use crate::core::diff::Change;
use crate::core::units::{SizeUnits, format_count, format_delta, format_size};
use crate::snapshots::{BrowseResult, DiffBrowseResult};
use crate::storage::SnapshotMeta;
use std::fmt::Write;

/// Start of the scan as a Unix timestamp (0 if unparseable).
pub fn scan_time(m: &SnapshotMeta) -> i64 {
    chrono::DateTime::parse_from_rfc3339(&m.started_at).map(|t| t.timestamp()).unwrap_or(0)
}

/// "Name/" for directories.
pub fn child_name(name: String, is_dir: bool) -> String {
    if is_dir { format!("{name}/") } else { name }
}

/// Share of the parent, "37%", or "<1%".
pub fn share(part: u64, whole: u64) -> String {
    if whole == 0 {
        return String::new();
    }
    let p = part as f64 * 100.0 / whole as f64;
    if p < 1.0 && part > 0 { "<1%".into() } else { format!("{p:.0}%") }
}

/// Allocated size, marked when part of the contents could not be read.
pub fn child_size(c: &Child, u: SizeUnits) -> String {
    if c.incomplete { format!("≥ {}", format_size(c.allocated_bytes, u)) } else { format_size(c.allocated_bytes, u) }
}

/// How long ago anything inside last changed (as recorded by the scan).
pub fn child_changed(c: &Child, now: i64) -> String {
    changed_ago(c.newest_mtime, now)
}

pub fn child_detail(c: &Child, u: SizeUnits, now: i64) -> Vec<String> {
    let mut out = vec![c.path.lossy()];
    let mut line = format!(
        "{} on disk, files {} · {} · last change {} ({})",
        format_size(c.allocated_bytes, u),
        format_size(c.logical_bytes, u),
        entries(c.entries),
        date(c.newest_mtime),
        child_changed(c, now)
    );
    if c.incomplete {
        line.push_str(" · some contents unreadable or excluded: a lower bound");
    }
    out.push(line);
    out
}

/// "new", "deleted", "changed", "visibility".
pub fn diff_change(c: &DiffChild) -> &'static str {
    match c.change {
        Change::Added => "new",
        Change::Deleted => "deleted",
        Change::NewlyVisible => "now visible",
        Change::NoLongerVisible => "not visible",
        Change::Modified | Change::Unchanged if c.is_dir() => "changed inside",
        Change::Modified => "changed",
        Change::Unchanged => "",
    }
}

pub fn diff_detail(c: &DiffChild, u: SizeUnits) -> Vec<String> {
    let mut out = vec![c.path.lossy()];
    let mut line = format!(
        "{} → {} ({}) · {} added, deleted or modified",
        format_size(c.old_allocated, u),
        format_size(c.new_allocated, u),
        format_delta(c.delta_allocated as i128, u),
        entries(c.changed_entries)
    );
    if c.visibility_changes > 0 {
        let _ = write!(line, " · {} entries only became (in)visible (access changed), not counted", c.visibility_changes);
    }
    out.push(line);
    out
}

fn snap(m: &SnapshotMeta) -> String {
    format!("#{} {} ({})", m.id, m.name, m.started_at.get(..16).unwrap_or(&m.started_at).replace('T', " "))
}

pub fn browse(r: &BrowseResult, u: SizeUnits) -> String {
    let mut out = String::new();
    let now = chrono::Local::now().timestamp();
    let l = &r.listing;
    let total = l.dir.as_ref().map(|d| d.allocated_bytes).unwrap_or(0);
    if let Some(d) = &l.dir {
        let _ = writeln!(out, "{} in snapshot {}: {} on disk, {}", d.path.lossy(), snap(&r.snapshot), child_size(d, u), entries(d.entries));
    }
    out.push('\n');
    let mut t = Table::new(&[("On disk", Align::Right), ("Share", Align::Right), ("Entries", Align::Right), ("Last change", Align::Right), ("Name", Align::Left)]);
    for c in &l.items {
        t.row(vec![
            child_size(c, u),
            share(c.allocated_bytes, total),
            format_count(c.entries),
            child_changed(c, now),
            child_name(c.name(), c.is_dir()),
        ]);
    }
    out.push_str(&t.render());
    if l.more_items > 0 {
        let _ = writeln!(out, "  … {} more item(s), {} in total", l.more_items, format_size(l.more_allocated_bytes, u));
    }
    out
}

pub fn diff_browse(r: &DiffBrowseResult, u: SizeUnits) -> String {
    let mut out = String::new();
    let l = &r.listing;
    if let Some(d) = &l.dir {
        let _ = writeln!(
            out,
            "{}: {} → {} ({}), {} changed — {} → {}",
            d.path.lossy(),
            format_size(d.old_allocated, u),
            format_size(d.new_allocated, u),
            format_delta(d.delta_allocated as i128, u),
            entries(d.changed_entries),
            snap(&r.old),
            snap(&r.new)
        );
    }
    out.push('\n');
    let mut t = Table::new(&[
        ("Size change", Align::Right),
        ("Before", Align::Right),
        ("After", Align::Right),
        ("Entries", Align::Right),
        ("What", Align::Left),
        ("Name", Align::Left),
    ]);
    for c in &l.items {
        t.row(vec![
            format_delta(c.delta_allocated as i128, u),
            format_size(c.old_allocated, u),
            format_size(c.new_allocated, u),
            format_count(c.changed_entries),
            diff_change(c).to_string(),
            child_name(c.name(), c.is_dir()),
        ]);
    }
    out.push_str(&t.render());
    if l.more_items > 0 {
        let _ = writeln!(out, "  … {} more changed item(s), {} net", l.more_items, format_delta(l.more_delta_allocated as i128, u));
    }
    if l.unchanged_items > 0 {
        let _ = writeln!(out, "  {} unchanged item(s) not listed", l.unchanged_items);
    }
    out
}
