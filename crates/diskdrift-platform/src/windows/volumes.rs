//! Windows volume enumeration, mount aliases and exact volume identities.

use super::native_path;
use crate::{MountInfo, ScanLayout};
use diskdrift_core::native::NativePath;
use diskdrift_core::record::{FileIdScheme, FileIdScope, TimestampResolution};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetVolumeInformationW,
    GetVolumePathNamesForVolumeNameW,
};

const ERROR_MORE_DATA: u32 = 234;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountedVolume {
    pub identity: NativePath,
    pub mount_path: NativePath,
    pub aliases: Vec<NativePath>,
}

#[derive(Debug, Clone)]
struct VolumeRecord {
    identity: NativePath,
    guid: Vec<u16>,
    mount_paths: Vec<OsString>,
    fs_type: String,
    name: Option<String>,
    serial: u32,
}

struct VolumeHandle(HANDLE);

impl Drop for VolumeHandle {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE && !self.0.is_null() {
            unsafe { FindVolumeClose(self.0) };
        }
    }
}

pub fn enumerate() -> io::Result<Vec<MountedVolume>> {
    Ok(enumerate_records()?
        .into_iter()
        .filter_map(|record| {
            let first = record.mount_paths.first()?.clone();
            let mut aliases = record
                .mount_paths
                .into_iter()
                .map(|p| native_path(p.as_os_str()))
                .collect::<Vec<_>>();
            sort_and_dedup_aliases(&mut aliases);
            Some(MountedVolume {
                identity: record.identity,
                mount_path: native_path(first.as_os_str()),
                aliases,
            })
        })
        .collect())
}

pub fn deduplicate_mounted_volumes(volumes: Vec<MountedVolume>) -> Vec<MountedVolume> {
    let mut by_identity: HashMap<NativePath, MountedVolume> = HashMap::new();
    for mut volume in volumes {
        let identity_key = semantic_native_path(&volume.identity);
        sort_and_dedup_aliases(&mut volume.aliases);
        if let Some(existing) = by_identity.get_mut(&identity_key) {
            existing.aliases.push(volume.mount_path);
            existing.aliases.append(&mut volume.aliases);
            sort_and_dedup_aliases(&mut existing.aliases);
        } else {
            by_identity.insert(identity_key, volume);
        }
    }
    let mut result: Vec<_> = by_identity.into_values().collect();
    result.sort_by(|a, b| a.mount_path.cmp(&b.mount_path));
    result
}

pub fn mounts() -> io::Result<crate::MountInventory> {
    let mut out = Vec::new();
    let (records, mut errors) = enumerate_records_with_errors()?;
    for volume in records {
        for mount in volume.mount_paths {
            match statfs_path(Path::new(&mount)) {
                Ok(mut info) => {
                    info.device = volume_guid_display(&volume.guid);
                    info.fs_type = volume.fs_type.clone();
                    info.dev = volume.serial as u64;
                    out.push(info);
                }
                Err(error) => {
                    tracing::warn!(
                        mount = %mount.to_string_lossy(),
                        "could not read mounted Windows volume: {error}"
                    );
                    errors.push(format!("{}: {error}", mount.display()));
                }
            }
        }
    }
    if out.is_empty() {
        return Err(io::Error::other(
            "could not read any mounted Windows volume",
        ));
    }
    Ok(crate::MountInventory {
        mounts: out,
        errors,
    })
}

pub fn statfs_path(path: &Path) -> io::Result<MountInfo> {
    let volume = volume_for_path(path)?;
    let capacity = crate::volume_capacity(&volume.mount_path)?;
    Ok(MountInfo {
        mount_point: volume.mount_path,
        device: volume.guid_string,
        fs_type: volume.fs_type,
        total_bytes: capacity.total_bytes.ok_or_else(|| {
            io::Error::other("Windows volume capacity did not report total bytes")
        })?,
        free_bytes: capacity
            .free_bytes
            .ok_or_else(|| io::Error::other("Windows volume capacity did not report free bytes"))?,
        available_bytes: capacity.available_bytes.ok_or_else(|| {
            io::Error::other("Windows volume capacity did not report available bytes")
        })?,
        read_only: false,
        local: true,
        browsable: true,
        dev: volume.serial as u64,
    })
}

