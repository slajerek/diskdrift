//! Host-neutral lossless native path values used by portability interfaces.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::cmp::Ordering;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathEncoding {
    UnixBytes,
    WindowsUtf16le,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NativeName {
    Unix(Vec<u8>),
    Windows(Vec<u16>),
}

impl Default for NativeName {
    fn default() -> Self { Self::Unix(Vec::new()) }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NativePath {
    Unix(Vec<u8>),
    Windows(Vec<u16>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    Posix,
    Drive,
    Unc,
    VolumeGuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NativeRoot {
    pub path: NativePath,
    pub root_kind: RootKind,
    pub verbatim: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NativeError {
    #[error("native path encoding does not match root kind")]
    EncodingMismatch,
    #[error("native path contains a NUL")]
    Nul,
    #[error("invalid {0} root")]
    InvalidRoot(&'static str),
    #[error("native path payload is not valid base64: {0}")]
    InvalidBase64(String),
    #[error("windows path payload has an odd byte length")]
    OddUtf16,
    #[error("native path display disagrees with its exact payload")]
    DisplayMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ForeignEncoding {
    #[error("unix byte path cannot be represented on a Windows host")]
    UnixOnWindows,
    #[error("Windows UTF-16 path cannot be represented on a Unix host")]
    WindowsOnUnix,
}

impl NativeName {
    pub fn unix(value: Vec<u8>) -> Self {
        Self::Unix(value)
    }
    pub fn windows(value: Vec<u16>) -> Self {
        Self::Windows(value)
    }
    pub fn encoding(&self) -> PathEncoding {
        match self {
            Self::Unix(_) => PathEncoding::UnixBytes,
            Self::Windows(_) => PathEncoding::WindowsUtf16le,
        }
    }
    pub fn display(&self) -> String {
        display_value(self)
    }
    pub fn len(&self) -> usize { match self { Self::Unix(v) => v.len(), Self::Windows(v) => v.len() } }
    pub fn is_empty(&self) -> bool { self.len() == 0 }
    pub fn as_unix_bytes(&self) -> Option<&[u8]> { match self { Self::Unix(v) => Some(v), Self::Windows(_) => None } }
    pub fn extend_from_slice(&mut self, suffix: &[u8]) { if let Self::Unix(v) = self { v.extend_from_slice(suffix); } }
}

impl NativePath {
    pub fn unix(value: Vec<u8>) -> Self {
        Self::Unix(value)
    }
    pub fn windows(value: Vec<u16>) -> Self {
        Self::Windows(value)
    }
    pub fn encoding(&self) -> PathEncoding {
        match self {
            Self::Unix(_) => PathEncoding::UnixBytes,
            Self::Windows(_) => PathEncoding::WindowsUtf16le,
        }
    }
    pub fn display(&self) -> String {
        display_value(self)
    }
}

impl NativeRoot {
    pub fn posix(path: Vec<u8>) -> Self {
        Self {
            path: NativePath::unix(path),
            root_kind: RootKind::Posix,
            verbatim: false,
        }
    }

    pub fn new(path: NativePath, root_kind: RootKind, verbatim: bool) -> Result<Self, NativeError> {
        let root = Self {
            path,
            root_kind,
            verbatim,
        };
        root.validate()?;
        Ok(root)
    }

    pub fn validate(&self) -> Result<(), NativeError> {
        let display = self.path.display();
        if display != self.path.display() {
            return Err(NativeError::DisplayMismatch);
        }
        match (&self.path, self.root_kind) {
            (NativePath::Unix(bytes), RootKind::Posix) => {
                if self.verbatim || bytes.contains(&0) || !bytes.starts_with(b"/") {
                    return Err(NativeError::InvalidRoot("posix"));
                }
            }
            (
                NativePath::Windows(units),
                kind @ (RootKind::Drive | RootKind::Unc | RootKind::VolumeGuid),
            ) => {
                if units.contains(&0) {
                    return Err(NativeError::Nul);
                }
                let s = decode_windows(units);
                if is_device_path(&s) {
                    return Err(NativeError::InvalidRoot("device"));
                }
                match kind {
                    RootKind::Drive if self.verbatim => {
                        if !is_verbatim_drive(&s) {
                            return Err(NativeError::InvalidRoot("drive"));
                        }
                    }
                    RootKind::Drive => {
                        if !is_absolute_drive(&s) {
                            return Err(NativeError::InvalidRoot("drive"));
                        }
                    }
                    RootKind::Unc => {
                        let prefix = if self.verbatim { r"\\?\UNC\" } else { r"\\" };
                        if !valid_unc(&s, prefix) {
                            return Err(NativeError::InvalidRoot("unc"));
                        }
                    }
                    RootKind::VolumeGuid => {
                        if !self.verbatim || !valid_volume_guid(&s) {
                            return Err(NativeError::InvalidRoot("volume_guid"));
                        }
                    }
                    RootKind::Posix => unreachable!(),
                }
            }
            _ => return Err(NativeError::EncodingMismatch),
        }
        Ok(())
    }
}

fn is_absolute_drive(s: &str) -> bool {
    s.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && s.as_bytes().get(1) == Some(&b':')
        && s.as_bytes().get(2) == Some(&b'\\')
}
fn is_verbatim_drive(s: &str) -> bool {
    s.len() >= 7 && s.as_bytes().get(0..4) == Some(br"\\?\") && is_absolute_drive(&s[4..])
}
fn is_device_path(s: &str) -> bool {
    s.starts_with(r"\\.\") || s.starts_with(r"\\?\GLOBALROOT\") || s.starts_with(r"\??\")
}
fn valid_unc(s: &str, prefix: &str) -> bool {
    let Some(rest) = s.strip_prefix(prefix) else {
        return false;
    };
    let mut parts = rest.split('\\');
    parts.next().is_some_and(|x| !x.is_empty())
        && parts.next().is_some_and(|x| !x.is_empty())
}
fn valid_volume_guid(s: &str) -> bool {
    let Some(rest) = s.strip_prefix(r"\\?\Volume{") else {
        return false;
    };
    let Some(end) = rest.find("}\\") else {
        return false;
    };
    let guid = &rest[..end];
    guid.len() == 36
        && guid.bytes().enumerate().all(|(i, c)| {
            matches!(i, 8 | 13 | 18 | 23) && c == b'-'
                || !matches!(i, 8 | 13 | 18 | 23) && c.is_ascii_hexdigit()
        })
}

#[cfg(test)]
mod root_tests {
    use super::*;

    fn windows(value: &str) -> NativePath {
        NativePath::windows(value.encode_utf16().collect())
    }

    #[test]
    fn unc_root_and_subdirectories_do_not_need_trailing_separators() {
        for path in [r#"\\server\share"#, r#"\\server\share\folder"#] {
            NativeRoot::new(windows(path), RootKind::Unc, false).unwrap();
        }
    }

    #[test]
    fn incomplete_unc_roots_are_rejected() {
        for path in [r#"\\server"#, r#"\\server\"#] {
            assert!(NativeRoot::new(windows(path), RootKind::Unc, false).is_err(), "{path}");
        }
    }
}

fn display_value(value: &impl NativeValue) -> String {
    value.units_display()
}
trait NativeValue {
    fn units_display(&self) -> String;
}
impl NativeValue for NativeName {
    fn units_display(&self) -> String {
        match self {
            Self::Unix(v) => String::from_utf8_lossy(v).into_owned(),
            Self::Windows(v) => decode_windows(v),
        }
    }
}
impl NativeValue for NativePath {
    fn units_display(&self) -> String {
        match self {
            Self::Unix(v) => String::from_utf8_lossy(v).into_owned(),
            Self::Windows(v) => decode_windows(v),
        }
    }
}
fn decode_windows(v: &[u16]) -> String {
    char::decode_utf16(v.iter().copied())
        .map(|c| c.unwrap_or('\u{fffd}'))
        .collect()
}

impl Ord for NativeName {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Unix(a), Self::Unix(b)) => a.cmp(b),
            (Self::Windows(a), Self::Windows(b)) => a.cmp(b),
            (Self::Unix(_), Self::Windows(_)) => Ordering::Less,
            (Self::Windows(_), Self::Unix(_)) => Ordering::Greater,
        }
    }
}
impl PartialOrd for NativeName {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for NativePath {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Unix(a), Self::Unix(b)) => a.cmp(b),
            (Self::Windows(a), Self::Windows(b)) => a.cmp(b),
            (Self::Unix(_), Self::Windows(_)) => Ordering::Less,
            (Self::Windows(_), Self::Unix(_)) => Ordering::Greater,
        }
    }
}
impl PartialOrd for NativePath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn serialize_value<S: Serializer>(
    path: &NativePath,
    s: S,
    root: Option<(&RootKind, bool)>,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut map = s.serialize_map(None)?;
    map.serialize_entry("path", &path.display())?;
    map.serialize_entry("pathEncoding", &path.encoding())?;
    match path {
        NativePath::Unix(v) if std::str::from_utf8(v).is_err() => {
            map.serialize_entry("pathBytes", &STANDARD.encode(v))?
        }
        NativePath::Windows(v) => {
            let bytes: Vec<u8> = v.iter().flat_map(|u| u.to_le_bytes()).collect();
            map.serialize_entry("pathWide", &STANDARD.encode(bytes))?;
        }
        _ => {}
    }
    if let Some((kind, verbatim)) = root {
        map.serialize_entry("rootKind", kind)?;
        if *kind != RootKind::Posix {
            map.serialize_entry("verbatim", &verbatim)?;
        }
    }
    map.end()
}

impl Serialize for NativeName {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_value(
            &match self {
                Self::Unix(v) => NativePath::Unix(v.clone()),
                Self::Windows(v) => NativePath::Windows(v.clone()),
            },
            s,
            None,
        )
    }
}
impl Serialize for NativePath {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_value(self, s, None)
    }
}
impl Serialize for NativeRoot {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_value(&self.path, s, Some((&self.root_kind, self.verbatim)))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawValue {
    path: String,
    #[serde(rename = "pathEncoding")]
    path_encoding: PathEncoding,
    #[serde(rename = "pathBytes")]
    path_bytes: Option<String>,
    #[serde(rename = "pathWide")]
    path_wide: Option<String>,
    #[serde(rename = "rootKind")]
    root_kind: Option<RootKind>,
    verbatim: Option<bool>,
}

fn decode_raw(raw: RawValue) -> Result<(NativePath, Option<RootKind>, Option<bool>), String> {
    let path = match raw.path_encoding {
        PathEncoding::UnixBytes => {
            let bytes = match raw.path_bytes {
                Some(v) => {
                    let bytes = STANDARD
                        .decode(v)
                        .map_err(|e| NativeError::InvalidBase64(e.to_string()).to_string())?;
                    if std::str::from_utf8(&bytes).is_ok()
                        || String::from_utf8_lossy(&bytes) != raw.path
                    {
                        return Err("pathBytes is inconsistent with path".into());
                    }
                    bytes
                }
                None => raw.path.as_bytes().to_vec(),
            };
            if raw.path_wide.is_some() {
                return Err("pathWide is forbidden for unix_bytes".into());
            }
            NativePath::Unix(bytes)
        }
        PathEncoding::WindowsUtf16le => {
            let bytes = STANDARD
                .decode(
                    raw.path_wide
                        .ok_or("pathWide is required for windows_utf16le")?,
                )
                .map_err(|e| NativeError::InvalidBase64(e.to_string()).to_string())?;
            let (pairs, remainder) = bytes.as_chunks::<2>();
            if !remainder.is_empty() || raw.path_bytes.is_some() {
                return Err(NativeError::OddUtf16.to_string());
            }
            NativePath::Windows(pairs.iter().map(|b| u16::from_le_bytes(*b)).collect())
        }
    };
    if path.display() != raw.path {
        return Err(NativeError::DisplayMismatch.to_string());
    }
    Ok((path, raw.root_kind, raw.verbatim))
}
impl<'de> Deserialize<'de> for NativeName {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let (p, k, v) = decode_raw(RawValue::deserialize(d)?).map_err(serde::de::Error::custom)?;
        if k.is_some() || v.is_some() {
            return Err(serde::de::Error::custom("root fields are forbidden"));
        }
        Ok(match p {
            NativePath::Unix(v) => Self::Unix(v),
            NativePath::Windows(v) => Self::Windows(v),
        })
    }
}
impl<'de> Deserialize<'de> for NativePath {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let (p, k, v) = decode_raw(RawValue::deserialize(d)?).map_err(serde::de::Error::custom)?;
        if k.is_some() || v.is_some() {
            return Err(serde::de::Error::custom("root fields are forbidden"));
        }
        Ok(p)
    }
}
impl<'de> Deserialize<'de> for NativeRoot {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let (path, kind, verbatim) =
            decode_raw(RawValue::deserialize(d)?).map_err(serde::de::Error::custom)?;
        let kind = kind.ok_or_else(|| serde::de::Error::custom("rootKind is required"))?;
        let verbatim = match kind {
            RootKind::Posix => {
                if verbatim.is_some() {
                    return Err(serde::de::Error::custom("verbatim is forbidden for posix roots"));
                }
                false
            }
            RootKind::Drive | RootKind::Unc | RootKind::VolumeGuid => verbatim
                .ok_or_else(|| serde::de::Error::custom("verbatim is required for windows roots"))?,
        };
        NativeRoot::new(path, kind, verbatim).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for NativeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}
impl fmt::Display for NativePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}
