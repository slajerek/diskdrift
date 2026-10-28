//! Path reconstruction for canonical record streams.

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// Tracks the path components of the current record while streaming a snapshot.
///
/// Component 0 is the scan root (a full path); component `d` is the name of the
/// record at depth `d`. Buffers are reused to avoid per-record allocation.
#[derive(Debug, Default, Clone)]
pub struct PathStack {
    comps: Vec<Vec<u8>>,
    len: usize,
}

impl PathStack {
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes `name` the component at `depth`, dropping anything deeper.
    ///
    /// Fails if `depth` skips a level, which means the stream is not in canonical order.
    pub fn set(&mut self, depth: u16, name: &[u8]) -> io::Result<()> {
        let depth = depth as usize;
        if depth > self.len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("record depth {depth} skips a level (current depth {})", self.len),
            ));
        }
        if depth < self.comps.len() {
            let buf = &mut self.comps[depth];
            buf.clear();
            buf.extend_from_slice(name);
        } else {
            self.comps.push(name.to_vec());
        }
        self.len = depth + 1;
        Ok(())
    }

    pub fn components(&self) -> &[Vec<u8>] {
        &self.comps[..self.len]
    }

    pub fn depth(&self) -> usize {
        self.len.saturating_sub(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Full path bytes of the current record.
    pub fn path_bytes(&self) -> Vec<u8> {
        join_components(self.components())
    }

    pub fn path_buf(&self) -> PathBuf {
        PathBuf::from(OsStr::from_bytes(&self.path_bytes()))
    }

    /// True if the current path equals `prefix` or lies below it.
    pub fn starts_with(&self, prefix: &[Vec<u8>]) -> bool {
        prefix.len() <= self.len && self.comps[..prefix.len()] == *prefix
    }
}

/// Joins a root path and child names with `/`.
pub fn join_components(comps: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(comps.iter().map(|c| c.len() + 1).sum());
    for (i, c) in comps.iter().enumerate() {
        if i > 0 && out.last() != Some(&b'/') {
            out.push(b'/');
        }
        out.extend_from_slice(c);
    }
    out
}

/// Canonical order of two record paths: component-wise byte order, a parent before
/// its descendants. This is depth-first pre-order with children sorted by name.
pub fn compare_paths(a: &[Vec<u8>], b: &[Vec<u8>]) -> Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        match x.as_slice().cmp(y.as_slice()) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    a.len().cmp(&b.len())
}

/// Splits an absolute path below `root` into components usable with [`PathStack`]:
/// `["/root", "a", "b"]`. Returns `None` if `path` is not inside `root`.
pub fn components_under(root: &[u8], path: &[u8]) -> Option<Vec<Vec<u8>>> {
    let trimmed_root = trim_trailing_slash(root);
    let trimmed_path = trim_trailing_slash(path);
    let mut comps = vec![root.to_vec()];
    if trimmed_path == trimmed_root {
        return Some(comps);
    }
    let rest = if trimmed_root.is_empty() {
        // Root is "/".
        trimmed_path.strip_prefix(b"/")?
    } else {
        let r = trimmed_path.strip_prefix(trimmed_root)?;
        r.strip_prefix(b"/")?
    };
    comps.extend(rest.split(|&c| c == b'/').filter(|c| !c.is_empty()).map(|c| c.to_vec()));
    Some(comps)
}

fn trim_trailing_slash(p: &[u8]) -> &[u8] {
    let mut p = p;
    while let Some(stripped) = p.strip_suffix(b"/") {
        p = stripped;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(parts: &[&str]) -> Vec<Vec<u8>> {
        parts.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    #[test]
    fn joins_root_and_names() {
        assert_eq!(join_components(&v(&["/", "Users", "mars"])), b"/Users/mars");
        assert_eq!(join_components(&v(&["/tmp/x", "a"])), b"/tmp/x/a");
        assert_eq!(join_components(&v(&["/tmp/x"])), b"/tmp/x");
    }

    #[test]
    fn stack_follows_depth() {
        let mut s = PathStack::new();
        s.set(0, b"/r").unwrap();
        s.set(1, b"a").unwrap();
        s.set(2, b"b").unwrap();
        assert_eq!(s.path_bytes(), b"/r/a/b");
        s.set(1, b"c").unwrap();
        assert_eq!(s.path_bytes(), b"/r/c");
        assert!(s.set(3, b"x").is_err());
    }

    #[test]
    fn canonical_order_is_preorder() {
        let mut paths = [
            v(&["/", "b"]),
            v(&["/", "a", "z"]),
            v(&["/"]),
            v(&["/", "a"]),
            v(&["/", "a-b"]),
            v(&["/", "a", "b"]),
        ];
        paths.sort_by(|a, b| compare_paths(a, b));
        let joined: Vec<String> =
            paths.iter().map(|p| String::from_utf8(join_components(p)).unwrap()).collect();
        assert_eq!(joined, ["/", "/a", "/a/b", "/a/z", "/a-b", "/b"]);
    }

    #[test]
    fn components_under_root() {
        assert_eq!(components_under(b"/", b"/Users/mars/").unwrap(), v(&["/", "Users", "mars"]));
        assert_eq!(components_under(b"/data", b"/data/x").unwrap(), v(&["/data", "x"]));
        assert_eq!(components_under(b"/data", b"/data").unwrap(), v(&["/data"]));
        assert!(components_under(b"/data", b"/database").is_none());
        assert!(components_under(b"/data", b"/other").is_none());
    }
}
