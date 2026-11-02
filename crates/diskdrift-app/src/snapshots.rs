//! Persistent scans and comparisons: `scan`, `snapshots`, `top`, `diff`.

use crate::config::Effective;
use diskdrift_core::browse::{self, DiffListing, Listing};
use diskdrift_core::diff::{self, DiffOptions, DiffReport};
use diskdrift_core::native::{NativeName, NativePath};
use diskdrift_core::paths::components_under;
use diskdrift_core::record::{FileIdScheme, Record};
use diskdrift_core::top::{self, TopOptions, TopReport};
use diskdrift_core::walk::{self, ExcludeRules, Progress, WalkError, WalkOptions};
use diskdrift_diagnostics::measurement::{Envelope, Quality};
use diskdrift_platform::{DiskInfo, ListerKind, MeasurementQuality as PlatformMeasurementQuality};
use diskdrift_storage::{
    MeasurementQuality, MigrationOutcome, Rebase, ScanOptions, SnapshotMeta, SpaceSample, Status,
    Store, StoreError, VolumeIdentity, meta,
};
use serde::{Serialize, Serializer};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// Bad input: exit code 2.
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{0}")]
    Runtime(String),
    /// The scanned disk matches a known one only doubtfully: the user says
    /// whether it is that disk (`ScanRequest::disk_choice`). Exit code 2.
    #[error("{0}")]
    DiskDoubt(String),
}

impl AppError {
    pub fn is_usage(&self) -> bool {
        matches!(
            self,
            AppError::Usage(_)
                | AppError::DiskDoubt(_)
                | AppError::Store(
                    StoreError::NotFound(_)
                        | StoreError::DuplicateName(_)
                        | StoreError::NoData { .. }
                        | StoreError::UnknownDisk(_)
                        | StoreError::DuplicateLabel { .. }
                        | StoreError::InvalidLabel { .. }
                        | StoreError::Invalid(_)
                )
        )
    }
}

fn core_store_error(error: std::io::Error) -> AppError {
    if let Some(path) = error
        .get_ref()
        .and_then(|cause| cause.downcast_ref::<diskdrift_core::paths::PathOrderError>())
    {
        AppError::Store(StoreError::PathOrder(*path))
    } else {
        AppError::Store(StoreError::Corrupt(error.to_string()))
    }
}

pub type Result<T> = std::result::Result<T, AppError>;

fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// OS-reported space of the volume holding `path`.
pub fn space_sample(path: &Path) -> Option<SpaceSample> {
    let m = diskdrift_platform::statfs_path(path).ok()?;
    let (cap, capacity_error) = match diskdrift_platform::volume_capacity(path) {
        Ok(cap) => (cap, None),
        Err(error) => (Default::default(), Some(error.to_string())),
    };
    let statfs_observed = m.total_bytes > 0;
    let quality = match cap.quality {
        PlatformMeasurementQuality::Unknown if statfs_observed => MeasurementQuality::Observed,
        PlatformMeasurementQuality::Observed => MeasurementQuality::Observed,
        PlatformMeasurementQuality::Estimated => MeasurementQuality::Estimated,
        PlatformMeasurementQuality::LowerBound => MeasurementQuality::LowerBound,
        PlatformMeasurementQuality::Unsupported => MeasurementQuality::Unsupported,
        PlatformMeasurementQuality::NotApplicable => MeasurementQuality::NotApplicable,
        PlatformMeasurementQuality::Unknown => MeasurementQuality::Unknown,
    };
    let provenance = match (cap.provenance, capacity_error) {
        (Some(mut provenance), Some(error)) => {
            provenance.push_str("; volume capacity unavailable: ");
            provenance.push_str(&error);
            Some(provenance)
        }
        (Some(provenance), None) => Some(provenance),
        (None, Some(error)) => Some(format!("volume capacity unavailable: {error}")),
        (None, None) => statfs_observed.then(|| "statfs".into()),
    };
    Some(SpaceSample {
        total_bytes: Some(m.total_bytes),
        free_bytes: Some(m.free_bytes),
        available_bytes: Some(m.available_bytes),
        available_important_bytes: cap.available_important_bytes,
        available_opportunistic_bytes: cap.available_opportunistic_bytes,
        domain: cap
            .domain
            .or_else(|| statfs_observed.then(|| "filesystem".into())),
        scope: cap
            .scope
            .or_else(|| statfs_observed.then(|| "mount".into())),
        observed_at: cap.observed_at.or_else(|| Some(now_rfc3339())),
        quality,
        provenance,
        timestamp_resolution: cap.timestamp_resolution,
    })
}

fn space_measurement(old: &SpaceSample, new: &SpaceSample) -> Envelope {
    let quality = [old.quality, new.quality]
        .into_iter()
        .map(|quality| match quality {
            MeasurementQuality::Unknown => Quality::Unknown,
            MeasurementQuality::Unsupported => Quality::Unsupported,
            MeasurementQuality::NotApplicable => Quality::NotApplicable,
            MeasurementQuality::LowerBound => Quality::LowerBound,
            MeasurementQuality::Estimated => Quality::Estimated,
            MeasurementQuality::Observed => Quality::Observed,
        })
        .max_by_key(|quality| match quality {
            Quality::Observed => 0,
            Quality::Estimated => 1,
            Quality::LowerBound => 2,
            Quality::NotApplicable => 3,
            Quality::Unsupported => 4,
            Quality::Unknown => 5,
        })
        .unwrap_or(Quality::Unknown);
    let provenance = match (&old.provenance, &new.provenance) {
        (Some(old), Some(new)) if old == new => Some(old.clone()),
        (Some(old), Some(new)) => Some(format!("{old}; {new}")),
        (Some(value), None) | (None, Some(value)) => Some(value.clone()),
        (None, None) => None,
    };
    let mut envelope = Envelope::with_quality(
        new.domain
            .clone()
            .or_else(|| old.domain.clone())
            .unwrap_or_else(|| "filesystem".into()),
        new.scope
            .clone()
            .or_else(|| old.scope.clone())
            .unwrap_or_else(|| "volume".into()),
        quality,
        provenance
            .clone()
            .unwrap_or_else(|| "OS-reported space samples".into()),
    );
    envelope.observed_at = new
        .observed_at
        .clone()
        .or_else(|| old.observed_at.clone())
        .or(envelope.observed_at);
    envelope.domain = new
        .domain
        .clone()
        .or_else(|| old.domain.clone())
        .or(envelope.domain);
    envelope.scope = new
        .scope
        .clone()
        .or_else(|| old.scope.clone())
        .or(envelope.scope);
    envelope.provenance = provenance.or(envelope.provenance);
    envelope
}

