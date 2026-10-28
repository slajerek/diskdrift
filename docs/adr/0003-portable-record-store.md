# ADR 0003: Portable records, paths and store version 4

- Status: **accepted; wire layout provisional until the format-freeze gate**
  (2026-10-26). No DDS2 writer may ship before that gate appends golden bytes
  and benchmark results to this ADR.
- Scope: the portable record/store contract used by baseline and later Linux
  and Windows diagnostics.
- Related decisions: ADR 0001 (binary reverse-delta store) and ADR 0002 (disk
  registry and lineage).

## Context

The repository already separates core algorithms, platform integration,
diagnostics, storage, application services, CLI and TUI. Its canonical record
stream and reverse-delta store are suitable for all supported systems.

The contracts crossing those boundaries are not portable. Names and paths are
Unix bytes joined with `/`; file identity is a Unix `(dev, ino)` shape;
missing record fields are sometimes represented by zero; mount, filesystem,
volume and capacity identity are conflated; and storage uses POSIX positional
I/O, permissions, locking and rename durability directly. Windows cannot be
added losslessly with local `cfg` blocks, while Linux bind mounts, overlay and
shared-storage stacks already expose semantic errors in the current model.

The format decision must precede broad Linux and Windows diagnostics. Adding
platform modules first would either lose data or require a second breaking
format change after users have accumulated cross-platform history.

## Decision

### Keep the architecture, replace the cross-platform contract

Keep the seven crate boundaries, bounded walker, canonical streams,
diff/delta/top/browse algorithms, shared app services and concrete binary
store. Platform selection remains static at compile time. Do not add a plugin
system or a trait over the whole application.

Use lossless tagged native names and roots:

- macOS and Linux names are arbitrary byte strings;
- Windows names are arbitrary UTF-16 code-unit sequences, including unpaired
  surrogates;
- roots distinguish POSIX, drive, UNC and volume-GUID forms; and
- canonical order is component-wise, parent first, with no normalization, case
  folding or locale: Unix compares unsigned byte values; Windows compares
  numeric `u16` values, not their little-endian serialization bytes.

Core does not split or join persistent paths with a platform separator.
Conversion to `PathBuf` happens only in the adapter for the current host.
Search may build a normalized derived key, but exact identity remains native.
CLI/config/JSON serialization is an application-interface concern; it must
round-trip these exact values but is not part of the DDS2 wire format.

File identity is 128 bits and scoped by a volume/file-ID scheme. Hardlink
deduplication and definite move detection require a stable `(volume, file_id)`.
When identity is absent or weak, results report unknown hardlink coverage or a
probable move rather than silently asserting identity.

Records use compact availability bits for metadata fields. Snapshot/report
DTOs carry richer quality, reason, scope and provenance. An absent value is
never converted to observed zero. Mounts, filesystems/volumes, capacity
domains, pools/containers and managed stores are distinct graph nodes; shared
domains are related, not summed.

### Store v4, metadata v3 and DDS2

Introduce one format transition:

- `STORE_VERSION = 4`;
- `META_FORMAT_VERSION = 3`; and
- `DISKS_VERSION = 2`; and
- `DDS2` data files whose header records path encoding and record schema once
  per stream.

DDS2 retains compressed blocks, indexes, checksums, canonical order and
reverse-delta history. It adds encoding-aware names, field availability,
128-bit file IDs and schema-selected timestamp precision. Encoding is not
tagged on every record, protecting the footprint budget.

#### DDS2 envelope and schema registry

The fixed header is eight bytes:

| Byte(s) | Field | Schema-1 rule |
|---|---|---|
| 0..3 | magic | ASCII `DDS2` |
| 4 | file kind | `1` delta operations, `2` records; reject other values |
| 5 | path encoding | `1` Unix bytes, `2` raw UTF-16LE code units; reject `0` and unknown values |
| 6 | record schema | `1`; reject unknown schemas unless a later reader defines them |
| 7 | stream flags | `0`; schema 1 rejects every unknown/nonzero bit |

Record schema 1 contains depth, name length in encoding units, kind, record
flags, a `u16` availability bitset, logical and allocated size, mtime/btime
seconds and nanoseconds, file ID low/high, link count and volume index. Its
availability bits are:

| Bit | Field | Dependency |
|---:|---|---|
| 0 | kind | none |
| 1 | logical size | none |
| 2 | allocated size | none |
| 3 | mtime seconds | none |
| 4 | btime seconds | none |
| 5 | file ID | a valid volume descriptor |
| 6 | link count | none |
| 7 | mtime nanoseconds | bit 3 |
| 8 | btime nanoseconds | bit 4 |

