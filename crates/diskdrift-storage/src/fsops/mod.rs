//! Small, platform-specific filesystem operations used by the store.
//!
//! The store deliberately keeps this boundary narrow.  In particular, a read-only
//! probe never creates a DiskDrift-owned file; only the writer lock path may do so.
//! Windows publication is atomic only when the native replacement API succeeds.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

pub(crate) use platform::Lock;

pub(crate) fn open_read_shared(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    platform::open_read_shared(path)
}

pub(crate) fn read_exact_at(
    file: &std::fs::File,
    buf: &mut [u8],
    offset: u64,
) -> std::io::Result<()> {
    platform::read_exact_at(file, buf, offset)
}

/// Tries to take the writer lock, creating its file when it does not exist.
pub(crate) fn try_lock_exclusive(path: &std::path::Path) -> std::io::Result<Option<Lock>> {
    platform::try_lock_exclusive(path)
}

/// Checks an existing lock without creating, removing, or repairing it.
pub(crate) fn probe_lock(path: &std::path::Path) -> std::io::Result<bool> {
    platform::probe_lock(path)
}

pub(crate) fn flush_file(file: &std::fs::File) -> std::io::Result<()> {
    failure_step()?;
    platform::flush_file(file)
}

/// Flushes a directory where the host provides a directory durability primitive.
pub(crate) fn sync_dir(path: &std::path::Path) -> std::io::Result<()> {
    failure_step()?;
    platform::sync_dir(path)
}

/// Flushes the parent directory of `path` where the host provides that primitive.
pub(crate) fn sync_parent(path: &std::path::Path) -> std::io::Result<()> {
    failure_step()?;
    platform::sync_parent(path)
}

/// Replaces `destination` with `temporary` using a native atomic operation.
/// There is intentionally no copy/delete fallback.
pub(crate) fn replace_atomic(
    temporary: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    failure_step()?;
    #[cfg(any(test, feature = "test-seams"))]
    if replacement_is_forced_unsupported(destination) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "atomic replacement is unavailable",
        ));
    }
    platform::replace_atomic(temporary, destination)
}

#[cfg(test)]
pub(crate) fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    platform::same_file(a, b)
}

pub(crate) fn create_private_dir(path: &std::path::Path) -> std::io::Result<()> {
    failure_step()?;
    platform::create_private_dir(path)
}

#[cfg(any(test, feature = "test-seams"))]
pub(crate) fn failure_step() -> std::io::Result<()> {
    FAILURE_STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.done += 1;
        if state.after == Some(state.done) {
            return Err(std::io::Error::other(format!(
                "injected fsops failure at operation {}",
                state.done
            )));
        }
        Ok(())
    })
}

#[cfg(not(any(test, feature = "test-seams")))]
pub(crate) fn failure_step() -> std::io::Result<()> {
    Ok(())
}

pub(crate) fn allocated_size(path: &std::path::Path) -> u64 {
    platform::allocated_size(path)
}

/// Test-only seam for a filesystem that cannot provide atomic replacement.
/// The returned guard restores the previous setting when dropped.
#[doc(hidden)]
#[cfg(any(test, feature = "test-seams"))]
pub(crate) fn force_unsupported_replacement_for(
    destination: &std::path::Path,
) -> ReplacementSupportGuard {
    force_unsupported_replacement(destination, usize::MAX)
}

/// Test-only seam that fails one matching replacement, then allows recovery to
/// publish its failure metadata through the same boundary.
#[doc(hidden)]
#[cfg(feature = "test-seams")]
pub(crate) fn force_unsupported_replacement_once_for(
    destination: &std::path::Path,
) -> ReplacementSupportGuard {
    force_unsupported_replacement(destination, 1)
}

#[cfg(any(test, feature = "test-seams"))]
fn force_unsupported_replacement(
    destination: &std::path::Path,
    remaining: usize,
) -> ReplacementSupportGuard {
    let previous = FORCE_UNSUPPORTED_REPLACEMENT.with(|path| {
        path.replace(Some(ReplacementFailure {
            destination: destination.to_path_buf(),
            remaining,
        }))
    });
    ReplacementSupportGuard { previous }
}

/// Test-only seam for failing one deterministic filesystem boundary.
#[doc(hidden)]
#[cfg(feature = "test-seams")]
pub(crate) fn fail_after(step: usize) -> FailureGuard {
    let previous = FAILURE_STATE.with(|state| {
        state.replace(FailureState {
            after: Some(step),
            done: 0,
        })
    });
    FailureGuard { previous }
}

#[cfg(any(test, feature = "test-seams"))]
#[derive(Clone, Copy)]
struct FailureState {
    after: Option<usize>,
    done: usize,
}

#[cfg(feature = "test-seams")]
pub(crate) struct FailureGuard {
    previous: FailureState,
}

#[cfg(feature = "test-seams")]
impl Drop for FailureGuard {
    fn drop(&mut self) {
        FAILURE_STATE.with(|state| {
            state.replace(self.previous);
        });
    }
}

#[doc(hidden)]
#[cfg(any(test, feature = "test-seams"))]
pub(crate) struct ReplacementSupportGuard {
    previous: Option<ReplacementFailure>,
}

#[cfg(any(test, feature = "test-seams"))]
struct ReplacementFailure {
    destination: std::path::PathBuf,
    remaining: usize,
}

#[cfg(any(test, feature = "test-seams"))]
impl Drop for ReplacementSupportGuard {
    fn drop(&mut self) {
        FORCE_UNSUPPORTED_REPLACEMENT.with(|path| {
            let _ = path.replace(self.previous.take());
        });
    }
}

thread_local! {
    #[cfg(any(test, feature = "test-seams"))]
    static FORCE_UNSUPPORTED_REPLACEMENT: std::cell::RefCell<Option<ReplacementFailure>> = const { std::cell::RefCell::new(None) };
    #[cfg(any(test, feature = "test-seams"))]
    static FAILURE_STATE: std::cell::RefCell<FailureState> = const { std::cell::RefCell::new(FailureState { after: None, done: 0 }) };
}

#[cfg(any(test, feature = "test-seams"))]
fn replacement_is_forced_unsupported(destination: &std::path::Path) -> bool {
    FORCE_UNSUPPORTED_REPLACEMENT.with(|path| {
        let mut path = path.borrow_mut();
        let Some(forced) = path.as_mut() else {
            return false;
        };
        if forced.destination != destination || forced.remaining == 0 {
            return false;
        }
        forced.remaining -= 1;
        true
    })
}

#[cfg(test)]
mod tests;
