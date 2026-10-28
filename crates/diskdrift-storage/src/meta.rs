//! Snapshot metadata, stored as one JSON document per snapshot.

pub use diskdrift_core::record::{FileIdScheme, TimestampResolution};
use diskdrift_core::native::NativePath;
use diskdrift_core::walk::{WalkIssue, WalkReport};
use serde::{Deserialize, Serialize};
use std::fmt;

/// 2: `disk` and `rootInVolume` (ADR 0002).
pub const META_FORMAT_VERSION: u32 = 2;
/// Reserved compatibility schema. Publication remains v2 until the activation task.
pub const META_FORMAT_VERSION_V3: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileIdScope { Volume, Filesystem, Global }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AllocationSemantics { FilesystemReported, SharedPossible, Unknown }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provenance { Observed(String), Estimated(String), Unknown }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeDescriptor {
    pub index: u16,
    pub platform: String,
    pub identity: NativePath,
    pub identity_strength: String,
    pub file_id_scheme: FileIdScheme,
    pub file_id_scope: FileIdScope,
    pub mount_key: NativePath,
    pub aliases: Vec<NativePath>,
    pub allocation: AllocationSemantics,
    pub allocation_provenance: Provenance,
    pub timestamp_resolution: TimestampResolution,
    pub provenance: Provenance,
}

/// In-memory metadata-v3 view. It is deliberately not published by the active
/// store writer; readers may validate it while migration/activation is deferred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataV3 {
    pub format_version: u32,
    pub volumes: Vec<VolumeDescriptor>,
}

impl MetadataV3 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.format_version != META_FORMAT_VERSION_V3 { return Err("not metadata-v3"); }
        for (expected, volume) in self.volumes.iter().enumerate() {
            if volume.index as usize != expected { return Err("volume descriptors must be ordered"); }
            volume.validate()?;
        }
        Ok(())
    }
}

impl VolumeDescriptor {
    pub fn new(index: u16, platform: impl Into<String>, file_id_scheme: FileIdScheme, file_id_scope: FileIdScope, allocation: AllocationSemantics, timestamp_resolution: TimestampResolution, provenance: Provenance) -> Self {
        Self { index, platform: platform.into(), identity: NativePath::unix(Vec::new()), identity_strength: "unknown".into(), file_id_scheme, file_id_scope, mount_key: NativePath::unix(Vec::new()), aliases: Vec::new(), allocation, allocation_provenance: Provenance::Unknown, timestamp_resolution, provenance }
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        if matches!(self.file_id_scheme, FileIdScheme::Unavailable) && !matches!(self.file_id_scope, FileIdScope::Volume) { return Err("unavailable file identity must be volume scoped"); }
        if self.platform.is_empty() { return Err("platform is required"); }
        if self.mount_key == NativePath::unix(Vec::new()) { return Err("mount key is required"); }
        if self.identity == NativePath::unix(Vec::new()) { return Err("identity is required"); }
        if self.aliases.windows(2).any(|pair| pair[0] >= pair[1]) { return Err("aliases must be ordered and unique"); }
        Ok(())
    }
}

/// Snapshot states (spec section 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    InProgress,
    Completed,
    /// Completed, but parts of the tree could not be read.
    Partial,
    Failed,
    Cancelled,
}

impl Status {
    /// Has entry data that can be read, compared and used as a delta base.
    pub fn is_usable(self) -> bool {
        matches!(self, Status::Completed | Status::Partial)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Status::InProgress => "IN_PROGRESS",
            Status::Completed => "COMPLETED",
            Status::Partial => "PARTIAL",
            Status::Failed => "FAILED",
            Status::Cancelled => "CANCELLED",
        })
    }
}

/// Identity of the scanned volume; diffs require the same identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct VolumeIdentity {
    pub uuid: Option<String>,
    pub name: Option<String>,
    pub fs_type: String,
    pub mount_point: String,
    /// The root lies in the macOS System+Data volume group.
    pub volume_group: bool,
}

/// OS-reported space at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SpaceSample {
    pub total_bytes: u64,
    /// statfs: strictly free.
    pub free_bytes: u64,
    /// statfs: available to unprivileged users.
    pub available_bytes: u64,
    /// CoreFoundation important-usage capacity, if reported.
    pub available_important_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScanOptions {
    pub stay_on_filesystem: bool,
    pub exclude: Vec<String>,
    pub lister: String,
    pub threads: usize,
    /// A careful scan (spec 3C): one thread, a volume mounted read-only.
    /// Optional in the file, so older metadata reads unchanged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub careful: bool,
}

