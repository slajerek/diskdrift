//! Wide Win32 directory enumeration and no-follow metadata reads.

use diskdrift_core::native::NativeName;
use diskdrift_core::record::{
    EntryFields, FileId, FileIdScheme, FileIdScope, Kind, TimestampResolution,
};
use diskdrift_core::walk::{DirLister, RawEntry};
use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
    FILE_INFO_BY_HANDLE_CLASS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_STANDARD_INFO, FIND_FIRST_EX_LARGE_FETCH, FileBasicInfo, FileIdInfo,
    FileStandardInfo, FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW,
    FindNextFileW, GetFileInformationByHandleEx, OPEN_EXISTING, WIN32_FIND_DATAW,
};

const ERROR_NO_MORE_FILES: u32 = 18;
const FILETIME_UNIX_EPOCH: i128 = 116_444_736_000_000_000;

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE && !self.0.is_null() {
            // SAFETY: the handle is owned by this guard and is closed exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

struct FindHandle(HANDLE);

impl Drop for FindHandle {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE && !self.0.is_null() {
            // SAFETY: the handle is owned by this guard and is closed exactly once.
            unsafe { FindClose(self.0) };
        }
    }
}

/// Win32 lister using raw UTF-16 names and `OPEN_REPARSE_POINT` metadata handles.
#[derive(Debug, Clone, Copy)]
pub struct WideLister;

impl WideLister {
    fn inspect(path: &Path) -> io::Result<RawEntry> {
        let path_wide = wide_path(path);
        // OPEN_REPARSE_POINT makes the final component itself observable instead
        // of following a symlink/junction into a different tree.
        let handle = unsafe {
            CreateFileW(
                path_wide.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(last_error());
        }
        let _handle = OwnedHandle(handle);
        let basic = query::<FILE_BASIC_INFO>(handle, FileBasicInfo)?;
        let standard = query::<FILE_STANDARD_INFO>(handle, FileStandardInfo)?;
        let id = query::<FILE_ID_INFO>(handle, FileIdInfo).ok();
        let attrs = basic.FileAttributes;
        let reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        let kind = if reparse {
            Kind::Symlink
        } else if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
            Kind::Dir
        } else {
            Kind::File
        };
        let mtime_value = filetime_i64_to_unix(basic.LastWriteTime);
        let btime_value = filetime_i64_to_unix(basic.CreationTime);
        let (mtime, mtime_nanos) = mtime_value.unwrap_or((0, 0));
        let (btime, btime_nanos) = btime_value.unwrap_or((0, 0));
        let mut available = EntryFields::KIND
            .union(EntryFields::LINK_COUNT)
            .union(timestamp_fields(mtime_value, btime_value));
        if standard.EndOfFile >= 0 {
            available = available.union(EntryFields::LOGICAL_SIZE);
        }
        if standard.AllocationSize >= 0 {
            available = available.union(EntryFields::ALLOCATED_SIZE);
        }
        let (file_id, dev) = match id {
            Some(id) => (
                file_id_from_bytes(id.FileId.Identifier),
                id.VolumeSerialNumber,
            ),
            None => (FileId::ZERO, 0),
        };
        if file_id != FileId::ZERO {
            available = available.union(EntryFields::FILE_ID);
        }
        let name = NativeName::windows(path_wide[..path_wide.len().saturating_sub(1)].to_vec());
        Ok(RawEntry {
            name,
            kind,
            size: if kind == Kind::Dir {
                0
            } else {
                standard.EndOfFile.max(0) as u64
            },
            alloc: standard.AllocationSize.max(0) as u64,
            mtime,
            mtime_nanos,
            btime,
            btime_nanos,
            ino: file_id.low,
            file_id,
            available,
            nlink: standard.NumberOfLinks,
            dev,
            timestamp_resolution: TimestampResolution::Nanoseconds,
            file_id_scheme: if file_id == FileId::ZERO {
                FileIdScheme::Unavailable
            } else {
                FileIdScheme::WindowsFileId128
            },
            file_id_scope: FileIdScope::Volume,
            is_mount_point: false,
            is_trigger: false,
            error: None,
        })
    }
}

