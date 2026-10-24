//! JSON representation of native paths (spec section 8): a string when the bytes
//! are valid UTF-8; otherwise a lossy string plus the exact bytes in base64.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::ops::Deref;

/// Native path bytes. Serialized flattened into its parent object as
/// `"path"` (+ `"pathBytes"` when not UTF-8).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct NativePath(pub Vec<u8>);

impl Deref for NativePath {
    type Target = Vec<u8>;
    fn deref(&self) -> &Vec<u8> {
        &self.0
    }
}

impl From<Vec<u8>> for NativePath {
    fn from(v: Vec<u8>) -> Self {
        NativePath(v)
    }
}

impl NativePath {
    pub fn lossy(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }
}

/// Writes `key` (and `key`+"Bytes" if needed) into a map being serialized.
pub fn serialize_entry<M: SerializeMap>(map: &mut M, key: &str, bytes: &[u8]) -> Result<(), M::Error> {
    match std::str::from_utf8(bytes) {
        Ok(s) => map.serialize_entry(key, s),
        Err(_) => {
            map.serialize_entry(key, &String::from_utf8_lossy(bytes))?;
            map.serialize_entry(&format!("{key}Bytes"), &STANDARD.encode(bytes))
        }
    }
}

impl Serialize for NativePath {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(None)?;
        serialize_entry(&mut m, "path", &self.0)?;
        m.end()
    }
}

impl<'de> Deserialize<'de> for NativePath {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Helper {
            path: String,
            #[serde(rename = "pathBytes")]
            path_bytes: Option<String>,
        }
        let h = Helper::deserialize(d)?;
        match h.path_bytes {
            Some(b) => STANDARD.decode(b).map(NativePath).map_err(serde::de::Error::custom),
            None => Ok(NativePath(h.path.into_bytes())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Item {
        #[serde(flatten)]
        path: NativePath,
        size: u64,
    }

    #[test]
    fn utf8_and_non_utf8() {
        let ok = Item { path: NativePath(b"/Users/a b/\xc3\xa9".to_vec()), size: 1 };
        let j = serde_json::to_string(&ok).unwrap();
        assert_eq!(j, r#"{"path":"/Users/a b/é","size":1}"#);
        assert_eq!(serde_json::from_str::<Item>(&j).unwrap(), ok);
        let bad = Item { path: NativePath(b"/tmp/\xff\xfe".to_vec()), size: 2 };
        let j = serde_json::to_string(&bad).unwrap();
        assert!(j.contains(r#""pathBytes":"L3RtcC///g==""#), "{j}");
        assert_eq!(serde_json::from_str::<Item>(&j).unwrap(), bad);
    }
}
