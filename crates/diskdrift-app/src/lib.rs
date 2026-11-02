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
    let (mounts, mounts_error) = match platform::mounts() {
        Ok(inventory) => (
            inventory.mounts,
            (!inventory.errors.is_empty()).then(|| inventory.errors.join("; ")),
        ),
        Err(error) => (Vec::new(), Some(error.to_string())),
    };
    let topology = diagnostics::apfs::topology(&mounts);
    let mut report =
        diagnostics::usage::build(&mounts, topology.as_deref().map_err(|e| e.to_string()), all);
    report.mounts_error = mounts_error;
    report
}

#[cfg(test)]
pub(crate) mod test_support {
    use diskdrift_core::native::NativePath;
    use diskdrift_core::record::{FileIdScheme, TimestampResolution};
    use diskdrift_storage::meta::{
        AllocationSemantics, FileIdScope, Provenance, SnapshotMeta, VolumeDescriptor,
        complete_volume_table,
    };

    pub(crate) fn unix_volume_descriptor(identity: &str, mount_key: &str) -> VolumeDescriptor {
        let mut descriptor = VolumeDescriptor::new(
            0,
            "linux",
            FileIdScheme::PosixInode64,
            FileIdScope::Volume,
            AllocationSemantics::FilesystemReported,
            TimestampResolution::Seconds,
            Provenance::Observed("synthetic fixture".into()),
        );
        descriptor.identity = NativePath::unix(identity.as_bytes().to_vec());
        descriptor.mount_key = NativePath::unix(mount_key.as_bytes().to_vec());
        descriptor.identity_strength = "uuid".into();
        descriptor
    }

    pub(crate) fn set_root_volume(meta: &mut SnapshotMeta, descriptor: VolumeDescriptor) {
        meta.volume.descriptor = Some(descriptor.clone());
        meta.volumes = complete_volume_table(&descriptor);
    }
}
