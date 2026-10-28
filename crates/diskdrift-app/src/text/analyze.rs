//! Text output for `analyze`, plus the per-item labels the TUI shows too.

use super::table::{Align, Table, ellipsize_start};
use super::{changed_ago, entries, verdict_label};
use crate::analyze::{AnalyzeItem, DirAnalysis};
use crate::core::units::{SizeUnits, format_size};
use crate::diagnostics::classify::verdict;
use crate::diagnostics::suspects::MeasureStatus;
use std::fmt::Write;

/// Size column: "12.3 GB", "≥ 4.1 GB" for lower bounds, "?" when unknown.
/// A verdict total's name as shown ("unknown" reads as "not recognised").
pub fn shown_verdict(v: &str) -> &str {
    if v == "unknown" { "not recognised" } else { v }
}

pub fn item_size(i: &AnalyzeItem, u: SizeUnits) -> String {
    match i.status {
        MeasureStatus::Complete => format_size(i.allocated_bytes, u),
        MeasureStatus::Partial | MeasureStatus::TimedOut => format!("≥ {}", format_size(i.allocated_bytes, u)),
        MeasureStatus::Inaccessible => "unreadable".into(),
        MeasureStatus::Absent | MeasureStatus::Skipped => "not measured".into(),
    }
}

/// How long ago anything inside changed.
pub fn item_changed(i: &AnalyzeItem, analyzed_at: i64) -> String {
    match i.newest_mtime {
        Some(t) => changed_ago(t, analyzed_at),
        None => "-".into(),
    }
}

/// "Slack", "no installed app", or empty.
pub fn item_owner(i: &AnalyzeItem) -> String {
    match (&i.assessment.owner, i.assessment.no_installed_owner) {
        (Some(o), _) => o.clone(),
        (None, true) => "no installed app".into(),
        _ => String::new(),
    }
}

/// Whether the Owner column says anything: only in app-data folders, where it
/// differs per item. Elsewhere the path above already says whose data it is.
pub fn has_owners(r: &DirAnalysis) -> bool {
    r.items.iter().any(|i| !item_owner(i).is_empty())
}

/// One line explaining an item: what it is, whose, and what to do.
pub fn item_detail(i: &AnalyzeItem, analyzed_at: i64) -> Vec<String> {
    let a = &i.assessment;
    let mut out = Vec::new();
    if a.what.is_empty() {
        out.push(format!("{} — DiskDrift has no notes on this name.", i.name));
    } else {
        out.push(format!("{} — {} [{}]", i.name, a.what, verdict(a.risk)));
    }
    if let Some(o) = &a.owner {
        out.push(format!("Belongs to {o} (installed)."));
    } else if a.no_installed_owner {
        out.push(
            "No installed app matches this folder: it may be left over from an uninstalled app, or belong to a plug-in or command-line tool."
                .into(),
        );
    }
    if let Some(t) = i.newest_mtime {
        out.push(format!("Last change inside: {} · {}.", changed_ago(t, analyzed_at), entries(i.entries)));
        if a.no_installed_owner && analyzed_at - t > 365 * 86_400 {
            out.push("Unchanged for over a year and no matching app installed: a likely leftover.".into());
        }
    }
    if let Some(adv) = &a.advice {
        out.push(adv.clone());
    }
    out
}

pub fn analyze(r: &DirAnalysis, u: SizeUnits) -> String {
    let mut out = String::new();
    let status = match r.status {
        MeasureStatus::Complete => "complete",
        MeasureStatus::Partial => "partial (unreadable folders)",
        MeasureStatus::TimedOut => "time budget ran out (lower bounds)",
        _ => "incomplete",
    };
    let _ = writeln!(
        out,
        "{}: {} allocated in {} entries — {} · {:.1}s",
        r.path,
        format_size(r.allocated_bytes, u),
        r.entries,
        status,
        r.elapsed_ms as f64 / 1000.0
    );
    if let Some(c) = &r.context {
        let _ = writeln!(out, "Inside: {} [{}] — {}", c.what, verdict(Some(c.risk)), c.from);
    }
    out.push_str("By verdict: ");
    let parts: Vec<String> =
        r.by_verdict.iter().map(|v| format!("{} {} ({})", shown_verdict(&v.verdict), format_size(v.allocated_bytes, u), v.items)).collect();
    out.push_str(&parts.join(" · "));
    out.push_str("\n\n");
    let owners = has_owners(r);
    let mut cols = vec![("On disk", Align::Right), ("Last change", Align::Right), ("Verdict", Align::Left), ("What", Align::Left)];
    if owners {
        cols.push(("Owner", Align::Left));
    }
    cols.push(("Name", Align::Left));
    let mut t = Table::new(&cols);
    for i in &r.items {
        let mut row = vec![
            item_size(i, u),
            item_changed(i, r.analyzed_at),
            verdict_label(i.assessment.risk).to_string(),
            ellipsize_start(&i.assessment.what, 40),
        ];
        if owners {
            row.push(ellipsize_start(&item_owner(i), 24));
        }
        row.push(format!("{}{}", i.name, if i.is_dir { "/" } else { "" }));
        t.row(row);
    }
    out.push_str(&t.render());
    if r.more_items > 0 {
        let _ = writeln!(out, "  … {} more item(s), {} in total", r.more_items, format_size(r.more_allocated_bytes, u));
    }
    out.push('\n');
    for i in r.items.iter().filter(|i| i.assessment.advice.is_some()).take(10) {
        let mut lines = item_detail(i, r.analyzed_at).into_iter();
        if let Some(first) = lines.next() {
            let _ = writeln!(out, "• {first}");
        }
        for l in lines {
            let _ = writeln!(out, "    {l}");
        }
    }
    out.push('\n');
    for n in &r.notes {
        let _ = writeln!(out, "Note: {n}");
    }
    out
}
