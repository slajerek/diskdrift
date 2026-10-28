//! Text output for `diskdrift find`.

use super::table::{Align, Table};
use crate::core::record::Kind;
use crate::core::units::{SizeUnits, format_count, format_size};
use crate::search::{Current, History, Lifetime, Place, ScanRef};
use std::fmt::Write;

fn kind(k: Kind) -> &'static str {
    match k {
        Kind::File => "file",
        Kind::Dir => "folder",
        Kind::Symlink => "link",
        Kind::Other => "other",
    }
}

/// When a scan started, "3 Oct 14:20" (with the year when it is not this year).
pub fn scan_when(rfc: &str, now: i64) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(rfc) else {
        return rfc.into();
    };
    let t = t.with_timezone(&chrono::Local);
    let this_year = chrono::DateTime::from_timestamp(now, 0).map(|n| n.with_timezone(&chrono::Local).format("%Y").to_string());
    if this_year.as_deref() == Some(&t.format("%Y").to_string()) { t.format("%-d %b %H:%M").to_string() } else { t.format("%-d %b %Y %H:%M").to_string() }
}

/// "scan #12 (3 Oct 14:20)".
pub fn scan_ref(s: &ScanRef, now: i64) -> String {
    format!("scan #{} ({})", s.id, scan_when(&s.started_at, now))
}

fn disk(p: &Place) -> String {
    if p.connected { p.disk_name.clone() } else { format!("{} (not connected)", p.disk_name) }
}

/// The matches in the newest scans.
pub fn current(c: &Current, u: SizeUnits, now: i64) -> String {
    let mut out = String::new();
    if !c.store_exists {
        let _ = writeln!(out, "No scans yet in {} (run `diskdrift scan` first): nothing to search.", c.store_path.display());
        return out;
    }
    let disks = {
        let mut d: Vec<&str> = c.searched.iter().map(|s| s.place.disk_name.as_str()).collect();
        d.sort_unstable();
        d.dedup();
        d.len()
    };
    let entries: u64 = c.searched.iter().map(|s| s.entries).sum();
    let scope = format!(
        "{} {} of {} {} ({}, {:.2} s)",
        c.searched.len(),
        if c.searched.len() == 1 { "latest scan" } else { "latest scans" },
        disks,
        if disks == 1 { "disk" } else { "disks" },
        super::entries(entries),
        c.elapsed_ms as f64 / 1000.0
    );
    let only = c.disk.as_ref().map(|d| format!(" (only {d})")).unwrap_or_default();
    if c.total == 0 {
        let _ = writeln!(out, "Not on any disk{only} as of the latest scans: searched {scope}.");
        return out;
    }
    let _ = writeln!(
        out,
        "{} {} for \"{}\"{only} in {scope}{}:",
        format_count(c.total),
        if c.total == 1 { "match" } else { "matches" },
        c.pattern,
        if c.truncated { format!(", the first {} shown", c.hits.len()) } else { String::new() }
    );
    out.push('\n');
    let mut t = Table::new(&[("Disk", Align::Left), ("Path", Align::Left), ("Kind", Align::Left), ("On disk", Align::Right), ("Modified", Align::Left), ("As of", Align::Left)]);
    for h in &c.hits {
        t.row([
            if h.place.connected { h.place.disk_name.clone() } else { format!("{}*", h.place.disk_name) },
            h.path.lossy(),
            kind(h.kind).into(),
            if h.kind == Kind::Dir { "-".into() } else { format_size(h.size, u) },
            super::date(h.mtime),
            scan_ref(&h.scan, now),
        ]);
    }
    out.push_str(&t.render());
    if c.hits.iter().any(|h| !h.place.connected) {
        out.push_str("  * not connected: the scan shows what was there; connect the disk to get at the file.\n");
    }
    if c.hits.iter().any(|h| h.place.connected && h.place.mount_point.as_ref().is_some_and(|m| !h.place.root.starts_with(&*m.to_string_lossy()))) {
        out.push_str("  Some disks are mounted somewhere else now than when they were scanned; the paths are as scanned.\n");
    }
    out
}

/// Printed before the history search starts, when nothing is current.
pub const SEARCHING_HISTORY: &str = "Searching the history for deleted files…";

