use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION, HANDLE, HLOCAL, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetTokenInformation, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, SetFileSecurityW, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FlushFileBuffers, GetCompressedFileSizeW, GetFileInformationByHandle, LOCKFILE_EXCLUSIVE_LOCK,
    LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    MoveFileExW, UnlockFileEx,
};
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const SHARING: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

pub struct Lock {
    file: File,
    path: PathBuf,
    remove_on_drop: bool,
}

impl Lock {
    pub fn write_owner(&mut self, owner: &str) -> io::Result<()> {
        self.file.set_len(0)?;
        self.file.write_all(owner.as_bytes())
    }

    pub(crate) fn preserve_file(&mut self) {
        self.remove_on_drop = false;
    }

    fn release(&mut self, after_remove: impl FnOnce(&Path)) {
        if self.remove_on_drop {
            let _ = fs::remove_file(&self.path);
            self.remove_on_drop = false;
            // The pathname is gone while this handle still owns the byte-range lock.
            after_remove(&self.path);
        }
        let mut overlapped = OVERLAPPED::default();
        unsafe {
            let _ = UnlockFileEx(
                self.file.as_raw_handle() as HANDLE,
                0,
                1,
                0,
                &mut overlapped,
            );
        }
    }

    #[cfg(test)]
    fn release_for_test(&mut self, after_remove: impl FnOnce(&Path)) {
        self.release(after_remove);
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        self.release(|_| {});
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn current_user_sid() -> io::Result<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if opened == 0 {
        return Err(io::Error::last_os_error());
    }

    let result = (|| {
        let mut required = 0;
        unsafe {
            let _ = GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut required);
        }
        if required == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0u8; required as usize];
        let read = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        };
        if read == 0 {
            return Err(io::Error::last_os_error());
        }

        let token_user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        let mut sid_text = std::ptr::null_mut();
        let converted = unsafe { ConvertSidToStringSidW(token_user.User.Sid, &mut sid_text) };
        if converted == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut length = 0;
        unsafe {
            while *sid_text.add(length) != 0 {
                length += 1;
            }
        }
        let sid = unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(sid_text, length)) };
        unsafe {
            let _ = LocalFree(sid_text as HLOCAL);
        }
        Ok(sid)
    })();
    unsafe {
        let _ = CloseHandle(token);
    }
    result
}

pub fn open_read_shared(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).share_mode(SHARING).open(path)
}

pub fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let count = file.seek_read(&mut buf[done..], offset + done as u64)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short positional read",
            ));
        }
        done += count;
    }
    Ok(())
}

fn lock_file(file: File, path: PathBuf) -> io::Result<Option<Lock>> {
    let mut overlapped = OVERLAPPED::default();
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle() as HANDLE,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if result != 0 {
        return Ok(Some(Lock {
            file,
            path,
            remove_on_drop: true,
        }));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        Ok(None)
    } else {
        Err(error)
    }
}

pub fn try_lock_exclusive(path: &Path) -> io::Result<Option<Lock>> {
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(SHARING)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    lock_file(file, path.to_path_buf())
}

pub fn probe_lock(path: &Path) -> io::Result<bool> {
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(SHARING)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) => {
            return Ok(true);
        }
        Err(error) => return Err(error),
    };
    let Some(mut lock) = lock_file(file, path.to_path_buf())? else {
        return Ok(true);
    };
    lock.preserve_file();
    Ok(false)
}

pub fn flush_file(file: &File) -> io::Result<()> {
    let result = unsafe { FlushFileBuffers(file.as_raw_handle() as HANDLE) };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Windows has no POSIX-equivalent parent-directory fsync.  Durability ends at
/// the flushed file and successful native replacement; startup validation and
/// checksums cover interruption after that boundary.
pub fn sync_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

pub fn sync_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}

