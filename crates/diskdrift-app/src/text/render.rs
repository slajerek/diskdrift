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

fn measured_size(
    v: Option<u64>,
    quality: Option<crate::diagnostics::measurement::Quality>,
    u: SizeUnits,
) -> String {
    match (quality, v) {
        (Some(crate::diagnostics::measurement::Quality::Observed), Some(b)) => format_size(b, u),
        (Some(crate::diagnostics::measurement::Quality::Estimated), Some(b)) => {
            format!("~{}", format_size(b, u))
        }
        (Some(crate::diagnostics::measurement::Quality::LowerBound), Some(b)) => {
            format!(">={}", format_size(b, u))
        }
        _ => "unknown".into(),
    }
}

fn is_apfs_volume(v: &VolumeUsage) -> bool {
    v.fs_type.eq_ignore_ascii_case("apfs")
}

fn has_macos_capacity(v: &VolumeUsage) -> bool {
    is_apfs_volume(v)
        || v.available_important_bytes.is_some()
        || v.purgeable_estimate_bytes.is_some()
}

pub(crate) fn usage_has_macos_terms(r: &UsageReport) -> bool {
    !r.containers.is_empty() || r.other_volumes.iter().any(has_macos_capacity)
}

pub(crate) fn doctor_has_macos_terms(r: &DoctorReport) -> bool {
    usage_has_macos_terms(&r.usage)
        || !r.snapshots.snapshots.is_empty()
        || r.access.full_disk_access != crate::diagnostics::access::Access::Unknown
}

fn volume_rows(t: &mut Table, vols: &[VolumeUsage], u: SizeUnits) {
    for v in vols {
        let size = |value| measured_size(value, v.measurement.quality, u);
        t.row([
            v.name.clone(),
            v.mount_point.display().to_string(),
            if v.roles.is_empty() {
                v.fs_type.clone()
            } else {
                v.roles.join(",")
            },
            size(v.in_use_bytes),
            size(v.available_bytes),
            size(v.available_important_bytes),
            measured_size(
                v.purgeable_estimate_bytes,
                v.reclaimable_measurement.quality,
                u,
            ),
        ]);
    }
}

fn volume_table(macos_terms: bool) -> Table {
    Table::new(&[
        ("Volume", Align::Left),
        ("Mount point", Align::Left),
        ("Role/type", Align::Left),
        ("In use", Align::Right),
        ("Free", Align::Right),
        ("Available", Align::Right),
        (
            if macos_terms {
                "Purgeable est."
            } else {
                "Reclaimable est."
            },
            Align::Right,
        ),
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
            measured_size(c.capacity_bytes, c.measurement.quality, u),
            measured_size(c.free_bytes, c.measurement.quality, u)
        );
        let mut t = volume_table(true);
        volume_rows(&mut t, &c.volumes, u);
        out.push_str(&t.render());
        out.push('\n');
    }
    if !r.other_volumes.is_empty() {
        out.push_str("Other volumes\n");
        let macos_terms = r.other_volumes.iter().any(has_macos_capacity);
        let mut t = volume_table(macos_terms);
        volume_rows(&mut t, &r.other_volumes, u);
        out.push_str(&t.render());
        out.push('\n');
    }
    if let Some(e) = &r.apfs_error {
        let label = if r.containers.is_empty() && !r.other_volumes.iter().any(is_apfs_volume) {
            "Volume details unavailable"
        } else {
            "APFS details unavailable"
        };
        let _ = writeln!(out, "{label}: {e}");
    }
    if let Some(e) = &r.mounts_error {
        let _ = writeln!(out, "Some mounted volumes unavailable: {e}");
    }
    let macos_terms = usage_has_macos_terms(r);
    let available = if macos_terms {
        "what macOS offers for important writes (as Finder)"
    } else {
        "space available to the caller"
    };
    let reclaimable = if macos_terms {
        "space macOS says it can reclaim on demand (estimate)"
    } else {
        "an OS-reported estimate of space that may be reclaimable"
    };
    out.push_str(&format!(
        "Free: strictly free blocks (statfs). Available: {available}.\n\
             Reclaimable est. = Available − Free: {reclaimable}.\n\
             Volumes in one container share its capacity; their sizes are not additive.\n"
    ));
    out
}