/// How the entry data is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StorageInfo {
    /// File-name stem of this snapshot's files (`YYMMDD-HHMMSS` of the scan start,
    /// local time, with `-N` appended when taken). `None` in stores written before
    /// store version 2, whose files are named after the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stem: Option<String>,
    /// `None`: a full checkpoint. `Some(id)`: a delta against snapshot `id`.
    pub base: Option<u64>,
    pub data_file: Option<String>,
    /// Bytes of the data file on disk (allocated).
    pub bytes_on_disk: u64,
    pub blocks: u64,
    /// Records (full) or ops (delta) in the data file.
    pub items: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotMeta {
    pub format_version: u32,
    pub id: u64,
    pub name: String,
    /// Where the scan happened (absolute). Live operations need it; comparisons
    /// of snapshots that know their disk use `disk` and `root_in_volume`.
    pub root: String,
    /// The short ID of the disk in the disk registry (ADR 0002). Absent in
    /// snapshots written before metadata version 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<String>,
    /// `root` relative to the volume's mount point (`/`, `/Docs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_in_volume: Option<String>,
    pub status: Status,
    /// RFC 3339 local times. A recursive scan is not atomic: entries were read
    /// at different moments between these two.
    pub started_at: String,
    pub finished_at: Option<String>,
    pub volume: VolumeIdentity,
    pub options: ScanOptions,
    pub space_before: Option<SpaceSample>,
    pub space_after: Option<SpaceSample>,
    pub storage: StorageInfo,
    pub counts: Option<WalkReport>,
    pub error: Option<String>,
    pub diskdrift_version: String,
}

/// `root` relative to `mount_point`, as `/` or `/a/b`; `None` when `root` is
/// not under it.
pub fn root_in_volume(root: &str, mount_point: &str) -> Option<String> {
    let rest = if mount_point == "/" { root } else { root.strip_prefix(mount_point.trim_end_matches('/'))? };
    if rest.is_empty() {
        return Some("/".into());
    }
    rest.starts_with('/').then(|| rest.trim_end_matches('/').to_string()).map(|r| if r.is_empty() { "/".into() } else { r })
}

impl SnapshotMeta {
    /// `root_in_volume` as recorded, or derived from the root and mount point.
    /// On the macOS System+Data volume group the root is seen from `/` (firm
    /// links), so the root itself is the folder within the disk.
    pub fn derived_root_in_volume(&self) -> String {
        if let Some(r) = &self.root_in_volume {
            return r.clone();
        }
        if self.volume.volume_group {
            return self.root.clone();
        }
        root_in_volume(&self.root, &self.volume.mount_point).unwrap_or_else(|| self.root.clone())
    }

    pub fn issues(&self) -> &[WalkIssue] {
        self.counts.as_ref().map(|c| c.issues.as_slice()).unwrap_or(&[])
    }

    /// Entries in the snapshot (0 when it holds no data).
    pub fn entries(&self) -> u64 {
        self.counts.as_ref().map(|c| c.entries).unwrap_or(0)
    }

    /// Why two snapshots are not comparable, if they are not. Snapshots that
    /// both know their disk are compared by disk and root in the volume, so a
    /// disk mounted somewhere else is still the same lineage (ADR 0002, 2.5).
    pub fn incompatibility(&self, other: &SnapshotMeta) -> Option<String> {
        match (&self.disk, &other.disk) {
            (Some(a), Some(b)) => {
                if a != b {
                    return Some(format!("different disks ({a} vs {b})"));
                }
                if self.root_in_volume != other.root_in_volume {
                    let show = |r: &Option<String>| r.clone().unwrap_or_else(|| "?".into());
                    return Some(format!("different folders on the disk ({} vs {})", show(&self.root_in_volume), show(&other.root_in_volume)));
                }
            }
            _ => {
                if self.root != other.root {
                    return Some(format!("different scan roots ({} vs {})", self.root, other.root));
                }
                match (&self.volume.uuid, &other.volume.uuid) {
                    (Some(a), Some(b)) if a != b => return Some(format!("different volumes ({a} vs {b})")),
                    (Some(_), None) | (None, Some(_)) => return Some("the volume of one scan is not identified".into()),
                    _ => {}
                }
            }
        }
        if self.options.exclude != other.options.exclude {
            return Some("different exclusion rules".into());
        }
        if self.options.stay_on_filesystem != other.options.stay_on_filesystem {
            return Some("different filesystem-boundary settings".into());
        }
        None
    }
}