impl DirLister for WideLister {
    fn name(&self) -> &'static str {
        "FindFirstFileExW"
    }

    fn stat(&self, path: &Path) -> io::Result<RawEntry> {
        Self::inspect(path)
    }

    fn list(&self, dir: &Path, out: &mut Vec<RawEntry>) -> io::Result<()> {
        let mut pattern = wide_path(dir);
        if !pattern.ends_with(&[b'\\' as u16]) {
            pattern.push(b'\\' as u16);
        }
        pattern.push(b'*' as u16);
        pattern.push(0);
        let mut data = WIN32_FIND_DATAW::default();
        let handle = unsafe {
            FindFirstFileExW(
                pattern.as_ptr(),
                FindExInfoBasic,
                &mut data as *mut WIN32_FIND_DATAW as *mut std::ffi::c_void,
                FindExSearchNameMatch,
                std::ptr::null(),
                FIND_FIRST_EX_LARGE_FETCH,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(last_error());
        }
        let _handle = FindHandle(handle);
        loop {
            let name = find_name(&data);
            if name != [b'.' as u16] && name != [b'.' as u16, b'.' as u16] {
                let child = dir.join(OsString::from_wide(&name));
                match Self::inspect(&child) {
                    Ok(mut entry) => {
                        // Keep the directory enumeration's exact leaf spelling;
                        // metadata APIs return the complete path, not the leaf.
                        entry.name = NativeName::windows(name);
                        out.push(entry);
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => out.push(RawEntry {
                        name: NativeName::windows(name),
                        error: Some(error.raw_os_error().unwrap_or(5)),
                        ..RawEntry::default()
                    }),
                }
            }
            let next = unsafe { FindNextFileW(handle, &mut data) };
            if next == 0 {
                let error = unsafe { GetLastError() };
                if error != ERROR_NO_MORE_FILES {
                    return Err(io::Error::from_raw_os_error(error as i32));
                }
                break;
            }
        }
        Ok(())
    }
}

fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn find_name(data: &WIN32_FIND_DATAW) -> Vec<u16> {
    let len = data
        .cFileName
        .iter()
        .position(|&u| u == 0)
        .unwrap_or(data.cFileName.len());
    data.cFileName[..len].to_vec()
}

fn query<T: Copy + Default>(handle: HANDLE, class: FILE_INFO_BY_HANDLE_CLASS) -> io::Result<T> {
    let mut value = T::default();
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            class,
            &mut value as *mut T as *mut std::ffi::c_void,
            std::mem::size_of::<T>() as u32,
        )
    };
    if ok == 0 {
        Err(last_error())
    } else {
        Ok(value)
    }
}

fn last_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

pub fn file_id_from_bytes(bytes: [u8; 16]) -> FileId {
    FileId::from_le_bytes(bytes)
}

pub fn filetime_to_unix_nanos(ticks: u64) -> Option<(i64, u32)> {
    if ticks == 0 {
        return None;
    }
    let nanos = (ticks as i128 - FILETIME_UNIX_EPOCH) * 100;
    let seconds = nanos.div_euclid(1_000_000_000);
    let subsecond = nanos.rem_euclid(1_000_000_000);
    Some((i64::try_from(seconds).ok()?, u32::try_from(subsecond).ok()?))
}

fn filetime_i64_to_unix(value: i64) -> Option<(i64, u32)> {
    (value > 0)
        .then(|| filetime_to_unix_nanos(value as u64))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_uses_euclidean_pre_epoch_units() {
        assert_eq!(
            filetime_to_unix_nanos(116444735999999999),
            Some((-1, 999_999_900))
        );
    }

    #[test]
    fn missing_filetimes_do_not_claim_timestamp_fields() {
        assert!(!timestamp_fields(None, None).contains(EntryFields::MTIME));
        assert!(!timestamp_fields(None, None).contains(EntryFields::BIRTH_TIME));
        assert!(timestamp_fields(Some((1, 0)), None).contains(EntryFields::MTIME));
        assert!(timestamp_fields(None, Some((1, 0))).contains(EntryFields::BIRTH_TIME));
    }
}

fn timestamp_fields(mtime: Option<(i64, u32)>, btime: Option<(i64, u32)>) -> EntryFields {
    let mut fields = EntryFields::empty();
    if mtime.is_some() {
        fields = fields
            .union(EntryFields::MTIME)
            .union(EntryFields::MTIME_NANOS);
    }
    if btime.is_some() {
        fields = fields
            .union(EntryFields::BIRTH_TIME)
            .union(EntryFields::BIRTH_TIME_NANOS);
    }
    fields
}