pub fn scan_layout(root: &Path) -> io::Result<ScanLayout> {
    scan_layout_checked(root)
}

pub fn scan_layout_checked(root: &Path) -> io::Result<ScanLayout> {
    let info = disk_info(root)?;
    let mount = statfs_path(root)?;
    let identity = info
        .native_identity
        .clone()
        .ok_or_else(|| io::Error::other("Windows volume identity unavailable"))?;
    Ok(ScanLayout {
        same_filesystem_devs: vec![mount.dev],
        alias_paths: info
            .native_aliases
            .iter()
            .filter_map(native_to_path)
            .collect(),
        windows_identity: Some(identity),
        windows_mount_key: info.native_mount_path,
        windows_aliases: info.native_aliases,
        windows_file_id_scheme: info.file_id_scheme,
        windows_file_id_scope: info.file_id_scope,
        windows_timestamp_resolution: info.timestamp_resolution,
        windows_allocation_semantics: info.allocation_semantics,
        windows_allocation_provenance: info.allocation_provenance,
        ..ScanLayout::default()
    })
}

pub fn disk_info(path: &Path) -> io::Result<crate::DiskInfo> {
    let mount = volume_for_path(path)?;
    let capacity = crate::volume_capacity(path)?;
    Ok(crate::DiskInfo {
        mount_point: mount.mount_path.clone(),
        fs_type: mount.fs_type,
        volume_name: mount.name,
        volume_uuid: Some(mount.serial.to_string()),
        capacity_bytes: capacity.total_bytes,
        bsd_name: None,
        device: crate::DeviceInfo::default(),
        native_identity: Some(mount.identity),
        native_mount_path: Some(native_path(mount.mount_path.as_os_str())),
        native_aliases: mount.aliases,
        file_id_scheme: FileIdScheme::WindowsFileId128,
        file_id_scope: FileIdScope::Volume,
        allocation_semantics: "filesystem_reported".into(),
        allocation_provenance: Some("FILE_STANDARD_INFO.AllocationSize".into()),
        timestamp_resolution: TimestampResolution::Nanoseconds,
    })
}

#[derive(Debug, Clone)]
struct PathVolume {
    identity: NativePath,
    guid_string: String,
    mount_path: PathBuf,
    aliases: Vec<NativePath>,
    fs_type: String,
    name: Option<String>,
    serial: u32,
}

fn volume_for_path(path: &Path) -> io::Result<PathVolume> {
    let mut records = enumerate_records()?;
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let path_key = semantic_path_key(
        path.as_os_str()
            .encode_wide()
            .collect::<Vec<_>>()
            .as_slice(),
    );
    records.sort_by_key(|r| std::cmp::Reverse(best_match_length(r, &path_key)));
    let record = records
        .into_iter()
        .find(|r| {
            r.mount_paths.iter().any(|mount| {
                let mount_key = semantic_path_key(
                    mount
                        .as_os_str()
                        .encode_wide()
                        .collect::<Vec<_>>()
                        .as_slice(),
                );
                path_matches_mount_key(&path_key, &mount_key)
            }) || path_matches_mount_key(
                &path_key,
                &semantic_path_key(&r.guid[..r.guid.len().saturating_sub(1)]),
            )
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "path is not on a mounted Windows volume",
            )
        })?;
    let first_mount = record.mount_paths.first().cloned().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Windows volume is not mounted and has no path alias",
        )
    })?;
    let mount_path = record
        .mount_paths
        .iter()
        .filter(|mount| {
            let mount_key = semantic_path_key(
                mount
                    .as_os_str()
                    .encode_wide()
                    .collect::<Vec<_>>()
                    .as_slice(),
            );
            path_matches_mount_key(&path_key, &mount_key)
        })
        .max_by_key(|mount| {
            semantic_path_key(
                mount
                    .as_os_str()
                    .encode_wide()
                    .collect::<Vec<_>>()
                    .as_slice(),
            )
            .len()
        })
        .cloned()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(first_mount));
    let mut aliases = record
        .mount_paths
        .iter()
        .map(|p| native_path(p.as_os_str()))
        .collect::<Vec<_>>();
    sort_and_dedup_aliases(&mut aliases);
    Ok(PathVolume {
        identity: record.identity,
        guid_string: String::from_utf16_lossy(&record.guid[..record.guid.len().saturating_sub(1)]),
        mount_path,
        aliases,
        fs_type: record.fs_type,
        name: record.name,
        serial: record.serial,
    })
}

