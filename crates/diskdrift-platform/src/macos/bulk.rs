//! Directory listing with `getattrlistbulk(2)`: names and metadata for many entries
//! per system call, without a separate `lstat` per entry.

use diskdrift_core::record::Kind;
use diskdrift_core::native::NativeName;
use diskdrift_core::record::{EntryFields, FileId};
use diskdrift_core::walk::{DirLister, RawEntry, StdLister};
use std::cell::RefCell;
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

// Not exported by the libc crate.
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const DIR_MNTSTATUS_TRIGGER: u32 = 0x2;

// fsobj_type_t values (sys/vnode.h).
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

const BUF_WORDS: usize = 32 * 1024; // 256 KiB, 8-byte aligned

thread_local! {
    static BUF: RefCell<Vec<u64>> = RefCell::new(vec![0u64; BUF_WORDS]);
}

/// Lister using `getattrlistbulk`.
pub struct BulkLister;

impl DirLister for BulkLister {
    fn name(&self) -> &'static str {
        "getattrlistbulk"
    }

    fn stat(&self, path: &Path) -> io::Result<RawEntry> {
        StdLister.stat(path)
    }

    fn list(&self, dir: &Path, out: &mut Vec<RawEntry>) -> io::Result<()> {
        let cpath = CString::new(dir.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let result = BUF.with(|buf| list_fd(fd, &mut buf.borrow_mut(), out));
        unsafe { libc::close(fd) };
        result
    }
}

fn list_fd(fd: i32, buf: &mut [u64], out: &mut Vec<RawEntry>) -> io::Result<()> {
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS
            | libc::ATTR_CMN_NAME
            | libc::ATTR_CMN_DEVID
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_CRTIME
            | libc::ATTR_CMN_MODTIME
            | libc::ATTR_CMN_FILEID
            | ATTR_CMN_ERROR,
        volattr: 0,
        dirattr: libc::ATTR_DIR_MOUNTSTATUS,
        fileattr: libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_TOTALSIZE | libc::ATTR_FILE_ALLOCSIZE,
        forkattr: 0,
    };
    let byte_len = std::mem::size_of_val(buf);
    loop {
        let n = unsafe {
            libc::getattrlistbulk(
                fd,
                (&mut attrs as *mut libc::attrlist).cast(),
                buf.as_mut_ptr().cast(),
                byte_len,
                0,
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            return Ok(());
        }
        let bytes: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), byte_len) };
        parse_entries(bytes, n as usize, out)?;
    }
}

fn rd_u32(b: &[u8], at: usize) -> io::Result<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_ne_bytes(s.try_into().unwrap()))
        .ok_or_else(|| bad("truncated u32"))
}

fn rd_i64(b: &[u8], at: usize) -> io::Result<i64> {
    b.get(at..at + 8)
        .map(|s| i64::from_ne_bytes(s.try_into().unwrap()))
        .ok_or_else(|| bad("truncated i64"))
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("getattrlistbulk: {what}"))
}