fn volume_descriptor(info: &DiskInfo) -> Option<meta::VolumeDescriptor> {
    let identity = info
        .native_identity
        .clone()
        .or_else(|| {
            info.volume_uuid
                .as_deref()
                .map(|uuid| diskdrift_core::native::NativePath::unix(uuid.as_bytes().to_vec()))
        })
        .or_else(|| {
            (!info.mount_point.as_os_str().is_empty())
                .then(|| diskdrift_platform::native_path(info.mount_point.as_os_str()))
        })?;
    let mount_key = info.native_mount_path.clone().or_else(|| {
        Some(diskdrift_platform::native_path(
            info.mount_point.as_os_str(),
        ))
    })?;
    let mut aliases = info.native_aliases.clone();
    aliases.sort();
    aliases.dedup();
    let identity_strength = if info.volume_uuid.is_some() {
        "uuid"
    } else {
        "weak"
    };
    let file_id_scheme =
        if !cfg!(windows) && matches!(info.file_id_scheme, FileIdScheme::Unavailable) {
            FileIdScheme::PosixInode64
        } else {
            info.file_id_scheme
        };
    let timestamp_resolution = if !cfg!(windows)
        && matches!(
            info.timestamp_resolution,
            diskdrift_core::record::TimestampResolution::Unknown
        ) {
        diskdrift_core::record::TimestampResolution::Nanoseconds
    } else {
        info.timestamp_resolution
    };
    let allocation = match info.allocation_semantics.as_str() {
        "filesystem_reported" => meta::AllocationSemantics::FilesystemReported,
        "shared_possible" => meta::AllocationSemantics::SharedPossible,
        _ if !cfg!(windows) && matches!(file_id_scheme, FileIdScheme::PosixInode64) => {
            meta::AllocationSemantics::FilesystemReported
        }
        _ => meta::AllocationSemantics::Unknown,
    };
    Some(meta::VolumeDescriptor {
        index: 0,
        platform: std::env::consts::OS.into(),
        identity,
        identity_strength: identity_strength.into(),
        file_id_scheme,
        file_id_scope: info.file_id_scope,
        mount_key,
        aliases,
        allocation,
        allocation_provenance: info
            .allocation_provenance
            .clone()
            .map(meta::Provenance::Observed)
            .or_else(|| {
                (!cfg!(windows) && matches!(file_id_scheme, FileIdScheme::PosixInode64))
                    .then(|| meta::Provenance::Observed("stat allocation blocks".into()))
            })
            .unwrap_or(meta::Provenance::Unknown),
        timestamp_resolution,
        provenance: meta::Provenance::Observed("platform volume adapter".into()),
    })
}

fn fallback_volume_descriptor(mount: &diskdrift_platform::MountInfo) -> meta::VolumeDescriptor {
    let mount_key = diskdrift_platform::native_path(mount.mount_point.as_os_str());
    let mut descriptor = meta::VolumeDescriptor::new(
        0,
        std::env::consts::OS,
        if cfg!(windows) {
            FileIdScheme::Unavailable
        } else {
            FileIdScheme::PosixInode64
        },
        meta::FileIdScope::Volume,
        if cfg!(windows) {
            meta::AllocationSemantics::Unknown
        } else {
            meta::AllocationSemantics::FilesystemReported
        },
        if cfg!(windows) {
            diskdrift_core::record::TimestampResolution::Unknown
        } else {
            diskdrift_core::record::TimestampResolution::Nanoseconds
        },
        if cfg!(windows) {
            meta::Provenance::Unknown
        } else {
            meta::Provenance::Observed("stat allocation blocks".into())
        },
    );
    descriptor.identity = mount_key.clone();
    descriptor.identity_strength = "weak".into();
    descriptor.mount_key = mount_key;
    descriptor
}

