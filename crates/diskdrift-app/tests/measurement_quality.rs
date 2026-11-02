use diskdrift_app::analyze::{AnalyzeItem, DirAnalysis, VerdictTotal};
use diskdrift_app::disks::{DiskRow, Volume, sighting_of};
use diskdrift_app::investigate::Candidate;
use diskdrift_app::prune::Removed;
use diskdrift_app::search::{Hit, Place, ScanRef};
use diskdrift_app::snapshots::TopResult;
use diskdrift_app::snapshots::{DiffResult, ScanOutcome, SnapshotList, space_sample};
use diskdrift_app::text::snapshots as snapshot_text;
use diskdrift_core::browse::{Child, children, diff_children};
use diskdrift_core::diff::{DiffOptions, DiffReport, diff};
use diskdrift_core::native::{NativeName, NativePath};
use diskdrift_core::record::{EntryFields, Kind, Record, flags};
use diskdrift_core::top::{TopItem, TopOptions, TopReport, top};
use diskdrift_diagnostics::classify::Assessment;
use diskdrift_diagnostics::measurement::Envelope;
use diskdrift_diagnostics::suspects::{LargeFile, MeasureStatus, Risk};
use diskdrift_diagnostics::usage::VolumeUsage;
use diskdrift_platform::DiskInfo;
use diskdrift_storage::disks::{Disk, Identity, Strength, VolumeFacts};
use diskdrift_storage::{MeasurementQuality, Rebase, SnapshotMeta, Status};
use serde_json::Value;
use std::path::Path;

#[test]
fn filesystem_space_sample_has_observation_envelope() {
    let sample = space_sample(Path::new("/")).expect("root filesystem must be readable");
    assert!(sample.observed_at.is_some());
    assert!(sample.domain.is_some());
    assert!(sample.scope.is_some());
    assert!(sample.provenance.is_some());
    assert!(matches!(
        sample.quality,
        MeasurementQuality::Observed | MeasurementQuality::Unknown
    ));
}

#[test]
fn quality_values_are_distinct_and_serializable() {
    let values = [
        MeasurementQuality::Observed,
        MeasurementQuality::Estimated,
        MeasurementQuality::LowerBound,
        MeasurementQuality::Unknown,
        MeasurementQuality::Unsupported,
        MeasurementQuality::NotApplicable,
    ];
    let json = serde_json::to_string(&values).unwrap();
    for value in [
        "observed",
        "estimated",
        "lower_bound",
        "unknown",
        "unsupported",
        "not_applicable",
    ] {
        assert!(json.contains(value), "missing {value} in {json}");
    }
}

#[test]
fn derived_envelope_constructors_have_observation_timestamps() {
    for envelope in [
        Envelope::observed("filesystem", "test", "test"),
        Envelope::estimated("filesystem", "test", "test"),
        Envelope::lower_bound("filesystem", "test", "test"),
    ] {
        let observed_at = envelope.observed_at.as_deref().expect("timestamp");
        assert!(chrono::DateTime::parse_from_rfc3339(observed_at).is_ok());
    }
    assert!(Envelope::default().observed_at.is_none());
}

#[test]
fn unknown_space_sample_omits_unavailable_bytes() {
    let json = serde_json::to_string(&diskdrift_storage::SpaceSample::default()).unwrap();
    assert!(!json.contains("totalBytes"));
    assert!(!json.contains("freeBytes"));
    assert!(!json.contains("availableBytes"));
    assert!(!json.contains("availableImportantBytes"));
    assert!(!json.contains("availableOpportunisticBytes"));

    let json = serde_json::to_string(&diskdrift_platform::VolumeCapacity::default()).unwrap();
    assert!(!json.contains("totalBytes"));
    assert!(!json.contains("availableImportantBytes"));
    assert!(!json.contains("availableOpportunisticBytes"));
}

fn assert_unknown_omits(json: &str, fields: &[&str]) {
    let value: Value = serde_json::from_str(json).unwrap();
    for field in fields {
        assert!(value.get(field).is_none(), "{field} was emitted in {json}");
    }
}

