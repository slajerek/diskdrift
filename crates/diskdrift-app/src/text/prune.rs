//! Text for removing many snapshots and compacting the store, shared by the CLI and the TUI.

use super::table::{Align, Table};
use crate::core::units::{SizeUnits, format_size};
use crate::prune::{Proposal, Removed};
use crate::storage::{CompactStep, Compaction};

fn scans(n: usize) -> String {
    if n == 1 { "1 scan".into() } else { format!("{n} scans") }
}

fn short_time(rfc: &str) -> String {
    rfc.get(..16).map(|s| s.replace('T', " ")).unwrap_or_else(|| rfc.to_string())
}

/// One line: how much it frees and what else happens.
pub fn proposal_summary(p: &Proposal, u: SizeUnits) -> String {
    let mut s = format!("Removing {} frees about {} in the store", scans(p.snapshots.len()), format_size(p.bytes, u));
    match p.rewritten {
        0 => s.push('.'),
        1 => s.push_str("; 1 kept scan is rewritten so it stays readable, which can take some of it back."),
        n => s.push_str(&format!("; {n} kept scans are rewritten so they stay readable, which can take some of it back.")),
    }
    s.push_str(" Scanned files are never touched.");
    s
}

/// The scans a selection would remove, and what removing them does.
pub fn proposal(p: &Proposal, u: SizeUnits) -> String {
    if p.snapshots.is_empty() {
        return format!("Nothing to remove ({} kept).\n", scans(p.kept));
    }
    let with_disk = {
        let mut d: Vec<_> = p.snapshots.iter().map(|r| r.disk_name.as_deref()).collect();
        d.dedup();
        d.len() > 1
    };
    let mut cols = vec![("ID", Align::Right), ("Name", Align::Left), ("Started", Align::Left)];
    if with_disk {
        cols.push(("Disk", Align::Left));
    }
    cols.extend([("Root", Align::Left), ("Status", Align::Left), ("Stored", Align::Right)]);
    let mut t = Table::new(&cols);
    for r in &p.snapshots {
        let m = &r.meta;
        let mut row = vec![m.id.to_string(), m.name.clone(), short_time(&m.started_at)];
        if with_disk {
            row.push(r.disk_name.clone().unwrap_or_else(|| "-".into()));
        }
        row.extend([m.root.clone(), m.status.to_string(), format_size(r.size_on_disk, u)]);
        t.row(row);
    }
    format!("{}{} {} kept.\n", t.render(), proposal_summary(p, u), scans(p.kept))
}

pub fn removed(r: &Removed, u: SizeUnits) -> String {
    if r.cancelled {
        return "Stopped: nothing was removed (scans rewritten so far stay valid).\n".into();
    }
    let mut s = format!("Removed {}, {} freed.", scans(r.removed.len()), format_size(r.freed_bytes, u));
    match r.rewritten.len() {
        0 => {}
        1 => s.push_str(" 1 kept scan was rewritten so it stays readable."),
        n => s.push_str(&format!(" {n} kept scans were rewritten so they stay readable.")),
    }
    if r.uncompacted > 0 {
        s.push_str(&format!(" Compacting the store (`diskdrift snapshots compact`) can store {} tighter.", scans(r.uncompacted)));
    }
    s.push_str(" Scanned files are never touched.\n");
    s
}

pub fn compact_step(s: &CompactStep) -> String {
    match s {
        CompactStep::Rewriting { id, done, total } => format!("rewriting snapshot {id} ({}/{total})", done + 1),
        CompactStep::Checking { id, done, total } => format!("checking snapshot {id} ({}/{total})", done + 1),
    }
}

pub fn compaction(c: &Compaction, u: SizeUnits) -> String {
    let size = |b| format_size(b, u);
    let mut s = String::new();
    if c.cancelled {
        s.push_str("Stopped: the store is valid as it is. ");
    }
    if c.rewritten.is_empty() && !c.cancelled {
        s.push_str("The store was already compact: every snapshot is stored as writing the scans one by one would store it. ");
    } else {
        s.push_str(&format!("Rewrote {}. ", scans(c.rewritten.len())));
    }
    match c.bytes_before.cmp(&c.bytes_after) {
        std::cmp::Ordering::Greater => s.push_str(&format!("Store {} → {} ({} freed). ", size(c.bytes_before), size(c.bytes_after), size(c.bytes_before - c.bytes_after))),
        std::cmp::Ordering::Less => s.push_str(&format!(
            "Store {} → {} ({} more: full copies replace deltas that were too long to read quickly). ",
            size(c.bytes_before),
            size(c.bytes_after),
            size(c.bytes_after - c.bytes_before)
        )),
        std::cmp::Ordering::Equal => s.push_str(&format!("Store {}. ", size(c.bytes_after))),
    }
    if !c.cancelled {
        s.push_str(&format!("Checksums of all {} valid.", scans(c.checked)));
    }
    s.push('\n');
    s
}
