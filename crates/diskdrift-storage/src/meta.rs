//! Snapshot metadata, stored as one JSON document per snapshot.

use diskdrift_core::native::{NativeName, NativePath, NativeRoot, PathEncoding};
use diskdrift_core::paths::{
    PathOrderError, compare_native_paths, components_under, join_components,
};
pub use diskdrift_core::record::{FileIdScheme, FileIdScope, TimestampResolution};
use diskdrift_core::walk::{WalkIssue, WalkReport};
use serde::{Deserialize, Serialize};
use std::fmt;

/// 2: `disk` and `rootInVolume` (ADR 0002).
pub const META_FORMAT_VERSION: u32 = 2;
/// Reserved compatibility schema. Publication remains v2 until the activation task.
pub const META_FORMAT_VERSION_V3: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AllocationSemantics {
    FilesystemReported,
    SharedPossible,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provenance {
    Observed(String),
    Estimated(String),
    Unknown,
}

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

/// Compatibility capabilities shared by two snapshot lineages after their
/// hard identity fences have passed. These values affect comparison precision,
/// not whether independent snapshots may coexist in one store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineageCompatibility {
    pub common_timestamp_resolution: TimestampResolution,
    pub allocated_deltas: bool,
    pub file_id_comparable: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error("invalid metadata-v3 descriptor: {0}")]
    Invalid(String),
    #[error(transparent)]
    PathOrder(#[from] PathOrderError),
}

/// In-memory metadata-v3 view. It is deliberately not published by the active
/// store writer; readers may validate it while migration/activation is deferred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataV3 {
    pub format_version: u32,
    pub volumes: Vec<VolumeDescriptor>,
}

impl MetadataV3 {
    pub fn validate(&self) -> Result<(), MetadataError> {
        if self.format_version != META_FORMAT_VERSION_V3 {
            return Err(MetadataError::Invalid("not metadata-v3".into()));
        }
        for (expected, volume) in self.volumes.iter().enumerate() {
            if volume.index as usize != expected {
                return Err(MetadataError::Invalid(
                    "volume descriptors must be ordered".into(),
                ));
            }
            volume.validate()?;
        }
        Ok(())
    }
}

impl VolumeDescriptor {
    pub fn new(
        index: u16,
        platform: impl Into<String>,
        file_id_scheme: FileIdScheme,
        file_id_scope: FileIdScope,
        allocation: AllocationSemantics,
        timestamp_resolution: TimestampResolution,
        provenance: Provenance,
    ) -> Self {
        Self {
            index,
            platform: platform.into(),
            identity: NativePath::unix(Vec::new()),
            identity_strength: "unknown".into(),
            file_id_scheme,
            file_id_scope,
            mount_key: NativePath::unix(Vec::new()),
            aliases: Vec::new(),
            allocation,
            allocation_provenance: Provenance::Unknown,
            timestamp_resolution,
            provenance,
        }
    }
    pub fn validate(&self) -> Result<(), MetadataError> {
        let platform = self.platform.to_ascii_lowercase();
        let windows = platform == "windows";
        let unix = matches!(platform.as_str(), "macos" | "linux" | "freebsd" | "unix");
        if !windows && !unix {
            return Err(MetadataError::Invalid(
                "platform is required and must identify a supported host".into(),
            ));
        }
        let expected_encoding = if windows {
            PathEncoding::WindowsUtf16le
        } else {
            PathEncoding::UnixBytes
        };
        if self.identity.encoding() != expected_encoding
            || self.mount_key.encoding() != expected_encoding
        {
            return Err(MetadataError::Invalid(
                "platform and native path encoding disagree".into(),
            ));
        }
        if self.identity == NativePath::unix(Vec::new())
            || self.identity == NativePath::windows(Vec::new())
        {
            return Err(MetadataError::Invalid("identity is required".into()));
        }
        if self.mount_key == NativePath::unix(Vec::new())
            || self.mount_key == NativePath::windows(Vec::new())
        {
            return Err(MetadataError::Invalid("mount key is required".into()));
        }
        if !matches!(self.identity_strength.as_str(), "uuid" | "serial" | "weak") {
            return Err(MetadataError::Invalid(
                "identity strength is invalid".into(),
            ));
        }
        let scheme_ok = if windows {
            matches!(
                self.file_id_scheme,
                FileIdScheme::WindowsFileId128 | FileIdScheme::Unavailable
            )
        } else {
            matches!(
                self.file_id_scheme,
                FileIdScheme::PosixInode64 | FileIdScheme::Unavailable
            )
        };
        if !scheme_ok {
            return Err(MetadataError::Invalid(
                "file-ID scheme does not match platform".into(),
            ));
        }
        if matches!(self.file_id_scheme, FileIdScheme::Unavailable)
            && !matches!(self.file_id_scope, FileIdScope::Volume)
        {
            return Err(MetadataError::Invalid(
                "unavailable file identity must be volume scoped".into(),
            ));
        }
        let mut previous = None;
        for alias in &self.aliases {
            if alias.encoding() != expected_encoding {
                compare_native_paths(&self.mount_key, alias)?;
                return Err(MetadataError::Invalid(
                    "alias encoding does not match platform".into(),
                ));
            }
            if let Some(previous) = previous
                && compare_native_paths(previous, alias)? != std::cmp::Ordering::Less
            {
                return Err(MetadataError::Invalid(
                    "aliases must be ordered and unique".into(),
                ));
            }
            previous = Some(alias);
        }
        if matches!(self.allocation, AllocationSemantics::Unknown)
            && !matches!(self.allocation_provenance, Provenance::Unknown)
        {
            return Err(MetadataError::Invalid(
                "unknown allocation must have unknown provenance".into(),
            ));
        }
        Ok(())
    }
}