fn snapshot_meta() -> SnapshotMeta {
    SnapshotMeta {
        format_version: 2,
        id: 1,
        name: "snapshot".into(),
        root: "/".into(),
        root_native: None,
        disk: None,
        root_in_volume: None,
        root_in_volume_native: None,
        status: Status::Completed,
        started_at: "2026-01-01T00:00:00Z".into(),
        finished_at: None,
        volume: Default::default(),
        volumes: Vec::new(),
        options: Default::default(),
        space_before: None,
        space_after: None,
        storage: Default::default(),
        counts: None,
        error: None,
        diskdrift_version: "test".into(),
    }
}

#[test]
fn remaining_public_measurements_omit_unknown_bytes_and_preserve_known_values() {
    let unknown_volume = Volume {
        identity: diskdrift_storage::Identity::new(diskdrift_storage::Strength::Uuid, "volume"),
        mount_point: "/Volumes/test".into(),
        name: Some("test".into()),
        fs_type: "apfs".into(),
        capacity_bytes: None,
    };
    let unknown_volume_json = serde_json::to_value(&unknown_volume).unwrap();
    assert!(unknown_volume_json.get("capacityBytes").is_none());
    assert_eq!(unknown_volume_json["quality"], "unknown");

    let known_volume = Volume {
        capacity_bytes: Some(42),
        ..unknown_volume.clone()
    };
    let known_volume_json = serde_json::to_value(&known_volume).unwrap();
    assert_eq!(known_volume_json["capacityBytes"], 42);
    assert_eq!(known_volume_json["quality"], "observed");

    for value in [
        serde_json::to_value(VolumeFacts::default()).unwrap(),
        serde_json::to_value(DiskInfo::default()).unwrap(),
    ] {
        assert!(value.get("capacityBytes").is_none());
    }

    let known_facts = serde_json::to_value(VolumeFacts {
        capacity_bytes: Some(42),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(known_facts["capacityBytes"], 42);

    let known_info = serde_json::to_value(DiskInfo {
        capacity_bytes: Some(42),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(known_info["capacityBytes"], 42);
    assert_eq!(known_info["quality"], "observed");

    let meta = snapshot_meta();
    let unknown_scan = ScanOutcome {
        snapshot: meta.clone(),
        cancelled: false,
        store_path: "/store".into(),
        store_bytes: 0,
        rebase: Rebase::Nothing,
        rebase_error: None,
        disk: None,
        migration: diskdrift_storage::MigrationOutcome::None,
    };
    assert_unknown_omits(
        &serde_json::to_string(&unknown_scan).unwrap(),
        &["storeBytes"],
    );

    let unknown_list = SnapshotList {
        store_path: "/store".into(),
        store_exists: false,
        store_bytes: 0,
        snapshots: Vec::new(),
        disk_count: 0,
        disk: None,
    };
    assert_unknown_omits(
        &serde_json::to_string(&unknown_list).unwrap(),
        &["storeBytes"],
    );

    let unknown_diff = DiffResult {
        old: meta.clone(),
        new: meta.clone(),
        path: None,
        non_equivalent: None,
        space: None,
        unreconciled_bytes: None,
        notes: Vec::new(),
        report: DiffReport::default(),
    };
    assert_unknown_omits(
        &serde_json::to_string(&unknown_diff).unwrap(),
        &["unreconciledBytes"],
    );

    let known_scan = ScanOutcome {
        store_bytes: 42,
        ..unknown_scan
    };
    let known_scan_json = serde_json::to_value(&known_scan).unwrap();
    assert_eq!(known_scan_json["storeBytes"], 42);
    assert_eq!(known_scan_json["quality"], "observed");

    let known_list = SnapshotList {
        store_exists: true,
        store_bytes: 42,
        ..unknown_list
    };
    let known_list_json = serde_json::to_value(&known_list).unwrap();
    assert_eq!(known_list_json["storeBytes"], 42);
    assert_eq!(known_list_json["quality"], "observed");

    let known_diff = DiffResult {
        unreconciled_bytes: Some(7),
        report: diff(
            Box::new(vec![root()].into_iter().map(Ok)),
            Box::new(vec![root()].into_iter().map(Ok)),
            DiffOptions::default(),
        )
        .unwrap(),
        ..unknown_diff
    };
    let known_diff_json = serde_json::to_value(&known_diff).unwrap();
    assert_eq!(known_diff_json["unreconciledBytes"], 7);
    assert_eq!(known_diff_json["quality"], "observed");
}

#[test]
fn unreconciled_bytes_stay_unknown_when_allocation_is_unknown_or_partial() {
    let (old, new) = unavailable_records();
    let unknown_report = diff(
        Box::new(old.into_iter().map(Ok)),
        Box::new(new.into_iter().map(Ok)),
        DiffOptions::default(),
    )
    .unwrap();
    let unknown = DiffResult {
        old: snapshot_meta(),
        new: snapshot_meta(),
        path: None,
        non_equivalent: None,
        space: None,
        unreconciled_bytes: Some(123_456_789),
        notes: Vec::new(),
        report: unknown_report,
    };
    let json = serde_json::to_value(&unknown).unwrap();
    assert!(json.get("unreconciledBytes").is_none(), "{json}");
    assert!(matches!(
        json["quality"].as_str(),
        Some("unknown") | Some("lower_bound")
    ));
    let text = snapshot_text::diff(
        &unknown,
        diskdrift_core::units::SizeUnits::Decimal,
        false,
        false,
    );
    assert!(text.contains("5. Unreconciled difference"), "{text}");
    assert!(text.contains("unknown"), "{text}");
    assert!(!text.contains("123.5 MB"), "{text}");

    let old = vec![root(), file(b"known", 7, 7, bytes_available())];
    let new = vec![root(), file(b"unknown", 0, 0, EntryFields::KIND)];
    let partial_report = diff(
        Box::new(old.into_iter().map(Ok)),
        Box::new(new.into_iter().map(Ok)),
        DiffOptions::default(),
    )
    .unwrap();
    let partial = DiffResult {
        old: snapshot_meta(),
        new: snapshot_meta(),
        path: None,
        non_equivalent: None,
        space: None,
        unreconciled_bytes: Some(123_456_789),
        notes: Vec::new(),
        report: partial_report,
    };
    let json = serde_json::to_value(&partial).unwrap();
    assert!(json.get("unreconciledBytes").is_none(), "{json}");
    assert_eq!(json["quality"], "lower_bound");
    let text = snapshot_text::diff(
        &partial,
        diskdrift_core::units::SizeUnits::Decimal,
        false,
        false,
    );
    assert!(text.contains("unknown"), "{text}");
    assert!(!text.contains("123.5 MB"), "{text}");
}

fn disk() -> Disk {
    Disk {
        id: "dtest".into(),
        identity: Identity::new(Strength::Uuid, "volume"),
        label: None,
        description: None,
        sets: Vec::new(),
        volume: VolumeFacts {
            name: Some("Data".into()),
            fs_type: "apfs".into(),
            capacity_bytes: Some(1000),
            ..Default::default()
        },
        device: None,
        first_seen: "now".into(),
        last_seen: "now".into(),
        last_mount_point: None,
        aliases: Vec::new(),
        last_mount_path: None,
    }
}

#[test]
fn disk_row_store_bytes_are_store_measurements_and_unknown_is_omitted() {
    let unknown = DiskRow {
        disk: disk(),
        known: false,
        connected: true,
        mount_point: Some("/Volumes/Data".into()),
        snapshots: 0,
        last_scan: None,
        last_size: None,
        last_root: None,
        store_bytes: 0,
    };
    let unknown_json = serde_json::to_value(&unknown).unwrap();
    assert!(unknown_json.get("storeBytes").is_none(), "{unknown_json}");
    assert_eq!(unknown_json["quality"], "unknown");

    let known = DiskRow {
        known: true,
        snapshots: 1,
        last_scan: Some("now".into()),
        last_size: Some(999),
        last_root: Some("/".into()),
        store_bytes: 42,
        ..unknown
    };
    let known_json = serde_json::to_value(&known).unwrap();
    assert_eq!(known_json["storeBytes"], 42);
    assert_eq!(known_json["domain"], "diskdrift_store");
    assert_eq!(known_json["scope"], "disk snapshots");
    assert_eq!(known_json["provenance"], "store accounting");
}

#[test]
fn doctor_reference_and_gap_have_separate_measurement_envelopes() {
    use diskdrift_app::diagnostics::access::{Access, AccessCheck};
    use diskdrift_app::diagnostics::doctor::{Check, DoctorReport, SnapshotInventory};
    use diskdrift_app::diagnostics::openfiles::OpenDeletedReport;

    let report = DoctorReport {
        generated_at: "now".into(),
        usage: diskdrift_app::diagnostics::usage::UsageReport {
            containers: Vec::new(),
            other_volumes: Vec::new(),
            apfs_error: None,
            mounts_error: None,
        },
        checks: vec![Check {
            name: "check".into(),
            status: "OK".into(),
            detail: String::new(),
        }],
        access: AccessCheck {
            full_disk_access: Access::Unknown,
            detail: String::new(),
        },
        snapshots: SnapshotInventory {
            snapshots: Vec::new(),
            reclaimable: "unknown".into(),
            reclaimable_measurement: Envelope::default(),
            errors: Vec::new(),
        },
        findings: Vec::new(),
        largest_files: Vec::new(),
        open_deleted: OpenDeletedReport {
            supported: false,
            scope: "unsupported".into(),
            total_bytes: None,
            distinct_files: None,
            files: Vec::new(),
            error: None,
            measurement: Envelope::default(),
        },
        reference_free_bytes: Some(120),
        reference_free_measurement: Envelope::estimated(
            "filesystem",
            "reference free space",
            "user supplied; not verified",
        ),
        unexplained_gap_bytes: Some(42),
        unexplained_gap_measurement: Envelope::estimated(
            "filesystem",
            "free-space accounting",
            "user-supplied reference minus OS-reported available space",
        ),
        conclusion: Vec::new(),
        sizes_from: None,
        files_modified: false,
        elapsed_ms: 0,
    };
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["referenceFreeBytes"], 120);
    assert_eq!(json["referenceFreeMeasurement"]["quality"], "estimated");
    assert_eq!(json["unexplainedGapBytes"], 42);
    assert_eq!(json["unexplainedGapMeasurement"]["quality"], "estimated");
}

#[test]
fn default_walk_report_omits_unmeasured_numeric_fields() {
    let json = serde_json::to_value(diskdrift_core::walk::WalkReport::default()).unwrap();
    for field in [
        "entries",
        "files",
        "dirs",
        "logicalBytes",
        "allocatedBytes",
        "elapsedMs",
        "peakPendingDirs",
    ] {
        assert!(json.get(field).is_none(), "{field} was emitted in {json}");
    }
    assert_eq!(json["measurement"]["quality"], "unknown");
}

#[test]
fn top_summary_does_not_render_unknown_aggregate_as_zero() {
    let mut report = TopReport::default();
    report.entries = 2;
    let result = TopResult {
        snapshot: snapshot_meta(),
        path: None,
        deepest: false,
        report,
    };

    let text = snapshot_text::top(
        &result,
        diskdrift_core::units::SizeUnits::Decimal,
        false,
        false,
    );

    assert!(text.contains("2 entries, unknown allocated"), "{text}");
    assert!(!text.contains("0 B"), "{text}");
}

fn assessment() -> Assessment {
    Assessment {
        what: String::new(),
        risk: Some(Risk::Medium),
        advice: None,
        owner: None,
        no_installed_owner: false,
    }
}

#[test]
fn unknown_app_and_core_measurements_omit_numeric_values() {
    let item = AnalyzeItem {
        name: "x".into(),
        path: "/x".into(),
        is_dir: true,
        status: MeasureStatus::Inaccessible,
        allocated_bytes: 0,
        logical_bytes: 0,
        entries: 1,
        newest_mtime: None,
        assessment: assessment(),
    };
    assert_unknown_omits(
        &serde_json::to_string(&item).unwrap(),
        &["allocatedBytes", "logicalBytes", "entries"],
    );

    let analysis = DirAnalysis {
        path: "/x".into(),
        context: None,
        status: MeasureStatus::Inaccessible,
        allocated_bytes: 0,
        logical_bytes: 0,
        entries: 1,
        items: vec![item],
        more_items: 0,
        more_allocated_bytes: 0,
        by_verdict: vec![VerdictTotal {
            verdict: "unknown".into(),
            allocated_bytes: 0,
            items: 0,
        }],
        installed_apps: 0,
        analyzed_at: 0,
        elapsed_ms: 0,
        sizes_from: None,
        notes: Vec::new(),
    };
    assert_unknown_omits(
        &serde_json::to_string(&analysis).unwrap(),
        &[
            "allocatedBytes",
            "logicalBytes",
            "entries",
            "moreAllocatedBytes",
        ],
    );

    let child = Child {
        path: NativePath::Unix(b"/x".to_vec()),
        kind: Kind::Dir,
        allocated_bytes: 0,
        logical_bytes: 0,
        entries: 1,
        newest_mtime: 0,
        incomplete: true,
    };
    let child_json = serde_json::to_string(&child).unwrap();
    assert!(child_json.contains("\"quality\":\"lower_bound\""));

    let top = TopItem {
        path: NativePath::Unix(b"/x".to_vec()),
        kind: Kind::Dir,
        logical_bytes: 0,
        allocated_bytes: 0,
        entries: 1,
        mtime: 0,
        incomplete: true,
    };
    let top_json = serde_json::to_string(&top).unwrap();
    assert!(top_json.contains("\"quality\":\"lower_bound\""));

    let large = LargeFile {
        path: "/x".into(),
        logical_bytes: 0,
        allocated_bytes: 0,
        measurement: Default::default(),
    };
    assert_unknown_omits(
        &serde_json::to_string(&large).unwrap(),
        &["allocatedBytes", "logicalBytes"],
    );
}

#[test]
fn unknown_search_and_prune_measurements_omit_numeric_values() {
    let hit = Hit {
        path: NativePath::Unix(b"/x".to_vec()),
        kind: Kind::File,
        size: 0,
        logical_size: 0,
        mtime: 0,
        available: EntryFields::empty(),
        measurement: Envelope::default(),
        scan: ScanRef {
            id: 1,
            name: "s".into(),
            started_at: "2026-01-01T00:00:00Z".into(),
        },
        place: Place {
            disk: None,
            disk_name: "d".into(),
            connected: false,
            mount_point: None,
            root: "/".into(),
        },
    };
    assert_unknown_omits(
        &serde_json::to_string(&hit).unwrap(),
        &["size", "logicalSize", "mtime"],
    );

    let removed = Removed {
        removed: Vec::new(),
        rewritten: Vec::new(),
        freed_bytes: 0,
        cancelled: true,
        uncompacted: 0,
        migration: diskdrift_storage::MigrationOutcome::None,
        measurement: Envelope::default(),
    };
    assert_unknown_omits(
        &serde_json::to_string(&removed).unwrap(),
        &["freedBytes", "uncompacted"],
    );

    assert_unknown_omits(
        &serde_json::to_string(&TopReport::default()).unwrap(),
        &["totalLogical", "totalAllocated", "entries"],
    );
    let diff = serde_json::to_string(&DiffReport::default()).unwrap();
    assert!(!diff.contains("netLogicalBytes"));
    assert!(!diff.contains("netAllocatedBytes"));
    assert!(!diff.contains("movedBytes"));
}

#[test]
fn reclaimability_and_persisted_volume_capacity_have_measurements() {
    let volume = VolumeUsage {
        name: "Data".into(),
        mount_point: "/".into(),
        device: "/dev/disk1".into(),
        fs_type: "apfs".into(),
        roles: Vec::new(),
        total_bytes: Some(100),
        free_bytes: Some(20),
        available_bytes: Some(30),
        available_important_bytes: Some(40),
        available_opportunistic_bytes: None,
        purgeable_estimate_bytes: Some(10),
        in_use_bytes: None,
        read_only: false,
        browsable: true,
        volume_uuid: None,
        measurement: Envelope::observed("filesystem", "mount", "statfs"),
        reclaimable_measurement: Envelope::estimated(
            "filesystem",
            "volume reclaimability",
            "important-usage available minus strictly free",
        ),
    };
    let volume_json = serde_json::to_value(volume).unwrap();
    assert_eq!(
        volume_json["reclaimableMeasurement"]["quality"],
        "estimated"
    );
    assert!(volume_json["reclaimableMeasurement"]["observedAt"].is_string());

    let volume = sighting_of(
        &DiskInfo {
            mount_point: "/".into(),
            fs_type: "apfs".into(),
            capacity_bytes: Some(42),
            ..Default::default()
        },
        "2026-10-29T00:00:00Z",
    )
    .volume;
    let persisted = serde_json::to_value(volume).unwrap();
    assert_eq!(persisted["capacityBytes"], 42);
    assert_eq!(persisted["quality"], "observed");
    assert!(persisted["observedAt"].is_string());

    let candidate = Candidate {
        rank: 1,
        label: "cache".into(),
        path: Some("/cache".into()),
        observed_bytes: Some(42),
        reclaimable: "up to 42 B".into(),
        reclaimable_measurement: Envelope::default(),
        risk: "low".into(),
        confidence: "unknown".into(),
        how: "inspect".into(),
        measurement: Envelope::default(),
        skipped: false,
        growth: None,
        within: None,
    };
    let candidate_json = serde_json::to_value(candidate).unwrap();
    assert_eq!(
        candidate_json["reclaimableMeasurement"]["quality"],
        "unknown"
    );
    assert!(candidate_json["reclaimableMeasurement"]["domain"].is_string());
}

fn unavailable_records() -> (Vec<Record>, Vec<Record>) {
    let root = Record {
        depth: 0,
        name: NativeName::unix(b"/r".to_vec()),
        kind: Kind::Dir,
        ..Default::default()
    };
    let available = Record {
        depth: 1,
        name: NativeName::unix(b"file".to_vec()),
        kind: Kind::File,
        size: 7,
        alloc: 7,
        mtime: 10,
        ..Default::default()
    };
    let unavailable = Record {
        depth: 1,
        name: NativeName::unix(b"file".to_vec()),
        kind: Kind::File,
        flags: flags::STAT_ERROR,
        available: EntryFields::empty(),
        ..Default::default()
    };
    (vec![root.clone(), available], vec![root, unavailable])
}

#[test]
fn stat_error_core_records_omit_zero_values_and_are_unknown() {
    let (_, unavailable) = unavailable_records();
    let stream = Box::new(unavailable.into_iter().map(Ok));
    let listing =
        diskdrift_core::browse::children(stream, &[NativeName::unix(b"/r".to_vec())], 10).unwrap();
    let child = listing.items.first().expect("unavailable child is listed");
    let json = serde_json::to_string(child).unwrap();
    assert_unknown_omits(
        &json,
        &["allocatedBytes", "logicalBytes", "entries", "newestMtime"],
    );
    assert!(json.contains("\"quality\":\"unknown\""), "{json}");

    let (_, unavailable) = unavailable_records();
    let top = diskdrift_core::top::top(
        Box::new(unavailable.into_iter().map(Ok)),
        &Default::default(),
    )
    .unwrap();
    let json = serde_json::to_string(top.files.first().expect("unavailable file is top")).unwrap();
    assert_unknown_omits(
        &json,
        &["allocatedBytes", "logicalBytes", "entries", "mtime"],
    );
    assert!(json.contains("\"quality\":\"unknown\""), "{json}");
}

#[test]
fn stat_error_diff_records_do_not_emit_zero_deltas_as_observed() {
    let (old, new) = unavailable_records();
    let report = diff(
        Box::new(old.into_iter().map(Ok)),
        Box::new(new.into_iter().map(Ok)),
        DiffOptions {
            sort: diskdrift_core::diff::DiffSort::Path,
            ..Default::default()
        },
    )
    .unwrap();
    let json = serde_json::to_string(&report).unwrap();
    assert_unknown_omits(
        &json,
        &[
            "oldLogical",
            "newLogical",
            "oldAllocated",
            "newAllocated",
            "deltaAllocated",
        ],
    );
    assert!(json.contains("\"quality\":\"unknown\""), "{json}");
}

fn root() -> Record {
    Record {
        depth: 0,
        name: NativeName::unix(b"/r".to_vec()),
        kind: Kind::Dir,
        ..Default::default()
    }
}

fn file(name: &[u8], size: u64, alloc: u64, available: EntryFields) -> Record {
    Record {
        depth: 1,
        name: NativeName::unix(name.to_vec()),
        kind: Kind::File,
        size,
        alloc,
        available,
        ..Default::default()
    }
}

fn bytes_available() -> EntryFields {
    EntryFields::KIND
        .union(EntryFields::LOGICAL_SIZE)
        .union(EntryFields::ALLOCATED_SIZE)
}

#[test]
fn added_and_deleted_tallies_distinguish_zero_from_unknown_bytes() {
    let zero = file(b"entry", 0, 0, bytes_available());
    let unknown = file(b"entry", 0, 0, EntryFields::KIND);
    let base = vec![root()];

    let added_zero = serde_json::to_value(
        diff(
            Box::new(base.clone().into_iter().map(Ok)),
            Box::new(vec![root(), zero.clone()].into_iter().map(Ok)),
            DiffOptions::default(),
        )
        .unwrap(),
    )
    .unwrap();
    let added_unknown = serde_json::to_value(
        diff(
            Box::new(base.clone().into_iter().map(Ok)),
            Box::new(vec![root(), unknown.clone()].into_iter().map(Ok)),
            DiffOptions::default(),
        )
        .unwrap(),
    )
    .unwrap();
    let deleted_zero = serde_json::to_value(
        diff(
            Box::new(vec![root(), zero].into_iter().map(Ok)),
            Box::new(base.clone().into_iter().map(Ok)),
            DiffOptions::default(),
        )
        .unwrap(),
    )
    .unwrap();
    let deleted_unknown = serde_json::to_value(
        diff(
            Box::new(vec![root(), unknown].into_iter().map(Ok)),
            Box::new(base.into_iter().map(Ok)),
            DiffOptions::default(),
        )
        .unwrap(),
    )
    .unwrap();

    for side in ["added", "deleted"] {
        let zero = if side == "added" {
            &added_zero
        } else {
            &deleted_zero
        };
        assert_eq!(zero["summary"][side]["logicalBytes"], 0);
        assert_eq!(zero["summary"][side]["allocatedBytes"], 0);
        assert_eq!(zero["summary"][side]["quality"], "observed");

        let unknown = if side == "added" {
            &added_unknown
        } else {
            &deleted_unknown
        };
        assert!(unknown["summary"][side].get("logicalBytes").is_none());
        assert!(unknown["summary"][side].get("allocatedBytes").is_none());
        assert_eq!(unknown["summary"][side]["quality"], "unknown");
        assert_eq!(unknown["quality"], "lower_bound");
    }
}

#[test]
fn browse_diff_and_top_aggregates_report_all_unknown_and_partial_quality() {
    let unknown_root = Record {
        available: EntryFields::KIND,
        ..root()
    };
    let known_root = root();
    let unknown_file = file(b"unknown", 0, 0, EntryFields::KIND);
    let known_zero = file(b"known", 0, 0, bytes_available());

    let listing = children(
        Box::new(vec![unknown_root.clone()].into_iter().map(Ok)),
        &[NativeName::unix(b"/r".to_vec())],
        10,
    )
    .unwrap();
    let listing_json = serde_json::to_value(&listing).unwrap();
    assert_eq!(listing_json["quality"], "unknown");
    assert!(listing_json["dir"].get("allocatedBytes").is_none());

    let partial_listing = children(
        Box::new(
            vec![known_root.clone(), known_zero.clone(), unknown_file.clone()]
                .into_iter()
                .map(Ok),
        ),
        &[NativeName::unix(b"/r".to_vec())],
        10,
    )
    .unwrap();
    let partial_listing_json = serde_json::to_value(&partial_listing).unwrap();
    assert_eq!(partial_listing_json["quality"], "lower_bound");
    assert_eq!(partial_listing_json["dir"]["allocatedBytes"], 0);

    let all_unknown_top = top(
        Box::new(vec![unknown_root.clone()].into_iter().map(Ok)),
        &TopOptions::default(),
    )
    .unwrap();
    let all_unknown_top_json = serde_json::to_value(&all_unknown_top).unwrap();
    assert_eq!(all_unknown_top_json["quality"], "unknown");
    assert!(all_unknown_top_json.get("totalAllocated").is_none());

    let partial_top = top(
        Box::new(
            vec![known_root.clone(), known_zero, unknown_file]
                .into_iter()
                .map(Ok),
        ),
        &TopOptions::default(),
    )
    .unwrap();
    let partial_top_json = serde_json::to_value(&partial_top).unwrap();
    assert_eq!(partial_top_json["quality"], "lower_bound");
    assert_eq!(partial_top_json["totalAllocated"], 0);

    let all_unknown_diff = diff_children(
        Box::new(vec![unknown_root.clone()].into_iter().map(Ok)),
        Box::new(vec![unknown_root].into_iter().map(Ok)),
        &[NativeName::unix(b"/r".to_vec())],
        10,
    )
    .unwrap();
    let all_unknown_diff_json = serde_json::to_value(&all_unknown_diff).unwrap();
    assert_eq!(all_unknown_diff_json["quality"], "unknown");
    assert!(all_unknown_diff_json["dir"].get("oldAllocated").is_none());
}
