//! Deleted-but-open files: unlinked files whose space is held until the last
//! process closes them. Found with `lsof +L1` (read-only).

#[cfg(unix)]
use crate::command;
use crate::measurement::Envelope;
use serde::Serialize;
use std::collections::HashSet;
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenDeletedFile {
    pub pid: u32,
    pub command: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(flatten)]
    pub measurement: Envelope,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenDeletedReport {
    pub supported: bool,
    /// Without root, lsof only sees the current user's processes.
    pub scope: String,
    /// Distinct files (by device+inode); a file open in several processes counts once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distinct_files: Option<usize>,
    /// Largest entries (one per process and file), at most 50.
    pub files: Vec<OpenDeletedFile>,
    pub error: Option<String>,
    #[serde(flatten)]
    pub measurement: Envelope,
}

/// Parses `lsof -nP +L1 -F pcftDisn` output.
pub fn parse_lsof(text: &str) -> (Option<u64>, usize, Vec<OpenDeletedFile>) {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let mut total = 0u64;
    let mut saw_file = false;
    let mut known_size = false;
    let (mut pid, mut cmd) = (0u32, String::new());
    let mut cur: Option<(String, String, String, Option<u64>, String)> = None; // type, dev, inode, size, name

    let mut flush =
        |cur: &mut Option<(String, String, String, Option<u64>, String)>, pid: u32, cmd: &str| {
            if let Some((ty, dev, ino, size, name)) = cur.take()
                && ty == "REG"
                && !name.is_empty()
            {
                saw_file = true;
                if seen.insert((dev, ino))
                    && let Some(size) = size
                {
                    total += size;
                    known_size = true;
                }
                files.push(OpenDeletedFile {
                    pid,
                    command: cmd.to_string(),
                    path: name,
                    size_bytes: size,
                    measurement: Envelope::default(),
                });
            }
        };

    for line in text.lines() {
        let Some(tag) = line.chars().next() else {
            continue;
        };
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
                cur = Some((
                    String::new(),
                    String::new(),
                    String::new(),
                    None,
                    String::new(),
                ));
            }
            't' => {
                if let Some(c) = cur.as_mut() {
                    c.0 = val.to_string()
                }
            }
            'D' => {
                if let Some(c) = cur.as_mut() {
                    c.1 = val.to_string()
                }
            }
            'i' => {
                if let Some(c) = cur.as_mut() {
                    c.2 = val.to_string()
                }
            }
            's' => {
                if let Some(c) = cur.as_mut() {
                    c.3 = val.parse().ok()
                }
            }
            'n' => {
                if let Some(c) = cur.as_mut() {
                    c.4 = val.to_string()
                }
            }
            _ => {}
        }
    }
    flush(&mut cur, pid, &cmd);
    files.sort_by(|a, b| match (a.size_bytes, b.size_bytes) {
        (Some(a), Some(b)) => b.cmp(&a),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.path.cmp(&b.path),
    });
    (
        if !saw_file || known_size {
            Some(total)
        } else {
            None
        },
        seen.len(),
        files,
    )
}

fn report_from_lsof(
    scope: &str,
    total: Option<u64>,
    distinct: usize,
    mut files: Vec<OpenDeletedFile>,
) -> OpenDeletedReport {
    let incomplete = files.iter().any(|file| file.size_bytes.is_none());
    files.truncate(50);
    for file in &mut files {
        file.measurement = if file.size_bytes.is_some() {
            Envelope::observed("process_open_file", "distinct_file", "lsof +L1")
        } else {
            Envelope::default()
        };
    }
    OpenDeletedReport {
        supported: true,
        scope: scope.into(),
        total_bytes: total,
        distinct_files: Some(distinct),
        files,
        error: None,
        measurement: if incomplete {
            Envelope::lower_bound(
                "process_open_file",
                "distinct_files",
                "lsof +L1; some file sizes unavailable",
            )
        } else {
            Envelope::observed("process_open_file", "distinct_files", "lsof +L1")
        },
    }
}

#[cfg(unix)]
pub fn scan(timeout: Duration) -> OpenDeletedReport {
    let root = unsafe { libc::geteuid() } == 0;
    let scope = if root {
        "all processes"
    } else {
        "current user's processes only (run as root to see all)"
    };
    // lsof exits 1 when some processes could not be inspected; output is still valid.
    let result = command::run("/usr/sbin/lsof", &["-nP", "+L1", "-F", "pcftDisn"], timeout)
        .or_else(|e| match e {
            command::CommandError::Failed { status: 1, .. } => Ok(Vec::new()),
            other => Err(other),
        });
    match result {
        Ok(out) => {
            let (total, distinct, files) = parse_lsof(&String::from_utf8_lossy(&out));
            report_from_lsof(scope, total, distinct, files)
        }
        Err(e) => OpenDeletedReport {
            supported: false,
            scope: scope.into(),
            total_bytes: None,
            distinct_files: None,
            files: Vec::new(),
            error: Some(e.to_string()),
            measurement: Envelope::default(),
        },
    }
}

#[cfg(not(unix))]
pub fn scan(_timeout: Duration) -> OpenDeletedReport {
    OpenDeletedReport {
        supported: false,
        scope: "unsupported on this platform".into(),
        total_bytes: None,
        distinct_files: None,
        files: Vec::new(),
        error: Some("deleted-but-open file scanning requires a Unix lsof implementation".into()),
        measurement: Envelope::default(),
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
        assert_eq!(
            total,
            Some(68_700),
            "the same file in two processes counts once"
        );
        assert_eq!(distinct, 2);
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].command, "loginwindow");
    }

    #[test]
    fn missing_lsof_size_is_unknown_not_zero() {
        let (total, distinct, files) = parse_lsof("p1\ncworker\nf3\ntREG\nDdev\ni1\nn/tmp/gone\n");
        assert_eq!(total, None);
        assert_eq!(distinct, 1);
        assert_eq!(files[0].size_bytes, None);
        assert!(
            !serde_json::to_string(&files[0])
                .unwrap()
                .contains("sizeBytes")
        );
    }

    #[test]
    fn partial_lsof_sizes_make_the_aggregate_a_lower_bound() {
        let text = "p1\ncworker\nf3\ntREG\nDdev\ni1\ns100\nn/tmp/known\n\
                    f4\ntREG\nDdev\ni2\nn/tmp/unknown\n";
        let (total, distinct, files) = parse_lsof(text);
        let report = report_from_lsof("test process scope", total, distinct, files);
        assert_eq!(report.total_bytes, Some(100));
        assert_eq!(report.distinct_files, Some(2));
        assert_eq!(
            report.measurement.quality,
            Some(crate::measurement::Quality::LowerBound)
        );
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["totalBytes"], 100);
        assert_eq!(json["quality"], "lower_bound");
        assert!(json["files"][1].get("sizeBytes").is_none());
    }
}