fn best_match_length(record: &VolumeRecord, path: &[u16]) -> usize {
    record
        .mount_paths
        .iter()
        .map(|mount| {
            semantic_path_key(
                mount
                    .as_os_str()
                    .encode_wide()
                    .collect::<Vec<_>>()
                    .as_slice(),
            )
        })
        .filter(|mount| path_matches_mount_key(path, mount))
        .map(|mount| mount.len())
        .chain(
            path_matches_mount_key(
                path,
                &semantic_path_key(&record.guid[..record.guid.len().saturating_sub(1)]),
            )
            .then_some(
                semantic_path_key(&record.guid[..record.guid.len().saturating_sub(1)]).len(),
            ),
        )
        .max()
        .unwrap_or(0)
}

fn enumerate_records() -> io::Result<Vec<VolumeRecord>> {
    enumerate_records_with_errors().map(|(records, _)| records)
}

fn enumerate_records_with_errors() -> io::Result<(Vec<VolumeRecord>, Vec<String>)> {
    let mut buffer = vec![0u16; 1024];
    let handle = unsafe { FindFirstVolumeW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::from_raw_os_error(
            unsafe { GetLastError() } as i32
        ));
    }
    let _handle = VolumeHandle(handle);
    let mut records = Vec::new();
    let mut errors = Vec::new();
    loop {
        let guid = trim_nul(&buffer);
        let guid_display = volume_guid_display(&guid);
        match volume_paths(&guid).and_then(|paths| {
            volume_information(&guid).map(|(name, fs_type, serial)| VolumeRecord {
                identity: NativePath::windows(guid[..guid.len().saturating_sub(1)].to_vec()),
                guid: guid.clone(),
                mount_paths: paths,
                fs_type,
                name,
                serial,
            })
        }) {
            Ok(record) => records.push(record),
            Err(error) => errors.push(format!("{guid_display}: {error}")),
        }
        if unsafe { FindNextVolumeW(handle, buffer.as_mut_ptr(), buffer.len() as u32) } == 0 {
            let error = unsafe { GetLastError() };
            if error == 18 {
                break;
            }
            return Err(io::Error::from_raw_os_error(error as i32));
        }
    }
    if records.is_empty() && !errors.is_empty() {
        return Err(io::Error::other(format!(
            "could not enumerate mounted Windows volumes: {}",
            errors.join("; ")
        )));
    }
    for error in &errors {
        tracing::warn!("could not enumerate Windows volume: {error}");
    }
    Ok((records, errors))
}

