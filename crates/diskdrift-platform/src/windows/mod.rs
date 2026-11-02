//! Native Windows adapters. All filesystem identity and names stay in UTF-16.

pub mod lister;
pub mod space;
pub mod volumes;

use diskdrift_core::native::{NativeError, NativePath, NativeRoot, RootKind};
use std::ffi::OsStr;

pub fn native_path(value: &OsStr) -> NativePath {
    use std::os::windows::ffi::OsStrExt;
    NativePath::windows(value.encode_wide().collect())
}

pub fn native_root(value: &OsStr) -> Result<NativeRoot, NativeError> {
    let path = native_path(value);
    let units = match &path {
        NativePath::Windows(units) => units,
        NativePath::Unix(_) => unreachable!(),
    };
    let (kind, verbatim) = if starts_with_ascii_case_insensitive(
        units,
        &[
            b'\\' as u16,
            b'\\' as u16,
            b'?' as u16,
            b'\\' as u16,
            b'V' as u16,
            b'o' as u16,
            b'l' as u16,
            b'u' as u16,
            b'm' as u16,
            b'e' as u16,
            b'{' as u16,
        ],
    ) {
        (RootKind::VolumeGuid, true)
    } else if starts_with_ascii_case_insensitive(
        units,
        &[
            b'\\' as u16,
            b'\\' as u16,
            b'?' as u16,
            b'\\' as u16,
            b'U' as u16,
            b'N' as u16,
            b'C' as u16,
            b'\\' as u16,
        ],
    ) {
        (RootKind::Unc, true)
    } else if starts_with_ascii_case_insensitive(
        units,
        &[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16],
    ) {
        (RootKind::Drive, true)
    } else if starts_with_ascii_case_insensitive(units, &[b'\\' as u16, b'\\' as u16]) {
        (RootKind::Unc, false)
    } else {
        (RootKind::Drive, false)
    };
    NativeRoot::new(path, kind, verbatim)
}

fn starts_with_ascii_case_insensitive(value: &[u16], prefix: &[u16]) -> bool {
    value.get(..prefix.len()).is_some_and(|head| {
        head.iter().zip(prefix).all(|(a, b)| {
            let lower = |unit: u16| {
                if (b'A' as u16..=b'Z' as u16).contains(&unit) {
                    unit + b'a' as u16 - b'A' as u16
                } else {
                    unit
                }
            };
            lower(*a) == lower(*b)
        })
    })
}
