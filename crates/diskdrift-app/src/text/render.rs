//! Human-readable output for diagnostic commands.

use super::table::{Align, Table, ellipsize_start};
use crate::config::Effective;
use crate::core::units::{SizeUnits, format_delta, format_size};
use crate::diagnostics::doctor::{Confidence, DoctorReport};
use crate::diagnostics::suspects::MeasureStatus;
use crate::diagnostics::usage::{UsageReport, VolumeUsage};
use crate::info::InfoReport;
use std::fmt::Write;

fn opt_size(v: Option<u64>, u: SizeUnits) -> String {
    v.map(|b| format_size(b, u)).unwrap_or_else(|| "-".into())
}

fn volume_rows(t: &mut Table, vols: &[VolumeUsage], u: SizeUnits) {
    for v in vols {
        t.row([
            v.name.clone(),
            v.mount_point.display().to_string(),
            if v.roles.is_empty() { v.fs_type.clone() } else { v.roles.join(",") },
            opt_size(v.in_use_bytes, u),
            format_size(v.available_bytes, u),
            opt_size(v.available_important_bytes, u),
            opt_size(v.purgeable_estimate_bytes, u),
        ]);
    }
}

fn volume_table() -> Table {
    Table::new(&[
        ("Volume", Align::Left),
        ("Mount point", Align::Left),
        ("Role/type", Align::Left),
        ("In use", Align::Right),
        ("Free", Align::Right),
        ("Available", Align::Right),
        ("Purgeable est.", Align::Right),
    ])
}

pub fn usage(r: &UsageReport, u: SizeUnits) -> String {
    let mut out = String::new();
    for c in &r.containers {
        let _ = writeln!(
            out,
            "APFS container {} on {} — {} capacity shared by its volumes, {} free",
            c.reference,
            c.physical_store,
            format_size(c.capacity_bytes, u),
            format_size(c.free_bytes, u)
        );
        let mut t = volume_table();
        volume_rows(&mut t, &c.volumes, u);
        out.push_str(&t.render());
        out.push('\n');
    }
    if !r.other_volumes.is_empty() {
        out.push_str("Other volumes\n");
        let mut t = volume_table();
        volume_rows(&mut t, &r.other_volumes, u);
        out.push_str(&t.render());
        out.push('\n');
    }
    if let Some(e) = &r.apfs_error {
        let _ = writeln!(out, "APFS details unavailable: {e}");
    }
    out.push_str(
        "Free: strictly free blocks (statfs). Available: what macOS offers for important writes (as Finder).\n\
         Purgeable est. = Available − Free: space macOS says it can reclaim on demand (estimate).\n\
         Volumes in one container share its capacity; their sizes are not additive.\n",
    );
    out
}

/// The answer first: how much space the main volume has left, how much of it
/// macOS may free by itself, and what the shared container means.
pub fn usage_summary(r: &UsageReport, u: SizeUnits) -> Vec<String> {
    let Some(v) = r.primary() else { return vec!["No volume found for /.".into()] };
    let avail = v.available_important_bytes.unwrap_or(v.available_bytes);
    let total = r.container_of(v).map(|c| c.capacity_bytes).unwrap_or(v.total_bytes);
    let pct = if total > 0 { avail as f64 * 100.0 / total as f64 } else { 0.0 };
    let low = if crate::diagnostics::usage::is_low(avail, total) { " That is low: under 20 GB or under 10%." } else { "" };
    let mut out = vec![format!("{}: {} available of {} ({pct:.0}%).{low}", v.name, format_size(avail, u), format_size(total, u))];
    match v.purgeable_estimate_bytes.filter(|&b| b > 0) {
        Some(p) => out.push(format!(
            "About {} of that is purgeable: macOS frees it by itself when space runs short (an estimate; strictly free now: {}).",
            format_size(p, u),
            format_size(v.free_bytes, u)
        )),
        None => out.push(format!("Strictly free now: {}; macOS reports nothing purgeable.", format_size(v.free_bytes, u))),
    }
    if let Some(c) = r.container_of(v).filter(|c| c.volumes.len() > 1) {
        out.push(format!("Its {} volumes share one {} container, so they share this free space too.", c.volumes.len(), format_size(c.capacity_bytes, u)));
    }
    out
}

