# Frozen DiskDrift v3 reader

This is a deliberately minimal standalone compatibility reader reduced from
historical DiskDrift storage behavior at revision
`58959fe5c41f52bb3ea4a05c2d8217ff25aa9794`. It has no path dependency on the
live workspace.

No historical Rust source is copied verbatim. The compatibility implementation
in `src/main.rs` was written as a standalone reduction of these exact
historical source files and API surfaces:

```text
crates/diskdrift-storage/src/lib.rs       Store opening and format-version gate
crates/diskdrift-storage/src/store.rs     snapshot listing, base links, and data-file selection
crates/diskdrift-storage/src/meta.rs      SnapshotMeta/StorageInfo JSON fields and status
crates/diskdrift-storage/src/container.rs DDS1 framing, index, block CRC, and zstd validation
crates/diskdrift-storage/src/codec.rs     varint path encoding used by the DDS1 index
```

The fixture files themselves are:

```text
Cargo.toml             standalone dependency manifest
Cargo.lock             standalone lockfile
src/main.rs            newly written compatibility code, not a copied source file
README.md              provenance and behavior contract
checksums.sha256       checksums for the four files above except this manifest
```

The reader accepts store markers through version 3, lists committed snapshot
metadata, and rejects a newer marker before reading metadata. Its
`snapshots verify <id>` command follows the selected snapshot's delta bases and
checks each DDS1 container, index checksum, compressed block checksum, and
zstd payload. It does not implement the full DiskDrift command surface or
record model. Its normalization contract is to emit parsed snapshot JSON as a
compact JSON array in file name order. `checksums.sha256` records hashes for
the four fixture files other than the checksum manifest itself.