fn volume_paths(guid: &[u16]) -> io::Result<Vec<OsString>> {
    volume_paths_with_query(vec![0u16; 1024], |buffer, length| {
        let ok = unsafe {
            GetVolumePathNamesForVolumeNameW(
                guid.as_ptr(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                length,
            )
        };
        if ok != 0 {
            QueryResult::Success
        } else {
            let error = unsafe { GetLastError() };
            if error == ERROR_MORE_DATA || *length as usize > buffer.len() {
                QueryResult::MoreData((*length as usize).max(buffer.len() + 1))
            } else {
                QueryResult::Error(error)
            }
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryResult {
    Success,
    MoreData(usize),
    Error(u32),
}

fn volume_paths_with_query<F>(mut buffer: Vec<u16>, mut query: F) -> io::Result<Vec<OsString>>
where
    F: FnMut(&mut [u16], &mut u32) -> QueryResult,
{
    loop {
        let mut length = 0u32;
        match query(&mut buffer, &mut length) {
            QueryResult::Success => return Ok(parse_multi_sz(&buffer, length as usize)),
            QueryResult::MoreData(required) => {
                let next = required
                    .max(buffer.len().saturating_mul(2))
                    .max(buffer.len() + 1);
                buffer.resize(next, 0);
            }
            QueryResult::Error(code) => {
                return Err(io::Error::other(format!(
                    "GetVolumePathNamesForVolumeNameW failed while reading mounted paths (error {code})"
                )));
            }
        }
    }
}

fn parse_multi_sz(buffer: &[u16], length: usize) -> Vec<OsString> {
    let limit = length.min(buffer.len());
    let mut paths = Vec::new();
    let mut start = 0;
    for i in 0..limit {
        if buffer[i] == 0 {
            if i > start {
                paths.push(OsString::from_wide(&buffer[start..i]));
            } else {
                break;
            }
            start = i + 1;
        }
    }
    paths
}

fn semantic_path_key(units: &[u16]) -> Vec<u16> {
    let mut key = units
        .iter()
        .map(|unit| {
            if *unit == b'/' as u16 {
                b'\\' as u16
            } else {
                *unit
            }
        })
        .collect::<Vec<_>>();
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
    if starts_with_units_case_insensitive(&key, VERBATIM_UNC) {
        key = [
            vec![b'\\' as u16, b'\\' as u16],
            key[VERBATIM_UNC.len()..].to_vec(),
        ]
        .concat();
    } else if starts_with_units_case_insensitive(&key, VERBATIM) {
        key = key[VERBATIM.len()..].to_vec();
    }
    while key.len() > 3 && key.last() == Some(&(b'\\' as u16)) {
        key.pop();
    }
    for unit in &mut key {
        if (b'A' as u16..=b'Z' as u16).contains(unit) {
            *unit += b'a' as u16 - b'A' as u16;
        }
    }
    key
}

fn starts_with_units_case_insensitive(value: &[u16], prefix: &[u16]) -> bool {
    value.get(..prefix.len()).is_some_and(|head| {
        head.iter()
            .zip(prefix)
            .all(|(a, b)| ascii_lower(*a) == ascii_lower(*b))
    })
}

fn ascii_lower(unit: u16) -> u16 {
    if (b'A' as u16..=b'Z' as u16).contains(&unit) {
        unit + b'a' as u16 - b'A' as u16
    } else {
        unit
    }
}

#[cfg(test)]
fn path_matches_mount(path: &[u16], mount: &[u16]) -> bool {
    path_matches_mount_key(&semantic_path_key(path), &semantic_path_key(mount))
}

fn path_matches_mount_key(path: &[u16], mount: &[u16]) -> bool {
    path == mount
        || path.starts_with(mount)
            && (mount.last() == Some(&(b'\\' as u16))
                || path.get(mount.len()) == Some(&(b'\\' as u16)))
}

fn semantic_native_path(path: &NativePath) -> NativePath {
    match path {
        NativePath::Windows(units) => NativePath::windows(semantic_path_key(units)),
        NativePath::Unix(bytes) => NativePath::unix(bytes.clone()),
    }
}

fn sort_and_dedup_aliases(aliases: &mut Vec<NativePath>) {
    aliases.sort_by_key(semantic_native_path);
    aliases.dedup_by(|left, right| semantic_native_path(left) == semantic_native_path(right));
}

fn volume_information(guid: &[u16]) -> io::Result<(Option<String>, String, u32)> {
    let mut name = [0u16; 256];
    let mut fs = [0u16; 256];
    let (mut serial, mut component, mut flags) = (0u32, 0u32, 0u32);
    let ok = unsafe {
        GetVolumeInformationW(
            guid.as_ptr(),
            name.as_mut_ptr(),
            name.len() as u32,
            &mut serial,
            &mut component,
            &mut flags,
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { GetLastError() } as i32
        ));
    }
    let name =
        String::from_utf16_lossy(&name[..name.iter().position(|&u| u == 0).unwrap_or(name.len())]);
    let fs = String::from_utf16_lossy(&fs[..fs.iter().position(|&u| u == 0).unwrap_or(fs.len())]);
    Ok(((!name.is_empty()).then_some(name), fs, serial))
}

fn trim_nul(value: &[u16]) -> Vec<u16> {
    let len = value.iter().position(|&u| u == 0).unwrap_or(value.len());
    let mut out = value[..len].to_vec();
    out.push(0);
    out
}

fn volume_guid_display(guid: &[u16]) -> String {
    String::from_utf16_lossy(&guid[..guid.len().saturating_sub(1)])
}

fn native_to_path(value: &NativePath) -> Option<PathBuf> {
    match value {
        NativePath::Windows(units) => Some(PathBuf::from(OsString::from_wide(units))),
        NativePath::Unix(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_keys_match_case_and_verbatim_aliases() {
        let drive =
            semantic_path_key(r#"C:\Data\Folder"#.encode_utf16().collect::<Vec<_>>().as_slice());
        let verbatim = semantic_path_key(
            r#"\\?\c:\data\folder"#.encode_utf16().collect::<Vec<_>>().as_slice(),
        );
        let unc = semantic_path_key(
            r#"\\Server\Share\Folder"#.encode_utf16().collect::<Vec<_>>().as_slice(),
        );
        let verbatim_unc = semantic_path_key(
            r#"\\?\unc\server\share\folder"#.encode_utf16().collect::<Vec<_>>().as_slice(),
        );
        assert_eq!(drive, verbatim);
        assert_eq!(unc, verbatim_unc);
    }

    #[test]
    fn semantic_mount_matching_respects_component_boundaries() {
        let mount = r#"C:\Mounted"#.encode_utf16().collect::<Vec<_>>();
        assert!(path_matches_mount(
            r#"c:\mounted\child"#.encode_utf16().collect::<Vec<_>>().as_slice(),
            &mount,
        ));
        assert!(!path_matches_mount(
            r#"C:\MountedElsewhere"#.encode_utf16().collect::<Vec<_>>().as_slice(),
            &mount,
        ));
    }

    #[test]
    fn volume_path_query_retries_using_required_length() {
        let expected = [r#"C:\"#, r#"C:\MountedFolder\"#];
        let encoded = expected
            .iter()
            .flat_map(|path| path.encode_utf16().chain(std::iter::once(0)))
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut calls = 0;
        let paths = volume_paths_with_query(vec![0; 2], |buffer, length| {
            calls += 1;
            if calls == 1 {
                *length = encoded.len() as u32;
                QueryResult::MoreData(encoded.len())
            } else {
                buffer.copy_from_slice(&encoded);
                *length = encoded.len() as u32;
                QueryResult::Success
            }
        })
        .unwrap();
        assert_eq!(
            paths,
            expected.iter().map(OsString::from).collect::<Vec<_>>()
        );
        assert_eq!(calls, 2);
    }

    #[test]
    fn volume_path_query_surfaces_adapter_errors() {
        let error = volume_paths_with_query(vec![0; 2], |_buffer, _length| QueryResult::Error(5))
            .unwrap_err();
        assert!(error.to_string().contains("mounted paths"));
        assert!(error.to_string().contains("error 5"));
    }
}