pub fn info(r: &InfoReport, u: SizeUnits) -> String {
    let mut out = String::new();
    let s = &r.system;
    let _ = writeln!(out, "DiskDrift {}", r.version);
    let _ = writeln!(
        out,
        "System:            {} {} · {} · {} CPUs",
        if s.os == "macos" { "macOS" } else { &s.os },
        s.os_version.as_deref().unwrap_or("?"),
        s.model.as_deref().unwrap_or("?"),
        s.cpus
    );
    let fda = match r.access.full_disk_access {
        crate::diagnostics::access::Access::Granted => "granted",
        crate::diagnostics::access::Access::NotGranted => "NOT granted",
        crate::diagnostics::access::Access::Unknown => "unknown",
    };
    let _ = writeln!(out, "Full Disk Access:  {fda}");
    let _ = writeln!(out, "Local snapshots:   {} (see `diskdrift doctor --quick`)", r.local_snapshots);
    settings_block(&mut out, &r.settings, r.store_exists);
    out.push('\n');
    for c in &r.containers {
        let _ = writeln!(
            out,
            "APFS container {} ({}) on {}: {} capacity, {} free",
            c.reference,
            c.uuid,
            c.physical_store,
            format_size(c.capacity_bytes, u),
            format_size(c.free_bytes, u)
        );
        let mut t = Table::new(&[
            ("Device", Align::Left),
            ("Name", Align::Left),
            ("Roles", Align::Left),
            ("In use", Align::Right),
            ("Mounted at", Align::Left),
        ]);
        for v in &c.volumes {
            t.row([
                v.device.clone(),
                v.name.clone(),
                v.roles.join(","),
                format_size(v.in_use_bytes, u),
                v.mount_point.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "-".into()),
            ]);
        }
        out.push_str(&t.render());
    }
    if let Some(e) = &r.apfs_error {
        let _ = writeln!(out, "APFS topology unavailable: {e}");
    }
    out
}

pub fn settings_block(out: &mut String, s: &Effective, store_exists: bool) {
    let _ = writeln!(
        out,
        "Config file:       {} ({}{})",
        s.config_path.value.display(),
        s.config_path.source,
        if s.config_exists { "" } else { ", not present — defaults in use" }
    );
    let _ = writeln!(
        out,
        "Snapshot store:    {} ({}{})",
        s.storage_path.value.display(),
        s.storage_path.source,
        if store_exists { "" } else { ", not created yet" }
    );
}

pub fn config_show(s: &Effective) -> String {
    let mut out = String::new();
    let mut t = Table::new(&[("Setting", Align::Left), ("Value", Align::Left), ("Source", Align::Left)]);
    t.row(["config file".into(), s.config_path.value.display().to_string(), format!("{}{}", s.config_path.source, if s.config_exists { "" } else { " (file not present)" })]);
    t.row(["storage.path".into(), s.storage_path.value.display().to_string(), s.storage_path.source.to_string()]);
    let r = &s.retention.value;
    for (key, n) in [("storage.keepLast", r.keep_last), ("storage.keepWeekly", r.keep_weekly), ("storage.keepMonthly", r.keep_monthly)] {
        t.row([key.into(), if n == 0 { "0 (off)".into() } else { n.to_string() }, s.retention.source.to_string()]);
    }
    t.row(["scan.defaultRoot".into(), s.default_root.value.display().to_string(), s.default_root.source.to_string()]);
    t.row([
        "scan.threads".into(),
        if s.threads.value == 0 { "0 (automatic)".into() } else { s.threads.value.to_string() },
        s.threads.source.to_string(),
    ]);
    t.row(["scan.stayOnFilesystem".into(), s.stay_on_filesystem.value.to_string(), s.stay_on_filesystem.source.to_string()]);
    t.row(["scan.followSymlinks".into(), "false".into(), "default (only supported value)".into()]);
    t.row([
        "scan.exclude".into(),
        if s.exclude.value.is_empty() { "[]".into() } else { s.exclude.value.join(", ") },
        s.exclude.source.to_string(),
    ]);
    t.row(["scan.autoOnStart".into(), s.auto_scan.value.to_string(), s.auto_scan.source.to_string()]);
    t.row(["scan.autoMaxAgeHours".into(), s.auto_max_age_hours.value.to_string(), s.auto_max_age_hours.source.to_string()]);
    t.row(["ui.confirmBeforeScan".into(), s.confirm_before_scan.value.to_string(), s.confirm_before_scan.source.to_string()]);
    t.row([
        "output.sizeUnits".into(),
        format!("{:?}", s.size_units.value).to_lowercase(),
        s.size_units.source.to_string(),
    ]);
    out.push_str(&t.render());
    out
}

