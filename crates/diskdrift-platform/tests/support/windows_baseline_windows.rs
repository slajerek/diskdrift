use diskdrift_core::native::{NativeName, NativePath, RootKind};
use diskdrift_core::record::{EntryFields, FileId, FileIdScheme, Kind, TimestampResolution};
use diskdrift_core::walk::{DirLister, WalkOptions, walk};
use diskdrift_platform::windows::lister::{WideLister, file_id_from_bytes, filetime_to_unix_nanos};
use diskdrift_platform::windows::space::capacity;
use diskdrift_platform::windows::volumes::{MountedVolume, deduplicate_mounted_volumes};
use diskdrift_platform::{ListerKind, disk_info, lister, native_path, native_root, scan_layout};
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

fn fixture_root() -> PathBuf {
    let path = std::env::var_os("DISKDRIFT_WINDOWS_FIXTURE").unwrap_or_else(|| {
        panic!(
            "required native Windows fixture unavailable: set DISKDRIFT_WINDOWS_FIXTURE to a disposable directory under target/"
        )
    });
    let path = PathBuf::from(path);
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("diskdrift crate must be in the workspace")
        .join("target");
    let canonical_target = target.canonicalize().unwrap_or(target);
    let canonical_path = path.canonicalize().unwrap_or(path.clone());
    assert!(
        canonical_path.starts_with(&canonical_target),
        "Windows fixtures must be under target/: {}",
        path.display()
    );
    assert!(
        canonical_path.is_dir(),
        "required native Windows fixture unavailable: {}",
        path.display()
    );
    path
}

