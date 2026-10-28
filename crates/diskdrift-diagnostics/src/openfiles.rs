//! Deleted-but-open files: unlinked files whose space is held until the last
//! process closes them. Found with `lsof +L1` (read-only).

use crate::command;
use serde::Serialize;
use std::collections::HashSet;
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenDeletedFile {
    pub pid: u32,
    pub command: String,
    pub path: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenDeletedReport {
    pub supported: bool,
    /// Without root, lsof only sees the current user's processes.
    pub scope: String,
    /// Distinct files (by device+inode); a file open in several processes counts once.
    pub total_bytes: u64,
    pub distinct_files: usize,
    /// Largest entries (one per process and file), at most 50.
    pub files: Vec<OpenDeletedFile>,
    pub error: Option<String>,
}

/// Parses `lsof -nP +L1 -F pcftDisn` output.
pub fn parse_lsof(text: &str) -> (u64, usize, Vec<OpenDeletedFile>) {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let mut total = 0u64;
    let (mut pid, mut cmd) = (0u32, String::new());
    let mut cur: Option<(String, String, String, u64, String)> = None; // type, dev, inode, size, name

    let mut flush = |cur: &mut Option<(String, String, String, u64, String)>, pid: u32, cmd: &str| {
        if let Some((ty, dev, ino, size, name)) = cur.take()
            && ty == "REG" && !name.is_empty() {
                if seen.insert((dev, ino)) {
                    total += size;
                }
                files.push(OpenDeletedFile { pid, command: cmd.to_string(), path: name, size_bytes: size });
            }
    };

    for line in text.lines() {
        let Some(tag) = line.chars().next() else { continue };
        let val = &line[tag.len_utf8()..];
        match tag {
            'p' => {
                flush(&mut cur, pid, &cmd);
                pid = val.parse().unwrap_or(0);
                cmd.clear();
            }
            'c' => cmd = val.to_string(),
            'f' => {
                flush(&mut cur, pid, &cmd);
                cur = Some((String::new(), String::new(), String::new(), 0, String::new()));
            }
            't' => if let Some(c) = cur.as_mut() { c.0 = val.to_string() },
            'D' => if let Some(c) = cur.as_mut() { c.1 = val.to_string() },
            'i' => if let Some(c) = cur.as_mut() { c.2 = val.to_string() },
            's' => if let Some(c) = cur.as_mut() { c.3 = val.parse().unwrap_or(0) },
            'n' => if let Some(c) = cur.as_mut() { c.4 = val.to_string() },
            _ => {}
        }
    }
    flush(&mut cur, pid, &cmd);
    files.sort_by_key(|f| std::cmp::Reverse(f.size_bytes));
    (total, seen.len(), files)
}

pub fn scan(timeout: Duration) -> OpenDeletedReport {
    let root = unsafe { libc::geteuid() } == 0;
    let scope = if root { "all processes" } else { "current user's processes only (run as root to see all)" };
    // lsof exits 1 when some processes could not be inspected; output is still valid.
    let result = command::run("/usr/sbin/lsof", &["-nP", "+L1", "-F", "pcftDisn"], timeout).or_else(|e| match e {
        command::CommandError::Failed { status: 1, .. } => Ok(Vec::new()),
        other => Err(other),
    });
    match result {
        Ok(out) => {
            let (total, distinct, mut files) = parse_lsof(&String::from_utf8_lossy(&out));
            files.truncate(50);
            OpenDeletedReport {
                supported: true,
                scope: scope.into(),
                total_bytes: total,
                distinct_files: distinct,
                files,
                error: None,
            }
        }
        Err(e) => OpenDeletedReport {
            supported: false,
            scope: scope.into(),
            total_bytes: 0,
            distinct_files: 0,
            files: Vec::new(),
            error: Some(e.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_dedups() {
        let text = "p179\ncloginwindow\nf3\ntREG\nD0x1000f\ni626\ns67700\nn/Library/x.lt9\n\
                    p434\ncdistnoted\nftxt\ntREG\nD0x1000f\ni626\ns67700\nn/Library/x.lt9\n\
                    f4\ntREG\nD0x1000f\ni999\ns1000\nn/tmp/big\nf5\ntDIR\nD0x1\ni1\ns5\nn/d\n";
        let (total, distinct, files) = parse_lsof(text);
        assert_eq!(total, 68_700, "the same file in two processes counts once");
        assert_eq!(distinct, 2);
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].command, "loginwindow");
    }
}
