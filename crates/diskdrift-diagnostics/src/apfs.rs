//! APFS container/volume topology and local snapshots, via read-only `diskutil`
//! and `tmutil` queries.

use crate::command::{self, CommandError};
use diskdrift_platform::MountInfo;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, thiserror::Error)]
pub enum ApfsError {
    #[error(transparent)]
    Command(#[from] CommandError),
    #[error("could not parse diskutil output: {0}")]
    Parse(String),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApfsContainer {
    pub reference: String,
    pub uuid: String,
    pub physical_store: String,
    /// Container size. Volumes share it; their capacities must never be added up.
    pub capacity_bytes: u64,
    pub free_bytes: u64,
    pub volumes: Vec<ApfsVolume>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApfsVolume {
    pub device: String,
    pub uuid: String,
    pub name: String,
    pub roles: Vec<String>,
    pub in_use_bytes: u64,
    pub encrypted: bool,
    pub locked: bool,
    pub mount_point: Option<PathBuf>,
}

impl ApfsVolume {
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawList {
    containers: Vec<RawContainer>,
}

#[derive(Deserialize)]
struct RawContainer {
    #[serde(rename = "ContainerReference")]
    reference: String,
    #[serde(rename = "APFSContainerUUID", default)]
    uuid: String,
    #[serde(rename = "DesignatedPhysicalStore", default)]
    physical_store: String,
    #[serde(rename = "CapacityCeiling", default)]
    capacity: u64,
    #[serde(rename = "CapacityFree", default)]
    free: u64,
    #[serde(rename = "Volumes", default)]
    volumes: Vec<RawVolume>,
}

#[derive(Deserialize)]
struct RawVolume {
    #[serde(rename = "DeviceIdentifier")]
    device: String,
    #[serde(rename = "APFSVolumeUUID", default)]
    uuid: String,
    #[serde(rename = "Name", default)]
    name: String,
    #[serde(rename = "Roles", default)]
    roles: Vec<String>,
    #[serde(rename = "CapacityInUse", default)]
    in_use: u64,
    #[serde(rename = "Encryption", default)]
    encrypted: bool,
    #[serde(rename = "Locked", default)]
    locked: bool,
}

/// Matches `/dev/disk3s1s1` (a sealed system snapshot) or `/dev/disk3s5` to volume
/// `disk3s1` / `disk3s5`.
fn device_matches(mount_device: &str, volume_device: &str) -> bool {
    let Some(dev) = mount_device.strip_prefix("/dev/") else {
        return false;
    };
    if dev == volume_device {
        return true;
    }
    dev.strip_prefix(volume_device)
        .and_then(|rest| rest.strip_prefix('s'))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Parses `diskutil apfs list -plist` output and attaches mount points.
pub fn parse_topology(plist_bytes: &[u8], mounts: &[MountInfo]) -> Result<Vec<ApfsContainer>, ApfsError> {
    let raw: RawList = plist::from_bytes(plist_bytes).map_err(|e| ApfsError::Parse(e.to_string()))?;
    Ok(raw
        .containers
        .into_iter()
        .map(|c| ApfsContainer {
            reference: c.reference,
            uuid: c.uuid,
            physical_store: c.physical_store,
            capacity_bytes: c.capacity,
            free_bytes: c.free,
            volumes: c
                .volumes
                .into_iter()
                .map(|v| {
                    // Prefer the shortest mount point: /System/Volumes/Data over its
                    // nested autofs maps.
                    let mount_point = mounts
                        .iter()
                        .filter(|m| device_matches(&m.device, &v.device))
                        .map(|m| m.mount_point.clone())
                        .min_by_key(|p| p.as_os_str().len());
                    ApfsVolume {
                        device: v.device,
                        uuid: v.uuid,
                        name: v.name,
                        roles: v.roles,
                        in_use_bytes: v.in_use,
                        encrypted: v.encrypted,
                        locked: v.locked,
                        mount_point,
                    }
                })
                .collect(),
        })
        .collect())
}

pub fn topology(mounts: &[MountInfo]) -> Result<Vec<ApfsContainer>, ApfsError> {
    let out = command::run("/usr/sbin/diskutil", &["apfs", "list", "-plist"], TIMEOUT)?;
    parse_topology(&out, mounts)
}

/// Kind of a local APFS snapshot, from its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SnapshotKind {
    TimeMachine,
    OsUpdate,
    Other,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalSnapshot {
    pub volume_device: String,
    pub volume_name: String,
    pub name: String,
    pub uuid: String,
    pub xid: u64,
    pub kind: SnapshotKind,
    /// Creation time parsed from a Time Machine snapshot name (local time).
    pub created: Option<String>,
    /// diskutil: the OS may delete it automatically when space is needed.
    pub purgeable: bool,
    /// diskutil: this snapshot currently limits shrinking the container.
    pub limiting_container_shrink: bool,
    /// tmutil marks the snapshot "(dataless)": its data has already been released.
    pub dataless: bool,
}

#[derive(Deserialize)]
struct RawSnapshots {
    #[serde(rename = "Snapshots", default)]
    snapshots: Vec<RawSnapshot>,
}

#[derive(Deserialize)]
struct RawSnapshot {
    #[serde(rename = "SnapshotName", default)]
    name: String,
    #[serde(rename = "SnapshotUUID", default)]
    uuid: String,
    #[serde(rename = "SnapshotXID", default)]
    xid: u64,
    #[serde(rename = "Purgeable", default)]
    purgeable: bool,
    #[serde(rename = "LimitingContainerShrink", default)]
    limiting: bool,
}

fn classify(name: &str) -> (SnapshotKind, Option<String>) {
    if let Some(rest) = name.strip_prefix("com.apple.TimeMachine.") {
        // com.apple.TimeMachine.2026-10-21-011905.local
        let stamp = rest.trim_end_matches(".local");
        let created = chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d-%H%M%S")
            .ok()
            .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string());
        (SnapshotKind::TimeMachine, created)
    } else if name.starts_with("com.apple.os.update-") {
        (SnapshotKind::OsUpdate, None)
    } else {
        (SnapshotKind::Other, None)
    }
}

pub fn parse_snapshots(plist_bytes: &[u8], volume: &ApfsVolume) -> Result<Vec<LocalSnapshot>, ApfsError> {
    let raw: RawSnapshots = plist::from_bytes(plist_bytes).map_err(|e| ApfsError::Parse(e.to_string()))?;
    Ok(raw
        .snapshots
        .into_iter()
        .map(|s| {
            let (kind, created) = classify(&s.name);
            LocalSnapshot {
                volume_device: volume.device.clone(),
                volume_name: volume.name.clone(),
                name: s.name,
                uuid: s.uuid,
                xid: s.xid,
                kind,
                created,
                purgeable: s.purgeable,
                limiting_container_shrink: s.limiting,
                dataless: false,
            }
        })
        .collect())
}

/// Names that `tmutil listlocalsnapshots /` marks "(dataless)".
pub fn parse_tmutil_dataless(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.trim().strip_suffix("(dataless)"))
        .map(|n| n.trim().to_string())
        .collect()
}

/// Local snapshots of the System and Data volumes in the container holding `/`,
/// and of every other mounted, browsable APFS volume.
pub fn local_snapshots(containers: &[ApfsContainer]) -> (Vec<LocalSnapshot>, Vec<String>) {
    let mut snapshots = Vec::new();
    let mut errors = Vec::new();
    for c in containers {
        for v in &c.volumes {
            let interesting = v.has_role("System") || v.has_role("Data") || (v.roles.is_empty() && v.mount_point.as_ref().is_some_and(|m| m.starts_with("/Volumes")));
            if !interesting || v.locked {
                continue;
            }
            match command::run("/usr/sbin/diskutil", &["apfs", "listSnapshots", "-plist", &v.device], TIMEOUT)
                .map_err(ApfsError::from)
                .and_then(|out| parse_snapshots(&out, v))
            {
                Ok(mut s) => snapshots.append(&mut s),
                Err(e) => errors.push(format!("{} ({}): {e}", v.name, v.device)),
            }
        }
    }
    if let Ok(out) = command::run("/usr/bin/tmutil", &["listlocalsnapshots", "/"], TIMEOUT) {
        let dataless = parse_tmutil_dataless(&String::from_utf8_lossy(&out));
        for s in &mut snapshots {
            if dataless.iter().any(|d| d == &s.name) {
                s.dataless = true;
            }
        }
    }
    (snapshots, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>Containers</key><array><dict>
<key>APFSContainerUUID</key><string>AF60</string>
<key>CapacityCeiling</key><integer>3996329328640</integer>
<key>CapacityFree</key><integer>113119731712</integer>
<key>ContainerReference</key><string>disk3</string>
<key>DesignatedPhysicalStore</key><string>disk0s2</string>
<key>Fusion</key><false/>
<key>Volumes</key><array>
 <dict><key>APFSVolumeUUID</key><string>3714</string><key>CapacityInUse</key><integer>18471469056</integer>
  <key>DeviceIdentifier</key><string>disk3s1</string><key>Encryption</key><true/><key>Locked</key><false/>
  <key>Name</key><string>Macintosh HD</string><key>Roles</key><array><string>System</string></array></dict>
 <dict><key>APFSVolumeUUID</key><string>C8F3</string><key>CapacityInUse</key><integer>3843796258816</integer>
  <key>DeviceIdentifier</key><string>disk3s5</string><key>Name</key><string>Macintosh HD - Data</string>
  <key>Roles</key><array><string>Data</string></array></dict>
</array></dict></array></dict></plist>"#;

    fn mount(device: &str, at: &str) -> MountInfo {
        MountInfo {
            mount_point: at.into(),
            device: device.into(),
            fs_type: "apfs".into(),
            total_bytes: 0,
            free_bytes: 0,
            available_bytes: 0,
            read_only: false,
            local: true,
            browsable: true,
            dev: 0,
        }
    }

    #[test]
    fn parses_topology_and_maps_mounts() {
        let mounts =
            [mount("/dev/disk3s1s1", "/"), mount("/dev/disk3s5", "/System/Volumes/Data"), mount("/dev/disk3s10", "/x")];
        let c = parse_topology(LIST.as_bytes(), &mounts).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].capacity_bytes, 3_996_329_328_640);
        assert_eq!(c[0].volumes[0].mount_point.as_deref(), Some(std::path::Path::new("/")));
        assert!(c[0].volumes[0].has_role("System"));
        assert_eq!(c[0].volumes[1].mount_point.as_deref(), Some(std::path::Path::new("/System/Volumes/Data")));
        assert!(!device_matches("/dev/disk3s10", "disk3s1"));
    }

    #[test]
    fn classifies_snapshot_names() {
        let (k, created) = classify("com.apple.TimeMachine.2026-10-21-011905.local");
        assert_eq!(k, SnapshotKind::TimeMachine);
        assert_eq!(created.as_deref(), Some("2026-10-21 01:19:05"));
        assert_eq!(classify("com.apple.os.update-ABC").0, SnapshotKind::OsUpdate);
        assert_eq!(classify("custom").0, SnapshotKind::Other);
    }

    #[test]
    fn parses_dataless_marker() {
        let text = "Snapshots for volume group containing disk /:\ncom.apple.TimeMachine.2026-10-07-142819.local (dataless)\ncom.apple.TimeMachine.2026-10-21-011905.local\n";
        assert_eq!(parse_tmutil_dataless(text), ["com.apple.TimeMachine.2026-10-07-142819.local"]);
    }
}