Bits 9..15 are zero and rejected by a schema-1 reader. An unset bit makes its
compact payload semantically absent even when the stored value is zero. UTF-16
name lengths count code units and payloads are raw little-endian `u16`, not
Unicode scalar values.

The DDS1 28-byte `DDSE` trailer, its CRC coverage and block-index fields remain
unchanged. Block context and index `first_path` use the stream's path encoding.
All order checks and binary searches decode components and use unsigned-byte
ordering for Unix or numeric-`u16` ordering for Windows; they never compare
UTF-16LE payload bytes. Delta files use the same path encoding and record schema
as their lineage. A reader rejects a delta/base edge whose descriptors differ.

The format-freeze gate appends exact schema-1 column transforms and golden byte
fixtures to this ADR after Windows names/file IDs and publication behavior are
validated and footprint/RSS benchmarks pass. No DDS2 writer ships before that
gate.
Future incompatible layouts receive a new record-schema number and never
reinterpret schema 1.

#### Metadata v3 volume table and registry v2

Metadata v3 contains an ordered volume table; each record's `vol: u16` indexes
it. Each descriptor stores:

- the index and platform;
- exact volume/filesystem identity and its strength;
- the file-ID scheme and scope;
- the mount key or alias evidence used during the scan;
- allocation semantics and sharing caveats;
- timestamp resolution; and
- field/capability provenance needed to interpret records.

At minimum the file-ID schemes are `posix_inode64`,
`windows_file_id128` and `unavailable`. DDS1 inode values migrate to
`posix_inode64` with `high = 0`; DDS2 macOS/Linux uses the same scheme when its
adapter supplies a Unix inode, so the first DDS1/DDS2 diff keeps definite move
and hardlink evidence when the volume scope also matches. Windows IDs use
`windows_file_id128`. An unresolved `vol` index is corruption, not unknown.

Registry v2 identities are platform-qualified and name their scheme, strength
and source. Mount aliases and last mount point use exact tagged native paths.
Windows uses volume GUID identity locally; Linux records filesystem UUID/device
evidence and namespace scope; macOS keeps ADR 0002 identity. Display strings
are never exact identity. Legacy weak or lossy identity is strengthened only
through the existing same/new/merge confirmation flow.

#### DDS1 interpretation

DDS1 is decoded into the portable in-memory record before comparison. The
legacy mapping is fixed:

| DDS1 condition | Portable availability |
|---|---|
| `STAT_ERROR` set | kind, logical size, allocated size, mtime, btime, file ID and link count unavailable; stored zero/default payloads are ignored |
| no `STAT_ERROR` | kind, logical size, allocated size, mtime and link count observed |
| `btime == 0` | btime seconds and nanoseconds unavailable, because DDS1 defined zero as unknown |
| `btime != 0` and no `STAT_ERROR` | btime seconds observed; btime nanoseconds unavailable |
| `ino == 0` | file ID unavailable, because DDS1 cannot distinguish a real zero from missing identity |
| `ino != 0` and no `STAT_ERROR` | `posix_inode64`, low = inode, high = 0 |
| every DDS1 timestamp | one-second resolution; nanoseconds unavailable |
| valid `vol` index | volume observed through the migrated metadata-v3 descriptor |

Other DDS1 record flags retain their existing meanings. A zero size, allocation,
mtime or link count without `STAT_ERROR` remains an observed value; only the
documented sentinels above become unavailable.

#### Comparison rules

Path encoding, canonical component order, unknown record schema and
incompatible volume-identity namespace are hard fences that
`--allow-incompatible` cannot override and that cannot share a delta chain.
Different roots, exclusions and boundary options retain the existing override.

`posix_inode64` is compatible across DDS1 and DDS2 when volume scope matches.
Other file-ID scheme mismatches disable definite move and cross-snapshot
identity conclusions. Timestamps compare at the coarser common resolution: the
two instants are quantized with Euclidean division by the larger resolution
before equality is tested. Thus DDS1 seconds versus DDS2 nanoseconds does not
mark an unchanged file modified. Only fields available on both sides
participate in content equality; an availability change is reported as metadata
coverage, not silently as a content modification.

Different allocation semantics make allocated-byte deltas non-equivalent, but
do not prevent path/logical-size comparison. Diff, delta, rebase, remove and
compact enforce the same descriptor rules; rebase never creates a delta across
a hard fence.

