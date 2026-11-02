//! Path reconstruction for canonical record streams.

use crate::native::{NativeName, NativePath, PathEncoding};
use std::cmp::Ordering;
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PathOrderError {
    #[error("cannot compare canonical paths with mixed encodings ({left:?} and {right:?})")]
    MixedEncoding {
        left: PathEncoding,
        right: PathEncoding,
    },
}

fn mixed(left: PathEncoding, right: PathEncoding) -> PathOrderError {
    PathOrderError::MixedEncoding { left, right }
}

fn encoding_of(path: &[NativeName]) -> Result<Option<PathEncoding>, PathOrderError> {
    let Some(first) = path.first() else {
        return Ok(None);
    };
    let encoding = first.encoding();
    for component in &path[1..] {
        if component.encoding() != encoding {
            return Err(mixed(encoding, component.encoding()));
        }
    }
    Ok(Some(encoding))
}

fn path_error(error: PathOrderError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error)
}

/// Tracks the path components of the current record while streaming a snapshot.
///
/// Component 0 is the scan root (a full path); component `d` is the name of the
/// record at depth `d`. Buffers are reused to avoid per-record allocation.
#[derive(Debug, Default, Clone)]
pub struct PathStack {
    comps: Vec<NativeName>,
    len: usize,
}

impl PathStack {
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes `name` the component at `depth`, dropping anything deeper.
    ///
    /// Fails if `depth` skips a level, which means the stream is not in canonical order.
    pub fn set(&mut self, depth: u16, name: &NativeName) -> io::Result<()> {
        let depth = depth as usize;
        if depth > self.len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "record depth {depth} skips a level (current depth {})",
                    self.len
                ),
            ));
        }
        if let Some(existing) = self.comps[..self.len].first().map(NativeName::encoding)
            && existing != name.encoding()
        {
            return Err(path_error(mixed(existing, name.encoding())));
        }
        if depth < self.comps.len() {
            let buf = &mut self.comps[depth];
            *buf = name.clone();
        } else {
            self.comps.push(name.clone());
        }
        self.len = depth + 1;
        Ok(())
    }

    pub fn components(&self) -> &[NativeName] {
        &self.comps[..self.len]
    }

    pub fn depth(&self) -> usize {
        self.len.saturating_sub(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Exact native path of the current record.
    pub fn path(&self) -> Result<NativePath, PathOrderError> {
        join_components(self.components())
    }

    /// True if the current path equals `prefix` or lies below it.
    pub fn starts_with(&self, prefix: &[NativeName]) -> Result<bool, PathOrderError> {
        let left = &self.comps[..self.len];
        let left_encoding = encoding_of(left)?;
        let right_encoding = encoding_of(prefix)?;
        if let (Some(left), Some(right)) = (left_encoding, right_encoding)
            && left != right
        {
            return Err(mixed(left, right));
        }
        Ok(prefix.len() <= self.len && left[..prefix.len()] == *prefix)
    }
}

/// Joins a root path and child names without changing their native encoding.
pub fn join_components(comps: &[NativeName]) -> Result<NativePath, PathOrderError> {
    let Some(encoding) = encoding_of(comps)? else {
        return Ok(NativePath::unix(Vec::new()));
    };
    match encoding {
        PathEncoding::UnixBytes => {
            let mut out =
                Vec::with_capacity(comps.iter().map(NativeName::len).sum::<usize>() + comps.len());
            for (i, component) in comps.iter().enumerate() {
                let NativeName::Unix(value) = component else {
                    unreachable!()
                };
                if i > 0 && out.last() != Some(&b'/') {
                    out.push(b'/');
                }
                out.extend_from_slice(value);
            }
            Ok(NativePath::unix(out))
        }
        PathEncoding::WindowsUtf16le => {
            let mut out =
                Vec::with_capacity(comps.iter().map(NativeName::len).sum::<usize>() + comps.len());
            for (i, component) in comps.iter().enumerate() {
                let NativeName::Windows(value) = component else {
                    unreachable!()
                };
                if i > 0 && out.last() != Some(&(b'\\' as u16)) {
                    out.push(b'\\' as u16);
                }
                out.extend_from_slice(value);
            }
            Ok(NativePath::windows(out))
        }
    }
}

/// Canonical order of two record paths: component-wise native order, a parent
/// before its descendants. Mixed encodings are not ordered.
pub fn compare_paths(a: &[NativeName], b: &[NativeName]) -> Result<Ordering, PathOrderError> {
    let a_encoding = encoding_of(a)?;
    let b_encoding = encoding_of(b)?;
    if let (Some(a), Some(b)) = (a_encoding, b_encoding)
        && a != b
    {
        return Err(mixed(a, b));
    }
    for (x, y) in a.iter().zip(b.iter()) {
        match x.cmp(y) {
            Ordering::Equal => continue,
            other => return Ok(other),
        }
    }
    Ok(a.len().cmp(&b.len()))
}

/// Compares complete native paths without silently ordering different host
/// encodings. This is used for metadata aliases, where the values are not
/// record components but must obey the same canonical-order fence.
pub fn compare_native_paths(a: &NativePath, b: &NativePath) -> Result<Ordering, PathOrderError> {
    if a.encoding() != b.encoding() {
        return Err(mixed(a.encoding(), b.encoding()));
    }
    Ok(a.cmp(b))
}

/// Splits an exact path below an exact root into components usable with
/// [`PathStack`]. Returns `None` if `path` is not inside `root`.
pub fn components_under(
    root: &NativePath,
    path: &NativePath,
) -> Result<Option<Vec<NativeName>>, PathOrderError> {
    if root.encoding() != path.encoding() {
        return Err(mixed(root.encoding(), path.encoding()));
    }
    match (root, path) {
        (NativePath::Unix(root), NativePath::Unix(path)) => Ok(split_components(root, path, b'/')
            .map(|parts| parts.into_iter().map(NativeName::unix).collect())),
        (NativePath::Windows(root), NativePath::Windows(path)) => {
            Ok(split_components(root, path, b'\\' as u16)
                .map(|parts| parts.into_iter().map(NativeName::windows).collect()))
        }
        _ => unreachable!(),
    }
}

fn split_components<T: Copy + Eq>(root: &[T], path: &[T], separator: T) -> Option<Vec<Vec<T>>> {
    let trimmed_root = trim_trailing(root, separator);
    let trimmed_path = trim_trailing(path, separator);
    let mut comps = vec![root.to_vec()];
    if trimmed_path == trimmed_root {
        return Some(comps);
    }
    let rest = if trimmed_root.is_empty() {
        trimmed_path.strip_prefix(&[separator])?
    } else {
        let r = trimmed_path.strip_prefix(trimmed_root)?;
        r.strip_prefix(&[separator])?
    };
    comps.extend(
        rest.split(|c| *c == separator)
            .filter(|c| !c.is_empty())
            .map(<[T]>::to_vec),
    );
    Some(comps)
}

fn trim_trailing<T: Copy + Eq>(p: &[T], separator: T) -> &[T] {
    let mut p = p;
    while p.last().is_some_and(|last| *last == separator) {
        let stripped = &p[..p.len() - 1];
        p = stripped;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(parts: &[&str]) -> Vec<NativeName> {
        parts
            .iter()
            .map(|s| NativeName::unix(s.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn joins_root_and_names() {
        assert_eq!(
            join_components(&v(&["/", "Users", "mars"])).unwrap(),
            NativePath::unix(b"/Users/mars".to_vec())
        );
        assert_eq!(
            join_components(&v(&["/tmp/x", "a"])).unwrap(),
            NativePath::unix(b"/tmp/x/a".to_vec())
        );
        assert_eq!(
            join_components(&v(&["/tmp/x"])).unwrap(),
            NativePath::unix(b"/tmp/x".to_vec())
        );
    }

    #[test]
    fn stack_follows_depth() {
        let mut s = PathStack::new();
        s.set(0, &NativeName::unix(b"/r".to_vec())).unwrap();
        s.set(1, &NativeName::unix(b"a".to_vec())).unwrap();
        s.set(2, &NativeName::unix(b"b".to_vec())).unwrap();
        assert_eq!(s.path().unwrap(), NativePath::unix(b"/r/a/b".to_vec()));
        s.set(1, &NativeName::unix(b"c".to_vec())).unwrap();
        assert_eq!(s.path().unwrap(), NativePath::unix(b"/r/c".to_vec()));
        assert!(s.set(3, &NativeName::unix(b"x".to_vec())).is_err());
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
        paths.sort_by(|a, b| compare_paths(a, b).unwrap());
        let joined: Vec<String> = paths
            .iter()
            .map(|p| join_components(p).unwrap().display())
            .collect();
        assert_eq!(joined, ["/", "/a", "/a/b", "/a/z", "/a-b", "/b"]);
    }

    #[test]
    fn components_under_root() {
        assert_eq!(
            components_under(
                &NativePath::unix(b"/".to_vec()),
                &NativePath::unix(b"/Users/mars/".to_vec())
            )
            .unwrap(),
            Some(v(&["/", "Users", "mars"]))
        );
        assert_eq!(
            components_under(
                &NativePath::unix(b"/data".to_vec()),
                &NativePath::unix(b"/data/x".to_vec())
            )
            .unwrap(),
            Some(v(&["/data", "x"]))
        );
        assert_eq!(
            components_under(
                &NativePath::unix(b"/data".to_vec()),
                &NativePath::unix(b"/data".to_vec())
            )
            .unwrap(),
            Some(v(&["/data"]))
        );
        assert!(
            components_under(
                &NativePath::unix(b"/data".to_vec()),
                &NativePath::unix(b"/database".to_vec())
            )
            .unwrap()
            .is_none()
        );
        assert!(
            components_under(
                &NativePath::unix(b"/data".to_vec()),
                &NativePath::unix(b"/other".to_vec())
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn mixed_encodings_are_rejected_instead_of_ordered() {
        let unix = vec![NativeName::unix(b"/".to_vec())];
        let windows = vec![NativeName::windows(vec![
            b'C' as u16,
            b':' as u16,
            b'\\' as u16,
        ])];
        assert!(matches!(
            compare_paths(&unix, &windows),
            Err(PathOrderError::MixedEncoding { .. })
        ));
        assert!(matches!(
            join_components(&[unix[0].clone(), windows[0].clone()]),
            Err(PathOrderError::MixedEncoding { .. })
        ));
    }
}
