//! End-to-end: scan, change files, scan again, compare. Everything (scanned tree,
//! config, store) lives in the test scratch directory under `target/`.

use diskdrift_core::testutil::TestDir;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Env {
    t: TestDir,
    tree: PathBuf,
}

impl Env {
    fn new(name: &str) -> Env {
        let t = TestDir::new(name);
        std::fs::write(t.path().join("config.json"), r#"{"schemaVersion":1}"#).unwrap();
        let tree = t.path().join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        Env { t, tree }
    }

    fn file(&self, rel: &str, len: usize) {
        let p = self.tree.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, vec![7u8; len]).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_diskdrift"))
            .arg("--config")
            .arg(self.t.path().join("config.json"))
            .arg("--db")
            .arg(self.t.path().join("store"))
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let o = self.run(args);
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        serde_json::from_slice(&o.stdout).unwrap_or_else(|e| panic!("{args:?}: {e}: {}", String::from_utf8_lossy(&o.stdout)))
    }

    fn tree_str(&self) -> String {
        self.tree.to_string_lossy().into_owned()
    }
}

fn path_of(v: &serde_json::Value) -> String {
    v["path"].as_str().unwrap().to_string()
}

#[test]
fn scan_change_scan_diff() {
    let e = Env::new("e2e-diff");
    e.file("keep/a.txt", 1000);
    e.file("shrink/big.bin", 2_000_000);
    e.file("gone/old.log", 300_000);
    e.file("move/from/video.mp4", 1_500_000);
    let root = e.tree_str();

    let s1 = e.json(&["scan", &root, "--name", "baseline", "--json"]);
    assert_eq!(s1["snapshot"]["status"], "COMPLETED");
    assert_eq!(s1["rebase"]["kind"], "nothing", "first snapshot: nothing to compact");
    assert!(!e.t.path().join("tree/.diskdrift").exists());

    // Changes: growth, new files, deletion, shrink, move.
    e.file("keep/a.txt", 50_000);
    e.file("new/dir/huge.dat", 5_000_000);
    std::fs::remove_dir_all(e.tree.join("gone")).unwrap();
    e.file("shrink/big.bin", 100_000);
    std::fs::create_dir_all(e.tree.join("move/to")).unwrap();
    std::fs::rename(e.tree.join("move/from/video.mp4"), e.tree.join("move/to/video.mp4")).unwrap();

    let s2 = e.json(&["scan", &root, "--name", "followup", "--json"]);
    assert_eq!(s2["rebase"]["kind"], "converted", "{}", s2["rebase"]);
    assert_eq!(s2["rebase"]["id"], 1, "the baseline becomes a delta against the new full snapshot");

    let list = e.json(&["snapshots", "--json"]);
    assert_eq!(list["snapshots"].as_array().unwrap().len(), 2);
    assert!(list["storeBytes"].as_u64().unwrap() > 0);

    let d = e.json(&["diff", "baseline", "followup", "--json"]);
    let s = &d["summary"];
    assert!(s["added"]["count"].as_u64().unwrap() >= 3, "new, new/dir, huge.dat: {s}");
    assert!(s["deleted"]["count"].as_u64().unwrap() >= 2, "gone and old.log: {s}");
    assert_eq!(s["moved"], 1, "{s}");
    assert_eq!(d["moves"][0]["to"].as_str().unwrap(), format!("{root}/move/to/video.mp4"));
    let growing: Vec<String> = d["growingDirs"].as_array().unwrap().iter().map(path_of).collect();
    assert!(growing.contains(&format!("{root}/new")), "{growing:?}");
    let first_file = &d["files"][0];
    assert_eq!(path_of(first_file), format!("{root}/new/dir/huge.dat"));
    assert!(d["nonEquivalent"].is_null());
    assert!(d["space"]["freeConsumedBytes"].is_number());

    // --path restricts; --min-size filters listed entries.
    let p = e.json(&["diff", "1", "2", "--path", &format!("{root}/keep"), "--json"]);
    assert_eq!(p["summary"]["grown"]["count"], 1);
    assert!(p["unreconciledBytes"].is_null(), "not attributable for a sub-path");
    let m = e.json(&["diff", "1", "2", "--min-size", "4MB", "--files", "--json"]);
    assert_eq!(m["files"].as_array().unwrap().len(), 1);

    // Human output mentions the five accounting lines.
    let o = e.run(&["diff", "latest~1", "latest"]);
    let text = String::from_utf8_lossy(&o.stdout);
    for line in ["OS free space change", "Tracked logical change", "Tracked allocated change", "Coverage changes", "Unreconciled difference"] {
        assert!(text.contains(line), "missing {line}:\n{text}");
    }

    // Top of the newest snapshot.
    let t = e.json(&["top", "--snapshot", "latest", "--limit", "3", "--json"]);
    assert_eq!(path_of(&t["files"][0]), format!("{root}/new/dir/huge.dat"));
    assert_eq!(path_of(&t["dirs"][0]), root);

    // Verify integrity.
    let v = e.json(&["snapshots", "verify", "followup", "--json"]);
    assert_eq!(v["ok"], true);
}