/// The answer first: how much space the main volume has left, any OS-reported
/// reclaimable estimate, and what the shared container means.
pub fn usage_summary(r: &UsageReport, u: SizeUnits) -> Vec<String> {
    let Some(v) = r.primary() else {
        return vec!["No primary volume found.".into()];
    };
    let Some(avail) = v.available_important_bytes.or(v.available_bytes) else {
        return vec![format!(
            "{}: available space is unknown ({})",
            v.name,
            v.mount_point.display()
        )];
    };
    let total = r
        .container_of(v)
        .map(|c| c.capacity_bytes)
        .unwrap_or(v.total_bytes);
    let macos_terms = r.container_of(v).is_some() || has_macos_capacity(v);
    let mut out = vec![match total {
        Some(total) if total > 0 => {
            let pct = avail as f64 * 100.0 / total as f64;
            let low = if crate::diagnostics::usage::is_low(avail, total) {
                " That is low: under 20 GB or under 10%."
            } else {
                ""
            };
            format!(
                "{}: {} available of {} ({pct:.0}%).{low}",
                v.name,
                measured_size(Some(avail), v.measurement.quality, u),
                format_size(total, u)
            )
        }
        _ => format!(
            "{}: {} available; total capacity is unknown.",
            v.name,
            measured_size(Some(avail), v.measurement.quality, u)
        ),
    }];
    match v.purgeable_estimate_bytes.filter(|&b| b > 0) {
        Some(p) if macos_terms => out.push(format!(
            "About {} of that is purgeable: macOS frees it by itself when space runs short (an estimate; strictly free now: {}).",
            measured_size(
                Some(p),
                v.reclaimable_measurement.quality,
                u,
            ),
            measured_size(v.free_bytes, v.measurement.quality, u)
        )),
        Some(p) => out.push(format!(
            "About {} of that is estimated reclaimable by the operating system (strictly free now: {}).",
            measured_size(
                Some(p),
                v.reclaimable_measurement.quality,
                u,
            ),
            measured_size(v.free_bytes, v.measurement.quality, u)
        )),
        None if macos_terms => out.push(format!(
            "Strictly free now: {}; macOS reports nothing purgeable.",
            measured_size(v.free_bytes, v.measurement.quality, u)
        )),
        None => out.push(format!(
            "Reclaimable estimate: unavailable; strictly free now: {}.",
            measured_size(v.free_bytes, v.measurement.quality, u)
        )),
    }
    if let Some(c) = r.container_of(v).filter(|c| c.volumes.len() > 1) {
        out.push(format!(
            "Its {} volumes share one {} container, so they share this free space too.",
            c.volumes.len(),
            measured_size(c.capacity_bytes, c.measurement.quality, u)
        ));
    }
    out
}

pub fn info(r: &InfoReport, u: SizeUnits) -> String {
    let mut out = String::new();
    let s = &r.system;
    let macos_terms = s.os == "macos";
    let apfs_terms = macos_terms || !r.containers.is_empty();
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
    if macos_terms {
        let _ = writeln!(out, "Full Disk Access:  {fda}");
    }
    if apfs_terms || r.local_snapshots > 0 {
        let _ = writeln!(
            out,
            "Local snapshots:   {} (see `diskdrift doctor --quick`)",
            r.local_snapshots
        );
    }
    settings_block(&mut out, &r.settings, r.store_exists);
    out.push('\n');
    for c in &r.containers {
        let _ = writeln!(
            out,
            "APFS container {} ({}) on {}: {} capacity, {} free",
            c.reference,
            c.uuid,
            c.physical_store,
            opt_size(c.capacity_bytes, u),
            opt_size(c.free_bytes, u)
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
                opt_size(v.in_use_bytes, u),
                v.mount_point
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "-".into()),
            ]);
        }
        out.push_str(&t.render());
    }
    if let Some(e) = &r.apfs_error {
        let label = if apfs_terms {
            "APFS topology unavailable"
        } else {
            "Volume topology unavailable"
        };
        let _ = writeln!(out, "{label}: {e}");
    }
    if let Some(e) = &r.mounts_error {
        let _ = writeln!(out, "Mounted-volume discovery incomplete: {e}");
    }
    out
}