pub fn replace_atomic(temporary: &Path, destination: &Path) -> io::Result<()> {
    // Without MOVEFILE_COPY_ALLOWED, Windows rejects cross-volume moves instead
    // of silently degrading this publication to copy-then-delete.
    let source = wide(temporary);
    let target = wide(destination);
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
pub fn same_file(a: &Path, b: &Path) -> bool {
    let Ok(first) = open_read_shared(a) else {
        return false;
    };
    let Ok(second) = open_read_shared(b) else {
        return false;
    };
    let mut first_info = BY_HANDLE_FILE_INFORMATION::default();
    let mut second_info = BY_HANDLE_FILE_INFORMATION::default();
    let first_ok =
        unsafe { GetFileInformationByHandle(first.as_raw_handle() as HANDLE, &mut first_info) };
    let second_ok =
        unsafe { GetFileInformationByHandle(second.as_raw_handle() as HANDLE, &mut second_info) };
    first_ok != 0
        && second_ok != 0
        && first_info.dwVolumeSerialNumber == second_info.dwVolumeSerialNumber
        && first_info.nFileIndexHigh == second_info.nFileIndexHigh
        && first_info.nFileIndexLow == second_info.nFileIndexLow
}

pub fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut current = path;
    while !current.exists() {
        missing.push(current.to_path_buf());
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
    }
    fs::create_dir_all(path)?;

    for created in missing.into_iter().rev() {
        set_private_acl(&created)?;
    }
    set_private_acl(path)
}

fn set_private_acl(path: &Path) -> io::Result<()> {
    // D:P prevents permissions from being inherited from a broader parent
    // directory. The user SID is explicit because Owner Rights is not an
    // equivalent current-user DACL entry.
    let user_sid = current_user_sid()?;
    let sddl = format!("D:P(A;OICI;FA;;;{user_sid})(A;OICI;FA;;;SY)");
    let sddl_wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let mut descriptor_len = 0;
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_wide.as_ptr(),
            1,
            &mut descriptor,
            &mut descriptor_len,
        )
    };
    if converted == 0 {
        return Err(io::Error::last_os_error());
    }

    let path_wide = wide(path);
    let applied = unsafe {
        SetFileSecurityW(
            path_wide.as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        )
    };
    let error = (applied == 0).then(io::Error::last_os_error);
    unsafe {
        let _ = LocalFree(descriptor as HLOCAL);
    }
    error.map_or(Ok(()), Err)
}