/// Builds the ordered table for a single-volume snapshot from its root
/// descriptor.
pub fn complete_volume_table(root: &VolumeDescriptor) -> Vec<VolumeDescriptor> {
    vec![root.clone()]
}

/// Snapshot states (spec section 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    InProgress,
    Completed,
    /// Completed, but parts of the tree could not be read.
    Partial,
    /// The metadata is present, but its data or reconstruction chain failed
    /// validation. The snapshot remains listable but cannot be used as a base.
    Damaged,
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
            Status::Damaged => "DAMAGED",
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
    /// Exact native volume capabilities when the platform adapter supplied them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<VolumeDescriptor>,
}

/// OS-reported space at one moment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SpaceSample {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    /// statfs: strictly free.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,
    /// statfs: available to unprivileged users.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_bytes: Option<u64>,
    /// CoreFoundation important-usage capacity, if reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_important_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_opportunistic_bytes: Option<u64>,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub observed_at: Option<String>,
    #[serde(default)]
    pub quality: MeasurementQuality,
    #[serde(default)]
    pub provenance: Option<String>,
    #[serde(default)]
    pub timestamp_resolution: TimestampResolution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MeasurementQuality {
    #[default]
    Unknown,
    Observed,
    Estimated,
    LowerBound,
    Unsupported,
    NotApplicable,
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
    /// Exact native root when the snapshot was created by a native-aware writer.
    /// Older metadata has only the display string above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_native: Option<NativeRoot>,
    /// The short ID of the disk in the disk registry (ADR 0002). Absent in
    /// snapshots written before metadata version 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<String>,
    /// `root` relative to the volume's mount point (`/`, `/Docs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_in_volume: Option<String>,
    /// Exact native root relative to the scanned volume, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_in_volume_native: Option<NativePath>,
    pub status: Status,
    /// RFC 3339 local times. A recursive scan is not atomic: entries were read
    /// at different moments between these two.
    pub started_at: String,
    pub finished_at: Option<String>,
    pub volume: VolumeIdentity,
    /// Ordered native volume descriptors used by record `vol` indexes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<VolumeDescriptor>,
    pub options: ScanOptions,
    pub space_before: Option<SpaceSample>,
    pub space_after: Option<SpaceSample>,
    pub storage: StorageInfo,
    pub counts: Option<WalkReport>,
    pub error: Option<String>,
    pub diskdrift_version: String,
}

/// A root that is either an exact native key or an explicitly display-only
/// legacy value. Legacy strings must not be treated as exact native bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RootValue {
    Exact(NativePath),
    LegacyDisplayOnly(String),
}

/// `root` relative to `mount_point`, as `/` or `/a/b`; `None` when `root` is
/// not under it.
pub fn root_in_volume(root: &str, mount_point: &str) -> Option<String> {
    let rest = if mount_point == "/" {
        root
    } else {
        root.strip_prefix(mount_point.trim_end_matches('/'))?
    };
    if rest.is_empty() {
        return Some("/".into());
    }
    rest.starts_with('/')
        .then(|| rest.trim_end_matches('/').to_string())
        .map(|r| if r.is_empty() { "/".into() } else { r })
}

