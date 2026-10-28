//! DiskDrift core: data model, scanner, diff and accounting. Platform-independent.

pub mod delta;
pub mod browse;
pub mod diff;
pub mod paths;
pub mod pathser;
pub mod record;
pub mod top;
pub mod units;
pub mod walk;

#[cfg(any(test, feature = "testutil"))]
pub mod testutil;
