use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, PermissionsExt};
use std::path::Path;

pub struct Lock {
    file: File,
}

impl Lock {
    pub fn write_owner(&mut self, owner: &str) -> io::Result<()> {
        self.file.set_len(0)?;
        self.file.write_all(owner.as_bytes())
    }
}

pub fn open_read_shared(path: &Path) -> io::Result<File> {
    File::open(path)
}

pub fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    file.read_exact_at(buf, offset)
}

fn lock_file(file: File) -> io::Result<Option<Lock>> {
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(Some(Lock { file }));
    }
    let error = io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(code) if code == libc::EACCES || code == libc::EAGAIN) {
        Ok(None)
    } else {
        Err(error)
    }
}

pub fn try_lock_exclusive(path: &Path) -> io::Result<Option<Lock>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    lock_file(file)
}

pub fn probe_lock(path: &Path) -> io::Result<bool> {
    let file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(lock_file(file)?.is_none())
}

pub fn flush_file(file: &File) -> io::Result<()> {
    file.sync_all()
}

pub fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

pub fn sync_parent(path: &Path) -> io::Result<()> {
    sync_dir(path.parent().unwrap_or(Path::new("/")))
}

pub fn replace_atomic(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary, destination)
}

#[cfg(test)]
pub fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(first), Ok(second)) => first.dev() == second.dev() && first.ino() == second.ino(),
        _ => false,
    }
}

pub fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true).mode(0o700).create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

pub fn allocated_size(path: &Path) -> u64 {
    fs::metadata(path)
        .map(|metadata| metadata.blocks() * 512)
        .unwrap_or(0)
}