#[test]
fn duplicate_name_and_unknown_snapshot_are_usage_errors() {
    let e = Env::new("e2e-dupe");
    e.file("x", 10);
    let root = e.tree_str();
    assert!(e.run(&["scan", &root, "--name", "a"]).status.success());
    assert_eq!(e.run(&["scan", &root, "--name", "a"]).status.code(), Some(2));
    assert_eq!(e.run(&["top", "--snapshot", "nope"]).status.code(), Some(2));
    assert_eq!(e.run(&["diff", "a", "missing"]).status.code(), Some(2));
}

#[test]
fn read_commands_without_store_do_not_create_it() {
    let e = Env::new("e2e-nostore");
    let l = e.json(&["snapshots", "--json"]);
    assert_eq!(l["storeExists"], false);
    assert_eq!(e.run(&["top"]).status.code(), Some(2));
    assert!(!e.t.path().join("store").exists());
}

#[test]
fn incompatible_snapshots_are_rejected_unless_allowed() {
    let e = Env::new("e2e-incompat");
    e.file("a/x", 10);
    e.file("b/y", 10);
    let root = e.tree_str();
    e.json(&["scan", &format!("{root}/a"), "--name", "A", "--json"]);
    e.json(&["scan", &format!("{root}/b"), "--name", "B", "--json"]);
    let o = e.run(&["diff", "A", "B"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("not comparable"));
    let d = e.json(&["diff", "A", "B", "--allow-incompatible", "--json"]);
    assert!(d["nonEquivalent"].as_str().unwrap().contains("different folders on the disk"));
}

#[test]
fn exclusions_and_non_utf8_paths() {
    let e = Env::new("e2e-exclude");
    e.file("node_modules/pkg/index.js", 5000);
    e.file("src/main.rs", 100);
    // APFS rejects names that are not valid UTF-8 (EILSEQ); other filesystems allow them.
    let weird = e.tree.join(std::ffi::OsStr::from_bytes(b"bad-\xff-name"));
    let non_utf8 = std::fs::write(&weird, vec![1u8; 9000]).is_ok();
    let root = e.tree_str();
    e.json(&["scan", &root, "--exclude", "node_modules", "--json"]);
    let t = e.json(&["top", "--files", "--limit", "10", "--json"]);
    let files = t["files"].as_array().unwrap();
    assert!(!files.iter().any(|f| path_of(f).contains("index.js")), "excluded");
    if !non_utf8 {
        return;
    }
    let bad = files.iter().find(|f| f.get("pathBytes").is_some()).expect("non-UTF-8 path carries pathBytes");
    let bytes = base64_decode(bad["pathBytes"].as_str().unwrap());
    assert!(bytes.ends_with(b"/bad-\xff-name"));
}

fn base64_decode(s: &str) -> Vec<u8> {
    // Minimal decoder to keep test dependencies small.
    let table = |c: u8| -> u8 {
        match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => 0,
        }
    };
    let clean: Vec<u8> = s.bytes().filter(|&c| c != b'=').collect();
    let mut out = Vec::new();
    for chunk in clean.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= (table(c) as u32) << (18 - 6 * i);
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..chunk.len() - 1]);
    }
    out
}

