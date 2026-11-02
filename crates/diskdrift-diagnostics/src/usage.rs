//! OS-reported space per volume, grouped by APFS container (spec 3.1, 4.1).

use crate::apfs::{ApfsContainer, ApfsVolume};
use crate::measurement::Envelope;
use diskdrift_platform::MountInfo;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeUsage {
    pub name: String,
    pub mount_point: PathBuf,
    pub device: String,
    pub fs_type: String,
    pub roles: Vec<String>,
    /// statfs: volume size. For APFS this is the shared container size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    /// statfs: strictly free blocks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,
    /// statfs: free blocks available to unprivileged users.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_bytes: Option<u64>,
    /// CoreFoundation: available for important usage (what Finder shows).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_important_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_opportunistic_bytes: Option<u64>,
    /// Important-usage available minus strictly free: an OS estimate of space it can
    /// reclaim on demand (local snapshots, evictable caches).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purgeable_estimate_bytes: Option<u64>,
    /// APFS: bytes in use by this volume within its container.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_use_bytes: Option<u64>,
    pub read_only: bool,
    pub browsable: bool,
    pub volume_uuid: Option<String>,
    #[serde(flatten)]
    pub measurement: Envelope,
    pub reclaimable_measurement: Envelope,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerUsage {
    pub reference: String,
    pub uuid: String,
    pub physical_store: String,
    /// Shared by all volumes in the container; shown once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,
    pub volumes: Vec<VolumeUsage>,
    #[serde(flatten)]
    pub measurement: Envelope,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageReport {
    pub containers: Vec<ContainerUsage>,
    /// Non-APFS or unmatched volumes.
    pub other_volumes: Vec<VolumeUsage>,
    pub apfs_error: Option<String>,
    pub mounts_error: Option<String>,
}

/// "Low" when under 10% of capacity or under 20 GB available.
pub fn is_low(available: u64, total: u64) -> bool {
    available < 20_000_000_000 || (total > 0 && available * 10 < total)
}

impl UsageReport {
    /// The platform's primary system volume, or the only visible volume.
    pub fn primary(&self) -> Option<&VolumeUsage> {
        let all = || {
            self.containers
                .iter()
                .flat_map(|c| c.volumes.iter())
                .chain(self.other_volumes.iter())
        };
        let system_root = diskdrift_platform::system_volume_root();
        all()
            .find(|v| {
                if cfg!(target_os = "windows") {
                    v.mount_point
                        .to_string_lossy()
                        .trim_end_matches(['\\', '/'])
                        .eq_ignore_ascii_case(
                            system_root.to_string_lossy().trim_end_matches(['\\', '/']),
                        )
                } else {
                    v.mount_point == system_root
                }
            })
            .or_else(|| {
                let mut volumes = all();
                let first = volumes.next();
                first.filter(|_| volumes.next().is_none())
            })
    }

    pub fn container_of(&self, v: &VolumeUsage) -> Option<&ContainerUsage> {
        self.containers
            .iter()
            .find(|c| c.volumes.iter().any(|x| x.mount_point == v.mount_point))
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    fn volume(path: &str) -> VolumeUsage {
        VolumeUsage {
            name: path.into(),
            mount_point: PathBuf::from(path),
            device: String::new(),
            fs_type: String::new(),
            roles: Vec::new(),
            total_bytes: None,
            free_bytes: None,
            available_bytes: None,
            available_important_bytes: None,
            available_opportunistic_bytes: None,
            purgeable_estimate_bytes: None,
            in_use_bytes: None,
            read_only: false,
            browsable: true,
            volume_uuid: None,
            measurement: Envelope::default(),
            reclaimable_measurement: Envelope::default(),
        }
    }

    #[test]
    fn unknown_optional_capacity_fields_are_omitted() {
        let json = serde_json::to_string(&volume("/")).unwrap();
        for field in [
            "availableImportantBytes",
            "availableOpportunisticBytes",
            "purgeableEstimateBytes",
            "inUseBytes",
        ] {
            assert!(!json.contains(field), "{field} was emitted in {json}");
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn primary_uses_windows_system_volume_when_roles_are_unavailable() {
        let root = diskdrift_platform::system_volume_root();
        let root_string = root.to_string_lossy().into_owned();
        let mut report = UsageReport {
            containers: Vec::new(),
            other_volumes: vec![volume(&root_string)],
            apfs_error: None,
            mounts_error: None,
        };
        assert_eq!(report.primary().map(|v| &v.mount_point), Some(&root));
        report.other_volumes[0].mount_point = PathBuf::from(r"D:\");
        assert!(report.primary().is_none());
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn primary_uses_posix_root_without_apfs_roles() {
        let mut report = UsageReport {
            containers: Vec::new(),
            other_volumes: vec![volume("/")],
            apfs_error: None,
            mounts_error: None,
        };
        assert_eq!(
            report.primary().map(|v| &v.mount_point),
            Some(&PathBuf::from("/"))
        );
        report.other_volumes[0].mount_point = PathBuf::from("/Volumes/Other");
        assert_eq!(
            report.primary().map(|v| &v.mount_point),
            Some(&PathBuf::from("/Volumes/Other"))
        );
    }

    #[test]
    fn primary_falls_back_to_the_only_volume_without_platform_paths() {
        let report = UsageReport {
            containers: Vec::new(),
            other_volumes: vec![volume("/data")],
            apfs_error: None,
            mounts_error: None,
        };
        assert_eq!(
            report.primary().map(|v| &v.mount_point),
            Some(&PathBuf::from("/data"))
        );
    }
}

/// Mounts worth showing by default: Finder-visible ones, `/` and the Data volume.
fn default_visible(m: &MountInfo) -> bool {
    if matches!(m.fs_type.as_str(), "devfs" | "autofs" | "nullfs") {
        return false;
    }
    m.browsable || diskdrift_platform::is_default_system_mount(&m.mount_point)
}

fn volume_usage(m: &MountInfo, apfs: Option<&ApfsVolume>, capacity: bool) -> VolumeUsage {
    let (cap, capacity_error) = if capacity {
        match diskdrift_platform::volume_capacity(&m.mount_point) {
            Ok(cap) => (cap, None),
            Err(error) => (Default::default(), Some(error.to_string())),
        }
    } else {
        (Default::default(), None)
    };
    let mut measurement = crate::measurement::from_capacity(&cap);
    if measurement.quality == Some(crate::measurement::Quality::Unknown) && m.total_bytes > 0 {
        measurement.quality = Some(crate::measurement::Quality::Observed);
        measurement
            .domain
            .get_or_insert_with(|| "filesystem".into());
        measurement.scope.get_or_insert_with(|| "mount".into());
        measurement.observed_at.get_or_insert_with(|| {
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        });
        measurement
            .provenance
            .get_or_insert_with(|| "statfs".into());
    }
    if let Some(error) = capacity_error {
        let provenance = measurement.provenance.get_or_insert_with(String::new);
        if !provenance.is_empty() {
            provenance.push_str("; ");
        }
        provenance.push_str("volume capacity unavailable: ");
        provenance.push_str(&error);
    }
    let reclaimable_measurement = if cap.purgeable_estimate().is_some() {
        let mut envelope = Envelope::estimated(
            "filesystem",
            "volume reclaimability",
            "important-usage available minus strictly free",
        );
        envelope.observed_at = cap
            .observed_at
            .clone()
            .or_else(|| measurement.observed_at.clone());
        envelope
    } else {
        Envelope::unknown(
            "filesystem",
            "volume reclaimability",
            "important-usage and strictly-free capacities unavailable",
        )
    };
    VolumeUsage {
        name: apfs
            .map(|v| v.name.clone())
            .or(cap.volume_name.clone())
            .unwrap_or_else(|| m.mount_point.display().to_string()),
        mount_point: m.mount_point.clone(),
        device: m.device.clone(),
        fs_type: m.fs_type.clone(),
        roles: apfs.map(|v| v.roles.clone()).unwrap_or_default(),
        total_bytes: Some(m.total_bytes),
        free_bytes: Some(m.free_bytes),
        available_bytes: Some(m.available_bytes),
        available_important_bytes: cap.available_important_bytes,
        available_opportunistic_bytes: cap.available_opportunistic_bytes,
        purgeable_estimate_bytes: cap.purgeable_estimate(),
        in_use_bytes: apfs.and_then(|v| v.in_use_bytes),
        read_only: m.read_only,
        browsable: m.browsable,
        volume_uuid: cap.volume_uuid.clone(),
        measurement,
        reclaimable_measurement,
    }
}

/// Builds the usage report. `all` includes hidden system volumes.
pub fn build(
    mounts: &[MountInfo],
    containers: Result<&[ApfsContainer], String>,
    all: bool,
) -> UsageReport {
    let visible: Vec<&MountInfo> = mounts
        .iter()
        .filter(|m| {
            if all {
                !matches!(m.fs_type.as_str(), "devfs" | "autofs")
            } else {
                default_visible(m)
            }
        })
        .collect();
    let mut report = UsageReport {
        containers: Vec::new(),
        other_volumes: Vec::new(),
        apfs_error: None,
        mounts_error: None,
    };
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
                measurement: c.measurement.clone(),
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
