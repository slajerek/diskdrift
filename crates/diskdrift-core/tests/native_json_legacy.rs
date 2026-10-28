use diskdrift_core::pathser::NativePath;
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct LegacyEntry {
    #[serde(flatten)]
    path: NativePath,
    size: u64,
    birth_time: i64,
    inode: u64,
}

#[test]
fn exact_legacy_json_round_trips_native_bytes_and_zero_sentinels() {
    let expected = include_str!("fixtures/native-json/legacy-entry.json");
    let entry: LegacyEntry = serde_json::from_str(expected).unwrap();
    assert_eq!(entry.path.0, b"/tmp/bad\xff".to_vec());
    assert_eq!(entry.size, 0);
    assert_eq!(entry.birth_time, 0);
    assert_eq!(entry.inode, 0);
    assert_eq!(serde_json::to_string(&entry).unwrap(), expected.trim());
}