#### Migration and commit point

The reader accepts DDS1 and DDS2. Store versions 1-4 open read-only without
writes. Migration is idempotent and does not rewrite DDS1 data in bulk.

To keep an interrupted migration readable by old builds, v4 is staged outside
the v1-v3 namespace:

1. Under the writer lock, create or resume `snapshots-v4/` metadata and
   `disks-v2.json`. Metadata v3 `dataFile` is store-relative, so it can point to
   an unchanged `snapshots/<legacy>.dds`; newly rewritten data lives under
   `snapshots-v4/`.
2. Build and verify the complete metadata-v3 and registry-v2 view. Files in
   `snapshots-v4/` and `disks-v2.json` are ignored by old readers while
   `store.json` still says version 1-3.
3. Flush staged files and directories to the degree supported by `fsops`.
4. Commit by one same-directory atomic replacement of `store.json` with a v4
   marker naming `snapshots-v4/` and `disks-v2.json`.

The committed marker is:

```json
{"format":"diskdrift-snapshots","version":4,"metadataDir":"snapshots-v4","registry":"disks-v2.json"}
```

Version 4 metadata paths are relative to the store root; they must remain below
it after lexical normalization. This permits legacy data in `snapshots/` and
new data in `snapshots-v4/` without duplicating either.

Before step 4, interruption leaves the old marker and complete old view; a new
build validates or rebuilds staging and resumes. After step 4, old builds reject
version 4 clearly and new builds see the complete v4 view. There is no metadata
generation mechanism. Normal snapshot publication remains unique temporary
data, flushed immutable data, then atomic metadata replacement. Recovery may
delete only unreferenced DiskDrift staging/data under the writer lock.

Legacy conversion rules are:

- store v1/meta v1 derives an exact Unix root from the DDS1 root record when
  data exists; otherwise the JSON root remains `legacy_string` exactness, and
  disk identity is weak and platform-qualified;
- store v2/meta v1 follows the same rule and retains timestamp stems;
- store v3/meta v2/registry v1 preserves lineage, marks established macOS
  identity or `legacy_unix`, and treats registry mount strings as display-only
  until the volume is seen again; and
- mixed v4 validates every stream descriptor and delta/base edge before use.

Golden migrations and injected interruption tests cover v1, v2, v3, registry
v1, both sides of the marker commit and every mixed DDS1/DDS2 operation.

### Platform storage operations

Add a private `storage::fsops::{unix,windows}` module for positional reads,
exclusive writer locks, file flushing and publication, parent durability where
available, private directory creation, same-file checks and allocated size.
The public store remains concrete.

Windows uses delete-sharing handles, `LockFileEx`, `FlushFileBuffers` and
same-volume publication with recovery. Because Windows does not offer the same
parent-directory durability primitive as POSIX, the guarantee is defined by
unique temporary names, flushed immutable data, atomic marker/metadata
replacement where the API succeeds, checksums, startup recovery and VM fault
evidence, not by claiming POSIX-equivalent `fsync` behavior.

## Alternatives considered

### Preserve DDS1 and add `cfg` blocks

Rejected as the architecture. It cannot losslessly preserve all Windows names,
does not represent 128-bit IDs or field availability, and would require a later
second format break. It remains acceptable only as a disposable compile spike.

### Runtime provider/plugin framework

Rejected. Trait objects and a broad provider API would enter hot paths, enlarge
the public contract and complicate the read-only audit before concrete platform
semantics are known. Static modules and the existing small stream/lister
contracts are sufficient.

### Rewrite every old snapshot during migration

Rejected. It creates unnecessary write amplification and temporary free-space
risk on machines where DiskDrift is used specifically because storage is low.
Mixed DDS1/DDS2 history is simpler and keeps read-only opening side-effect free.

## Consequences

- Phase 1 must change core, storage, app DTOs and TUI navigation before broad
  platform diagnostics begin.
- Snapshot format compatibility, footprint and peak RSS need benchmarks before
  DDS2 is frozen.
- A snapshot from another OS can be listed, verified, browsed, searched and
  analyzed offline; live operations are capability-gated by host/path encoding.
- Platform-specific measurements may expose different fields, but all use the
  same domain/scope/quality/provenance contract.
- Existing recorded macOS behavior and performance budgets remain regression
  gates.
- Linux and Windows establish native per-filesystem baselines before receiving
  fixed performance thresholds.
- The migration and every platform adapter remain subject to the repository's
  read-only and no-false-precision rules.
