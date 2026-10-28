use serde_json::Value;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn fail(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("diskdrift-v3-reader: {message}");
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let db = match args
        .iter()
        .position(|arg| arg == "--db")
        .and_then(|i| args.get(i + 1))
    {
        Some(path) => PathBuf::from(path),
        None => return fail("--db is required"),
    };
    if args.iter().any(|arg| arg == "--help") {
        println!("diskdrift-v3-reader --db STORE snapshots --json");
        return ExitCode::SUCCESS;
    }
    let Some(snapshots) = args.iter().position(|arg| arg == "snapshots") else {
        return fail("only snapshots commands are supported");
    };
    match args.get(snapshots + 1).map(String::as_str) {
        Some("--json") if args.iter().any(|arg| arg == "--json") => read_store(&db),
        Some("verify") => match args.get(snapshots + 2).and_then(|id| id.parse().ok()) {
            Some(id) => verify_store(&db, id),
            None => fail("snapshots verify requires a numeric id"),
        },
        _ => fail("supported commands are snapshots --json and snapshots verify <id>"),
    }
}

fn read_store(db: &Path) -> ExitCode {
    let Some(marker) = read_marker(db) else {
        return fail("invalid store marker");
    };
    if let Err(error) = supported_marker(&marker) {
        return fail(error);
    }
    let snapshots = match snapshot_metadata(db) {
        Ok(snapshots) => snapshots,
        Err(error) => return fail(error),
    };
    println!("{}", serde_json::to_string(&snapshots).unwrap());
    ExitCode::SUCCESS
}

fn read_marker(db: &Path) -> Option<Value> {
    fs::read_to_string(db.join("store.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

fn supported_marker(marker: &Value) -> Result<(), String> {
    if marker["format"] != "diskdrift-snapshots" {
        return Err("not a DiskDrift snapshot store".into());
    }
    let version = marker["version"].as_u64().unwrap_or(0);
    if version > 3 {
        return Err(format!(
            "store format version {version} is newer than this reader supports (3)"
        ));
    }
    Ok(())
}

fn snapshot_metadata(db: &Path) -> Result<Vec<Value>, String> {
    let mut snapshots = Vec::new();
    let mut files: Vec<_> = match fs::read_dir(db.join("snapshots")) {
        Ok(entries) => entries.filter_map(Result::ok).collect(),
        Err(error) => return Err(error.to_string()),
    };
    files.sort_by_key(|entry| entry.file_name());
    for entry in files {
        if entry.path().extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let value: Value = match fs::read_to_string(entry.path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
        {
            Some(value) => value,
            None => return Err("invalid snapshot metadata".into()),
        };
        if value["status"] == "IN_PROGRESS" {
            continue;
        }
        snapshots.push(value);
    }
    Ok(snapshots)
}

fn verify_store(db: &Path, id: u64) -> ExitCode {
    let Some(marker) = read_marker(db) else {
        return fail("invalid store marker");
    };
    if let Err(error) = supported_marker(&marker) {
        return fail(error);
    }
    let snapshots = match snapshot_metadata(db) {
        Ok(snapshots) => snapshots,
        Err(error) => return fail(error),
    };
    let Some(snapshot) = snapshots
        .iter()
        .find(|value| value["id"].as_u64() == Some(id))
    else {
        return fail(format!("snapshot {id} not found"));
    };
    let mut seen = Vec::new();
    if let Err(error) = verify_snapshot(db, snapshot, &snapshots, &mut seen) {
        return fail(error);
    }
    println!("verified snapshot {id}");
    ExitCode::SUCCESS
}

fn verify_snapshot(
    db: &Path,
    snapshot: &Value,
    all: &[Value],
    seen: &mut Vec<u64>,
) -> Result<(), String> {
    let id = snapshot["id"]
        .as_u64()
        .ok_or("snapshot has no numeric id")?;
    if seen.contains(&id) {
        return Err(format!("cyclic snapshot chain at {id}"));
    }
    seen.push(id);
    if let Some(base) = snapshot["storage"]["base"].as_u64() {
        let parent = all
            .iter()
            .find(|value| value["id"].as_u64() == Some(base))
            .ok_or_else(|| format!("snapshot {id} base {base} not found"))?;
        verify_snapshot(db, parent, all, seen)?;
    }
    let data_file = snapshot["storage"]["dataFile"]
        .as_str()
        .ok_or_else(|| format!("snapshot {id} has no data file"))?;
    verify_data_file(&db.join("snapshots").join(data_file))
        .map_err(|error| format!("snapshot {id}: {error}"))
}

fn verify_data_file(path: &Path) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    if bytes.len() < 36 || &bytes[..4] != b"DDS1" || &bytes[bytes.len() - 4..] != b"DDSE" {
        return Err("invalid DDS1 container".into());
    }
    let trailer = bytes.len() - 28;
    let index_offset = u64::from_le_bytes(bytes[trailer..trailer + 8].try_into().unwrap()) as usize;
    let index_len =
        u32::from_le_bytes(bytes[trailer + 8..trailer + 12].try_into().unwrap()) as usize;
    let index_crc = u32::from_le_bytes(bytes[trailer + 12..trailer + 16].try_into().unwrap());
    let index_end = index_offset
        .checked_add(index_len)
        .ok_or("index overflow")?;
    if index_offset < 8
        || index_end > trailer
        || crc32fast::hash(&bytes[index_offset..index_end]) != index_crc
    {
        return Err("invalid DDS1 index".into());
    }
    let mut index = &bytes[index_offset..index_end];
    let blocks = read_varint(&mut index)? as usize;
    for _ in 0..blocks {
        let offset = read_varint(&mut index)? as usize;
        let compressed = read_varint(&mut index)? as usize;
        let raw_len = read_varint(&mut index)? as usize;
        let _count = read_varint(&mut index)?;
        if index.len() < 4 {
            return Err("truncated DDS1 index".into());
        }
        let crc = u32::from_le_bytes(index[..4].try_into().unwrap());
        index = &index[4..];
        skip_path(&mut index)?;
        let end = offset.checked_add(compressed).ok_or("block overflow")?;
        if offset < 8 || end > index_offset {
            return Err("DDS1 block out of bounds".into());
        }
        if crc32fast::hash(&bytes[offset..end]) != crc {
            return Err("DDS1 block checksum mismatch".into());
        }
        let raw = zstd::bulk::decompress(&bytes[offset..end], raw_len)
            .map_err(|error| error.to_string())?;
        if raw.len() != raw_len {
            return Err("DDS1 raw length mismatch".into());
        }
    }
    if !index.is_empty() {
        return Err("trailing DDS1 index data".into());
    }
    Ok(())
}

fn read_varint(input: &mut &[u8]) -> Result<u64, String> {
    let mut value = 0;
    for shift in (0..64).step_by(7) {
        let byte = *input.first().ok_or("truncated varint")?;
        *input = &input[1..];
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("varint overflow".into())
}

fn skip_path(input: &mut &[u8]) -> Result<(), String> {
    let components = read_varint(input)?;
    for _ in 0..components {
        let length = read_varint(input)? as usize;
        if input.len() < length {
            return Err("truncated DDS1 path".into());
        }
        *input = &input[length..];
    }
    Ok(())
}