/// Parses `count` packed entries. Field order follows getattrlist(2): the returned
/// attribute set, then ATTR_CMN_ERROR, then the remaining common attributes in bit
/// order, then directory and file attributes.
fn parse_entries(b: &[u8], count: usize, out: &mut Vec<RawEntry>) -> io::Result<()> {
    let mut pos = 0usize;
    for _ in 0..count {
        let len = rd_u32(b, pos)? as usize;
        if len == 0 || pos + len > b.len() {
            return Err(bad("bad entry length"));
        }
        let mut f = pos + 4;
        let common = rd_u32(b, f)?;
        let dirattr = rd_u32(b, f + 8)?;
        let fileattr = rd_u32(b, f + 12)?;
        f += 20; // attribute_set_t

        let mut e = RawEntry { nlink: 1, ..RawEntry::default() };
        let mut available = EntryFields::empty();
        if common & ATTR_CMN_ERROR != 0 {
            let errno = rd_u32(b, f)?;
            if errno != 0 {
                e.error = Some(errno as i32);
            }
            f += 4;
        }
        if common & libc::ATTR_CMN_NAME != 0 {
            let off = rd_u32(b, f)? as i32;
            let nlen = rd_u32(b, f + 4)? as usize;
            let start = (f as isize + off as isize) as usize;
            let name = b.get(start..start + nlen).ok_or_else(|| bad("name out of range"))?;
            let name = name.strip_suffix(&[0]).unwrap_or(name);
            e.name = NativeName::unix(name.to_vec());
            f += 8;
        }
        if common & libc::ATTR_CMN_DEVID != 0 {
            e.dev = rd_u32(b, f)? as i32 as i64 as u64;
            f += 4;
        }
        if common & libc::ATTR_CMN_OBJTYPE != 0 {
            e.kind = match rd_u32(b, f)? {
                VREG => Kind::File,
                VDIR => Kind::Dir,
                VLNK => Kind::Symlink,
                _ => Kind::Other,
            };
            available = available.union(EntryFields::KIND);
            f += 4;
        }
        if common & libc::ATTR_CMN_CRTIME != 0 {
            e.btime = rd_i64(b, f)?;
            e.btime_nanos = rd_i64(b, f + 8)? as u32;
            available = available.union(EntryFields::BIRTH_TIME).union(EntryFields::BIRTH_TIME_NANOS);
            f += 16;
        }
        if common & libc::ATTR_CMN_MODTIME != 0 {
            e.mtime = rd_i64(b, f)?;
            e.mtime_nanos = rd_i64(b, f + 8)? as u32;
            available = available.union(EntryFields::MTIME).union(EntryFields::MTIME_NANOS);
            f += 16;
        }
        if common & libc::ATTR_CMN_FILEID != 0 {
            e.ino = rd_i64(b, f)? as u64;
            e.file_id = FileId { low: e.ino, high: 0 };
            available = available.union(EntryFields::FILE_ID);
            f += 8;
        }
        if dirattr & libc::ATTR_DIR_MOUNTSTATUS != 0 {
            let st = rd_u32(b, f)?;
            e.is_mount_point = st & libc::DIR_MNTSTATUS_MNTPOINT != 0;
            e.is_trigger = st & DIR_MNTSTATUS_TRIGGER != 0;
            f += 4;
        }
        if fileattr & libc::ATTR_FILE_LINKCOUNT != 0 {
            e.nlink = rd_u32(b, f)?;
            available = available.union(EntryFields::LINK_COUNT);
            f += 4;
        }
        if fileattr & libc::ATTR_FILE_TOTALSIZE != 0 {
            e.size = rd_i64(b, f)?.max(0) as u64;
            available = available.union(EntryFields::LOGICAL_SIZE);
            f += 8;
        }
        if fileattr & libc::ATTR_FILE_ALLOCSIZE != 0 {
            e.alloc = rd_i64(b, f)?.max(0) as u64;
            available = available.union(EntryFields::ALLOCATED_SIZE);
        }
        e.available = available;
        if e.error.is_some() {
            e.clear_metadata();
        }
        if !e.name.is_empty() {
            out.push(e);
        }
        pos += len;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskdrift_core::testutil::TestDir;

    #[test]
    fn matches_std_lister() {
        let t = TestDir::new("bulk-vs-std");
        t.file("small", 10);
        t.file("big", 100_000);
        t.file("sub/inner", 1);
        std::os::unix::fs::symlink("small", t.path().join("link")).unwrap();
        std::fs::hard_link(t.path().join("big"), t.path().join("big-hard")).unwrap();
        std::fs::write(t.path().join("na\u{ef}ve \u{1F600}"), b"u").unwrap();

        let mut bulk = Vec::new();
        BulkLister.list(t.path(), &mut bulk).unwrap();
        let mut std = Vec::new();
        StdLister.list(t.path(), &mut std).unwrap();
        bulk.sort_by(|a, b| a.name.cmp(&b.name));
        std.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(bulk.len(), std.len());
        for (a, b) in bulk.iter().zip(std.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.kind, b.kind, "{:?}", a.name.display());
            assert_eq!(a.ino, b.ino);
            assert_eq!(a.dev, b.dev);
            assert_eq!(a.mtime, b.mtime);
            assert_eq!(a.btime, b.btime);
            if a.kind != Kind::Dir {
                assert_eq!(a.size, b.size, "{:?}", a.name.display());
                assert_eq!(a.alloc, b.alloc, "{:?}", a.name.display());
                assert_eq!(a.nlink, b.nlink);
            }
            assert!(a.error.is_none());
        }
    }

    #[test]
    fn large_directory_spans_multiple_calls() {
        let t = TestDir::new("bulk-large");
        for i in 0..5000 {
            std::fs::write(t.path().join(format!("file-with-a-longish-name-{i:05}")), b"").unwrap();
        }
        let mut out = Vec::new();
        BulkLister.list(t.path(), &mut out).unwrap();
        assert_eq!(out.len(), 5000);
    }

    #[test]
    fn detects_mount_points() {
        // /System/Volumes/Data is a mount point on every APFS system since 10.15.
        let mut out = Vec::new();
        if BulkLister.list(Path::new("/System/Volumes"), &mut out).is_ok()
            && let Some(data) = out.iter().find(|e| e.name == NativeName::unix(b"Data".to_vec())) {
                assert!(data.is_mount_point);
            }
    }
}
