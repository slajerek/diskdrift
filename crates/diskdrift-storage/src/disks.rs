//! The disk registry (ADR 0002, section 2.3): what DiskDrift knows about each
//! disk it has scanned, with the user's labels, descriptions and sets.
//!
//! ```text
//! <store>/disks.json   the registry, written atomically
//! <store>/disks.lock   flock'ed for the length of one read-modify-write
//! ```
//! The registry has its own short lock, not the scan writer lock, so a label can
//! be edited while a scan runs. Reading never creates anything: a missing
//! `disks.json` is an empty registry.

use crate::meta::SnapshotMeta;
use crate::store::{Result, StoreError, sync_dir, write_atomic};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::{Path, PathBuf};

pub const DISKS_FORMAT: &str = "diskdrift-disks";
pub const DISKS_VERSION: u32 = 1;
const DISKS_FILE: &str = "disks.json";
const DISKS_LOCK: &str = "disks.lock";

/// How sure the identity is (ADR 0002, section 2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strength {
    /// A volume UUID (APFS, HFS+, ...).
    Uuid,
    /// The UUID derived from a FAT/exFAT volume serial.
    Serial,
    /// Filesystem type, volume name and capacity: never merged without asking.
    Weak,
}

impl Strength {
    fn as_str(self) -> &'static str {
        match self {
            Strength::Uuid => "uuid",
            Strength::Serial => "serial",
            Strength::Weak => "weak",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Identity {
    pub strength: Strength,
    pub source: String,
}

impl Identity {
    pub fn new(strength: Strength, source: impl Into<String>) -> Identity {
        Identity { strength, source: source.into() }
    }

    /// The `weak` identity of a volume without a UUID.
    pub fn weak(fs_type: &str, volume_name: Option<&str>, capacity_bytes: Option<u64>) -> Identity {
        let cap = capacity_bytes.map(|c| c.to_string()).unwrap_or_default();
        Identity::new(Strength::Weak, format!("{fs_type}|{}|{cap}", volume_name.unwrap_or("")))
    }

    /// `d` and the first `digits` hex digits of SHA-256 of `<strength>:<source>`.
    pub fn short_id(&self, digits: usize) -> String {
        let hash = sha256(format!("{}:{}", self.strength.as_str(), self.source).as_bytes());
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        format!("d{}", &hex[..digits.min(hex.len())])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeFacts {
    pub name: Option<String>,
    pub fs_type: String,
    pub capacity_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceFacts {
    pub model: Option<String>,
    pub vendor: Option<String>,
    pub bus: Option<String>,
    pub serial: Option<String>,
    pub media_bytes: Option<u64>,
    pub internal: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Disk {
    pub id: String,
    pub identity: Identity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub sets: Vec<String>,
    pub volume: VolumeFacts,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceFacts>,
    pub first_seen: String,
    pub last_seen: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_mount_point: Option<String>,
}

impl Disk {
    /// The label, else the volume name, else the short ID.
    pub fn display_name(&self) -> &str {
        self.label.as_deref().or(self.volume.name.as_deref()).unwrap_or(&self.id)
    }
}

/// One sighting of a mounted disk, as a scan reports it.
#[derive(Debug, Clone)]
pub struct Sighting {
    pub identity: Identity,
    pub volume: VolumeFacts,
    pub device: Option<DeviceFacts>,
    pub mount_point: String,
    pub at: String,
}

impl Sighting {
    /// The sighting a snapshot's metadata records (its volume as the scan saw
    /// it, no device facts). Used by the store version 3 migration and for
    /// snapshots written without a disk. FAT and exFAT UUIDs in metadata may be
    /// random (ADR 0002, 2.2), so those volumes, and volumes without a UUID, get
    /// a `weak` identity: filesystem, volume name and mount point.
    pub fn of_snapshot(m: &SnapshotMeta) -> Sighting {
        let v = &m.volume;
        let capacity = m.space_after.or(m.space_before).map(|s| s.total_bytes);
        let fat = matches!(v.fs_type.as_str(), "msdos" | "exfat");
        let identity = match &v.uuid {
            Some(u) if !fat => Identity::new(Strength::Uuid, u.clone()),
            _ => Identity::new(Strength::Weak, format!("{}|{}|@{}", v.fs_type, v.name.as_deref().unwrap_or(""), v.mount_point)),
        };
        Sighting {
            identity,
            volume: VolumeFacts { name: v.name.clone(), fs_type: v.fs_type.clone(), capacity_bytes: capacity },
            device: None,
            mount_point: v.mount_point.clone(),
            at: m.started_at.clone(),
        }
    }
}

/// Why a sighting is not simply recorded against a known disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Doubt {
    /// The identity is known, but the device serial differs: a copy of the disk?
    Clone,
    /// Only a `weak` identity matched.
    Weak,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observed {
    /// A known disk; its facts and "last seen" were updated.
    Known(String),
    /// A disk seen for the first time; added.
    New(String),
    /// Matches a known disk, but the user decides (ADR 0002, owner's decision 3):
    /// nothing was changed. Then `touch` it, or `add` it as a separate disk.
    Ask { existing: String, doubt: Doubt },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Registry {
    pub disks: Vec<Disk>,
}

#[derive(Serialize, Deserialize)]
struct Document {
    format: String,
    version: u32,
    #[serde(default)]
    disks: Vec<Disk>,
}

/// Holds `<store>/disks.lock` (released when dropped).
pub struct DisksLock {
    _file: File,
}

pub fn disks_path(store: &Path) -> PathBuf {
    store.join(DISKS_FILE)
}

fn io_at(path: &Path) -> impl FnOnce(std::io::Error) -> StoreError + '_ {
    move |source| StoreError::Io { path: path.to_path_buf(), source }
}

/// Waits for and takes the registry lock. Hold it only for one read-modify-write.
pub fn lock(store: &Path) -> Result<DisksLock> {
    let p = store.join(DISKS_LOCK);
    let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&p).map_err(io_at(&p))?;
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io_at(&p)(std::io::Error::last_os_error()));
    }
    Ok(DisksLock { _file: f })
}

fn invalid(label: &str, why: &str) -> StoreError {
    StoreError::InvalidLabel { label: label.into(), why: why.into() }
}

fn is_short_id(s: &str) -> bool {
    s.len() >= 9 && s.starts_with('d') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

impl Registry {
    /// Reads `<store>/disks.json`; a missing file is an empty registry.
    pub fn load(store: &Path) -> Result<Registry> {
        let p = disks_path(store);
        let bytes = match std::fs::read(&p) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Registry::default()),
            Err(e) => return Err(io_at(&p)(e)),
        };
        let doc: Document = serde_json::from_slice(&bytes).map_err(|e| StoreError::Corrupt(format!("{}: {e}", p.display())))?;
        if doc.format != DISKS_FORMAT {
            return Err(StoreError::Corrupt(format!("{} is not a DiskDrift disk registry", p.display())));
        }
        if doc.version > DISKS_VERSION {
            return Err(StoreError::UnsupportedDisksVersion { found: doc.version });
        }
        Ok(Registry { disks: doc.disks })
    }

    /// Writes the registry atomically (temp file, fsync, rename, fsync directory).
    pub fn save(&self, store: &Path, _lock: &DisksLock) -> Result<()> {
        let p = disks_path(store);
        let doc = Document { format: DISKS_FORMAT.into(), version: DISKS_VERSION, disks: self.disks.clone() };
        let bytes = serde_json::to_vec_pretty(&doc).expect("registry serializes");
        write_atomic(&p, &bytes).map_err(io_at(&p))?;
        sync_dir(store).map_err(io_at(store))
    }

    /// Lock, load, apply `f`, save when it succeeds, unlock.
    pub fn update<T>(store: &Path, f: impl FnOnce(&mut Registry) -> Result<T>) -> Result<T> {
        let lock = lock(store)?;
        let mut reg = Registry::load(store)?;
        let out = f(&mut reg)?;
        reg.save(store, &lock)?;
        Ok(out)
    }

    pub fn get(&self, id: &str) -> Option<&Disk> {
        self.disks.iter().find(|d| d.id == id)
    }

    fn get_mut(&mut self, id: &str) -> Result<&mut Disk> {
        self.disks.iter_mut().find(|d| d.id == id).ok_or_else(|| StoreError::UnknownDisk(id.into()))
    }

    pub fn find(&self, identity: &Identity) -> Option<&Disk> {
        self.disks.iter().find(|d| &d.identity == identity)
    }

    /// A disk by short ID, label (compared case-insensitively) or a unique
    /// prefix of the short ID, in that order.
    pub fn resolve(&self, reference: &str) -> Option<&Disk> {
        let lower = reference.to_lowercase();
        if let Some(d) = self.get(&lower).or_else(|| self.disks.iter().find(|d| d.label.as_deref().is_some_and(|l| l.to_lowercase() == lower))) {
            return Some(d);
        }
        if lower.len() < 2 || !lower.starts_with('d') {
            return None;
        }
        let mut matches = self.disks.iter().filter(|d| d.id.starts_with(&lower));
        match (matches.next(), matches.next()) {
            (Some(d), None) => Some(d),
            _ => None,
        }
    }

    /// The short ID for `identity`: 8 hex digits, longer when that collides
    /// with another disk's ID.
    pub fn short_id_for(&self, identity: &Identity) -> String {
        if let Some(d) = self.find(identity) {
            return d.id.clone();
        }
        let mut digits = 8;
        loop {
            let id = identity.short_id(digits);
            if self.get(&id).is_none() || digits >= 64 {
                return id;
            }
            digits = if digits < 12 { 12 } else { digits + 4 };
        }
    }

    /// Records a sighting: updates a known disk, adds a new one, or leaves the
    /// decision to the user (a possible clone, a weak match).
    pub fn observe(&mut self, s: &Sighting) -> Observed {
        let Some(known) = self.find(&s.identity) else {
            return Observed::New(self.add(s));
        };
        let id = known.id.clone();
        if s.identity.strength == Strength::Weak {
            return Observed::Ask { existing: id, doubt: Doubt::Weak };
        }
        let serial_now = s.device.as_ref().and_then(|d| d.serial.as_deref());
        let serial_was = known.device.as_ref().and_then(|d| d.serial.as_deref());
        if let (Some(now), Some(was)) = (serial_now, serial_was)
            && now != was
        {
            return Observed::Ask { existing: id, doubt: Doubt::Clone };
        }
        self.touch(&id, s).expect("found above");
        Observed::Known(id)
    }

    /// Adds `s` as a new disk and returns its short ID. A separate identity
    /// for a clone is the caller's (for example the UUID plus the device serial).
    pub fn add(&mut self, s: &Sighting) -> String {
        let id = self.short_id_for(&s.identity);
        if self.get(&id).is_some() {
            self.touch(&id, s).expect("exists");
            return id;
        }
        self.disks.push(Disk {
            id: id.clone(),
            identity: s.identity.clone(),
            label: None,
            description: None,
            sets: Vec::new(),
            volume: s.volume.clone(),
            device: s.device.clone(),
            first_seen: s.at.clone(),
            last_seen: s.at.clone(),
            last_mount_point: Some(s.mount_point.clone()),
        });
        id
    }

    /// Records `s` against the known disk `id`. The facts and "last seen" come
    /// from the newest sighting, so recording an older one (a migration run
    /// again) changes only "first seen".
    pub fn touch(&mut self, id: &str, s: &Sighting) -> Result<()> {
        let d = self.get_mut(id)?;
        if s.at < d.first_seen {
            d.first_seen = s.at.clone();
        }
        if s.at < d.last_seen {
            return Ok(());
        }
        d.volume = s.volume.clone();
        if s.device.is_some() {
            d.device = s.device.clone();
        }
        d.last_seen = s.at.clone();
        d.last_mount_point = Some(s.mount_point.clone());
        Ok(())
    }

    /// The disk of `s`: a known one (touched) or a new one (added). For
    /// metadata the store records, where there is nobody to ask.
    pub fn record(&mut self, s: &Sighting) -> String {
        match self.find(&s.identity).map(|d| d.id.clone()) {
            Some(id) => {
                self.touch(&id, s).expect("found");
                id
            }
            None => self.add(s),
        }
    }

    /// Sets or clears (`None`, or blank) the label of disk `id`. Labels are
    /// unique in a store, ignoring case, and usable in references, so they
    /// cannot contain `:` or look like a short ID.
    pub fn set_label(&mut self, id: &str, label: Option<&str>) -> Result<()> {
        let label = label.map(str::trim).filter(|l| !l.is_empty());
        if let Some(l) = label {
            if l.contains(':') || l.contains('/') {
                return Err(invalid(l, "a label cannot contain ':' or '/'"));
            }
            if l.chars().any(char::is_control) {
                return Err(invalid(l, "a label cannot contain control characters"));
            }
            if is_short_id(&l.to_lowercase()) {
                return Err(invalid(l, "a label cannot look like a disk ID (d followed by hex digits)"));
            }
            if l.eq_ignore_ascii_case("latest") {
                return Err(invalid(l, "'latest' is reserved for snapshot references"));
            }
            let lower = l.to_lowercase();
            if let Some(other) = self.disks.iter().find(|d| d.id != id && d.label.as_deref().is_some_and(|o| o.to_lowercase() == lower)) {
                return Err(StoreError::DuplicateLabel { label: l.into(), disk: other.id.clone() });
            }
        }
        self.get_mut(id)?.label = label.map(String::from);
        Ok(())
    }

    pub fn set_description(&mut self, id: &str, description: Option<&str>) -> Result<()> {
        self.get_mut(id)?.description = description.map(str::trim).filter(|s| !s.is_empty()).map(String::from);
        Ok(())
    }

    /// Puts disk `id` in `set` (sets work like tags; adding twice is a no-op).
    pub fn add_to_set(&mut self, id: &str, set: &str) -> Result<()> {
        let set = set.trim();
        if set.is_empty() || set.contains(':') || set.chars().any(char::is_control) {
            return Err(invalid(set, "a set name cannot be empty or contain ':'"));
        }
        let d = self.get_mut(id)?;
        if !d.sets.iter().any(|s| s.eq_ignore_ascii_case(set)) {
            d.sets.push(set.into());
        }
        Ok(())
    }

    pub fn remove_from_set(&mut self, id: &str, set: &str) -> Result<()> {
        self.get_mut(id)?.sets.retain(|s| !s.eq_ignore_ascii_case(set.trim()));
        Ok(())
    }

    /// Removes the entry of disk `id`. Its snapshots are the caller's to remove
    /// first (ADR 0002: forgetting a disk removes its snapshots, then its entry).
    pub fn forget(&mut self, id: &str) -> Result<Disk> {
        let i = self.disks.iter().position(|d| d.id == id).ok_or_else(|| StoreError::UnknownDisk(id.into()))?;
        Ok(self.disks.remove(i))
    }
}

/// SHA-256 (FIPS 180-4). Only short IDs use it, so there is no streaming API.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
        0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
        0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 32];
    for (i, x) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskdrift_core::testutil::TestDir;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn sighting(identity: Identity, serial: Option<&str>, at: &str) -> Sighting {
        Sighting {
            identity,
            volume: VolumeFacts { name: Some("Backup".into()), fs_type: "hfs".into(), capacity_bytes: Some(500) },
            device: Some(DeviceFacts { serial: serial.map(String::from), bus: Some("USB".into()), ..Default::default() }),
            mount_point: "/Volumes/Backup".into(),
            at: at.into(),
        }
    }