fn status_label(s: MeasureStatus) -> &'static str {
    match s {
        MeasureStatus::Absent => "absent",
        MeasureStatus::Inaccessible => "inaccessible",
        MeasureStatus::Complete => "complete",
        MeasureStatus::Partial => "partial",
        MeasureStatus::TimedOut => "time limit",
        MeasureStatus::Skipped => "skipped",
    }
}

pub fn doctor(r: &DoctorReport, u: SizeUnits) -> String {
    doctor_report(r, u, false)
}

/// The doctor's report with its conclusion first, for a screen read top-down.
pub fn doctor_summary_first(r: &DoctorReport, u: SizeUnits) -> String {
    doctor_report(r, u, true)
}

fn doctor_report(r: &DoctorReport, u: SizeUnits, summary_first: bool) -> String {
    let mut out = String::new();
    if summary_first {
        out.push_str("In short:\n");
        for c in &r.conclusion {
            let _ = writeln!(out, "  • {c}");
        }
        out.push_str("  Tab shows the recovery suggestions, ranked, where each location can be opened.\n\nDetails\n");
    } else {
        out.push_str("DiskDrift Doctor — quick diagnosis\n");
    }
    if let Some(v) = r.usage.primary() {
        let avail = v.available_important_bytes.unwrap_or(v.available_bytes);
        let _ = writeln!(out, "Available now:      {} on {} ({})", format_size(avail, u), v.name, v.mount_point.display());
        let _ = writeln!(out, "Strictly free:      {}", format_size(v.free_bytes, u));
        if let Some(p) = v.purgeable_estimate_bytes {
            let _ = writeln!(out, "Purgeable estimate: {} (OS estimate, reclaimable on demand)", format_size(p, u));
        }
        if let Some(c) = r.usage.container_of(v) {
            let _ = writeln!(
                out,
                "Container:          {} — {} shared capacity, {} free",
                c.reference,
                format_size(c.capacity_bytes, u),
                format_size(c.free_bytes, u)
            );
        }
    }
    if let Some(reference) = r.reference_free_bytes {
        let _ = writeln!(out, "Reference supplied: {} free earlier (not verified)", format_size(reference, u));
        if let Some(gap) = r.unexplained_gap_bytes {
            let _ = writeln!(out, "Change since then:  {}", format_delta(-gap, u));
        }
    }

    out.push_str("\nSystem checks:\n");
    let mut t = Table::new(&[("Check", Align::Left), ("Status", Align::Left), ("Detail", Align::Left)]);
    for c in &r.checks {
        t.row([c.name.clone(), c.status.clone(), c.detail.clone()]);
    }
    out.push_str(&t.render());

    if !r.snapshots.snapshots.is_empty() {
        out.push_str("\nLocal APFS snapshots (sizes are not reported by APFS; reclaimable: unknown):\n");
        let mut t = Table::new(&[
            ("Volume", Align::Left),
            ("Snapshot", Align::Left),
            ("Created", Align::Left),
            ("Notes", Align::Left),
        ]);
        for s in &r.snapshots.snapshots {
            let mut notes = Vec::new();
            if s.purgeable {
                notes.push("purgeable");
            }
            if s.limiting_container_shrink {
                notes.push("limits shrink");
            }
            if s.dataless {
                notes.push("dataless");
            }
            t.row([
                s.volume_name.clone(),
                ellipsize_start(&s.name, 60),
                s.created.clone().unwrap_or_else(|| "-".into()),
                notes.join(", "),
            ]);
        }
        out.push_str(&t.render());
    }
    for e in &r.snapshots.errors {
        let _ = writeln!(out, "  snapshot listing failed: {e}");
    }

    out.push_str("\nLarge storage locations (allocated size; indented rows are already included in the row above them):\n");
    if let Some(src) = &r.sizes_from {
        let _ = writeln!(out, "Sizes from {src}, not measured now; locations outside that scan were measured now.");
    }
    let mut t = Table::new(&[
        ("Location", Align::Left),
        ("Allocated", Align::Right),
        ("Logical", Align::Right),
        ("Coverage", Align::Left),
        ("Risk", Align::Left),
        ("Path", Align::Left),
    ]);
    let tops: Vec<_> = r.findings.iter().filter(|f| f.within.is_none()).collect();
    for f in tops {
        let row = |f: &crate::diagnostics::doctor::Finding, indent: &str| {
            let size = |b: u64| match f.confidence {
                Confidence::Unknown => "unknown".to_string(),
                Confidence::LowerBound => format!("≥{}", format_size(b, u)),
                Confidence::Observed => format_size(b, u),
            };
            [
                format!("{indent}{}", f.label),
                size(f.allocated_bytes),
                size(f.logical_bytes),
                status_label(f.status).to_string(),
                f.risk.label().to_string(),
                ellipsize_start(&f.path.display().to_string(), 60),
            ]
        };
        t.row(row(f, ""));
        for c in r.findings.iter().filter(|c| c.within.as_deref() == Some(f.id.as_str())) {
            t.row(row(c, "  └ "));
        }
    }
    out.push_str(&t.render());

    if !r.largest_files.is_empty() {
        out.push_str("\nLargest files seen in these locations:\n");
        let mut t = Table::new(&[("Allocated", Align::Right), ("Logical", Align::Right), ("Path", Align::Left)]);
        for f in r.largest_files.iter().take(10) {
            t.row([format_size(f.allocated_bytes, u), format_size(f.logical_bytes, u), f.path.clone()]);
        }
        out.push_str(&t.render());
    }

    let od = &r.open_deleted;
    if od.supported {
        let _ = writeln!(
            out,
            "\nDeleted but still open files: {} in {} file(s) ({})",
            format_size(od.total_bytes, u),
            od.distinct_files,
            od.scope
        );
        for f in od.files.iter().take(5).filter(|f| f.size_bytes > 0) {
            let _ = writeln!(out, "  {:>10}  {} (pid {}) {}", format_size(f.size_bytes, u), f.command, f.pid, f.path);
        }
    } else {
        let _ = writeln!(
            out,
            "\nDeleted but still open files: unknown ({})",
            od.error.as_deref().unwrap_or("not supported")
        );
    }

    out.push_str("\nRecommendations are for manual inspection; risk = how costly removing data there can be.\n");
    for f in r.findings.iter().filter(|f| f.within.is_none() && f.allocated_bytes >= 1_000_000_000).take(5) {
        let _ = writeln!(out, "  {}: {}", f.label, f.recommendation);
    }

    if !summary_first {
        out.push_str("\nConclusion:\n");
        for c in &r.conclusion {
            let _ = writeln!(out, "  {c}");
        }
    }
    let _ = writeln!(out, "No files were changed. ({:.1}s)", r.elapsed_ms as f64 / 1000.0);
    out
}