/// Exact native equivalent of [`root_in_volume`]. A legacy display mount point
/// cannot produce this value and is intentionally handled by callers as a
/// display-only fallback.
pub fn root_in_volume_native(root: &NativePath, mount_point: &NativePath) -> Option<NativePath> {
    if let (NativePath::Windows(root), NativePath::Windows(mount_point)) = (root, mount_point) {
        let root_parts = windows_structural_parts(root);
        let mount_parts = windows_structural_parts(mount_point);
        if mount_parts.len() > root_parts.len()
            || !mount_parts
                .iter()
                .zip(&root_parts)
                .all(|(mount, root)| windows_units_equal_ignore_case(mount, root))
        {
            return None;
        }
        let mut relative = vec![b'\\' as u16];
        for (index, part) in root_parts.iter().skip(mount_parts.len()).enumerate() {
            if index > 0 {
                relative.push(b'\\' as u16);
            }
            relative.extend_from_slice(part);
        }
        return Some(NativePath::windows(relative));
    }
    let components = components_under(mount_point, root).ok().flatten()?;
    let separator = match root.encoding() {
        PathEncoding::UnixBytes => NativeName::unix(vec![b'/']),
        PathEncoding::WindowsUtf16le => NativeName::windows(vec![b'\\' as u16]),
    };
    let mut relative = vec![separator];
    relative.extend(components.into_iter().skip(1));
    join_components(&relative).ok()
}

fn windows_structural_parts(path: &[u16]) -> Vec<Vec<u16>> {
    const VERBATIM: &[u16] = &[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    const VERBATIM_UNC: &[u16] = &[
        b'\\' as u16,
        b'\\' as u16,
        b'?' as u16,
        b'\\' as u16,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        b'\\' as u16,
    ];
    let mut path = path
        .iter()
        .map(|unit| {
            if *unit == b'/' as u16 {
                b'\\' as u16
            } else {
                *unit
            }
        })
        .collect::<Vec<_>>();
    if windows_units_starts_with_ignore_case(&path, VERBATIM_UNC) {
        path = [
            vec![b'\\' as u16, b'\\' as u16],
            path[VERBATIM_UNC.len()..].to_vec(),
        ]
        .concat();
    } else if windows_units_starts_with_ignore_case(&path, VERBATIM) {
        path = path[VERBATIM.len()..].to_vec();
    }
    while path.last() == Some(&(b'\\' as u16)) {
        path.pop();
    }
    path.split(|unit| *unit == b'\\' as u16)
        .filter(|part| !part.is_empty())
        .map(<[u16]>::to_vec)
        .collect()
}

fn windows_units_starts_with_ignore_case(value: &[u16], prefix: &[u16]) -> bool {
    value.get(..prefix.len()).is_some_and(|head| {
        head.iter()
            .zip(prefix)
            .all(|(left, right)| windows_ascii_lower(*left) == windows_ascii_lower(*right))
    })
}

fn windows_units_equal_ignore_case(left: &[u16], right: &[u16]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| windows_ascii_lower(*left) == windows_ascii_lower(*right))
}

fn windows_ascii_lower(unit: u16) -> u16 {
    if (b'A' as u16..=b'Z' as u16).contains(&unit) {
        unit + b'a' as u16 - b'A' as u16
    } else {
        unit
    }
}

impl SnapshotMeta {
    /// Validates the fields that are required in a store-v4 metadata view.
    /// Legacy display strings remain accepted as an explicit fallback; they
    /// are never promoted to exact native identity here.
    pub fn validate_v4(&self) -> Result<(), MetadataError> {
        if self.format_version != META_FORMAT_VERSION_V3 {
            return Err(MetadataError::Invalid(
                "store-v4 snapshots require metadata-v3".into(),
            ));
        }
        if let Some(root) = &self.root_native {
            if let Some(root_in_volume) = &self.root_in_volume_native
                && root.path.encoding() != root_in_volume.encoding()
            {
                return Err(MetadataError::Invalid(
                    "exact roots use different native path encodings".into(),
                ));
            }
            if let Some(descriptor) = &self.volume.descriptor {
                descriptor.validate()?;
                if descriptor.mount_key.encoding() != root.path.encoding() {
                    return Err(MetadataError::Invalid(
                        "root and volume descriptor encodings disagree".into(),
                    ));
                }
            }
        } else if let Some(root_in_volume) = &self.root_in_volume_native
            && let Some(descriptor) = &self.volume.descriptor
            && descriptor.mount_key.encoding() != root_in_volume.encoding()
        {
            return Err(MetadataError::Invalid(
                "exact root-in-volume and volume descriptor encodings disagree".into(),
            ));
        }
        if let Some(descriptor) = &self.volume.descriptor {
            descriptor.validate()?;
            let table_descriptor =
                self.volumes.get(descriptor.index as usize).ok_or_else(|| {
                    MetadataError::Invalid("volume descriptor index is not in the table".into())
                })?;
            if table_descriptor != descriptor {
                return Err(MetadataError::Invalid(
                    "root volume descriptor does not match the volume table".into(),
                ));
            }
        }
        for (expected, descriptor) in self.volumes.iter().enumerate() {
            if descriptor.index as usize != expected {
                return Err(MetadataError::Invalid(
                    "volume descriptors must be ordered".into(),
                ));
            }
            descriptor.validate()?;
        }
        Ok(())
    }

