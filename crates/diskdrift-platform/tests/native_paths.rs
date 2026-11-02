#[cfg(unix)]
use diskdrift_core::native::NativeRoot;
#[cfg(windows)]
use diskdrift_core::native::RootKind;
use diskdrift_core::native::{ForeignEncoding, NativePath};
use diskdrift_platform::{native_path, native_root, path_from_native};

#[cfg(unix)]
#[test]
fn unix_native_names_and_roots_preserve_invalid_bytes() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let bytes = b"/tmp/native-\xff";
    let os = std::ffi::OsStr::from_bytes(bytes);
    assert_eq!(native_path(os), NativePath::unix(bytes.to_vec()));

    let root = native_root(os).unwrap();
    assert_eq!(root, NativeRoot::posix(bytes.to_vec()));
    let round_trip = path_from_native(&root.path).unwrap();
    assert_eq!(round_trip.into_os_string().into_vec(), bytes);
}

#[cfg(unix)]
#[test]
fn windows_native_paths_are_rejected_at_the_unix_path_boundary() {
    let path = NativePath::windows(vec![b'C' as u16, b':' as u16, b'\\' as u16, 0xd800]);
    assert_eq!(path_from_native(&path), Err(ForeignEncoding::WindowsOnUnix));
}

#[cfg(windows)]
#[test]
fn windows_native_names_and_roots_preserve_lone_surrogates() {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let units = vec![b'C' as u16, b':' as u16, b'\\' as u16, 0xd800];
    let os = std::ffi::OsString::from_wide(&units);
    assert_eq!(native_path(&os), NativePath::windows(units.clone()));

    let root = native_root(&os).unwrap();
    assert_eq!(root.root_kind, RootKind::Drive);
    assert_eq!(root.path, NativePath::windows(units.clone()));
    let round_trip = path_from_native(&root.path).unwrap();
    assert_eq!(
        round_trip
            .into_os_string()
            .encode_wide()
            .collect::<Vec<_>>(),
        units
    );
}

#[cfg(windows)]
#[test]
fn unix_native_paths_are_rejected_at_the_windows_path_boundary() {
    let path = NativePath::unix(b"/tmp/native-\xff".to_vec());
    assert_eq!(path_from_native(&path), Err(ForeignEncoding::UnixOnWindows));
}
