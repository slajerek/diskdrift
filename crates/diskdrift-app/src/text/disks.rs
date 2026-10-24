//! Text output for `diskdrift disks`.

use super::table::{Align, Table};
use crate::core::units::{SizeUnits, format_size};
use crate::disks::{DiskList, Forget};
use crate::storage::{Disk, Strength};
use std::fmt::Write;

fn strength(s: Strength) -> &'static str {
    match s {
        Strength::Uuid => "uuid",
        Strength::Serial => "serial",
        Strength::Weak => "weak",
    }
}

/// When a scan started, counted back from `now` ("3 days ago").
fn ago(rfc: &str, now: i64) -> String {
    match chrono::DateTime::parse_from_rfc3339(rfc) {
        Ok(t) => crate::analyze::ago(now - t.timestamp()),
        Err(_) => rfc.into(),
    }
}

pub fn disk_list(l: &DiskList, u: SizeUnits, now: i64) -> String {
    if l.disks.is_empty() {
        return format!("No disks in {} yet. A scan records the disk it was taken on.\n", l.store_path.display());
    }
    let mut t = Table::new(&[
        ("Disk", Align::Left),
        ("ID", Align::Left),
        ("Volume", Align::Left),
        ("FS", Align::Left),
        ("Capacity", Align::Right),
        ("Connected", Align::Left),
        ("Last scan", Align::Left),
        ("Scans", Align::Right),
        ("Stored", Align::Right),
        ("Sets", Align::Left),
        ("Identity", Align::Left),
    ]);
    for r in &l.disks {
        let d = &r.disk;
        t.row([
            d.display_name().to_string(),
            d.id.clone(),
            d.volume.name.clone().unwrap_or_else(|| "-".into()),
            d.volume.fs_type.clone(),
            d.volume.capacity_bytes.map(|b| format_size(b, u)).unwrap_or_else(|| "-".into()),
            r.mount_point.as_ref().map(|p| format!("yes, {}", p.display())).unwrap_or_else(|| "no".into()),
            match (&r.last_scan, r.known) {
                (_, false) => "new, not scanned".into(),
                (Some(s), true) => ago(s, now),
                (None, true) => "never".into(),
            },
            r.snapshots.to_string(),
            format_size(r.store_bytes, u),
            if d.sets.is_empty() { "-".into() } else { d.sets.join(", ") },
            strength(d.identity.strength).to_string(),
        ]);
    }

    let mut out = t.render();
    for r in &l.disks {
        if let Some(desc) = &r.disk.description {
            let _ = writeln!(out, "  {}: {desc}", r.disk.display_name());
        }
    }
    if l.disks.iter().any(|r| r.disk.identity.strength == Strength::Weak) {
        out.push_str(
            "  weak: the volume reports no UUID (or an unreliable one), so it is recognised by its filesystem, name and size or mount point; \
             DiskDrift asks before treating a match as the same disk. Two entries for one disk can be joined with `disks merge`.\n",
        );
    }
    if l.disks.iter().any(|r| !r.known) {
        out.push_str("  new: connected, not in the store yet; DiskDrift never scans a disk on its own the first time. `disks label <mount point> <label>` records it, `scan <mount point>` scans it.\n");
    }
    let _ = writeln!(out, "Store: {}", l.store_path.display());
    out
}

/// One disk's entry after an edit.
pub fn disk_entry(d: &Disk) -> String {
    let mut out = format!("Disk {}", d.id);
    if let Some(v) = &d.volume.name {
        let _ = write!(out, " ({v})");
    }
    match &d.label {
        Some(l) => {
            let _ = write!(out, ": label {l}");
        }
        None => out.push_str(": no label"),
    }
    if let Some(desc) = &d.description {
        let _ = write!(out, ", \"{desc}\"");
    }
    if !d.sets.is_empty() {
        let _ = write!(out, ", sets: {}", d.sets.join(", "));
    }
    out.push('\n');
    out
}

pub fn forget(f: &Forget, u: SizeUnits) -> String {
    let n = f.snapshots.len();
    let scans = if n == 1 { "1 snapshot".to_string() } else { format!("{n} snapshots") };
    if f.done {
        format!(
            "Forgot disk {} ({}): removed {scans}, {} freed. Scanned files are never touched.\n",
            f.disk.display_name(),
            f.disk.id,
            format_size(f.bytes, u)
        )
    } else {
        format!(
            "Forgetting disk {} ({}) removes its {scans} ({} in the store) and its label, description and sets. \
             Files on the disk are never touched.\n",
            f.disk.display_name(),
            f.disk.id,
            format_size(f.bytes, u)
        )
    }
}