    fn uuid(s: &str) -> Identity {
        Identity::new(Strength::Uuid, s)
    }

    #[test]
    fn sha256_matches_the_standard_vectors() {
        assert_eq!(hex(&sha256(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hex(&sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(
            hex(&sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // 56 and 64 bytes: the padding needs a second block.
        assert_eq!(hex(&sha256(&[b'a'; 64])), "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb");
        assert_eq!(hex(&sha256(&vec![b'a'; 1_000_000])), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
    }

    #[test]
    fn short_ids_are_d_and_8_hex_digits_and_depend_on_the_strength() {
        let id = uuid("9F0E").short_id(8);
        assert_eq!(id.len(), 9);
        assert!(is_short_id(&id));
        assert_eq!(id, format!("d{}", &hex(&sha256(b"uuid:9F0E"))[..8]));
        assert_ne!(id, Identity::new(Strength::Serial, "9F0E").short_id(8));
    }

    #[test]
    fn a_colliding_short_id_is_extended_to_12_digits() {
        let mut reg = Registry::default();
        let a = uuid("A");
        let b = uuid("B");
        // Pretend another disk already took B's 8-digit ID.
        reg.add(&sighting(a, None, "t1"));
        reg.disks[0].id = b.short_id(8);
        let id = reg.short_id_for(&b);
        assert_eq!(id, b.short_id(12));
        assert_eq!(reg.add(&sighting(b.clone(), None, "t2")), id);
        // A known identity keeps the ID it has.
        assert_eq!(reg.short_id_for(&b), id);
    }

    #[test]
    fn a_missing_registry_is_empty_and_loading_creates_nothing() {
        let dir = TestDir::new("disks-missing");
        assert_eq!(Registry::load(dir.path()).unwrap(), Registry::default());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn the_registry_round_trips_through_its_file() {
        let dir = TestDir::new("disks-roundtrip");
        let id = Registry::update(dir.path(), |r| {
            let id = r.add(&sighting(uuid("U1"), Some("WX1"), "2026-10-22T10:00:00+02:00"));
            r.set_label(&id, Some("backupy-2014"))?;
            r.set_description(&id, Some(" WD 500 GB, shelf 2 "))?;
            r.add_to_set(&id, "stare-dyski-2014")?;
            r.add_to_set(&id, "Stare-Dyski-2014")?;
            Ok(id)
        })
        .unwrap();
        let reg = Registry::load(dir.path()).unwrap();
        let d = reg.get(&id).unwrap();
        assert_eq!(d.label.as_deref(), Some("backupy-2014"));
        assert_eq!(d.description.as_deref(), Some("WD 500 GB, shelf 2"));
        assert_eq!(d.sets, ["stare-dyski-2014"]);
        assert_eq!(d.device.as_ref().unwrap().serial.as_deref(), Some("WX1"));
        let text = std::fs::read_to_string(disks_path(dir.path())).unwrap();
        assert!(text.contains("\"format\": \"diskdrift-disks\"") && text.contains("\"firstSeen\"") && text.contains("\"strength\": \"uuid\""));
        // Nothing temporary left behind.
        let mut names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["disks.json", "disks.lock"]);
    }

    #[test]
    fn a_failed_update_writes_nothing() {
        let dir = TestDir::new("disks-failed");
        let id = Registry::update(dir.path(), |r| Ok(r.add(&sighting(uuid("U1"), None, "t1")))).unwrap();
        let before = std::fs::read(disks_path(dir.path())).unwrap();
        let err = Registry::update(dir.path(), |r| {
            r.set_description(&id, Some("changed"))?;
            r.set_label("d00000000", Some("x"))
        });
        assert!(matches!(err, Err(StoreError::UnknownDisk(_))));
        assert_eq!(std::fs::read(disks_path(dir.path())).unwrap(), before);
    }

    #[test]
    fn newer_or_foreign_registries_are_refused() {
        let dir = TestDir::new("disks-version");
        std::fs::write(disks_path(dir.path()), r#"{"format":"diskdrift-disks","version":2,"disks":[]}"#).unwrap();
        assert!(matches!(Registry::load(dir.path()), Err(StoreError::UnsupportedDisksVersion { found: 2 })));
        std::fs::write(disks_path(dir.path()), r#"{"format":"other","version":1}"#).unwrap();
        assert!(matches!(Registry::load(dir.path()), Err(StoreError::Corrupt(_))));
        std::fs::write(disks_path(dir.path()), "{").unwrap();
        assert!(matches!(Registry::load(dir.path()), Err(StoreError::Corrupt(_))));
    }

    #[test]
    fn a_known_disk_is_touched_and_a_new_one_added() {
        let mut reg = Registry::default();
        let Observed::New(id) = reg.observe(&sighting(uuid("U1"), Some("WX1"), "t1")) else { panic!() };
        let mut later = sighting(uuid("U1"), Some("WX1"), "t2");
        later.mount_point = "/Volumes/Backup 1".into();
        later.volume.name = Some("Backup 1".into());
        assert_eq!(reg.observe(&later), Observed::Known(id.clone()));
        let d = reg.get(&id).unwrap();
        assert_eq!((d.first_seen.as_str(), d.last_seen.as_str()), ("t1", "t2"));
        assert_eq!(d.last_mount_point.as_deref(), Some("/Volumes/Backup 1"));
        assert_eq!(d.display_name(), "Backup 1");
        assert!(matches!(reg.observe(&sighting(uuid("U2"), None, "t3")), Observed::New(_)));
        assert_eq!(reg.disks.len(), 2);
    }

    #[test]
    fn a_known_uuid_on_another_device_and_a_weak_match_are_asked_about() {
        let mut reg = Registry::default();
        let Observed::New(id) = reg.observe(&sighting(uuid("U1"), Some("WX1"), "t1")) else { panic!() };
        let copy = sighting(uuid("U1"), Some("ZZ9"), "t2");
        assert_eq!(reg.observe(&copy), Observed::Ask { existing: id.clone(), doubt: Doubt::Clone });
        assert_eq!(reg.get(&id).unwrap().last_seen, "t1", "nothing changed before the user decides");
        // Without a serial on either side there is nothing to tell them apart.
        let mut no_serial = copy.clone();
        no_serial.device = None;
        assert_eq!(reg.observe(&no_serial), Observed::Known(id.clone()));
        // The user splits the copy off: a separate identity.
        let split = Sighting { identity: uuid("U1+ZZ9"), ..copy };
        assert_ne!(reg.add(&split), id);

        let weak = Identity::weak("ntfs", Some("Old"), Some(100));
        let Observed::New(w) = reg.observe(&sighting(weak.clone(), None, "t1")) else { panic!() };
        assert_eq!(reg.observe(&sighting(weak, None, "t2")), Observed::Ask { existing: w, doubt: Doubt::Weak });
    }

    #[test]
    fn labels_are_unique_ignoring_case_and_usable_in_references() {
        let mut reg = Registry::default();
        let a = reg.add(&sighting(uuid("A"), None, "t"));
        let b = reg.add(&sighting(uuid("B"), None, "t"));
        reg.set_label(&a, Some("Backupy-2014")).unwrap();
        assert!(matches!(reg.set_label(&b, Some("backupy-2014")), Err(StoreError::DuplicateLabel { disk, .. }) if disk == a));
        reg.set_label(&a, Some("BACKUPY-2014")).unwrap();
        assert_eq!(reg.resolve("backupy-2014").unwrap().id, a);
        assert_eq!(reg.resolve(&b).unwrap().id, b);
        assert!(reg.resolve("nothing").is_none());
        assert_eq!(reg.resolve(&b[..5].to_uppercase()).unwrap().id, b, "a prefix of the ID");
        assert!(reg.resolve("d").is_none(), "too short, and matches both");
        for bad in ["a:b", "a/b", "d1a2b3c4d", "D1A2B3C4D", "latest", "x\ty"] {
            assert!(matches!(reg.set_label(&b, Some(bad)), Err(StoreError::InvalidLabel { .. })), "{bad}");
        }
        reg.set_label(&a, Some("  ")).unwrap();
        assert_eq!(reg.get(&a).unwrap().label, None);
        assert!(matches!(reg.set_label("d00000000", Some("x")), Err(StoreError::UnknownDisk(_))));
    }

    #[test]
    fn sets_work_like_tags_and_forgetting_removes_the_entry() {
        let mut reg = Registry::default();
        let a = reg.add(&sighting(uuid("A"), None, "t"));
        reg.add_to_set(&a, "one").unwrap();
        reg.add_to_set(&a, "two").unwrap();
        reg.remove_from_set(&a, "ONE").unwrap();
        assert_eq!(reg.get(&a).unwrap().sets, ["two"]);
        assert!(reg.add_to_set(&a, " ").is_err());
        assert_eq!(reg.forget(&a).unwrap().id, a);
        assert!(reg.get(&a).is_none());
        assert!(matches!(reg.forget(&a), Err(StoreError::UnknownDisk(_))));
    }

    #[test]
    fn concurrent_updates_do_not_lose_each_other() {
        let dir = TestDir::new("disks-concurrent");
        let path = dir.path().to_path_buf();
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let p = path.clone();
                std::thread::spawn(move || Registry::update(&p, |r| Ok(r.add(&sighting(uuid(&format!("U{i}")), None, "t")))).unwrap())
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(Registry::load(&path).unwrap().disks.len(), 8);
    }
}
