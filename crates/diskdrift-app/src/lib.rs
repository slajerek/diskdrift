//! Application services: the one implementation of every command, used by both
//! the CLI and the TUI.

pub mod analyze;
pub mod careful;
pub mod config;
pub mod disks;
pub mod info;
pub mod investigate;
pub mod prune;
pub mod search;
pub mod snapshots;
pub mod text;

pub use diskdrift_core as core;
pub use diskdrift_diagnostics as diagnostics;
pub use diskdrift_platform as platform;
pub use diskdrift_storage as storage;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `diskdrift usage`: OS-reported space per volume.
pub fn usage(all: bool) -> diagnostics::usage::UsageReport {
    let mounts = platform::mounts().unwrap_or_default();
    let topology = diagnostics::apfs::topology(&mounts);
    diagnostics::usage::build(&mounts, topology.as_deref().map_err(|e| e.to_string()), all)
}