fn lifetime_lines(out: &mut String, l: &Lifetime, u: SizeUnits, now: i64) {
    let size = if l.kind == Kind::Dir { String::new() } else { format!(", {}", format_size(l.size, u)) };
    let _ = writeln!(out, "{}  ({}{size}, modified {})", l.path.lossy(), kind(l.kind), super::date(l.mtime));
    let _ = writeln!(out, "  on {}, scanned at {}", disk(&l.place), l.place.root);
    match &l.gone_in {
        Some(g) => {
            let _ = writeln!(out, "  last seen in {}, gone in {}: deleted or moved between those two.", scan_ref(&l.last_seen, now), scan_ref(g, now));
        }
        None => {
            let _ = writeln!(out, "  still there in {}.", scan_ref(&l.last_seen, now));
        }
    }
    let mut since = match &l.appeared_in {
        Some(a) => format!("  appeared in {}", scan_ref(a, now)),
        None => format!("  already there in the oldest scan, {}", scan_ref(&l.oldest, now)),
    };
    if l.changes > 0 {
        let _ = write!(since, "; its size or date changed {} {}", l.changes, if l.changes == 1 { "time" } else { "times" });
    }
    let _ = writeln!(out, "{since}.");
    if let Some(m) = &l.moved_to {
        let _ = writeln!(out, "  probably moved to {m} (a file of the same name appeared there in the same scan; by name only).");
    }
    if let Some(why) = &l.uncertain {
        let _ = writeln!(out, "  uncertain: {why}, so it may still be there.");
    }
}

/// The history part: what is gone, and (with `--history`) when the current
/// matches appeared. `found_now`: there were current matches.
pub fn history(h: &History, found_now: bool, u: SizeUnits, now: i64) -> String {
    let mut out = String::new();
    if h.cancelled {
        let _ = writeln!(out, "History search stopped after {} of {} scan pairs.", h.pairs_done, h.pairs);
    }
    if h.too_many {
        let _ = writeln!(
            out,
            "The history holds more than {} changes that match, so the search stopped (the newest scans first); older ones may be missing. Type more of the name to narrow it.",
            format_count(crate::search::MAX_EVENTS as u64)
        );
    }
    if h.pairs == 0 {
        out.push_str("There is no older scan to search: every disk and folder has a single scan.\n");
        return out;
    }
    let read = format!("{} of {} pairs of adjacent scans read, {:.2} s", h.pairs_done, h.pairs, h.elapsed_ms as f64 / 1000.0);
    if h.gone.is_empty() {
        if found_now {
            let _ = writeln!(out, "Nothing that matches is gone from a disk ({read}).");
        } else {
            let _ = writeln!(out, "Not in the history either ({read}).");
        }
    } else {
        let _ = writeln!(
            out,
            "In the history: {} {} no longer there ({read}){}:",
            format_count(h.gone_total),
            if h.gone_total == 1 { "path" } else { "paths" },
            if h.gone_total > h.gone.len() as u64 { format!(", the {} most recently gone shown", h.gone.len()) } else { String::new() }
        );
        for l in &h.gone {
            out.push('\n');
            lifetime_lines(&mut out, l, u, now);
        }
        out.push_str(
            "\nDiskDrift records metadata, not file contents, and cannot bring a file back. Look in the Trash, \
             in Time Machine if it backs up that disk, or in the disk's own backups.\n",
        );
    }
    if !h.current.is_empty() {
        let _ = writeln!(out, "\nHistory of the current matches that changed:");
        for l in &h.current {
            out.push('\n');
            lifetime_lines(&mut out, l, u, now);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::pathser::NativePath;

    fn scan(id: u64, at: &str) -> ScanRef {
        ScanRef { id, name: format!("s{id}"), started_at: at.into() }
    }

    #[test]
    fn a_gone_file_reads_as_the_roadmap_says() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-22T12:00:00+02:00").unwrap().timestamp();
        let l = Lifetime {
            path: NativePath(b"/Volumes/backupy-2014/Docs/waznasprawa10.txt".to_vec()),
            kind: Kind::File,
            size: 12_000,
            mtime: 1_700_000_000,
            place: Place { disk: Some("d1".into()), disk_name: "backupy-2014".into(), connected: false, mount_point: None, root: "/Volumes/backupy-2014".into() },
            last_seen: scan(12, "2026-10-03T14:20:00+02:00"),
            gone_in: Some(scan(13, "2026-10-10T09:05:00+02:00")),
            appeared_in: Some(scan(4, "2025-10-03T14:20:00+02:00")),
            oldest: scan(1, "2025-01-01T00:00:00+01:00"),
            changes: 1,
            uncertain: None,
            moved_to: Some("/Volumes/backupy-2014/Old/waznasprawa10.txt".into()),
        };
        let h = History { gone: vec![l], gone_total: 1, pairs_done: 12, pairs: 12, elapsed_ms: 113, ..Default::default() };
        let text = history(&h, false, SizeUnits::Decimal, now);
        assert!(text.contains("on backupy-2014 (not connected)"), "{text}");
        assert!(text.contains("last seen in scan #12 (3 Oct 14:20), gone in scan #13 (10 Oct 09:05): deleted or moved between those two."), "{text}");
        assert!(text.contains("appeared in scan #4 (3 Oct 2025 14:20); its size or date changed 1 time."), "{text}");
        assert!(text.contains("probably moved to /Volumes/backupy-2014/Old/waznasprawa10.txt"), "{text}");
        assert!(text.contains("cannot bring a file back"), "{text}");
    }
}
