//! Text output for `doctor --deep` and `recover`.

use super::table::{Align, Table, ellipsize_start};
use crate::core::units::{SizeUnits, format_size};
use crate::investigate::{DeepReport, RecoverReport};
use std::fmt::Write;

pub fn deep(r: &DeepReport, u: SizeUnits) -> String {
    let mut out = super::render::doctor(&r.quick, u);
    out.push_str("\n══ Deep analysis (saved snapshots) ══\n");
    match (&r.latest, &r.latest_top) {
        (Some(m), Some(t)) => {
            let _ = writeln!(out, "Latest inventory: snapshot {} '{}' of {} ({}, {} entries)", m.id, m.name, m.root, m.status, t.entries);
            let mut tab = Table::new(&[("Allocated", Align::Right), ("Logical", Align::Right), ("Directory", Align::Left)]);
            for d in t.dirs.iter().take(15) {
                tab.row([format_size(d.allocated_bytes, u), format_size(d.logical_bytes, u), ellipsize_start(&d.path.lossy(), 100)]);
            }
            out.push_str(&tab.render());
        }
        _ => out.push_str("No saved snapshot yet.\n"),
    }
    if let Some(d) = &r.change {
        out.push('\n');
        out.push_str(&super::snapshots::diff(d, u, false, true));
    }
    out.push_str("\nPossible reasons for space the inventory does not explain:\n");
    for c in &r.unexplained_candidates {
        let _ = writeln!(out, "  - {c}");
    }
    out.push_str("\nDeep conclusion:\n");
    for c in &r.conclusion {
        let _ = writeln!(out, "  {c}");
    }
    out
}

pub fn recover(r: &RecoverReport, _u: SizeUnits) -> String {
    let mut out = String::from("Recovery suggestions (read-only; nothing is deleted)\n\n");
    if r.candidates.is_empty() {
        out.push_str("No large candidates found.\n");
    }
    for c in &r.candidates {
        let _ = writeln!(out, "{:>2}. {}  — reclaimable: {}  · risk: {} · {}", c.rank, c.label, c.reclaimable, c.risk, c.confidence);
        if let Some(p) = &c.path {
            let _ = writeln!(out, "    {p}");
        }
        if let Some(w) = &c.within {
            let _ = writeln!(out, "    (inside {w}: counted there too)");
        }
        let _ = writeln!(out, "    → {}", c.how);
    }
    out.push('\n');
    for n in &r.notes {
        let _ = writeln!(out, "Note: {n}");
    }
    out
}
