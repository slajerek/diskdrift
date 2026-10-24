# DiskDrift

DiskDrift is a fast, read-only disk-space investigation tool for macOS (APFS), with a CLI and a full-screen terminal UI.
It tells you where your free space went: it reads what the OS reports (purgeable space, local snapshots), measures the usual suspects, and compares saved metadata scans to show exactly what grew.
Work in progress — the development code lives in the [`devel`](../../tree/devel) branch.