pub fn allocated_size(path: &Path) -> u64 {
    let path_wide = wide(path);
    let mut high = 0u32;
    let low = unsafe { GetCompressedFileSizeW(path_wide.as_ptr(), &mut high) };
    if low == u32::MAX {
        fs::metadata(path)
            .map(|metadata| metadata.len())
            .unwrap_or(0)
    } else {
        ((high as u64) << 32) | low as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskdrift_core::testutil::TestDir;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
        GetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, GetAce, GetSecurityDescriptorControl,
        GetTokenInformation, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows_sys::core::PWSTR;

    fn current_user_sid() -> String {
        let mut token: HANDLE = std::ptr::null_mut();
        assert_ne!(
            unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
            0
        );
        let mut required = 0;
        unsafe {
            let _ = GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut required);
        }
        assert!(required > 0);
        let mut buffer = vec![0u8; required as usize];
        assert_ne!(
            unsafe {
                GetTokenInformation(
                    token,
                    TokenUser,
                    buffer.as_mut_ptr().cast(),
                    required,
                    &mut required,
                )
            },
            0
        );
        let token_user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        let mut sid_text: PWSTR = std::ptr::null_mut();
        assert_ne!(
            unsafe { ConvertSidToStringSidW(token_user.User.Sid, &mut sid_text) },
            0
        );
        let mut length = 0;
        unsafe {
            while *sid_text.add(length) != 0 {
                length += 1;
            }
        }
        let sid = unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(sid_text, length)) };
        unsafe {
            let _ = LocalFree(sid_text as HLOCAL);
            let _ = CloseHandle(token);
        }
        sid
    }

    fn sid_string(sid: windows_sys::Win32::Security::PSID) -> String {
        let mut sid_text: PWSTR = std::ptr::null_mut();
        assert_ne!(unsafe { ConvertSidToStringSidW(sid, &mut sid_text) }, 0);
        let mut length = 0;
        unsafe {
            while *sid_text.add(length) != 0 {
                length += 1;
            }
        }
        let sid = unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(sid_text, length)) };
        unsafe {
            let _ = LocalFree(sid_text as HLOCAL);
        }
        sid
    }

    #[test]
    fn lock_path_is_removed_before_a_new_owner_can_acquire_it() {
        let dir = TestDir::new("windows-lock-release");
        let path = dir.path().join("lock");
        let mut lock = try_lock_exclusive(&path).unwrap().unwrap();

        lock.release_for_test(|path| {
            assert!(!path.exists(), "the lock pathname must be removed first");
            let replacement = try_lock_exclusive(path)
                .expect("the replacement attempt should reach the native lock operation");
            assert!(
                replacement.is_none(),
                "a replacement lock must fail because the old handle still owns the lock"
            );
        });
        assert!(!path.exists());
    }

    #[test]
    fn private_directories_have_explicit_current_user_and_system_acl() {
        let dir = TestDir::new("windows-private-directory");
        let private = dir.path().join("private").join("nested");
        create_private_dir(&private).unwrap();
        let user_sid = current_user_sid();

        for path in [dir.path().join("private"), private] {
            let path_wide = wide(&path);
            let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let result = unsafe {
                GetNamedSecurityInfoW(
                    path_wide.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut dacl,
                    std::ptr::null_mut(),
                    &mut descriptor,
                )
            };
            assert_eq!(result, 0);
            assert!(!dacl.is_null());
            let mut control = 0;
            let mut revision = 0;
            assert_ne!(
                unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
                0
            );
            assert_ne!(control & SE_DACL_PROTECTED, 0);

            let mut entries = Vec::new();
            let ace_count = unsafe { (*dacl).AceCount } as u32;
            for index in 0..ace_count {
                let mut ace = std::ptr::null_mut();
                assert_ne!(unsafe { GetAce(dacl, index, &mut ace) }, 0);
                let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
                assert_eq!(
                    allowed.Header.AceType, ACCESS_ALLOWED_ACE_TYPE as u8,
                    "private directory ACL must contain only allow entries"
                );
                entries.push((
                    sid_string(std::ptr::addr_of!(allowed.SidStart).cast_mut().cast()),
                    allowed.Mask,
                    allowed.Header.AceFlags,
                ));
            }
            assert_eq!(entries.len(), 2);
            for (sid, mask, flags) in &entries {
                assert_eq!(*mask, FILE_ALL_ACCESS);
                assert_eq!(
                    *flags & (1 | 2),
                    1 | 2,
                    "explicit entries must inherit to files and directories"
                );
                assert_eq!(*flags & 16, 0, "entries must not already be inherited");
                assert!(
                    sid == &user_sid || sid == "S-1-5-18",
                    "unexpected ACL SID {sid}"
                );
            }
            assert!(entries.iter().any(|(sid, _, _)| sid == &user_sid));
            assert!(entries.iter().any(|(sid, _, _)| sid == "S-1-5-18"));

            let mut sddl: PWSTR = std::ptr::null_mut();
            let mut sddl_len = 0;
            let converted = unsafe {
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    descriptor,
                    1,
                    DACL_SECURITY_INFORMATION,
                    &mut sddl,
                    &mut sddl_len,
                )
            };
            assert_ne!(converted, 0);
            let text = unsafe {
                String::from_utf16_lossy(std::slice::from_raw_parts(sddl, sddl_len as usize))
            };
            unsafe {
                let _ = LocalFree(descriptor as HLOCAL);
                let _ = LocalFree(sddl as HLOCAL);
            }
            assert!(
                text.starts_with("D:P"),
                "private directory ACL must not inherit broad permissions: {text}"
            );
            assert!(
                text.contains(&format!("(A;OICI;FA;;;{user_sid})")),
                "private directory ACL must grant the current user explicitly: {text}"
            );
            assert!(
                text.contains("(A;OICI;FA;;;SY)"),
                "private directory ACL must grant SYSTEM explicitly: {text}"
            );
            assert!(
                !text.contains(";;;OW") && !text.contains(";;;CO"),
                "private directory ACL must not use owner placeholders: {text}"
            );
        }
    }
}