pub fn settings_block(out: &mut String, s: &Effective, store_exists: bool) {
    let _ = writeln!(
        out,
        "Config file:       {} ({}{})",
        s.config_path.value.display(),
        s.config_path.source,
        if s.config_exists {
            ""
        } else {
            ", not present — defaults in use"
        }
    );
    let _ = writeln!(
        out,
        "Snapshot store:    {} ({}{})",
        s.storage_path.value.display(),
        s.storage_path.source,
        if store_exists {
            ""
        } else {
            ", not created yet"
        }
    );
}

pub fn config_show(s: &Effective) -> String {
    let mut out = String::new();
    let mut t = Table::new(&[
        ("Setting", Align::Left),
        ("Value", Align::Left),
        ("Source", Align::Left),
    ]);
    t.row([
        "config file".into(),
        s.config_path.value.display().to_string(),
        format!(
            "{}{}",
            s.config_path.source,
            if s.config_exists {
                ""
            } else {
                " (file not present)"
            }
        ),
    ]);
    t.row([
        "storage.path".into(),
        s.storage_path.value.display().to_string(),
        s.storage_path.source.to_string(),
    ]);
    let r = &s.retention.value;
    for (key, n) in [
        ("storage.keepLast", r.keep_last),
        ("storage.keepWeekly", r.keep_weekly),
        ("storage.keepMonthly", r.keep_monthly),
    ] {
        t.row([
            key.into(),
            if n == 0 {
                "0 (off)".into()
            } else {
                n.to_string()
            },
            s.retention.source.to_string(),
        ]);
    }
    t.row([
        "scan.defaultRoot".into(),
        s.default_root.value.display().to_string(),
        s.default_root.source.to_string(),
    ]);
    t.row([
        "scan.threads".into(),
        if s.threads.value == 0 {
            "0 (automatic)".into()
        } else {
            s.threads.value.to_string()
        },
        s.threads.source.to_string(),
    ]);
    t.row([
        "scan.stayOnFilesystem".into(),
        s.stay_on_filesystem.value.to_string(),
        s.stay_on_filesystem.source.to_string(),
    ]);
    t.row([
        "scan.followSymlinks".into(),
        "false".into(),
        "default (only supported value)".into(),
    ]);
    t.row([
        "scan.exclude".into(),
        if s.exclude.value.is_empty() {
            "[]".into()
        } else {
            s.exclude.value.join(", ")
        },
        s.exclude.source.to_string(),
    ]);
    t.row([
        "scan.autoOnStart".into(),
        s.auto_scan.value.to_string(),
        s.auto_scan.source.to_string(),
    ]);
    t.row([
        "scan.autoMaxAgeHours".into(),
        s.auto_max_age_hours.value.to_string(),
        s.auto_max_age_hours.source.to_string(),
    ]);
    t.row([
        "ui.confirmBeforeScan".into(),
        s.confirm_before_scan.value.to_string(),
        s.confirm_before_scan.source.to_string(),
    ]);
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
    let macos_terms = doctor_has_macos_terms(r);
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
        if let Some(avail) = v.available_important_bytes.or(v.available_bytes) {
            let _ = writeln!(
                out,
                "Available now:      {} on {} ({})",
                format_size(avail, u),
                v.name,
                v.mount_point.display()
            );
            let _ = writeln!(out, "Strictly free:      {}", opt_size(v.free_bytes, u));
            if let Some(p) = v.purgeable_estimate_bytes {
                let _ = writeln!(
                    out,
                    "{} estimate: {} (OS estimate, reclaimable on demand)",
                    if macos_terms {
                        "Purgeable"
                    } else {
                        "Reclaimable"
                    },
                    measured_size(Some(p), v.reclaimable_measurement.quality, u)
                );
            }
            if let Some(c) = r.usage.container_of(v) {
                let _ = writeln!(
                    out,
                    "Container:          {} — {} shared capacity, {} free",
                    c.reference,
                    measured_size(c.capacity_bytes, c.measurement.quality, u),
                    measured_size(c.free_bytes, c.measurement.quality, u)
                );
            }
        } else {
            let _ = writeln!(
                out,
                "Available now:      unknown on {} ({})",
                v.name,
                v.mount_point.display()
            );
        }
    }
    if let Some(reference) = r.reference_free_bytes
        && matches!(
            r.reference_free_measurement.quality,
            Some(
                crate::diagnostics::measurement::Quality::Observed
                    | crate::diagnostics::measurement::Quality::Estimated
                    | crate::diagnostics::measurement::Quality::LowerBound
            )
        )
    {
        let _ = writeln!(
            out,
            "Reference supplied: {} free earlier (not verified)",
            format_size(reference, u)
        );
        if let Some(gap) = r.unexplained_gap_bytes {
            let value = match r.unexplained_gap_measurement.quality {
                Some(crate::diagnostics::measurement::Quality::Observed) => format_delta(-gap, u),
                Some(crate::diagnostics::measurement::Quality::Estimated) => {
                    format!("estimated {}", format_delta(-gap, u))
                }
                Some(crate::diagnostics::measurement::Quality::LowerBound) => {
                    format!("at least {}", format_delta(-gap, u))
                }
                _ => "unknown".into(),
            };
            let _ = writeln!(out, "Change since then:  {value}");
        }
    }

    out.push_str("\nSystem checks:\n");
    let mut t = Table::new(&[
        ("Check", Align::Left),
        ("Status", Align::Left),
        ("Detail", Align::Left),
    ]);
    for c in &r.checks {
        if !macos_terms
            && matches!(
                c.name.as_str(),
                "APFS container / volume mapping" | "Full Disk Access coverage"
            )
        {
            continue;
        }
        t.row([c.name.clone(), c.status.clone(), c.detail.clone()]);
    }
    out.push_str(&t.render());

    if !r.snapshots.snapshots.is_empty() {
        out.push_str(
            "\nLocal APFS snapshots (sizes are not reported by APFS; reclaimable: unknown):\n",
        );
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
        let _ = writeln!(
            out,
            "Sizes from {src}, not measured now; locations outside that scan were measured now."
        );
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
            let size = |b: Option<u64>| match (f.confidence, b) {
                (_, None) | (Confidence::Unknown, _) => "unknown".to_string(),
                (Confidence::LowerBound, Some(b)) => format!("≥{}", format_size(b, u)),
                (Confidence::Observed, Some(b)) => format_size(b, u),
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
        for c in r
            .findings
            .iter()
            .filter(|c| c.within.as_deref() == Some(f.id.as_str()))
        {
            t.row(row(c, "  └ "));
        }
    }
    out.push_str(&t.render());

    if !r.largest_files.is_empty() {
        out.push_str("\nLargest files seen in these locations:\n");
        let mut t = Table::new(&[
            ("Allocated", Align::Right),
            ("Logical", Align::Right),
            ("Path", Align::Left),
        ]);
        for f in r.largest_files.iter().take(10) {
            t.row([
                measured_size(Some(f.allocated_bytes), f.measurement.quality, u),
                measured_size(Some(f.logical_bytes), f.measurement.quality, u),
                f.path.clone(),
            ]);
        }
        out.push_str(&t.render());
    }

    let od = &r.open_deleted;
    if od.supported {
        let _ = writeln!(
            out,
            "\nDeleted but still open files: {} in {} file(s) ({})",
            measured_size(od.total_bytes, od.measurement.quality, u),
            od.distinct_files
                .map(|count| count.to_string())
                .unwrap_or_else(|| "unknown".into()),
            od.scope
        );
        for f in od
            .files
            .iter()
            .take(5)
            .filter(|f| f.size_bytes.is_some_and(|s| s > 0))
        {
            let _ = writeln!(
                out,
                "  {:>10}  {} (pid {}) {}",
                measured_size(f.size_bytes, f.measurement.quality, u),
                f.command,
                f.pid,
                f.path
            );
        }
    } else {
        let _ = writeln!(
            out,
            "\nDeleted but still open files: unknown ({})",
            od.error.as_deref().unwrap_or("not supported")
        );
    }

    out.push_str("\nRecommendations are for manual inspection; risk = how costly removing data there can be.\n");
    for f in r
        .findings
        .iter()
        .filter(|f| {
            f.within.is_none()
                && f.allocated_bytes
                    .is_some_and(|bytes| bytes >= 1_000_000_000)
        })
        .take(5)
    {
        let _ = writeln!(out, "  {}: {}", f.label, f.recommendation);
    }

    if !summary_first {
        out.push_str("\nConclusion:\n");
        for c in &r.conclusion {
            let _ = writeln!(out, "  {c}");
        }
    }
    let _ = writeln!(
        out,
        "No files were changed. ({:.1}s)",
        r.elapsed_ms as f64 / 1000.0
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::access::{Access, AccessCheck};
    use crate::diagnostics::doctor::{Check, DoctorReport, SnapshotInventory};
    use crate::diagnostics::measurement::Envelope;
    use crate::diagnostics::openfiles::OpenDeletedReport;
    use crate::diagnostics::suspects::LargeFile;
    use crate::diagnostics::usage::{ContainerUsage, UsageReport, VolumeUsage};
    use std::path::PathBuf;

    fn volume(fs_type: &str) -> VolumeUsage {
        VolumeUsage {
            name: "data".into(),
            mount_point: PathBuf::from("/data"),
            device: "device".into(),
            fs_type: fs_type.into(),
            roles: Vec::new(),
            total_bytes: Some(100),
            free_bytes: Some(20),
            available_bytes: Some(25),
            available_important_bytes: None,
            available_opportunistic_bytes: None,
            purgeable_estimate_bytes: None,
            in_use_bytes: Some(75),
            read_only: false,
            browsable: true,
            volume_uuid: None,
            measurement: Envelope::observed("filesystem", "volume", "test"),
            reclaimable_measurement: Envelope::default(),
        }
    }

    fn generic_usage(fs_type: &str) -> UsageReport {
        UsageReport {
            containers: Vec::new(),
            other_volumes: vec![volume(fs_type)],
            apfs_error: None,
            mounts_error: None,
        }
    }

    fn apfs_usage() -> UsageReport {
        let mut v = volume("apfs");
        v.roles = vec!["Data".into()];
        v.available_important_bytes = Some(40);
        v.purgeable_estimate_bytes = Some(20);
        UsageReport {
            containers: vec![ContainerUsage {
                reference: "disk3".into(),
                uuid: "container".into(),
                physical_store: "disk0s2".into(),
                capacity_bytes: Some(100),
                free_bytes: Some(20),
                volumes: vec![v],
                measurement: Envelope::observed("apfs_container", "container", "test"),
            }],
            other_volumes: Vec::new(),
            apfs_error: None,
            mounts_error: None,
        }
    }

    fn assert_platform_neutral(text: &str) {
        for word in ["Finder", "Full Disk Access", "APFS", "macOS", "Purgeable"] {
            assert!(!text.contains(word), "{word} leaked into:\n{text}");
        }
    }

    #[test]
    fn linux_and_windows_usage_text_is_platform_neutral() {
        for fs_type in ["ext4", "ntfs"] {
            let report = generic_usage(fs_type);
            assert_platform_neutral(&usage(&report, SizeUnits::Decimal));
            assert_platform_neutral(&usage_summary(&report, SizeUnits::Decimal).join("\n"));
        }
    }

    #[test]
    fn apfs_usage_text_keeps_macos_terms() {
        let report = apfs_usage();
        let text = usage(&report, SizeUnits::Decimal);
        assert!(text.contains("APFS container"), "{text}");
        assert!(text.contains("as Finder"), "{text}");
        assert!(text.contains("Purgeable est."), "{text}");
        let summary = usage_summary(&report, SizeUnits::Decimal).join("\n");
        assert!(summary.contains("macOS"), "{summary}");
    }

    #[test]
    fn generic_doctor_text_hides_macos_only_checks() {
        let report = DoctorReport {
            generated_at: "now".into(),
            usage: generic_usage("ntfs"),
            checks: vec![
                Check {
                    name: "APFS container / volume mapping".into(),
                    status: "NONE".into(),
                    detail: "0 containers".into(),
                },
                Check {
                    name: "Full Disk Access coverage".into(),
                    status: "UNKNOWN".into(),
                    detail: "unsupported".into(),
                },
            ],
            access: AccessCheck {
                full_disk_access: Access::Unknown,
                detail: "unsupported".into(),
            },
            snapshots: SnapshotInventory {
                snapshots: Vec::new(),
                reclaimable: "unknown".into(),
                reclaimable_measurement: Envelope::default(),
                errors: Vec::new(),
            },
            findings: Vec::new(),
            largest_files: Vec::new(),
            open_deleted: OpenDeletedReport {
                supported: false,
                scope: "unsupported".into(),
                total_bytes: None,
                distinct_files: None,
                files: Vec::new(),
                error: None,
                measurement: Envelope::default(),
            },
            reference_free_bytes: Some(120),
            reference_free_measurement: Envelope::estimated(
                "filesystem",
                "reference free space",
                "user supplied; not verified",
            ),
            unexplained_gap_bytes: Some(42),
            unexplained_gap_measurement: Envelope::estimated(
                "filesystem",
                "free-space accounting",
                "user-supplied reference minus OS-reported available space",
            ),
            conclusion: Vec::new(),
            sizes_from: None,
            files_modified: false,
            elapsed_ms: 0,
        };
        let text = doctor(&report, SizeUnits::Decimal);
        assert_platform_neutral(&text);
        assert!(text.contains("Change since then:  estimated"), "{text}");
    }

    fn doctor_with_largest_files(largest_files: Vec<LargeFile>) -> DoctorReport {
        DoctorReport {
            generated_at: "now".into(),
            usage: generic_usage("ntfs"),
            checks: Vec::new(),
            access: AccessCheck {
                full_disk_access: Access::Unknown,
                detail: "unsupported".into(),
            },
            snapshots: SnapshotInventory {
                snapshots: Vec::new(),
                reclaimable: "unknown".into(),
                reclaimable_measurement: Envelope::default(),
                errors: Vec::new(),
            },
            findings: Vec::new(),
            largest_files,
            open_deleted: OpenDeletedReport {
                supported: false,
                scope: "unsupported".into(),
                total_bytes: None,
                distinct_files: None,
                files: Vec::new(),
                error: None,
                measurement: Envelope::default(),
            },
            reference_free_bytes: None,
            reference_free_measurement: Envelope::default(),
            unexplained_gap_bytes: None,
            unexplained_gap_measurement: Envelope::default(),
            conclusion: Vec::new(),
            sizes_from: None,
            files_modified: false,
            elapsed_ms: 0,
        }
    }

    #[test]
    fn largest_file_text_uses_each_file_measurement_quality() {
        let files = vec![
            LargeFile {
                path: "observed-file".into(),
                logical_bytes: 100,
                allocated_bytes: 100,
                measurement: Envelope::observed("filesystem", "path", "test"),
            },
            LargeFile {
                path: "estimated-file".into(),
                logical_bytes: 200,
                allocated_bytes: 200,
                measurement: Envelope::estimated("filesystem", "path", "test"),
            },
            LargeFile {
                path: "lower-bound-file".into(),
                logical_bytes: 300,
                allocated_bytes: 300,
                measurement: Envelope::lower_bound("filesystem", "path", "test"),
            },
            LargeFile {
                path: "unknown-file".into(),
                logical_bytes: 400,
                allocated_bytes: 400,
                measurement: Envelope::default(),
            },
        ];
        let text = doctor(&doctor_with_largest_files(files), SizeUnits::Decimal);

        let line = |path: &str| text.lines().find(|line| line.contains(path)).unwrap();
        assert!(line("observed-file").contains(&format_size(100, SizeUnits::Decimal)));
        assert!(
            line("estimated-file").contains(&format!("~{}", format_size(200, SizeUnits::Decimal)))
        );
        assert!(
            line("lower-bound-file")
                .contains(&format!(">={}", format_size(300, SizeUnits::Decimal)))
        );
        let unknown = line("unknown-file");
        assert!(unknown.contains("unknown"));
        assert!(!unknown.contains(&format_size(400, SizeUnits::Decimal)));
    }
}