#[test]
fn unreadable_directory_makes_partial_snapshot() {
    use std::os::unix::fs::PermissionsExt;
    let e = Env::new("e2e-partial");
    e.file("open/a", 10);
    e.file("locked/secret", 10);
    let locked = e.tree.join("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let s = e.json(&["scan", &e.tree_str(), "--json"]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(s["snapshot"]["status"], "PARTIAL");
    assert_eq!(s["snapshot"]["counts"]["inaccessibleDirs"], 1);
}

#[test]
fn snapshots_remove_frees_store_space() {
    let e = Env::new("e2e-remove");
    e.file("x", 10);
    let root = e.tree_str();
    e.json(&["scan", &root, "--name", "one", "--json"]);
    e.file("y", 10);
    e.json(&["scan", &root, "--name", "two", "--json"]);
    // "one" is stored as a delta against "two": removing "two" rewrites "one" as full.
    let r = e.json(&["snapshots", "remove", "two", "--json"]);
    assert_eq!(r["removed"], serde_json::json!([2]));
    assert_eq!(r["rewritten"], serde_json::json!([1]));
    assert_eq!(e.json(&["snapshots", "verify", "one", "--json"])["ok"], true);
    assert!(e.tree.join("x").exists() && e.tree.join("y").exists(), "scanned files untouched");
    // Several: listed and refused without a terminal unless --yes.
    e.file("z", 10);
    e.json(&["scan", &root, "--name", "three", "--json"]);
    e.json(&["scan", &root, "--name", "four", "--json"]);
    let o = e.run(&["snapshots", "remove", "one", "three"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("Removing 2 scans frees about"), "{o:?}");
    let r = e.json(&["snapshots", "remove", "one", "three", "--yes", "--json"]);
    assert_eq!(r["removed"].as_array().unwrap().len(), 2);
    let p = e.json(&["snapshots", "prune", "--keep-last", "1", "--yes", "--json"]);
    assert_eq!(p["snapshots"], serde_json::json!([]), "only four is left: {p}");
    let c = e.json(&["snapshots", "compact", "--json"]);
    assert_eq!((c["checked"].as_u64(), c["cancelled"].as_bool()), (Some(1), Some(false)));
}

#[test]
fn scan_root_must_exist() {
    let e = Env::new("e2e-noroot");
    let o = e.run(&["scan", &e.t.path().join("does-not-exist").to_string_lossy()]);
    assert_eq!(o.status.code(), Some(2));
    assert!(!Path::new(&e.t.path().join("store")).exists(), "nothing is created for a bad root");
}

#[test]
fn write_failure_keeps_earlier_snapshots() {
    let e = Env::new("e2e-diskfull");
    e.file("a", 10);
    let root = e.tree_str();
    e.json(&["scan", &root, "--name", "ok", "--json"]);
    // Names that do not compress away, so the snapshot data exceeds the limit.
    for i in 0..4000u64 {
        let h = i.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (i << 7);
        e.file(&format!("many/{h:016x}"), (i % 97) as usize);
    }
    // ulimit -f is in 512-byte blocks: no file may grow beyond 8 KiB.
    let cmd = format!(
        "ulimit -f 16; exec '{}' --config '{}' --db '{}' scan '{}' --name big",
        env!("CARGO_BIN_EXE_diskdrift"),
        e.t.path().join("config.json").display(),
        e.t.path().join("store").display(),
        root
    );
    let o = Command::new("/bin/sh").arg("-c").arg(cmd).output().unwrap();
    assert_eq!(o.status.code(), Some(1), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stderr).contains("disk space"), "{}", String::from_utf8_lossy(&o.stderr));
    let l = e.json(&["snapshots", "--json"]);
    let snaps = l["snapshots"].as_array().unwrap();
    assert_eq!(snaps[0]["status"], "COMPLETED");
    assert_eq!(snaps[1]["status"], "FAILED");
    assert_eq!(e.json(&["snapshots", "verify", "ok", "--json"])["ok"], true);
    assert!(
        !std::fs::read_dir(e.t.path().join("store/snapshots")).unwrap().flatten().any(|f| f.path().extension().is_some_and(|x| x == "tmp")),
        "no temporary data left behind"
    );
    // The store stays usable.
    e.json(&["scan", &root, "--name", "after", "--json"]);
}

#[test]
fn scans_record_their_disk_and_disks_can_be_labelled_and_referenced() {
    let e = Env::new("e2e-disks");
    e.file("a/x", 10);
    let root = e.tree_str();
    let s1 = e.json(&["scan", &root, "--name", "one", "--json"]);
    let disk = s1["disk"]["disk"]["id"].as_str().unwrap().to_string();
    assert!(disk.starts_with('d') && disk.len() == 9, "{disk}");
    assert_eq!(s1["snapshot"]["disk"], disk.as_str());
    e.file("a/y", 5000);
    let s2 = e.json(&["scan", &root, "--name", "two", "--json"]);
    assert_eq!(s2["disk"]["new"], false, "the same disk the second time");

    // The list: connected (the test tree is on a mounted volume), two scans.
    let l = e.json(&["disks", "--json"]);
    let row = &l["disks"][0];
    assert_eq!(row["id"], disk.as_str());
    assert_eq!(row["connected"], true);
    assert_eq!(row["snapshots"], 2);
    let text = String::from_utf8_lossy(&e.run(&["disks"]).stdout).into_owned();
    assert!(text.contains(&disk) && text.contains("yes,"), "{text}");

    // Label, describe, sets; references through the label.
    let d = e.json(&["disks", "label", &disk[..5], "Robocze-2026", "--json"]);
    assert_eq!(d["label"], "Robocze-2026");
    assert_eq!(e.run(&["disks", "label", &disk, "a:b"]).status.code(), Some(2), "invalid label");
    e.json(&["disks", "describe", "robocze-2026", "the test tree", "--json"]);
    e.json(&["disks", "set", "add", "robocze-2026", "tests", "--json"]);
    let d = e.json(&["disks", "set", "add", "robocze-2026", "tmp", "--json"]);
    assert_eq!(d["sets"], serde_json::json!(["tests", "tmp"]));
    let d = e.json(&["disks", "set", "remove", "robocze-2026", "TMP", "--json"]);
    assert_eq!(d["sets"], serde_json::json!(["tests"]));
    assert_eq!(e.json(&["browse", "--snapshot", "robocze-2026:latest~1", "--json"])["snapshot"]["name"], "one");
    let diff = e.json(&["diff", "robocze-2026:latest~1", "robocze-2026:latest", "--json"]);
    assert_eq!(diff["summary"]["added"]["count"], 1);
    assert_eq!(e.run(&["top", "--snapshot", "nope:latest"]).status.code(), Some(2), "unknown disk");
    let only = e.json(&["snapshots", "--disk", "robocze-2026", "--json"]);
    assert_eq!(only["snapshots"].as_array().unwrap().len(), 2);
    assert_eq!(e.run(&["snapshots", "--disk", "nope"]).status.code(), Some(2));
    let files: Vec<String> = std::fs::read_dir(e.t.path().join("store/snapshots"))
        .unwrap()
        .map(|f| f.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(files.iter().all(|f| f.starts_with(&disk)), "{files:?}");

    // Forget: refused without --yes when nobody can be asked, then done.
    let o = e.run(&["disks", "forget", "robocze-2026"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("removes its 2 snapshots"));
    assert_eq!(e.json(&["snapshots", "--json"])["snapshots"].as_array().unwrap().len(), 2, "nothing removed");
    let f = e.json(&["disks", "forget", "robocze-2026", "--yes", "--json"]);
    assert_eq!(f["done"], true);
    assert_eq!(f["snapshots"].as_array().unwrap().len(), 2);
    assert!(e.json(&["snapshots", "--json"])["snapshots"].as_array().unwrap().is_empty());
    // The volume is still connected: it is listed again, as new (not in the store).
    let left = e.json(&["disks", "--json"]);
    assert!(left["disks"].as_array().unwrap().iter().all(|d| d["known"] == false && d["snapshots"] == 0), "{left}");
    assert!(e.tree.join("a/y").exists(), "scanned files are never touched");
}

#[test]
fn disk_listings_do_not_create_a_store() {
    let e = Env::new("e2e-disks-nostore");
    let l = e.json(&["disks", "--json"]);
    assert_eq!(l["storeExists"], false);
    assert_eq!(e.run(&["disks", "label", "dabc", "x"]).status.code(), Some(2));
    assert!(!e.t.path().join("store").exists());
}

#[test]
fn find_searches_the_latest_scan_then_the_history() {
    let e = Env::new("e2e-find");
    e.file("Docs/Ważna sprawa 10.txt", 12_000);
    e.file("Docs/plan.doc", 500);
    let root = e.tree_str();
    e.json(&["scan", &root, "--name", "one", "--json"]);
    std::fs::remove_file(e.tree.join("Docs/Ważna sprawa 10.txt")).unwrap();
    e.file("Docs/notes.txt", 10);
    e.json(&["scan", &root, "--name", "two", "--json"]);

    // Found now: no history unless asked.
    let r = e.json(&["find", "PLAN", "--json"]);
    assert_eq!(r["total"], 1);
    assert_eq!(path_of(&r["hits"][0]), format!("{root}/Docs/plan.doc"));
    assert!(r["history"].is_null());

    // Gone: the history says when, in the text and in JSON.
    let o = e.run(&["find", "wazna"]);
    assert!(o.status.success());
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(text.contains("Not on any disk as of the latest scans"), "{text}");
    assert!(text.contains("Searching the history for deleted files…"), "{text}");
    assert!(text.contains("last seen in scan #1") && text.contains("gone in scan #2"), "{text}");
    let r = e.json(&["find", "wazna", "--json"]);
    let gone = &r["history"]["gone"][0];
    assert_eq!(path_of(gone), format!("{root}/Docs/Ważna sprawa 10.txt"));
    assert_eq!((gone["lastSeen"]["id"].as_u64(), gone["goneIn"]["id"].as_u64()), (Some(1), Some(2)));
    assert_eq!(gone["size"].as_u64().map(|s| s >= 12_000), Some(true));

    // Never: the history is not searched; a glob and a bad regex.
    assert!(e.json(&["find", "wazna", "--no-history", "--json"])["history"].is_null());
    assert_eq!(e.json(&["find", "*.TXT", "--json"])["total"], 1);
    assert_eq!(e.run(&["find", "--regex", "("]).status.code(), Some(2));
    assert_eq!(e.run(&["find", "x", "--disk", "nope"]).status.code(), Some(2));
}

#[test]
fn find_without_a_store_says_so_and_creates_nothing() {
    let e = Env::new("e2e-find-nostore");
    let o = e.run(&["find", "anything"]);
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).contains("No scans yet"));
    assert!(!e.t.path().join("store").exists());
}

#[test]
fn a_careful_scan_is_marked_and_sets_limit_find() {
    let e = Env::new("e2e-careful");
    e.file("Docs/plan.doc", 10);
    let root = e.tree_str();
    let o = e.run(&["scan", &root, "--careful", "--json"]);
    assert!(o.status.success());
    // The test tree is on a read-write volume: it says so, and scans anyway.
    assert!(String::from_utf8_lossy(&o.stderr).contains("mounted read-write"));
    let s: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(s["snapshot"]["options"]["careful"], true);
    assert_eq!(s["snapshot"]["options"]["threads"], 1);
    let text = String::from_utf8_lossy(&e.run(&["snapshots"]).stdout).into_owned();
    assert!(text.contains("complete, careful"), "{text}");
    // Sets: none yet is a usage error; then the disk's set finds it.
    assert_eq!(e.run(&["find", "plan", "--set", "stare"]).status.code(), Some(2));
    let disk = s["disk"]["disk"]["id"].as_str().unwrap().to_string();
    assert!(e.run(&["disks", "set", "add", &disk, "stare"]).status.success());
    assert_eq!(e.json(&["find", "plan", "--set", "stare", "--json"])["total"], 1);
}