fn scan_volume_table(
    layout: &diskdrift_platform::ScanLayout,
    root_mount: &diskdrift_platform::MountInfo,
    root_info: Option<&DiskInfo>,
    previous: Option<&SnapshotMeta>,
    mounts: &[diskdrift_platform::MountInfo],
) -> Result<(Vec<u64>, Vec<meta::VolumeDescriptor>)> {
    let mut by_device = HashMap::<u64, meta::VolumeDescriptor>::new();
    if let Some(previous) = previous
        && let Some(counts) = &previous.counts
    {
        for (&device, descriptor) in counts.devices.iter().zip(&previous.volumes) {
            by_device
                .entry(device)
                .or_insert_with(|| descriptor.clone());
        }
    }

    let descriptor_for = |mount: &diskdrift_platform::MountInfo| {
        if mount.mount_point == root_mount.mount_point {
            root_info
                .and_then(volume_descriptor)
                .unwrap_or_else(|| fallback_volume_descriptor(mount))
        } else {
            diskdrift_platform::disk_info(&mount.mount_point)
                .ok()
                .and_then(|info| volume_descriptor(&info))
                .unwrap_or_else(|| fallback_volume_descriptor(mount))
        }
    };
    by_device.insert(root_mount.dev, descriptor_for(root_mount));
    for &device in &layout.same_filesystem_devs {
        by_device
            .entry(device)
            .or_insert_with(|| fallback_volume_descriptor(root_mount));
    }
    for mount in mounts {
        by_device.insert(mount.dev, descriptor_for(mount));
    }

    let mut devices = Vec::new();
    let mut seen = HashSet::new();
    if let Some(previous) = previous
        && let Some(counts) = &previous.counts
    {
        for &device in &counts.devices {
            if seen.insert(device) && by_device.contains_key(&device) {
                devices.push(device);
            }
        }
    }
    for mount in mounts {
        if seen.insert(mount.dev) {
            devices.push(mount.dev);
        }
    }
    if seen.insert(root_mount.dev) {
        devices.push(root_mount.dev);
    }
    if devices.len() > usize::from(u16::MAX) {
        return Err(AppError::Runtime(
            "scan encountered more volumes than the snapshot format can index".into(),
        ));
    }

    let table = devices
        .iter()
        .enumerate()
        .map(|(index, device)| {
            let mut descriptor = by_device
                .get(device)
                .cloned()
                .ok_or_else(|| AppError::Runtime(format!("no descriptor for device {device}")))?;
            descriptor.index = index as u16;
            Ok(descriptor)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((devices, table))
}

/// Opens the configured store if it exists (read-only commands never create it).
pub fn open_store(settings: &Effective) -> Result<Option<Store>> {
    let p = &settings.storage_path.value;
    if !Store::exists(p) {
        return Ok(None);
    }
    Ok(Some(Store::open(p)?))
}

pub(crate) fn require_store(settings: &Effective) -> Result<Store> {
    open_store(settings)?.ok_or_else(|| {
        AppError::Usage(format!(
            "no snapshots yet in {} (run `diskdrift scan` first)",
            settings.storage_path.value.display()
        ))
    })
}

#[derive(Debug, Clone)]
pub struct ScanRequest {
    pub root: PathBuf,
    pub root_native: Option<diskdrift_core::native::NativeRoot>,
    pub name: Option<String>,
    /// Exclusion patterns (config plus command line).
    pub exclude: Vec<String>,
    pub threads: usize,
    pub stay_on_filesystem: bool,
    pub lister: ListerKind,
    /// Run at low CPU and I/O priority (the automatic scan at startup).
    pub background: bool,
    /// The answer when the disk matches a known one only doubtfully.
    pub disk_choice: Option<crate::disks::DiskChoice>,
    /// A careful scan (spec 3C): one thread at low priority, marked careful.
    pub careful: bool,
}

impl ScanRequest {
    pub fn from_settings(
        settings: &Effective,
        root: Option<PathBuf>,
        name: Option<String>,
        extra_exclude: &[String],
    ) -> Self {
        Self::from_settings_with_native_root(settings, root, None, name, extra_exclude)
    }

    pub fn from_settings_with_native_root(
        settings: &Effective,
        root: Option<PathBuf>,
        root_native: Option<diskdrift_core::native::NativeRoot>,
        name: Option<String>,
        extra_exclude: &[String],
    ) -> Self {
        let mut exclude = settings.exclude.value.clone();
        exclude.extend(extra_exclude.iter().cloned());
        ScanRequest {
            root_native: root_native.or_else(|| {
                root.is_none()
                    .then(|| settings.default_root_native.value.clone())
            }),
            root: root.unwrap_or_else(|| settings.default_root.value.clone()),
            name,
            exclude,
            threads: settings.threads.value,
            stay_on_filesystem: settings.stay_on_filesystem.value,
            lister: ListerKind::Auto,
            background: false,
            disk_choice: None,
            careful: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScanOutcome {
    pub snapshot: SnapshotMeta,
    pub cancelled: bool,
    pub store_path: PathBuf,
    pub store_bytes: u64,
    /// What happened to the previous snapshot of the same root.
    pub rebase: Rebase,
    /// The previous snapshot could not be compacted (it stays full and valid).
    pub rebase_error: Option<String>,
    /// The disk the scan is recorded against (`None` when the OS could not
    /// describe the volume; the store then derives it from the metadata).
    pub disk: Option<crate::disks::ScanDisk>,
    pub migration: MigrationOutcome,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanOutcomeWire<'a> {
    snapshot: &'a SnapshotMeta,
    cancelled: bool,
    store_path: &'a PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    store_bytes: Option<u64>,
    rebase: &'a Rebase,
    rebase_error: &'a Option<String>,
    disk: &'a Option<crate::disks::ScanDisk>,
    migration: MigrationOutcome,
    #[serde(flatten)]
    measurement: Envelope,
}

impl Serialize for ScanOutcome {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let known = self.store_bytes > 0;
        ScanOutcomeWire {
            snapshot: &self.snapshot,
            cancelled: self.cancelled,
            store_path: &self.store_path,
            store_bytes: known.then_some(self.store_bytes),
            rebase: &self.rebase,
            rebase_error: &self.rebase_error,
            disk: &self.disk,
            migration: self.migration,
            measurement: if known {
                Envelope::observed("diskdrift store", "snapshot store", "store accounting")
            } else {
                Envelope::default()
            },
        }
        .serialize(serializer)
    }
}

fn resolve_root(
    root: &Path,
    exact: Option<&diskdrift_core::native::NativeRoot>,
) -> Result<PathBuf> {
    if let Some(exact) = exact {
        exact
            .validate()
            .map_err(|e| AppError::Usage(format!("scan root {}: {e}", exact.path.display())))?;
    }
    let root = exact
        .map(|r| {
            diskdrift_platform::path_from_native(&r.path)
                .map_err(|e| AppError::Usage(format!("scan root {}: {e}", r.path.display())))
        })
        .transpose()?
        .unwrap_or_else(|| root.to_path_buf());
    let abs = std::path::absolute(&root)
        .map_err(|e| AppError::Usage(format!("{}: {e}", root.display())))?;
    let canon = abs
        .canonicalize()
        .map_err(|e| AppError::Usage(format!("scan root {}: {e}", abs.display())))?;
    if !canon.is_dir() {
        return Err(AppError::Usage(format!(
            "scan root {} is not a directory",
            canon.display()
        )));
    }
    Ok(canon)
}

/// Default snapshot name: the local start time.
pub fn default_name() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H-%M-%S").to_string()
}

/// Whether `name` is the start time `default_name` gives: it says nothing the
/// start time does not.
pub fn is_default_name(name: &str) -> bool {
    chrono::NaiveDateTime::parse_from_str(name, "%Y-%m-%dT%H-%M-%S").is_ok()
}

/// "scan #4 of 2026-10-21 13:13 (25 min ago)": where reused sizes come from.
pub fn scan_label(m: &SnapshotMeta, now: i64) -> String {
    let started = chrono::DateTime::parse_from_rfc3339(&m.started_at)
        .map(|t| t.timestamp())
        .unwrap_or(0);
    let ago = match now - started {
        s if s < 3600 => format!("{} min ago", (s / 60).max(1)),
        s => crate::analyze::ago(s),
    };
    format!(
        "scan #{} of {} ({ago})",
        m.id,
        m.started_at
            .get(..16)
            .unwrap_or(&m.started_at)
            .replace('T', " ")
    )
}

/// Below this much available space no automatic scan starts (it writes to the
/// store on the same disk).
pub const AUTO_SCAN_MIN_FREE: u64 = 2_000_000_000;

/// Whether the TUI should rescan in the background at start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoScan {
    Start {
        root: PathBuf,
        name: String,
        /// Entries of the previous scan of the root, for a progress estimate.
        expected_entries: Option<u64>,
    },
    /// Not started, and why.
    Skip(String),
}

/// The rule for the automatic scan: enabled, the user has scanned this root
/// before (the first scan is always explicit), the latest scan is older than
/// the configured age and the disk is not nearly full.
pub fn auto_scan(settings: &Effective, list: &SnapshotList, now: i64) -> AutoScan {
    let root = settings
        .default_root
        .value
        .canonicalize()
        .unwrap_or_else(|_| settings.default_root.value.clone());
    if !settings.auto_scan.value {
        return AutoScan::Skip("automatic scan is off (Settings)".into());
    }
    let Some(latest) = list
        .snapshots
        .iter()
        .rev()
        .map(|r| &r.meta)
        .find(|m| m.status.is_usable() && Path::new(&m.root) == root)
    else {
        return AutoScan::Skip(format!(
            "no earlier scan of {}: the first one is up to you (S)",
            root.display()
        ));
    };
    let started = chrono::DateTime::parse_from_rfc3339(&latest.started_at)
        .map(|t| t.timestamp())
        .unwrap_or(0);
    let age = now - started;
    let max = settings.auto_max_age_hours.value as i64 * 3600;
    if age < max {
        return AutoScan::Skip(format!(
            "latest scan of {} is {} old; the next automatic one after {} h",
            root.display(),
            crate::analyze::age(age).replace("just now", "under a minute"),
            settings.auto_max_age_hours.value
        ));
    }
    if let Some(s) = space_sample(&root)
        && let Some(avail) = s.available_important_bytes.or(s.available_bytes)
        && avail < AUTO_SCAN_MIN_FREE
    {
        return AutoScan::Skip(format!(
            "only {} available: no automatic scan",
            diskdrift_core::units::format_size(avail, settings.size_units.value)
        ));
    }
    AutoScan::Start {
        root,
        name: format!("auto-{}", default_name()),
        expected_entries: latest.counts.as_ref().map(|c| c.entries),
    }
}

/// Runs a full metadata scan and stores it as a snapshot.
pub fn scan(
    settings: &Effective,
    req: &ScanRequest,
    cancel: &AtomicBool,
    progress: Option<&Progress>,
) -> Result<ScanOutcome> {
    let requested_root_native = req.root_native.clone();
    let root = resolve_root(&req.root, req.root_native.as_ref())?;
    let store_path = std::path::absolute(&settings.storage_path.value)
        .map_err(|e| AppError::Runtime(e.to_string()))?;
    let store = Store::create(&store_path)?;
    let (lock, migration) = store.lock()?;
    let name = req.name.clone().unwrap_or_else(default_name);

    let layout = diskdrift_platform::scan_layout(&root)
        .map_err(|e| AppError::Runtime(format!("{}: {e}", root.display())))?;
    let mount = diskdrift_platform::statfs_path(&root)
        .map_err(|e| AppError::Runtime(format!("{}: {e}", root.display())))?;
    let mounts = diskdrift_platform::mounts()
        .map_err(|e| AppError::Runtime(format!("reading mounted volumes: {e}")))?
        .mounts;
    let cap = diskdrift_platform::volume_capacity(&root)
        .map_err(|e| AppError::Runtime(format!("{}: {e}", root.display())))?;
    let root_native = match requested_root_native {
        Some(root_native) => root_native,
        None => diskdrift_platform::native_root(root.as_os_str())
            .map_err(|e| AppError::Usage(format!("scan root {}: {e}", root.display())))?,
    };
    let root_in_volume_native = meta::root_in_volume_native(
        &root_native.path,
        &diskdrift_platform::native_path(mount.mount_point.as_os_str()),
    );
    let started_at = now_rfc3339();
    let disk_info = diskdrift_platform::disk_info(&root).ok();
    let disk = match &disk_info {
        Some(info) => Some(crate::disks::record_scan_disk_with_lock(
            &store,
            &lock,
            info,
            &started_at,
            req.disk_choice,
        )?),
        None => None,
    };
    let listed = store.list()?;
    let previous = listed
        .iter()
        .rev()
        .find(|m| m.status.is_usable() && Path::new(&m.root) == root);
    let (known_devices, volumes) =
        scan_volume_table(&layout, &mount, disk_info.as_ref(), previous, &mounts)?;
    let root_volume_index = known_devices
        .iter()
        .position(|device| *device == mount.dev)
        .ok_or_else(|| AppError::Runtime("scan root volume has no descriptor".into()))?;
    let descriptor = volumes[root_volume_index].clone();
    let lister = diskdrift_platform::lister(req.lister);
    // Careful: one thread spares a weak disk; low priority like a background scan.
    let gentle = req.background || req.careful;
    let mut opts = WalkOptions {
        threads: if req.careful { 1 } else { req.threads },
        stay_on_filesystem: req.stay_on_filesystem,
        same_filesystem_devs: layout.same_filesystem_devs.clone(),
        alias_paths: layout.alias_paths.clone(),
        exclude: ExcludeRules::from_patterns(&req.exclude),
        on_thread_start: gentle.then_some(diskdrift_platform::lower_thread_priority as fn()),
        ..WalkOptions::default()
    };
    if gentle {
        diskdrift_platform::lower_thread_priority();
    }
    // Never scan our own store: it changes while we write it.
    if let Ok(canon_store) = store_path.canonicalize()
        && canon_store.starts_with(&root)
    {
        opts.alias_paths.insert(canon_store);
    }
    let mut template = SnapshotMeta {
        format_version: meta::META_FORMAT_VERSION,
        id: 0,
        name,
        root: root.to_string_lossy().into_owned(),
        root_native: Some(root_native),
        disk: disk.as_ref().map(|d| d.disk.id.clone()),
        root_in_volume: None,
        root_in_volume_native,
        status: Status::InProgress,
        started_at,
        finished_at: None,
        volume: VolumeIdentity {
            // Disk Arbitration's UUID: stable for FAT volumes (ADR 0002, 2.2).
            uuid: disk_info
                .as_ref()
                .and_then(|i| i.volume_uuid.clone())
                .or_else(|| cap.volume_uuid.clone()),
            name: cap.volume_name.clone(),
            fs_type: mount.fs_type.clone(),
            mount_point: mount.mount_point.to_string_lossy().into_owned(),
            volume_group: layout.volume_group,
            descriptor: Some(descriptor.clone()),
        },
        volumes,
        options: ScanOptions {
            stay_on_filesystem: req.stay_on_filesystem,
            exclude: opts.exclude.patterns(),
            lister: lister.name().to_string(),
            threads: opts.effective_threads(),
            careful: req.careful,
        },
        space_before: space_sample(&root),
        space_after: None,
        storage: Default::default(),
        counts: None,
        error: None,
        diskdrift_version: crate::VERSION.to_string(),
    };
    template.root_in_volume = Some(template.derived_root_in_volume());
    // Keep the walk's device indexes identical to the metadata table.
    opts.known_devices = known_devices;
    let mut writer = store.begin(lock, template, None)?;

    let mut store_error: Option<StoreError> = None;
    let mut sink = |rec: &Record, _: &Path| -> std::io::Result<()> {
        writer.push(rec).map_err(|e| {
            let msg = e.to_string();
            store_error = Some(e);
            std::io::Error::other(msg)
        })
    };
    let result = walk::walk(&root, lister.as_ref(), &opts, &mut sink, cancel, progress);
    let finished = now_rfc3339();
    let snapshot = match result {
        Err(WalkError::Root { path, source }) => {
            let m = writer.abort(
                Status::Failed,
                Some(format!("{path}: {source}")),
                None,
                finished,
            );
            return Err(AppError::Runtime(m.error.unwrap_or_default()));
        }
        Err(WalkError::Sink(e)) => {
            let err = store_error
                .take()
                .map(AppError::Store)
                .unwrap_or_else(|| AppError::Runtime(e.to_string()));
            writer.abort(Status::Failed, Some(err.to_string()), None, finished);
            return Err(err);
        }
        Ok(report) if report.cancelled => {
            let m = writer.abort(Status::Cancelled, None, Some(report), finished);
            return Ok(ScanOutcome {
                snapshot: m,
                cancelled: true,
                store_bytes: store.size_on_disk(),
                store_path,
                rebase: Rebase::Nothing,
                rebase_error: None,
                disk,
                migration,
            });
        }
        Ok(report) => writer.commit_and_rebase(report, space_sample(&root), finished)?,
    };
    let (snapshot, rebase) = snapshot;
    let (rebase, rebase_error) = match rebase {
        Ok(r) => (r, None),
        Err(e) => (Rebase::Nothing, Some(e)),
    };
    Ok(ScanOutcome {
        snapshot,
        cancelled: false,
        store_bytes: store.size_on_disk(),
        store_path,
        rebase,
        rebase_error,
        disk,
        migration,
    })
}

#[derive(Debug, Clone)]
pub struct SnapshotRow {
    pub meta: SnapshotMeta,
    /// Allocated bytes of this snapshot's own files.
    pub size_on_disk: u64,
    /// The disk's label, else its volume name, else its short ID.
    pub disk_name: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotRowWire<'a> {
    #[serde(flatten)]
    meta: &'a SnapshotMeta,
    size_on_disk: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    disk_name: &'a Option<String>,
    #[serde(flatten)]
    measurement: Envelope,
}

impl Serialize for SnapshotRow {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        SnapshotRowWire {
            meta: &self.meta,
            size_on_disk: self.size_on_disk,
            disk_name: &self.disk_name,
            measurement: Envelope::observed(
                "diskdrift store",
                "snapshot files",
                "store accounting",
            ),
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotList {
    pub store_path: PathBuf,
    pub store_exists: bool,
    pub store_bytes: u64,
    pub snapshots: Vec<SnapshotRow>,
    /// Disks with snapshots in the store (all of them, also when filtered).
    pub disk_count: usize,
    /// Only this disk's snapshots are listed (`snapshots --disk`).
    pub disk: Option<crate::storage::Disk>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotListWire<'a> {
    store_path: &'a PathBuf,
    store_exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    store_bytes: Option<u64>,
    snapshots: &'a [SnapshotRow],
    disk_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    disk: &'a Option<crate::storage::Disk>,
    #[serde(flatten)]
    measurement: Envelope,
}

impl Serialize for SnapshotList {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        SnapshotListWire {
            store_path: &self.store_path,
            store_exists: self.store_exists,
            store_bytes: self.store_exists.then_some(self.store_bytes),
            snapshots: &self.snapshots,
            disk_count: self.disk_count,
            disk: &self.disk,
            measurement: if self.store_exists {
                Envelope::observed("diskdrift store", "snapshot store", "store accounting")
            } else {
                Envelope::default()
            },
        }
        .serialize(serializer)
    }
}

pub fn list(settings: &Effective) -> Result<SnapshotList> {
    list_disk(settings, None)
}

/// The snapshots, all or those of the disk `disk` names.
pub fn list_disk(settings: &Effective, disk: Option<&str>) -> Result<SnapshotList> {
    let store_path = settings.storage_path.value.clone();
    let Some(store) = open_store(settings)? else {
        if let Some(d) = disk {
            return Err(AppError::Usage(format!(
                "no disk '{d}': there are no snapshots yet in {}",
                store_path.display()
            )));
        }
        return Ok(SnapshotList {
            store_path,
            store_exists: false,
            store_bytes: 0,
            snapshots: Vec::new(),
            disk_count: 0,
            disk: None,
        });
    };
    let reg = store.registry()?;
    let filter = disk.map(|d| crate::disks::resolve(&store, d)).transpose()?;
    let metas = store.list()?;
    let mut ids: Vec<&str> = metas.iter().filter_map(|m| m.disk.as_deref()).collect();
    ids.sort_unstable();
    ids.dedup();
    let disk_count = ids.len();
    let snapshots = metas
        .into_iter()
        .filter(|m| {
            filter
                .as_ref()
                .is_none_or(|d| m.disk.as_deref() == Some(d.id.as_str()))
        })
        .map(|m| SnapshotRow {
            size_on_disk: store.snapshot_size(&m),
            disk_name: crate::disks::disk_name(&reg, &m),
            meta: m,
        })
        .collect();
    Ok(SnapshotList {
        store_bytes: store.size_on_disk(),
        store_path,
        store_exists: true,
        snapshots,
        disk_count,
        disk: filter,
    })
}

/// Removes a snapshot from the store (DiskDrift's own data only).
/// Returns the removed snapshot, bytes freed and snapshots rewritten to keep them readable.
pub fn remove(
    settings: &Effective,
    reference: &str,
) -> Result<(SnapshotMeta, u64, Vec<u64>, MigrationOutcome)> {
    let store = require_store(settings)?;
    let m = store.resolve(reference)?;
    let (lock, migration) = store.lock()?;
    let (freed, folded) = store.remove(&lock, m.id)?;
    Ok((m, freed, folded, migration))
}

/// Integrity check of a snapshot (and its delta base).
pub fn verify(settings: &Effective, reference: &str) -> Result<(SnapshotMeta, u64)> {
    let store = require_store(settings)?;
    let m = store.resolve(reference)?;
    let n = store.verify(m.id)?;
    Ok((m, n))
}

pub(crate) fn exact_root(store: &Store, meta: &SnapshotMeta) -> Result<NativePath> {
    if let Some(root) = &meta.root_native {
        return Ok(root.path.clone());
    }
    let mut records = store.records(meta.id)?;
    let first = records
        .next()
        .transpose()
        .map_err(|e| {
            AppError::Store(StoreError::Corrupt(format!(
                "reading snapshot #{} root: {e}",
                meta.id
            )))
        })?
        .ok_or_else(|| {
            AppError::Store(StoreError::Corrupt(format!(
                "snapshot #{} has no root record",
                meta.id
            )))
        })?;
    Ok(match first.name {
        NativeName::Unix(bytes) => NativePath::unix(bytes),
        NativeName::Windows(units) => NativePath::windows(units),
    })
}

fn root_path(store: &Store, meta: &SnapshotMeta) -> Result<PathBuf> {
    let root = exact_root(store, meta)?;
    diskdrift_platform::path_from_native(&root).map_err(|e| AppError::Runtime(e.to_string()))
}

fn prefix_for(
    store: &Store,
    meta: &SnapshotMeta,
    path: Option<&Path>,
) -> Result<Option<Vec<NativeName>>> {
    let Some(p) = path else { return Ok(None) };
    let abs = std::path::absolute(p).map_err(|e| AppError::Usage(e.to_string()))?;
    let root = exact_root(store, meta)?;
    let path = diskdrift_platform::native_path(abs.as_os_str());
    let comps = components_under(&root, &path)
        .map_err(|e| AppError::Usage(e.to_string()))?
        .ok_or_else(|| {
            AppError::Usage(format!(
                "{} is not inside the scan root {} of snapshot {}",
                abs.display(),
                meta.root,
                meta.id
            ))
        })?;
    Ok(Some(comps))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TopResult {
    pub snapshot: SnapshotMeta,
    pub path: Option<PathBuf>,
    /// Directories are the deepest large ones only (see `TopOptions::deepest`).
    pub deepest: bool,
    #[serde(flatten)]
    pub report: TopReport,
}

pub fn top(
    settings: &Effective,
    reference: &str,
    path: Option<&Path>,
    mut opts: TopOptions,
) -> Result<TopResult> {
    let store = require_store(settings)?;
    let m = store.resolve(reference)?;
    // The algorithms filter by prefix themselves and need the ancestors' records
    // for path context, so they always get the whole stream.
    let stream = store.records(m.id)?;
    opts.prefix = prefix_for(&store, &m, path)?;
    let report = top::top(stream, &opts).map_err(core_store_error)?;
    Ok(TopResult {
        snapshot: m,
        path: path.map(Path::to_path_buf),
        deepest: opts.deepest,
        report,
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowseResult {
    pub snapshot: SnapshotMeta,
    pub path: PathBuf,
    #[serde(flatten)]
    pub listing: Listing,
}

/// One directory level of a snapshot: its children, largest first.
/// `path` defaults to the scan root.
pub fn browse(
    settings: &Effective,
    reference: &str,
    path: Option<&Path>,
    limit: usize,
) -> Result<BrowseResult> {
    let store = require_store(settings)?;
    let m = store.resolve(reference)?;
    let path = path
        .map(Path::to_path_buf)
        .unwrap_or(root_path(&store, &m)?);
    let dir = prefix_for(&store, &m, Some(&path))?.unwrap_or_default();
    let listing = browse::children(store.records(m.id)?, &dir, limit).map_err(core_store_error)?;
    if listing.dir.is_none() {
        return Err(AppError::Usage(format!(
            "{} is not in snapshot {}",
            path.display(),
            m.id
        )));
    }
    Ok(BrowseResult {
        snapshot: m,
        path,
        listing,
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffBrowseResult {
    pub old: SnapshotMeta,
    pub new: SnapshotMeta,
    pub path: PathBuf,
    #[serde(flatten)]
    pub listing: DiffListing,
}

/// One directory level of a comparison: the children that changed, largest change first.
pub fn diff_browse(
    settings: &Effective,
    a: &str,
    b: &str,
    path: Option<&Path>,
    limit: usize,
) -> Result<DiffBrowseResult> {
    let store = require_store(settings)?;
    let (old, new) = (store.resolve(a)?, store.resolve(b)?);
    for m in [&old, &new] {
        if !m.status.is_usable() {
            return Err(AppError::Store(StoreError::NoData {
                id: m.id,
                status: m.status,
            }));
        }
    }
    if old.incompatibility(&new).is_some() {
        return Err(AppError::Usage(format!(
            "snapshots {} and {} have different roots",
            old.id, new.id
        )));
    }
    let path = path
        .map(Path::to_path_buf)
        .unwrap_or(root_path(&store, &new)?);
    let dir = prefix_for(&store, &new, Some(&path))?.unwrap_or_default();
    // Paths are shown under the newer scan's root (the disk may have moved).
    let new_root = exact_root(&store, &new)?;
    let listing = browse::diff_children(
        store.records_as(old.id, &new_root)?,
        store.records(new.id)?,
        &dir,
        limit,
    )
    .map_err(core_store_error)?;
    if listing.dir.is_none() {
        return Err(AppError::Usage(format!(
            "{} is in neither snapshot",
            path.display()
        )));
    }
    Ok(DiffBrowseResult {
        old,
        new,
        path,
        listing,
    })
}

/// OS-level space change between the end of two scans.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpaceChange {
    /// Positive: free space decreased (consumed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free_consumed_bytes: Option<i128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_consumed_bytes: Option<i128>,
    #[serde(flatten)]
    pub measurement: Envelope,
}

#[derive(Debug, Clone)]
pub struct DiffResult {
    pub old: SnapshotMeta,
    pub new: SnapshotMeta,
    pub path: Option<PathBuf>,
    /// `None` when the snapshots are equivalent; otherwise why they are not.
    pub non_equivalent: Option<String>,
    pub space: Option<SpaceChange>,
    /// Space the OS reports consumed minus the tracked allocated growth. A
    /// diagnostic, not proof of missing files.
    pub unreconciled_bytes: Option<i128>,
    pub notes: Vec<String>,
    pub report: DiffReport,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DiffResultWire<'a> {
    old: &'a SnapshotMeta,
    new: &'a SnapshotMeta,
    path: &'a Option<PathBuf>,
    non_equivalent: &'a Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    space: &'a Option<SpaceChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unreconciled_bytes: Option<i128>,
    notes: &'a [String],
    summary: &'a diff::DiffSummary,
    files: &'a [diskdrift_core::diff::DiffItem],
    growing_dirs: &'a [diskdrift_core::diff::DiffItem],
    shrinking_dirs: &'a [diskdrift_core::diff::DiffItem],
    shrinking_files: &'a [diskdrift_core::diff::DiffItem],
    moves: &'a [diskdrift_core::diff::MoveItem],
    #[serde(flatten)]
    measurement: Envelope,
}

fn quality_rank(quality: Option<Quality>) -> u8 {
    match quality.unwrap_or(Quality::Unknown) {
        Quality::Observed => 0,
        Quality::Estimated => 1,
        Quality::LowerBound => 2,
        Quality::NotApplicable => 3,
        Quality::Unsupported => 4,
        Quality::Unknown => 5,
    }
}

fn diff_measurement(result: &DiffResult) -> Envelope {
    let mut measurement = result.report.measurement();
    if let Some(space) = &result.space
        && quality_rank(space.measurement.quality) > quality_rank(measurement.quality)
    {
        measurement = space.measurement.clone();
    }
    measurement
}

impl Serialize for DiffResult {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        DiffResultWire {
            old: &self.old,
            new: &self.new,
            path: &self.path,
            non_equivalent: &self.non_equivalent,
            space: &self.space,
            unreconciled_bytes: (self.report.summary.net_allocated_quality() == Quality::Observed)
                .then_some(self.unreconciled_bytes)
                .flatten(),
            notes: &self.notes,
            summary: &self.report.summary,
            files: &self.report.files,
            growing_dirs: &self.report.growing_dirs,
            shrinking_dirs: &self.report.shrinking_dirs,
            shrinking_files: &self.report.shrinking_files,
            moves: &self.report.moves,
            measurement: diff_measurement(self),
        }
        .serialize(serializer)
    }
}

/// Whether `old` can be compared with `new`: both hold data, `old` is the
/// earlier scan, and both cover the same root, volume and scan options.
pub fn comparable(old: &SnapshotMeta, new: &SnapshotMeta) -> bool {
    old.id < new.id
        && old.status.is_usable()
        && new.status.is_usable()
        && old.incompatibility(new).is_none()
}

/// The comparison to show first, as indices into `metas` (oldest first): the
/// newest scan against the previous one comparable with it, falling back to
/// older scans when the newest has nothing to compare with.
pub fn latest_pair(metas: &[&SnapshotMeta]) -> Option<(usize, usize)> {
    (0..metas.len()).rev().find_map(|new| {
        (0..new)
            .rev()
            .find(|&old| comparable(metas[old], metas[new]))
            .map(|old| (old, new))
    })
}

/// The newest scan before `new` that it can be compared with (`metas` oldest first).
pub fn previous_comparable<'a>(
    metas: &[&'a SnapshotMeta],
    new: &SnapshotMeta,
) -> Option<&'a SnapshotMeta> {
    metas.iter().rev().find(|m| comparable(m, new)).copied()
}

/// The newest scan with data of `root` (`metas` oldest first), for an estimate
/// of how long scanning it again takes.
pub fn last_scan_of<'a>(metas: &[&'a SnapshotMeta], root: &Path) -> Option<&'a SnapshotMeta> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    metas
        .iter()
        .rev()
        .find(|m| m.status.is_usable() && Path::new(&m.root) == root)
        .copied()
}

/// A scan root as typed in the TUI: `~` expanded, a full path, an existing
/// folder. The error says what to type instead.
pub fn check_root(input: &str, home: &Path) -> std::result::Result<PathBuf, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err(
            "Type the folder to scan: / for the whole disk, ~ for your home folder.".into(),
        );
    }
    let path = match input.strip_prefix('~') {
        Some("") => home.to_path_buf(),
        Some(rest) if rest.starts_with('/') => home.join(rest.trim_start_matches('/')),
        _ => PathBuf::from(input),
    };
    if !path.is_absolute() {
        return Err(format!(
            "{input} is not a full path: start with / or ~ (for example ~/Library)."
        ));
    }
    match std::fs::metadata(&path) {
        Ok(m) if m.is_dir() => Ok(path),
        Ok(_) => Err(format!("{input} is a file; type the folder that holds it.")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(format!("{input} does not exist."))
        }
        Err(e) => Err(format!("{input}: {e}")),
    }
}

pub fn diff(
    settings: &Effective,
    a: &str,
    b: &str,
    path: Option<&Path>,
    allow_incompatible: bool,
    mut opts: DiffOptions,
) -> Result<DiffResult> {
    let store = require_store(settings)?;
    let old = store.resolve(a)?;
    let new = store.resolve(b)?;
    for m in [&old, &new] {
        if !m.status.is_usable() {
            return Err(AppError::Store(StoreError::NoData {
                id: m.id,
                status: m.status,
            }));
        }
    }
    let non_equivalent = old.incompatibility(&new);
    if let Some(why) = &non_equivalent
        && !allow_incompatible
    {
        return Err(AppError::Usage(format!(
            "snapshots {} and {} are not comparable: {why} (use --allow-incompatible to compare anyway)",
            old.id, new.id
        )));
    }
    let prefix = prefix_for(&store, &new, path)?;
    // One lineage at two mount points compares under the newer root; scans that
    // are not comparable at all (--allow-incompatible) are compared as they are.
    let new_root = exact_root(&store, &new)?;
    let sa = if non_equivalent.is_none() {
        store.records_as(old.id, &new_root)?
    } else {
        store.records(old.id)?
    };
    let sb = store.records(new.id)?;
    opts.prefix = prefix.clone();
    let lineage = old.lineage_compatibility(&new);
    opts.file_id_comparable = non_equivalent.is_none() && lineage.file_id_comparable;
    let allocated_deltas = non_equivalent.is_none() && lineage.allocated_deltas;
    opts.allocated_deltas = allocated_deltas;
    let report = diff::diff(sa, sb, opts).map_err(core_store_error)?;

    let space = match (&old.space_after, &new.space_after) {
        (Some(x), Some(y)) => Some(SpaceChange {
            free_consumed_bytes: x
                .free_bytes
                .zip(y.free_bytes)
                .map(|(old, new)| old as i128 - new as i128),
            available_consumed_bytes: match (
                x.available_important_bytes,
                y.available_important_bytes,
            ) {
                (Some(p), Some(q)) => Some(p as i128 - q as i128),
                _ => None,
            },
            measurement: space_measurement(x, y),
        }),
        _ => None,
    };
    let unreconciled_bytes = match (&space, &prefix, &non_equivalent) {
        (Some(s), None, None) if report.summary.net_allocated_quality() == Quality::Observed => s
            .free_consumed_bytes
            .map(|free| free - report.summary.net_allocated_bytes),
        _ => None,
    };
    let mut notes = Vec::new();
    for m in [&old, &new] {
        if m.status == Status::Partial {
            let c = m.counts.as_ref();
            notes.push(format!(
                "Snapshot {} is PARTIAL: {} unreadable directories, {} unreadable entries; their contents are not compared.",
                m.id,
                c.map(|c| c.inaccessible_dirs).unwrap_or(0),
                c.map(|c| c.stat_errors).unwrap_or(0)
            ));
        }
    }
    let s = &report.summary;
    if s.newly_visible.count + s.no_longer_visible.count > 0 {
        notes.push(format!(
            "{} entries became visible and {} stopped being visible because access changed; they are excluded from growth.",
            s.newly_visible.count, s.no_longer_visible.count
        ));
    }
    if unreconciled_bytes.is_some() && new.root != "/" && new.root != new.volume.mount_point {
        notes.push(format!(
            "The scans cover {} only: the OS space change includes everything else on the volume, so the unreconciled difference mostly reflects activity outside the scanned tree.",
            new.root
        ));
    }
    notes.push(
        "Scans are not atomic: files changing while a scan runs are captured at different moments."
            .into(),
    );
    if old.volume.fs_type == "apfs" {
        notes.push(
            "On APFS the sum of allocated sizes is not exclusive physical usage: clones share blocks and local snapshots keep deleted data.".into(),
        );
    }
    if prefix.is_some() {
        notes.push("Restricted to a path: OS space change and unreconciled difference are not attributable and are omitted.".into());
    }
    if let Some(why) = &non_equivalent {
        notes.push(format!("NON-EQUIVALENT comparison ({why}): differences may reflect scan settings, not real changes."));
    }
    if !allocated_deltas {
        notes.push(
            "Allocated-byte changes are unavailable because the snapshots use different allocation measurement semantics; logical and path changes remain comparable."
                .into(),
        );
    }
    Ok(DiffResult {
        old,
        new,
        path: path.map(Path::to_path_buf),
        non_equivalent,
        space,
        unreconciled_bytes,
        notes,
        report,
    })
}

#[cfg(test)]
mod pair_tests {
    use super::*;
    use crate::test_support::{set_root_volume, unix_volume_descriptor};

    fn meta(id: u64, root: &str, status: Status) -> SnapshotMeta {
        let mut meta = SnapshotMeta {
            format_version: 1,
            id,
            name: format!("s{id}"),
            root: root.into(),
            root_native: Some(diskdrift_core::native::NativeRoot::posix(
                root.as_bytes().to_vec(),
            )),
            disk: None,
            root_in_volume: None,
            root_in_volume_native: None,
            status,
            started_at: "2026-10-21T10:00:00+02:00".into(),
            finished_at: None,
            volume: Default::default(),
            volumes: Vec::new(),
            options: Default::default(),
            space_before: None,
            space_after: None,
            storage: Default::default(),
            counts: None,
            error: None,
            diskdrift_version: String::new(),
        };
        set_root_volume(&mut meta, unix_volume_descriptor("fixture-volume", "/"));
        meta
    }

    #[test]
    fn default_names_are_recognised() {
        assert!(is_default_name(&default_name()));
        assert!(!is_default_name("baseline"));
        assert!(!is_default_name("auto-2026-10-21T13-13-41"));
    }

    #[test]
    fn space_change_quality_is_conservative() {
        let observed = diskdrift_storage::SpaceSample {
            free_bytes: Some(100),
            quality: diskdrift_storage::MeasurementQuality::Observed,
            ..Default::default()
        };
        let lower_bound = diskdrift_storage::SpaceSample {
            free_bytes: Some(90),
            quality: diskdrift_storage::MeasurementQuality::LowerBound,
            ..Default::default()
        };

        let envelope = space_measurement(&observed, &lower_bound);

        assert_eq!(
            envelope.quality,
            Some(diskdrift_diagnostics::measurement::Quality::LowerBound)
        );
    }

    #[test]
    fn explicit_native_root_survives_scan_request_construction() {
        let settings = crate::config::resolve_with(
            &crate::config::Overrides::default(),
            Path::new("/tmp/diskdrift-home"),
            Path::new("/tmp/diskdrift-cwd"),
        )
        .unwrap();
        let exact = diskdrift_core::native::NativeRoot::posix(b"/tmp/exact-\xff".to_vec());
        let request = ScanRequest::from_settings_with_native_root(
            &settings,
            Some(PathBuf::from("/tmp/display")),
            Some(exact.clone()),
            None,
            &[],
        );
        assert_eq!(request.root_native, Some(exact));
    }

    #[test]
    fn volume_descriptor_uses_stable_volume_facts_when_native_keys_are_absent() {
        let info = diskdrift_platform::DiskInfo {
            mount_point: PathBuf::from("/"),
            fs_type: "apfs".into(),
            volume_uuid: Some("fixture-volume".into()),
            ..Default::default()
        };
        let descriptor = volume_descriptor(&info).expect("fixture has a volume identity");

        assert_eq!(
            descriptor.identity,
            NativePath::unix(b"fixture-volume".to_vec())
        );
        assert_eq!(descriptor.mount_key, NativePath::unix(b"/".to_vec()));
        assert_eq!(descriptor.identity_strength, "uuid");
    }

    #[test]
    fn previous_comparable_and_last_scan_of_a_root() {
        let root_a = std::env::temp_dir().join("diskdrift-pair-a");
        let root_b = std::env::temp_dir().join("diskdrift-pair-b");
        let root_c = std::env::temp_dir().join("diskdrift-pair-c");
        let root_a = root_a.to_string_lossy().into_owned();
        let root_b = root_b.to_string_lossy().into_owned();
        let root_c = root_c.to_string_lossy().into_owned();
        let all = [
            meta(1, &root_a, Status::Completed),
            meta(2, &root_b, Status::Completed),
            meta(3, &root_a, Status::Cancelled),
            meta(4, &root_a, Status::Partial),
        ];
        let metas: Vec<&SnapshotMeta> = all.iter().collect();
        assert_eq!(previous_comparable(&metas, &all[3]).map(|m| m.id), Some(1));
        assert_eq!(previous_comparable(&metas, &all[1]).map(|m| m.id), None);
        assert_eq!(
            last_scan_of(&metas[..3], Path::new(&root_a)).map(|m| m.id),
            Some(1),
            "#3 holds no data"
        );
        assert_eq!(last_scan_of(&metas, Path::new(&root_c)).map(|m| m.id), None);
    }

    #[test]
    fn check_root_expands_home_and_says_what_is_wrong() {
        let t = diskdrift_core::testutil::TestDir::new("check_root");
        let dir = t.path().to_path_buf();
        t.dir("Library");
        t.file("file", 1);
        assert_eq!(check_root("~", &dir).unwrap(), dir);
        assert_eq!(
            check_root(" ~/Library ", &dir).unwrap(),
            dir.join("Library")
        );
        assert_eq!(check_root(&dir.display().to_string(), &dir).unwrap(), dir);
        assert!(
            check_root("Library", &dir)
                .unwrap_err()
                .contains("not a full path")
        );
        assert!(
            check_root("~/missing", &dir)
                .unwrap_err()
                .contains("does not exist")
        );
        assert!(
            check_root("~/file", &dir)
                .unwrap_err()
                .contains("is a file")
        );
        assert!(
            check_root("", &dir)
                .unwrap_err()
                .contains("/ for the whole disk")
        );
    }

    #[test]
    fn latest_pair_skips_other_roots_and_scans_without_data() {
        let all = [
            meta(1, "/a", Status::Completed),
            meta(2, "/b", Status::Completed),
            meta(3, "/a", Status::Cancelled),
            meta(4, "/a", Status::Completed),
        ];
        let metas: Vec<&SnapshotMeta> = all.iter().collect();
        assert_eq!(
            latest_pair(&metas),
            Some((0, 3)),
            "#4 against #1: #3 holds no data, #2 is another root"
        );
        assert!(
            !comparable(metas[3], metas[0]),
            "the older scan comes first"
        );
        assert!(!comparable(metas[0], metas[1]));
        // The newest scan (#5, /c) has no partner: fall back to the newest pair there is.
        let all = [
            meta(1, "/a", Status::Completed),
            meta(2, "/a", Status::Completed),
            meta(5, "/c", Status::Completed),
        ];
        let metas: Vec<&SnapshotMeta> = all.iter().collect();
        assert_eq!(latest_pair(&metas), Some((0, 1)));
        assert_eq!(latest_pair(&metas[1..]), None);
    }

    #[cfg(unix)]
    #[test]
    fn diff_browse_rejects_distinct_native_roots_with_same_display() {
        use diskdrift_core::native::NativeName;
        use diskdrift_core::record::{Kind, Record};
        use diskdrift_core::testutil::TestDir;
        use diskdrift_core::walk::WalkReport;
        use diskdrift_storage::Store;

        fn write_snapshot(store: &Store, meta: SnapshotMeta, root: Vec<u8>) -> SnapshotMeta {
            let (lock, _) = store.lock().unwrap();
            let mut writer = store.begin(lock, meta, None).unwrap();
            writer
                .push(&Record {
                    depth: 0,
                    name: NativeName::unix(root),
                    kind: Kind::Dir,
                    ..Default::default()
                })
                .unwrap();
            writer
                .commit(
                    WalkReport {
                        entries: 1,
                        ..Default::default()
                    },
                    None,
                    "now".into(),
                )
                .unwrap()
        }

        let t = TestDir::new("diff-browse-native-root-collision");
        let db = t.path().join("store");
        let store = Store::create(&db).unwrap();
        let old_root = vec![b'/', 0xff];
        let new_root = vec![b'/', 0xfe];
        let display = String::from_utf8_lossy(&old_root).into_owned();
        let mut old = meta(1, &display, Status::Completed);
        old.root_native = Some(diskdrift_core::native::NativeRoot::posix(old_root.clone()));
        let mut new = meta(2, &display, Status::Completed);
        new.root_native = Some(diskdrift_core::native::NativeRoot::posix(new_root.clone()));
        let old = write_snapshot(&store, old, old_root);
        let new = write_snapshot(&store, new, new_root);
        assert_eq!(old.root, new.root);
        assert_ne!(old.root_native, new.root_native);

        let settings = crate::config::resolve_with(
            &crate::config::Overrides {
                db: Some(db),
                ..Default::default()
            },
            t.path(),
            t.path(),
        )
        .unwrap();
        let error = diff_browse(&settings, &old.name, &new.name, None, 10).unwrap_err();
        assert!(
            matches!(&error, AppError::Usage(message) if message.contains("different roots")),
            "{error}"
        );
    }
}
