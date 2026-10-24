# ADR 0001 — Snapshot storage engine

- Status: **accepted** (2026-10-21, M1)
- Deciders: implementation per spec section 7; measurements in
  [`docs/benchmarks/2026-10-21-full-report.md`](../benchmarks/2026-10-21-full-report.md)
  (raw: `2026-10-21-full-results.json`, `2026-10-21-production-history.json`)
- Machine: Mac14,6 (Apple M2 Max, 12 cores, 64 GB), macOS 15.6.1, APFS on the
  internal Apple SSD (encrypted Data volume), rustc 1.99.1.

## Decision

1. **Binary format, one immutable file per snapshot**, records stored
   **column by column in zstd (level 3) blocks of 256 KiB** with value transforms
   (allocated size predicted from logical size; inode, mtime and depth as deltas;
   birth time relative to mtime). Each block carries the path context of its first
   record (blocks decode independently; prefix queries seek) and a CRC32; the file
   ends with a block index and a trailer.
2. **History: newest snapshot full, older snapshots as reverse deltas.** When a
   scan commits, the previous comparable snapshot is rewritten as the delta that
   turns the new snapshot back into it. An older snapshot stays a full *keyframe*
   whenever converting it would make any reconstruction chain longer than 30
   steps, or when the delta would be more than half its full size.