    pub fn root_value(&self) -> RootValue {
        self.root_native
            .as_ref()
            .map(|root| RootValue::Exact(root.path.clone()))
            .unwrap_or_else(|| RootValue::LegacyDisplayOnly(self.root.clone()))
    }

    pub fn derived_root_in_volume_value(&self) -> RootValue {
        if let Some(root) = &self.root_in_volume_native {
            return RootValue::Exact(root.clone());
        }
        if let Some(root) = &self.root_in_volume {
            return RootValue::LegacyDisplayOnly(root.clone());
        }
        if self.volume.volume_group {
            return self.root_value();
        }
        RootValue::LegacyDisplayOnly(self.derived_root_in_volume())
    }

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

    /// Returns a hard volume-identity fence. Mount paths and aliases are
    /// intentionally excluded: they describe where a volume was seen, not
    /// which volume it is.
    pub fn volume_identity_incompatibility(&self, other: &SnapshotMeta) -> Option<String> {
        match (
            self.volume.descriptor.as_ref(),
            other.volume.descriptor.as_ref(),
        ) {
            (Some(left), Some(right)) if left.identity != right.identity => {
                Some("incompatible volume identity descriptors".into())
            }
            (Some(_), None) | (None, Some(_)) => {
                Some("one snapshot lacks an exact volume identity descriptor".into())
            }
            _ => None,
        }
    }

    /// Returns a hard fence when the per-record volume indexes no longer name
    /// the same volumes. Mount paths and aliases may change between scans.
    pub fn volume_table_incompatibility(&self, other: &SnapshotMeta) -> Option<String> {
        if self.volumes.len() != other.volumes.len() {
            return Some("incompatible volume tables".into());
        }
        if self
            .volumes
            .iter()
            .zip(&other.volumes)
            .any(|(left, right)| {
                left.index != right.index
                    || left.platform != right.platform
                    || left.identity != right.identity
            })
        {
            return Some("incompatible volume tables".into());
        }
        None
    }

    /// Computes the non-fencing capabilities used by the core comparison and
    /// delta pipeline. Unknown allocation semantics deliberately turn off
    /// allocated-byte deltas while leaving logical/path comparison available.
    pub fn lineage_compatibility(&self, other: &SnapshotMeta) -> LineageCompatibility {
        let (common_timestamp_resolution, allocated_deltas, file_id_comparable) =
            if self.volumes.is_empty() && other.volumes.is_empty() {
                let left = self.volume.descriptor.as_ref();
                let right = other.volume.descriptor.as_ref();
                let timestamp = left
                    .map(|descriptor| descriptor.timestamp_resolution)
                    .unwrap_or(TimestampResolution::Unknown)
                    .common(
                        right
                            .map(|descriptor| descriptor.timestamp_resolution)
                            .unwrap_or(TimestampResolution::Unknown),
                    );
                let allocated = match (left, right) {
                    (Some(left), Some(right)) => {
                        matches!(left.allocation, AllocationSemantics::FilesystemReported)
                            && matches!(right.allocation, AllocationSemantics::FilesystemReported)
                    }
                    (None, None) => false,
                    _ => false,
                };
                let comparable = match (left, right) {
                    (Some(left), Some(right)) => {
                        left.file_id_scheme == right.file_id_scheme
                            && left.file_id_scope == right.file_id_scope
                            && !matches!(left.file_id_scheme, FileIdScheme::Unavailable)
                    }
                    _ => true,
                };
                (timestamp, allocated, comparable)
            } else if self.volumes.len() == other.volumes.len() {
                self.volumes.iter().zip(&other.volumes).fold(
                    (TimestampResolution::Nanoseconds, true, true),
                    |(timestamp, allocated, comparable), (left, right)| {
                        (
                            timestamp.common(
                                left.timestamp_resolution.common(right.timestamp_resolution),
                            ),
                            allocated
                                && matches!(
                                    (left.allocation, right.allocation),
                                    (
                                        AllocationSemantics::FilesystemReported,
                                        AllocationSemantics::FilesystemReported
                                    )
                                ),
                            comparable
                                && left.file_id_scheme == right.file_id_scheme
                                && left.file_id_scope == right.file_id_scope
                                && !matches!(left.file_id_scheme, FileIdScheme::Unavailable),
                        )
                    },
                )
            } else {
                (TimestampResolution::Unknown, false, false)
            };
        LineageCompatibility {
            common_timestamp_resolution,
            allocated_deltas,
            file_id_comparable,
        }
    }

