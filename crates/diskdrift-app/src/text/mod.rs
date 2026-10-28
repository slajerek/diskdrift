//! Plain-text rendering of reports, shared by the CLI and the TUI so both show the
//! same numbers in the same words.

pub mod analyze;
pub mod browse;
pub mod careful;
pub mod disks;
pub mod investigate;
pub mod prune;
pub mod render;
pub mod search;
pub mod snapshots;
pub mod table;

pub use analyze::analyze;
pub use browse::{browse, diff_browse};
pub use disks::{disk_entry, disk_list, forget};
pub use render::*;
pub use investigate::{deep, recover};
pub use snapshots::*;

/// "1 entry", "1,731,280 entries".
pub fn entries(n: u64) -> String {
    if n == 1 { "1 entry".into() } else { format!("{} entries", crate::core::units::format_count(n)) }
}

/// A Unix time as a local date, "2024-03-12".
pub fn date(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0).map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string()).unwrap_or_else(|| "?".into())
}

/// The verdict as shown: "not recognised" rather than "unknown" for a name
/// DiskDrift has no notes on.
pub fn verdict_label(risk: Option<crate::diagnostics::suspects::Risk>) -> &'static str {
    match risk {
        None => "not recognised",
        r => crate::diagnostics::classify::verdict(r),
    }
}

/// When something last changed, relative to `now`. A time in the future (a
/// file with a wrong clock) is shown as its date, not as "just now".
pub fn changed_ago(t: i64, now: i64) -> String {
    if t > now + 60 { format!("{} (future)", date(t)) } else { crate::analyze::ago(now - t) }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_words_read_right() {
        assert_eq!(entries(1), "1 entry");
        assert_eq!(entries(1_731_280), "1,731,280 entries");
        assert_eq!(changed_ago(1_000, 1_000 + 3 * 86_400), "3 days ago");
        assert!(changed_ago(2_000_000_000, 1_000_000_000).ends_with("(future)"));
        assert_eq!(verdict_label(None), "not recognised");
    }
}
