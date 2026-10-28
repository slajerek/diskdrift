//! OS-reported space per volume, grouped by APFS container (spec 3.1, 4.1).

use crate::apfs::{ApfsContainer, ApfsVolume};
use diskdrift_platform::MountInfo;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeUsage {
    pub name: String,
    pub mount_point: PathBuf,
    pub device: String,
    pub fs_type: String,
    pub roles: Vec<String>,
    /// statfs: volume size. For APFS this is the shared container size.
    pub total_bytes: u64,
    /// statfs: strictly free blocks.
    pub free_bytes: u64,
    /// statfs: free blocks available to unprivileged users.
    pub available_bytes: u64,
    /// CoreFoundation: available for important usage (what Finder shows).
    pub available_important_bytes: Option<u64>,
    pub available_opportunistic_bytes: Option<u64>,
    /// Important-usage available minus strictly free: an OS estimate of space it can
    /// reclaim on demand (local snapshots, evictable caches).
    pub purgeable_estimate_bytes: Option<u64>,
    /// APFS: bytes in use by this volume within its container.
    pub in_use_bytes: Option<u64>,
    pub read_only: bool,
    pub browsable: bool,
    pub volume_uuid: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerUsage {
    pub reference: String,
    pub uuid: String,
    pub physical_store: String,
    /// Shared by all volumes in the container; shown once.
    pub capacity_bytes: u64,
    pub free_bytes: u64,
    pub volumes: Vec<VolumeUsage>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageReport {
    pub containers: Vec<ContainerUsage>,
    /// Non-APFS or unmatched volumes.
    pub other_volumes: Vec<VolumeUsage>,
    pub apfs_error: Option<String>,
}

/// "Low" when under 10% of capacity or under 20 GB available.
pub fn is_low(available: u64, total: u64) -> bool {
    available < 20_000_000_000 || (total > 0 && available * 10 < total)
}

impl UsageReport {
    /// The volume holding user data for `/` (macOS Data role), or `/` itself.
    pub fn primary(&self) -> Option<&VolumeUsage> {
        let all = || self.containers.iter().flat_map(|c| c.volumes.iter()).chain(self.other_volumes.iter());
        all()
            .find(|v| v.roles.iter().any(|r| r == "Data") && v.mount_point == Path::new("/System/Volumes/Data"))
            .or_else(|| all().find(|v| v.mount_point == Path::new("/")))
    }

    pub fn container_of(&self, v: &VolumeUsage) -> Option<&ContainerUsage> {
        self.containers.iter().find(|c| c.volumes.iter().any(|x| x.mount_point == v.mount_point))
    }
}

/// Mounts worth showing by default: Finder-visible ones, `/` and the Data volume.
fn default_visible(m: &MountInfo) -> bool {
    if matches!(m.fs_type.as_str(), "devfs" | "autofs" | "nullfs") {
        return false;
    }
    m.browsable || m.mount_point == Path::new("/") || m.mount_point == Path::new("/System/Volumes/Data")
}

fn volume_usage(m: &MountInfo, apfs: Option<&ApfsVolume>, capacity: bool) -> VolumeUsage {
    let cap = if capacity { diskdrift_platform::volume_capacity(&m.mount_point) } else { Default::default() };
    VolumeUsage {
        name: apfs
            .map(|v| v.name.clone())
            .or(cap.volume_name.clone())
            .unwrap_or_else(|| m.mount_point.display().to_string()),
        mount_point: m.mount_point.clone(),
        device: m.device.clone(),
        fs_type: m.fs_type.clone(),
        roles: apfs.map(|v| v.roles.clone()).unwrap_or_default(),
        total_bytes: m.total_bytes,
        free_bytes: m.free_bytes,
        available_bytes: m.available_bytes,
        available_important_bytes: cap.available_important_bytes,
        available_opportunistic_bytes: cap.available_opportunistic_bytes,
        purgeable_estimate_bytes: cap.purgeable_estimate(),
        in_use_bytes: apfs.map(|v| v.in_use_bytes),
        read_only: m.read_only,
        browsable: m.browsable,
        volume_uuid: cap.volume_uuid.clone(),
    }
}

/// Builds the usage report. `all` includes hidden system volumes.
pub fn build(mounts: &[MountInfo], containers: Result<&[ApfsContainer], String>, all: bool) -> UsageReport {
    let visible: Vec<&MountInfo> =
        mounts.iter().filter(|m| if all { !matches!(m.fs_type.as_str(), "devfs" | "autofs") } else { default_visible(m) }).collect();
    let mut report = UsageReport { containers: Vec::new(), other_volumes: Vec::new(), apfs_error: None };
    let containers = match containers {
        Ok(c) => c,
        Err(e) => {
            report.apfs_error = Some(e);
            &[]
        }
    };
    let mut used = vec![false; visible.len()];
    for c in containers {
        let mut vols = Vec::new();
        for v in &c.volumes {
            let Some(mp) = &v.mount_point else { continue };
            if let Some(i) = visible.iter().position(|m| &m.mount_point == mp) {
                used[i] = true;
                vols.push(volume_usage(visible[i], Some(v), true));
            }
        }
        if !vols.is_empty() {
            report.containers.push(ContainerUsage {
                reference: c.reference.clone(),
                uuid: c.uuid.clone(),
                physical_store: c.physical_store.clone(),
                capacity_bytes: c.capacity_bytes,
                free_bytes: c.free_bytes,
                volumes: vols,
            });
        }
    }
    for (i, m) in visible.iter().enumerate() {
        if !used[i] {
            report.other_volumes.push(volume_usage(m, None, m.local));
        }
    }
    report
}
