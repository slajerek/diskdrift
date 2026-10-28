//! End-to-end tests of the `diskdrift` binary. Every invocation passes `--config`
//! and `--db` inside the test scratch directory, so nothing under the real
//! `~/.diskdrift` is read or created.

use diskdrift_core::testutil::TestDir;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn bin() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_diskdrift"));
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    c
}

fn run_in(t: &TestDir, args: &[&str]) -> Output {
    let cfg = t.path().join("config.json");
    let db = t.path().join("store");
    let mut c = bin();
    c.arg("--config").arg(&cfg).arg("--db").arg(&db).args(args);
    if !cfg.exists() {
        std::fs::write(&cfg, r#"{"schemaVersion":1}"#).unwrap();
    }
    c.output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn bare_invocation_without_tty_prints_help() {
    let o = bin().output().unwrap();
    assert!(o.status.success());
    let out = stdout(&o);
    assert!(out.contains("Usage:"), "{out}");
    assert!(!out.contains('\x1b'), "no escape sequences into a pipe");
}

#[test]
fn tui_without_tty_is_a_usage_error() {
    let o = bin().arg("tui").output().unwrap();
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn usage_json_is_valid() {
    let t = TestDir::new("cli-usage");
    let o = run_in(&t, &["usage", "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(v["containers"].is_array());
    assert!(!t.path().join("store").exists(), "usage must not create the store");
}

#[test]
fn info_reports_effective_paths() {
    let t = TestDir::new("cli-info");
    let o = run_in(&t, &["info", "--json"]);
    assert!(o.status.success());
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["settings"]["storagePath"]["source"], "commandLine");
    assert_eq!(v["settings"]["configPath"]["source"], "commandLine");
}

#[test]
fn invalid_config_exits_with_usage_error_naming_field() {
    let t = TestDir::new("cli-badcfg");
    std::fs::write(t.path().join("config.json"), r#"{"schemaVersion":1,"scan":{"threads":-1}}"#).unwrap();
    let o = run_in(&t, &["config", "show"]);
    assert_eq!(o.status.code(), Some(2));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("scan.threads"), "{err}");
    assert_eq!(std::fs::read_to_string(t.path().join("config.json")).unwrap(), r#"{"schemaVersion":1,"scan":{"threads":-1}}"#, "never rewritten");
}

#[test]
fn config_init_and_show() {
    let t = TestDir::new("cli-cfginit");
    let cfg = t.path().join("sub/config.json");
    let o = bin().arg("--config").arg(&cfg).args(["config", "init"]).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(cfg.exists());
    let again = bin().arg("--config").arg(&cfg).args(["config", "init"]).output().unwrap();
    assert_eq!(again.status.code(), Some(2), "refuses to overwrite");
    let show = bin().arg("--config").arg(&cfg).args(["config", "show", "--json"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(v["storagePath"]["source"], "configFile");
}

#[test]
fn bad_size_argument_is_a_usage_error() {
    let t = TestDir::new("cli-badsize");
    let o = run_in(&t, &["doctor", "--reference-free", "lots"]);
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn drive_relative_root_is_rejected_before_scan() {
    let t = TestDir::new("cli-drive-relative");
    let o = run_in(&t, &["scan", "C:relative"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("invalid scan root"));
    assert!(!t.path().join("store").exists());
}

#[test]
fn doctor_quick_json_is_read_only() {
    let t = TestDir::new("cli-doctor");
    let o = run_in(&t, &["doctor", "--quick", "--budget", "2", "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["filesModified"], false);
    assert!(v["checks"].as_array().unwrap().len() >= 3);
    assert_eq!(v["snapshots"]["reclaimable"], "unknown");
    assert!(!t.path().join("store").exists());
    assert!(!Path::new(&t.path().join("logs")).exists());
}

#[test]
fn analyze_lists_children_and_rejects_files() {
    let t = TestDir::new("cli-analyze");
    t.file("dir/Caches/blob", 40_000);
    t.file("dir/notes.txt", 100);
    let dir = t.path().join("dir");
    let o = run_in(&t, &["analyze", dir.to_str().unwrap(), "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["name"], "Caches");
    assert_eq!(items[0]["assessment"]["what"], "cache");
    let text = stdout(&run_in(&t, &["analyze", dir.to_str().unwrap()]));
    assert!(text.contains("regenerable") && text.contains("Note:"), "{text}");
    let o = run_in(&t, &["analyze", dir.join("notes.txt").to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(2), "a file is a usage error");
}

#[test]
#[cfg(not(windows))]
fn browse_lists_a_level_and_its_changes() {
    let t = TestDir::new("cli-browse");
    t.file("tree/a/big", 300_000);
    t.file("tree/b/small", 1_000);
    let tree = t.path().join("tree");
    let tree = tree.to_str().unwrap();
    assert!(run_in(&t, &["scan", tree, "--name", "one"]).status.success());
    t.file("tree/a/more", 500_000);
    assert!(run_in(&t, &["scan", tree, "--name", "two", "--disk", "same"]).status.success());

    let o = run_in(&t, &["browse", "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(items[0]["path"].as_str().unwrap().ends_with("/a"), "largest first: {v}");
    let text = stdout(&run_in(&t, &["browse", &format!("{tree}/a")]));
    assert!(text.contains("big") && text.contains("more"), "{text}");

    let text = stdout(&run_in(&t, &["browse", "--compare", "one"]));
    assert!(text.contains("a/") && text.contains("1 unchanged item(s) not listed"), "{text}");
    let o = run_in(&t, &["browse", "/elsewhere"]);
    assert_eq!(o.status.code(), Some(2), "outside the scan root is a usage error");
}