3. **Durability:** temp file → fsync → rename → fsync directory, then the JSON
   metadata (same pattern). A single writer is enforced with `flock`; readers
   never lock. Files no metadata refers to (interrupted rebase or removal) and
   IN_PROGRESS metadata without a live writer are cleaned up / marked FAILED when
   the next writer takes the lock. `snapshots remove` folds dependents (rewrites
   them against the removed snapshot's base, or as full) so history never breaks.

No SQLite, no LMDB. The `SnapshotStore` abstraction from spec section 7 is
`diskdrift_storage::Store` (streaming writer with commit/abort, metadata
enumeration, ordered record streams, delta reconstruction); diff, top and delta
logic stay in `diskdrift-core` and see only canonical record streams.

## Candidates and method

Six variants behind one interface (`benches/diskdrift-bench`), all storing the
same canonical record stream (depth-first, children sorted by name, each record
holding only its own name — full paths are never stored):

| variant | storage model |
|---|---|
| `binary` | row-encoded varint records in zstd blocks, file per snapshot |
| `binary-columnar` | the same container, columnar blocks with value transforms (chosen) |
| `sqlite-rows` | one row per entry, typed columns, `WITHOUT ROWID` PK `(snap, seq)`, WAL |
| `sqlite-indexed` | rows + indexes on `(snap, alloc)` and `(snap, parent, name)` |
| `sqlite-blocks` | SQLite as a container for the `binary` row blocks |
| `lmdb` (heed) | one key/value per entry, key `(snap, seq)`, `MDB_APPEND` |

Workloads (spec 7.1): synthetic trees of 100k / 1M / 5M / 10M entries (5 trials up
to 1M, 3 above; median and nearest-rank p95), subsequent snapshots at 0 / 0.1 / 1 /
10 / 50 % change, 100-snapshot histories at 1M entries with ~1 % change per
snapshot under several delta strategies, SIGKILL during ingest, write failure
under `RLIMIT_FSIZE` (a stand-in for a full disk), and scanner plus end-to-end runs
on three real trees (`/System/Library` 0.43M, `/Applications` 1.24M,
`~/Library` 1.73M entries). Every operation ran in a fresh child process; peak RSS
is that process's `ru_maxrss`. Filesystem cache state could not be controlled
(no root); reads follow writes (warm).

## Results (medians)

**One snapshot, 10M synthetic entries**

| variant | import | full read | footprint | B/entry | import RSS |
|---|---:|---:|---:|---:|---:|
| binary-columnar | **3.16 s** | **0.77 s** | **289 MB** | **28.8** | 46 MB |
| binary | 3.84 s | 1.06 s | 358 MB | 35.6 | 43 MB |
| sqlite-blocks | 4.53 s | 1.24 s | 352 MB | 35.1 | 123 MB |
| lmdb | 3.25 s | 1.09 s | 638 MB | 63.5 | **697 MB** |
| sqlite-rows | 10.3 s | 2.60 s | 671 MB | 66.8 | 130 MB |
| sqlite-indexed | 21.5 s | 2.62 s | 1107 MB | 110.2 | 149 MB |

Synthetic names are random strings and compress far worse than real ones. On
**real trees, end to end (scan + store)**: binary-columnar **5.9 B/entry**
(`/Applications`), 3.9 (`/System/Library`), 16.9 (`~/Library`, cache files with
hash names); binary rows 10.1 / 7.2 / 21.2; sqlite-blocks ≈ binary rows; LMDB and
SQLite rows 65–81 B/entry. A full snapshot of this Mac's ~9M-entry Data volume is
therefore in the order of 50–150 MB with the chosen format, versus 0.6–0.7 GB per
snapshot with row-per-entry engines.

Queries at 10M: stream top-100 files+dirs 0.79 s (binary-columnar) vs 2.8 s
(SQLite rows); SQLite's index answers top-100 in 1 ms but costs +65 % footprint
and 2× import time; prefix query (≈1 % subtree) 10 ms via the block index vs
1.3 s for a SQLite rows scan (29 ms with its path index).

**Subsequent snapshot / diff at 1M** (any variant, dominated by the shared merge
code): a delta costs a full comparison (0.36–0.6 s) and is 0.4 MB at 1 % change,
3 MB at 10 %, 13 MB at 50 % for the block formats (SQLite rows ~2×). Full diff of
two 1M snapshots 0.13 s (binary formats) to 0.63 s (SQLite rows).

**History of 100 snapshots at 1M entries, ~1 % change each**, footprint relative
to one full snapshot:

| strategy (binary-columnar) | after 10 | after 30 | after 100 | adjacent diff | oldest read |
|---|---:|---:|---:|---:|---:|
| full every time | 10.0× | 30.1× | 100.8× | 0.13 s | 0.08 s |
| forward deltas, checkpoint every 10 | 1.27× | 3.81× | 12.7× | 0.30 s | 0.10 s |
| forward deltas, checkpoint every 30 | 1.27× | 2.78× | 9.6× | 0.30 s | 0.10 s |
| chained deltas, checkpoint every 10 | 1.14× | 3.42× | 11.4× | 1.23 s | 0.62 s |
| forward, new checkpoint when delta > 25 % | 1.27× | 2.78× | 13.8× | — | — |
| **reverse deltas, keyframe ≤ 30 (production code)** | **1.14×** | **1.46×** | **5.5×** | **0.19 s** | 1.9 s |

Forward cumulative deltas re-store the same changes in every delta; checkpoints
then dominate. Reverse deltas store each change once and keep the snapshot people
compare most (the newest) one step away. Cost: each scan also rewrites the
previous snapshot (median write step 0.44 s per 1M entries including the
rebase); reading the oldest snapshot of a 30-step chain takes 1.9 s at 1M.
Measured with the shipping store (`examples/production_history.rs`); peak RSS
185 MB including the 30-step distant diff.

**Crash safety and write failure:** all six variants kept snapshot 0 intact
record-for-record after SIGKILL mid-import and after EFBIG mid-write, never listed
the interrupted snapshot as complete, and accepted further snapshots. (In the
SQLite variants the size limit was hit by the WAL checkpoint *after* the commit,
so snapshot 1 was complete and listed — consistent, just not a failure of the
snapshot itself. The binary store leaves only a temp file that recovery deletes.) The production
store is additionally tested for crash leftovers, orphan files, truncation and
bit flips (`crates/diskdrift-storage`), and end-to-end for a write failure under
`ulimit -f` (`crates/diskdrift-cli/tests/snapshots.rs`).

## Why

- **Footprint (priority 2):** the columnar format is 2.3× smaller than SQLite
  rows and LMDB on synthetic data and 11–14× smaller on real trees; reverse
  deltas keep 30 snapshots at 1.46× one snapshot.
- **Speed (priority 1):** fastest import and read of all candidates; storage never
  limits a scan (end-to-end equals scanner-only time on every real tree).
- **Memory:** constant per stream; LMDB needs ~70 bytes of RSS per entry for a
  single-transaction import (697 MB at 10M), which alone breaks the 256 MB budget.
- **Simplicity of the required queries:** everything DiskDrift needs (full
  ordered stream, merge-diff, top-N, prefix) is a sequential pass. SQL's
  flexibility buys a 1 ms top-N at a large cost in size and import time; nothing in
  the spec needs ad-hoc queries.
- `sqlite-blocks` would be the fallback (same footprint, SQLite's atomicity), but
  it is slower to read (1.6×), uses 2–3× the RSS and adds a large C dependency
  for transactional guarantees the immutable-file design gets from rename.

## Spec 2A budgets (reference 3M entries; measured or scaled linearly)

| budget | target | result | status |
|---|---|---|---|
| full scan, warm | ≤ 60 s / 3M (≥ 50k/s) | 381k/s `/Applications` (8 threads); 62k/s `~/Library` (one 157k-subdirectory directory serializes listing) | met |
| full scan, cold | ≤ 3× warm | first traversal 365k/s vs 381k/s warm (`/Applications`) | met (cache state uncontrolled) |
| ingest overhead | ≤ 1.3× scanner-only | 1.0× on all real trees | met |
| peak RSS during scan | < 256 MB | 225 MB for the whole disk (`diskdrift scan /`, 8.72M entries, below); 199 MB for `~/Library` (1.73M); was 260 MB on `~/Library` before bounding wide-directory queues and buffers | met, little headroom |
| adjacent diff, 3M | ≤ 5 s | 0.19 s per 1M (newest pair) → ≈ 0.6 s | met |
| `top`, 3M | ≤ 2 s | 79 ms per 1M → ≈ 0.25 s | met |
| one snapshot | ≤ 40 B/entry | 28.8 synthetic, 3.9–16.9 real | met |
| 30 snapshots at ~1 % | ≤ 3× one snapshot | 1.46× | met |
| no idle growth | no WAL/journal left | immutable files; nothing grows after commit | met |

## First full-disk scan (acceptance run)

`diskdrift scan / --name first-baseline` on this Mac (release build, Full Disk
Access not granted, store inside the project at `.diskdrift/data`):
**8,719,478 entries in 82 s** (106k entries/s; 7.05M files, 1.51M directories),
3.5 TB allocated tracked, 583 unreadable (root-only/TCC) directories → PARTIAL,
13 other filesystems skipped, 85,531 extra hardlink paths counted once,
**stored in 84.9 MB (9.7 B/entry)**, peak RSS 225 MB. `/System/Volumes/Data`
contributes only its 33 non-firmlinked entries (no double counting). A future
follow-up scan will add the full new snapshot and turn this one into a delta.

## Scanner findings (spec 5)

- `getattrlistbulk` vs `readdir`+`lstat`: 1.6× faster single-threaded (92k/s vs
  63k/s on `/Applications`), 1.55× at 8 threads (381k/s vs 245k/s). Requested
  attributes are free: names-only enumeration costs the same.
- Single-threaded throughput (~100k entries/s) matches `du`; the parallel walker
  is ~3.9× faster than `du` on the Data volume.
- APFS throughput peaks at 6–8 threads and drops beyond (12–16 threads slower);
  the default is now `min(cores, 8)`. On the sealed System volume more than 2–4
  threads is slower still (signature verification), but whole-disk scans are
  dominated by the Data volume.
- A directory with 157k subdirectories (`ImageConversionService/tmp`) made the
  walker queue all of them at once (157k pending tasks, 222 MB). Child directories
  are now queued in windows of 512 ahead of the consumer, listing buffers of huge
  directories are released, and the read-ahead budget is 100k entries: 151 MB at
  8 threads, same speed.
- One run lost a subtree to a transient listing error; listings are retried once
  for errors other than EACCES/ENOENT/ENOTDIR.
- macOS stats both volumes of the System+Data group with the same `st_dev`; the
  group is detected from the mount table and firmlinks (spec section 5 updated).
- The system allocator does not reuse memory freed by a different thread than
  the one that allocated it (records are decoded on reader threads): RSS grew
  ~10 MB per snapshot written in one process. The binary uses mimalloc (flat at
  ~110 MB over 100 snapshots); delta streams decode on the caller's thread so a
  30-step chain does not create 30 threads.

## Consequences

- We own a file format: it is versioned (`DDS1` magic + kind byte; metadata
  `formatVersion`; `store.json` version), checksummed per block and per index, and
  `snapshots verify` re-reads everything. Changing it needs a migration path.
- Each scan pays a rebase (≈0.4 s per million entries) for the space savings;
  failures there leave the previous snapshot full and valid and are reported.
- Old snapshots deep in a chain are slower to read (≤ 30 steps); diffs across
  long spans of history are the rare case and stay within seconds per million
  entries.
- Ad-hoc SQL over snapshots is not available; JSON output covers scripting.