#[test]
fn native_roots_preserve_drive_unc_guid_and_verbatim_units() {
    let cases = [
        (r"C:\", RootKind::Drive, false),
        (r"\\server\share", RootKind::Unc, false),
        (r"\\?\C:\data", RootKind::Drive, true),
        (r"\\?\UNC\server\share", RootKind::Unc, true),
        (
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\",
            RootKind::VolumeGuid,
            true,
        ),
    ];
    for (text, kind, verbatim) in cases {
        let os = OsString::from_wide(&text.encode_utf16().collect::<Vec<_>>());
        let root = native_root(&os).unwrap();
        assert_eq!(root.root_kind, kind);
        assert_eq!(root.verbatim, verbatim);
        assert_eq!(
            root.path,
            NativePath::windows(text.encode_utf16().collect())
        );
    }

    for (text, kind) in [
        (r"\\?\unc\SERVER\Share\", RootKind::Unc),
        (
            r"\\?\volume{01234567-89AB-CDEF-0123-456789ABCDEF}\",
            RootKind::VolumeGuid,
        ),
    ] {
        let os = OsString::from_wide(&text.encode_utf16().collect::<Vec<_>>());
        let root = native_root(&os).unwrap();
        assert_eq!(root.root_kind, kind);
        assert_eq!(
            root.path,
            NativePath::windows(text.encode_utf16().collect())
        );
    }
}

#[test]
fn native_names_keep_non_bmp_and_lone_surrogates_without_utf8() {
    let units = vec![
        b'C' as u16,
        b':'.into(),
        b'\\'.into(),
        0xd800,
        b'x'.into(),
        0xd83d,
        b'y'.into(),
    ];
    let os = OsString::from_wide(&units);
    assert_eq!(native_path(&os), NativePath::windows(units.clone()));
    let name = NativeName::windows(units.clone());
    assert_eq!(name, NativeName::windows(os.encode_wide().collect()));
    assert_eq!(
        name.display().chars().filter(|c| *c == '\u{fffd}').count(),
        2
    );
}

#[test]
fn windows_metadata_helpers_preserve_file_id_and_filetime_precision() {
    let bytes: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    assert_eq!(file_id_from_bytes(bytes), FileId::from_le_bytes(bytes));
    assert_eq!(filetime_to_unix_nanos(116444736000000000), Some((0, 0)));
    assert_eq!(filetime_to_unix_nanos(116444736000000001), Some((0, 100)));
    assert_eq!(
        filetime_to_unix_nanos(116444735999999999),
        Some((-1, 999_999_900))
    );
}

#[test]
fn wide_lister_reports_exact_names_ids_links_reparse_and_allocation() {
    let root = fixture_root();
    let lister = WideLister;
    let mut entries = Vec::new();
    lister.list(&root, &mut entries).unwrap();
    let lone = entries
        .iter()
        .find(|entry| matches!(&entry.name, NativeName::Windows(units) if units.contains(&0xd800)))
        .expect("required fixture entry with a lone surrogate is unavailable");
    assert!(lone.available.contains(EntryFields::FILE_ID));
    assert_eq!(lone.file_id_scheme, FileIdScheme::WindowsFileId128);
    assert_eq!(lone.timestamp_resolution, TimestampResolution::Nanoseconds);
    assert!(lone.nlink > 0);
    assert!(
        lone.available.contains(EntryFields::ALLOCATED_SIZE),
        "regular-file allocation provenance is unavailable"
    );
    assert!(
        entries.iter().any(|entry| entry.kind == Kind::Symlink),
        "required reparse fixture entry is unavailable"
    );
    let sparse = entries
        .iter()
        .find(|entry| entry.name.display().eq_ignore_ascii_case("sparse.bin"))
        .expect("required sparse fixture entry is unavailable");
    assert!(sparse.available.contains(EntryFields::ALLOCATED_SIZE));
    assert!(sparse.size >= 1_048_576);
    assert!(
        sparse.alloc < sparse.size,
        "sparse allocation was not observed"
    );

    let reparse = entries
        .iter()
        .find(|entry| entry.name.display().eq_ignore_ascii_case("reparse-link"))
        .expect("required reparse fixture entry is unavailable");
    assert_eq!(reparse.kind, Kind::Symlink);
    assert_eq!(reparse.dev, lister.stat(&root).unwrap().dev);
    let reparse_path = root.join("reparse-link");
    assert_eq!(lister.stat(&reparse_path).unwrap().kind, Kind::Symlink);

    let mut records = Vec::new();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let report = walk(
        &root,
        &lister,
        &WalkOptions {
            threads: 1,
            ..WalkOptions::default()
        },
        &mut |record: &diskdrift_core::record::Record, _: &std::path::Path| {
            records.push(record.clone());
            Ok(())
        },
        &cancel,
        None,
    )
    .unwrap();
    assert_eq!(report.entries, records.len() as u64);
    assert_eq!(
        records
            .iter()
            .filter(|record| record.name.display().eq_ignore_ascii_case("regular.txt"))
            .count(),
        1,
        "reparse link target was followed"
    );
}

#[test]
fn capacity_reports_an_envelope_and_sparse_allocation_without_false_precision() {
    let root = fixture_root();
    let sample = capacity(&root).expect("required native Windows capacity fixture unavailable");
    assert!(sample.total_bytes.is_some());
    assert!(sample.free_bytes.is_some());
    assert!(sample.available_bytes.is_some());
    assert_eq!(sample.timestamp_resolution, TimestampResolution::Seconds);
    assert_eq!(
        sample.quality,
        diskdrift_platform::MeasurementQuality::Observed
    );
    assert!(
        sample
            .provenance
            .as_deref()
            .is_some_and(|value| value.contains("lpTotalNumberOfFreeBytes"))
    );
}

#[test]
fn native_volume_metadata_and_checked_layout_preserve_exact_aliases() {
    let root = fixture_root();
    let info = disk_info(&root).expect("required native Windows disk-info fixture unavailable");
    let layout = scan_layout(&root)
        .expect("required native Windows scan layout fixture unavailable");
    let identity = info
        .native_identity
        .clone()
        .expect("Windows volume identity is unavailable");
    let mount = info
        .native_mount_path
        .clone()
        .expect("Windows mount key is unavailable");
    assert_eq!(layout.windows_identity, Some(identity.clone()));
    assert_eq!(layout.windows_mount_key, Some(mount.clone()));
    assert_eq!(layout.windows_aliases, info.native_aliases);
    assert_eq!(
        layout.windows_file_id_scheme,
        FileIdScheme::WindowsFileId128
    );
    assert_eq!(
        layout.windows_file_id_scope,
        diskdrift_core::record::FileIdScope::Volume
    );
    assert_eq!(layout.windows_allocation_semantics, "filesystem_reported");
    assert_eq!(
        layout.windows_allocation_provenance.as_deref(),
        Some("FILE_STANDARD_INFO.AllocationSize")
    );
    assert_eq!(
        layout.windows_timestamp_resolution,
        TimestampResolution::Nanoseconds
    );
    assert!(info.native_aliases.iter().any(|alias| alias == &mount));
}

#[test]
fn volume_lookup_accepts_case_insensitive_verbatim_fixture_paths() {
    let root = fixture_root();
    let units = root.as_os_str().encode_wide().collect::<Vec<_>>();
    let lower = units
        .iter()
        .map(|unit| {
            if (*unit >= b'A' as u16) && (*unit <= b'Z' as u16) {
                *unit + 32
            } else {
                *unit
            }
        })
        .collect::<Vec<_>>();
    let lower = PathBuf::from(OsString::from_wide(&lower));
    let verbatim = PathBuf::from(OsString::from_wide(
        &[r"\\?\".encode_utf16().collect::<Vec<_>>(), units].concat(),
    ));
    let expected = disk_info(&root).expect("required native Windows disk-info fixture unavailable");
    assert_eq!(
        disk_info(&lower).unwrap().native_identity,
        expected.native_identity
    );
    assert_eq!(
        disk_info(&verbatim).unwrap().native_identity,
        expected.native_identity
    );
}

#[test]
fn mounted_folder_aliases_are_deduplicated_by_exact_volume_identity() {
    let root = fixture_root();
    let identity = native_path(root.as_os_str());
    let mounts = vec![
        MountedVolume {
            identity: identity.clone(),
            mount_path: native_path(OsStr::new(r"C:\")),
            aliases: vec![identity.clone()],
        },
        MountedVolume {
            identity: identity.clone(),
            mount_path: native_path(OsStr::new(r"C:\mounted")),
            aliases: vec![native_path(OsStr::new(r"C:\mounted"))],
        },
    ];
    let deduped = deduplicate_mounted_volumes(mounts);
    assert_eq!(deduped.len(), 1);
    assert_eq!(deduped[0].aliases.len(), 2);
}

#[test]
fn scan_layout_aliases_are_omitted_by_the_real_walker() {
    let root = fixture_root();
    let layout = scan_layout(&root)
        .expect("required native Windows scan layout fixture unavailable");
    let alias = layout
        .alias_paths
        .iter()
        .find(|path| path.starts_with(&root) && *path != &root)
        .cloned()
        .expect("required real mounted-folder alias is unavailable");
    let lister = lister(ListerKind::Auto);
    let mut seen = Vec::new();
    let report = walk(
        &root,
        lister.as_ref(),
        &WalkOptions {
            threads: 1,
            same_filesystem_devs: layout.same_filesystem_devs,
            alias_paths: layout.alias_paths,
            ..WalkOptions::default()
        },
        &mut |_: &diskdrift_core::record::Record, path: &std::path::Path| {
            seen.push(path.to_path_buf());
            Ok(())
        },
        &std::sync::atomic::AtomicBool::new(false),
        None,
    )
    .expect("real scan layout walk failed");

    assert!(report.entries > 0);
    assert!(
        seen.iter().all(|path| !path.starts_with(&alias)),
        "mounted-folder alias was walked twice: {}",
        alias.display()
    );
}

#[test]
fn unavailable_windows_layout_is_an_explicit_error() {
    let missing = std::env::current_dir()
        .unwrap()
        .join("target")
        .join("windows-fixture-unavailable");
    let error = scan_layout(&missing).expect_err("unavailable volume must not become a default layout");
    assert!(error.to_string().contains("volume") || error.to_string().contains("path"));
}
