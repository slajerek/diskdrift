use super::*;
use diskdrift_core::testutil::TestDir;
use std::fs::{self, OpenOptions};
use std::io::Write;

#[test]
fn positional_reads_are_exact_and_readers_share_files() {
    let dir = TestDir::new("fsops-reads");
    let path = dir.path().join("payload");
    fs::write(&path, b"0123456789abcdef").unwrap();

    let first = open_read_shared(&path).unwrap();
    let second = open_read_shared(&path).unwrap();
    let mut a = [0; 4];
    let mut b = [0; 5];
    read_exact_at(&first, &mut a, 3).unwrap();
    read_exact_at(&second, &mut b, 8).unwrap();
    assert_eq!(&a, b"3456");
    assert_eq!(&b, b"89abc");
}

#[test]
fn flush_and_parent_sync_are_available() {
    let dir = TestDir::new("fsops-flush");
    let path = dir.path().join("payload");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    file.write_all(b"durable enough for the fsops boundary")
        .unwrap();
    flush_file(&file).unwrap();
    sync_parent(&path).unwrap();
}

#[test]
fn writer_lock_is_exclusive_and_probe_does_not_create() {
    let dir = TestDir::new("fsops-locks");
    let path = dir.path().join("lock");

    assert!(!probe_lock(&path).unwrap());
    assert!(!path.exists(), "a read-only probe must not create lock");

    let first = try_lock_exclusive(&path).unwrap().unwrap();
    assert!(path.exists());
    assert!(probe_lock(&path).unwrap());
    assert!(try_lock_exclusive(&path).unwrap().is_none());

    drop(first);
    assert!(!probe_lock(&path).unwrap());
    let second = try_lock_exclusive(&path).unwrap().unwrap();
    drop(second);
}

#[test]
fn stale_lock_file_can_be_taken_over() {
    let dir = TestDir::new("fsops-stale-lock");
    let path = dir.path().join("lock");
    fs::write(&path, b"dead-pid").unwrap();

    let mut lock = try_lock_exclusive(&path).unwrap().unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"dead-pid");
    lock.write_owner("live-pid").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"live-pid");
}

#[test]
fn same_file_checks_identity_not_only_paths() {
    let dir = TestDir::new("fsops-same-file");
    let first = dir.path().join("first");
    let alias = dir.path().join("alias");
    let other = dir.path().join("other");
    fs::write(&first, b"same").unwrap();
    fs::hard_link(&first, &alias).unwrap();
    fs::write(&other, b"same").unwrap();

    assert!(same_file(&first, &alias));
    assert!(!same_file(&first, &other));
}

#[test]
fn private_directories_and_allocated_size_are_host_backed() {
    let dir = TestDir::new("fsops-directory");
    let private = dir.path().join("private").join("nested");
    create_private_dir(&private).unwrap();
    assert!(private.is_dir());
    let file = private.join("large");
    fs::write(&file, vec![b'x'; 8192]).unwrap();
    assert!(allocated_size(&file) > 0);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(dir.path().join("private"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&private).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}

#[test]
fn replacement_is_atomic_over_an_existing_destination() {
    let dir = TestDir::new("fsops-replace");
    let destination = dir.path().join("generation");
    let temporary = dir.path().join("generation.tmp");
    fs::write(&destination, b"old").unwrap();
    fs::write(&temporary, b"new").unwrap();

    replace_atomic(&temporary, &destination).unwrap();
    assert_eq!(fs::read(&destination).unwrap(), b"new");
    assert!(!temporary.exists());
}

#[test]
fn unsupported_replacement_fails_without_a_non_atomic_fallback() {
    let dir = TestDir::new("fsops-unsupported-replace");
    let destination = dir.path().join("generation");
    let temporary = dir.path().join("generation.tmp");
    fs::write(&destination, b"old").unwrap();
    fs::write(&temporary, b"new").unwrap();

    let reset = force_unsupported_replacement_for(&destination);
    let result = replace_atomic(&temporary, &destination);
    drop(reset);

    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
    assert_eq!(fs::read(&destination).unwrap(), b"old");
    assert_eq!(fs::read(&temporary).unwrap(), b"new");
}

#[test]
fn unsupported_replacement_override_is_thread_local() {
    let dir = TestDir::new("fsops-thread-local-replace");
    let destination = dir.path().join("generation");
    let temporary = dir.path().join("generation.tmp");
    fs::write(&destination, b"old").unwrap();
    fs::write(&temporary, b"new").unwrap();
    let destination_for_thread = destination.clone();
    let temporary_for_thread = temporary.clone();

    let reset = force_unsupported_replacement_for(&destination);
    let result =
        std::thread::spawn(move || replace_atomic(&temporary_for_thread, &destination_for_thread))
            .join()
            .unwrap();
    drop(reset);

    result.unwrap();
    assert_eq!(fs::read(&destination).unwrap(), b"new");
}