    pub fn issues(&self) -> &[WalkIssue] {
        self.counts
            .as_ref()
            .map(|c| c.issues.as_slice())
            .unwrap_or(&[])
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
                if self.derived_root_in_volume_value() != other.derived_root_in_volume_value() {
                    let show = |r: RootValue| match r {
                        RootValue::Exact(path) => path.display(),
                        RootValue::LegacyDisplayOnly(value) => value,
                    };
                    return Some(format!(
                        "different folders on the disk ({} vs {})",
                        show(self.derived_root_in_volume_value()),
                        show(other.derived_root_in_volume_value())
                    ));
                }
            }
            _ => {
                if self.root_value() != other.root_value() {
                    let show = |r: RootValue| match r {
                        RootValue::Exact(path) => path.display(),
                        RootValue::LegacyDisplayOnly(value) => value,
                    };
                    return Some(format!(
                        "different scan roots ({} vs {})",
                        show(self.root_value()),
                        show(other.root_value())
                    ));
                }
                match (&self.volume.uuid, &other.volume.uuid) {
                    (Some(a), Some(b)) if a != b => {
                        return Some(format!("different volumes ({a} vs {b})"));
                    }
                    (Some(_), None) | (None, Some(_)) => {
                        return Some("the volume of one scan is not identified".into());
                    }
                    _ => {}
                }
            }
        }
        if let (RootValue::Exact(left), RootValue::Exact(right)) =
            (self.root_value(), other.root_value())
            && left.encoding() != right.encoding()
        {
            return Some("incompatible native root path encodings".into());
        }
        if let Some(reason) = self.volume_identity_incompatibility(other) {
            return Some(reason);
        }
        if let Some(reason) = self.volume_table_incompatibility(other) {
            return Some(reason);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn windows(path: &str) -> NativePath {
        NativePath::windows(path.encode_utf16().collect())
    }

    #[test]
    fn root_in_volume_native_accepts_case_and_verbatim_aliases_without_lowering_suffixes() {
        assert_eq!(
            root_in_volume_native(&windows(r"\\?\c:\Data\Folder"), &windows(r"C:\")),
            Some(windows(r"\Data\Folder")),
        );
        assert_eq!(
            root_in_volume_native(
                &windows(r"\\?\unc\SERVER\Share\Docs"),
                &windows(r"\\server\share"),
            ),
            Some(windows(r"\Docs")),
        );
    }

    #[test]
    fn complete_volume_table_starts_with_the_fixture_root_descriptor() {
        let mut root = VolumeDescriptor::new(
            0,
            "linux",
            FileIdScheme::PosixInode64,
            FileIdScope::Volume,
            AllocationSemantics::FilesystemReported,
            TimestampResolution::Seconds,
            Provenance::Observed("test".into()),
        );
        root.identity = NativePath::unix(b"fixture-volume".to_vec());
        root.mount_key = NativePath::unix(b"/".to_vec());
        root.identity_strength = "uuid".into();

        assert_eq!(complete_volume_table(&root), vec![root]);
    }

    #[test]
    fn descriptorless_legacy_snapshots_do_not_claim_allocation_comparability() {
        let legacy = || SnapshotMeta {
            format_version: META_FORMAT_VERSION,
            id: 1,
            name: "legacy".into(),
            root: "/".into(),
            root_native: None,
            disk: None,
            root_in_volume: None,
            root_in_volume_native: None,
            status: Status::Completed,
            started_at: "2026-10-01T00:00:00Z".into(),
            finished_at: None,
            volume: VolumeIdentity::default(),
            volumes: Vec::new(),
            options: ScanOptions::default(),
            space_before: None,
            space_after: None,
            storage: StorageInfo::default(),
            counts: None,
            error: None,
            diskdrift_version: "test".into(),
        };

        assert!(!legacy().lineage_compatibility(&legacy()).allocated_deltas);
    }
}
