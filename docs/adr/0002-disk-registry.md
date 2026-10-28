# ADR 0002: Disk registry and disk identity in snapshots

- Status: **accepted** (2026-10-22, the owner's decisions in section 6). Phases 1 (spec revision 0.1.5), 2 (platform `disk_info`), 3 (storage `disks` registry), 4 (metadata and comparability), 5 (store version 3 and migration), 6 (app and CLI), 7 (TUI My disks) and 8 (benchmarks) done: **implemented**.
- Roadmap: step 3 (`ROADMAP.md` 3.1a, 3.1b), the foundation for steps 4–6
  (multiple disks, search, careful scan).
- Inputs: ADR 0001 (store format), `docs/research/search-index.md` (search
  needs a lineage key, not a snapshot format change),
  `docs/research/careful-scan.md` (careful mode mounts at its own mount point).

## 1. Problem

Today a snapshot knows *where* it was scanned, but not *which disk* it was:

- **Identity is the root path.** `SnapshotMeta::incompatibility` (meta.rs)
  requires `root` to be equal. `VolumeIdentity.uuid` is compared only when
  both sides have one; with either missing, scans are treated as the same
  volume. `rebase_previous` (store.rs) and the app's `comparable` /
  `previous_comparable` / `latest_pair` all build on this rule.
- **External disks break this.** The same USB disk mounts as `/Volumes/Backup`,
  next time as `/Volumes/Backup 1` (a name clash), or under DiskDrift's own
  mount point in careful mode. Two different disks named `Backup` both mount at
  `/Volumes/Backup`. The first case is treated as a different disk; the second,
  if either lacks a UUID, as the same disk.
- **The root path is inside the data.** The root record's `name` is the full
  root path (`record.rs`: "For the root record this is the full root path").
  So even with a relaxed identity rule, a diff or reverse delta between two
  mounts of one disk would see the root itself as changed.
- **The disk exists nowhere but in snapshot metadata.** There is nowhere to
  keep a label (`backupy-2014`), a description, a set, or "this disk is not
  connected", and nothing survives once its last snapshot is removed.
- **File names say nothing about the disk.** `snapshots/261021-121552.json` is
  the scan start only.

## 2. Decision (proposed)

### 2.1 The unit is a volume, grouped by device

A **disk** in DiskDrift is one *volume* (one filesystem). That is what is
scanned, what has a UUID, and what a label names. Volumes on the same physical
device (an external drive with two partitions, the APFS volumes of one
container) are grouped by **device information** for display and recognition:
model, vendor, bus, media size, and the serial number when IOKit gives one. The
device is never the identity.

The macOS system disk is one disk. The System and Data volume group is scanned
as one tree today; the disk ID is the Data volume's UUID, the volume that holds
user data and is already recorded for `/`.

### 2.2 Disk ID

| Source | Strength | When |
|---|---|---|
| Volume UUID from Disk Arbitration (`kDADiskDescriptionVolumeUUIDKey`) | `uuid` | APFS, HFS+, and others macOS reports one for |
| The UUID Disk Arbitration derives from the FAT/exFAT volume serial (a version 3 UUID) | `serial` | FAT32, exFAT |
| Filesystem type + volume name + capacity | `weak` | no UUID (network volumes, some NTFS) |

- `weak` identities are never merged with an existing disk silently: the user
  confirms.
- **Where the UUID comes from (verified 2026-10-22, phase 2):**
  - Use **Disk Arbitration's** UUID, not CoreFoundation's
    `kCFURLVolumeUUIDStringKey`, which scans record today. On a FAT32 image,
    CoreFoundation returned a different, random (version 4) UUID on a second
    mount. Disk Arbitration and `diskutil` returned the same serial-derived
    UUID both times. exFAT, HFS+ and APFS were stable in both.
  - For `/`, describe `/System/Volumes/Data`. Disk Arbitration's UUID for `/`
    is the sealed system snapshot's, which changes with every macOS update.
    CoreFoundation already gave the Data volume's UUID for `/`.
  - Consequence for migration: a FAT volume's `volume.uuid` in existing
    metadata may be one of those random values. So the migration treats
    `msdos` and `exfat` snapshots as `weak` (one identity per distinct fs, name
    and mount point) until the disk is scanned again and its serial-derived
    UUID is known. The user can then merge the histories.
- The stored key is the source string. Its **short ID**, `d` followed by the
  first 8 hex digits of SHA-256 of `<strength>:<source>` (e.g. `d1a2b3c4`), is
  used in file names and the CLI. On a collision the registry extends it to 12
  digits.
- **Clones:** a `dd` copy of a disk has the same UUID on a different device.
  When a known UUID appears with a different device serial, DiskDrift says "this
  looks like a copy of backupy-2014" and lets the user keep one disk or split
  them (a new ID, `uuid` plus device serial). This matters for archives,
  because people clone old disks.
- **Reformat:** a reformatted disk has a new UUID, so it is a new disk. The
  user can give it the old label; histories stay separate, since the content
  is unrelated.

### 2.3 Registry in the store

`<store>/disks.json`, versioned, written atomically (temp file, fsync, rename,
fsync directory), with its own short lock `<store>/disks.lock`. It is not under
the scan writer lock, so a label can be edited while a scan runs; the scan
updates "last seen" under the same short lock.

```json
{
  "format": "diskdrift-disks", "version": 1,
  "disks": [{
    "id": "d1a2b3c4",
    "identity": { "strength": "uuid", "source": "9F0E…-…" },
    "label": "backupy-2014",
    "description": "WD 500 GB, shelf 2",
    "sets": ["stare-dyski-2014"],
    "volume": { "name": "Backup", "fsType": "hfs", "capacityBytes": 500107862016 },
    "device": { "model": "WDC WD5000", "vendor": "WD", "bus": "USB", "serial": "WX…", "mediaBytes": 500107862016, "internal": false },
    "firstSeen": "2026-10-22T10:00:00+02:00",
    "lastSeen": "2026-10-22T10:20:00+02:00",
    "lastMountPoint": "/Volumes/Backup"
  }]
}
```

- **Store, not `config.json`:** labels describe the data, so they travel with
  the store and stay right under a different `--db`.
- **Labels are optional and unique** in a store (case-insensitive). They can be
  used wherever a disk is referenced. Without a label, the volume name is shown.
- **Sets are a list**, so a disk can be in more than one. Nothing uses them
  until roadmap step 6; they are in the schema now so it does not change again.
- **Only scans and explicit edits write the registry.** `doctor --quick`,
  `usage`, `info`, `snapshots` and the TUI's listing read it, and when it is
  missing they derive it in memory from snapshot metadata. Nothing is created.
- **Forgetting a disk** removes its snapshots (the existing `remove` with
  folding) and then its entry. A disk with no snapshots left keeps its entry
  until it is forgotten, so a label is not lost by removing old scans.

### 2.4 Snapshot metadata

`META_FORMAT_VERSION` 1 → 2, with new fields that default to absent for old
documents:

- `disk`: the short disk ID.
- `rootInVolume`: the root relative to the volume's mount point, e.g. `/` or
  `/Docs`. For `/` on the system disk it is `/`.

The absolute `root` stays: it is where the scan happened, and live operations
(analyse, Doctor) still need it.

### 2.5 Comparability: disk + root in volume

`incompatibility` becomes:

1. When both have `disk`, compare `disk` and `rootInVolume` instead of `root`.
2. Otherwise use today's rule, but a missing UUID on one side no longer means
   "same volume" when the other side has one.
3. Exclusions and the filesystem-boundary setting are compared as today.

A **lineage** (one disk + root in volume + options) is what deltas, Compare,
History "changes since the previous scan" and the search index (research
section 5) are keyed by.

### 2.6 Different mount points: a root-rename adapter, no data format change

Two scans of one lineage can have different absolute roots (`/Volumes/Backup`
against `/Volumes/Backup 1`). Their streams differ only in the root record's
name.

- A stream adapter in `diskdrift-core`, `with_root_name(stream, name)`,
  rewrites the root record of the older stream to the newer root before diff,
  delta and browse. Core still sees only canonical record streams.
- Paths shown for a comparison are under the newer scan's root.
- The data file format (ADR 0001) is unchanged. The alternative, a relative
  root inside the data, would be a data format change for no gain.

### 2.7 File names carry the disk

The stem becomes `<diskid>-<YYMMDD-HHMMSS>`, e.g.
`d1a2b3c4-261021-121552.json`, `d1a2b3c4-261021-121552.r…dds`. A directory
listing shows which disk each file belongs to, and a disk's files sort
together. The ID is never the label, because labels change.

The alternative, a directory per disk (`snapshots/d1a2b3c4/…`), makes
"forget a disk" a directory removal, but changes every path in store.rs. The
prefix is the smaller change, and forgetting a disk goes through `remove`
anyway.

### 2.8 Store version 3 and migration

- `STORE_VERSION` 2 → 3. A version 2 store stays readable without migration:
  `list` fills in `disk` and `rootInVolume` in memory from `volume.uuid` and
  `volume.mountPoint`, and derives the registry entries. This is how version 1
  stays readable today.
- The migration runs when the writer lock is first taken, by the same pattern
  as `migrate()` for v1 → v2:
  1. Write `disks.json` from the metadata. Snapshots without a UUID get `weak`
     identities, one per distinct fs + name + mount point; they are flagged in
     the TUI so the user can merge them.
  2. For each snapshot: hard-link its data to the new name, write the new
     metadata, remove the old names.
  3. Write `store.json` version 3 last.
- An interruption leaves duplicates that `read_entries` already resolves.
  Tests simulate an interruption after each step, as the v1 → v2 tests do.
- Older DiskDrift builds refuse a version 3 store (`UnsupportedVersion`). That
  is the existing behaviour.

## 3. What this does not decide

- How the search index is stored: a separate derived cache under
  `<store>/index/` keyed by lineage, to get its own ADR (research
  search-index.md, section 5).
- Mount detection and careful mode: roadmap steps 4 and 6. This ADR only gives
  them the disk ID, the device information and the registry.

## 4. Consequences

- Scans of one external disk at different mount points become comparable,
  delta-compressed and searchable as one history.
- Two disks that share a name or mount point are no longer mixed up.
- A disk has a place for its label, description and sets, and outlives its
  last snapshot.
- Costs:
  - a migration that renames every snapshot file once;
  - one more file, and one more lock, in the store;
  - a FAT/exFAT identity that is weaker than a UUID (labelled as such);
  - an adapter that every two-snapshot operation has to apply.

## 5. Implementation plan

Each phase ends with `cargo test`, `cargo clippy --all-targets` and the smoke
test passing, and the spec updated.

1. **Spec first.** Section 3A (the My disks screen), section 8 (store layout,
   the registry, comparability by lineage), section 12 (this leaves the deferred
   list), and the revision history.
2. **Platform:** `diskdrift-platform::disk_info(path)`, returning the volume
   UUID, name, fs type, capacity and mount point (already there), plus device
   model, vendor, bus, internal/removable, media size, and serial when IOKit
   has one (Disk Arbitration `DADiskCreateFromVolumePath` description). It is
   read-only. Tests run on `/`; external disks are tested by hand.
   - Done: `diskdrift_platform::disk_info` (Disk Arbitration and IOKit) and
     the `disk_info` example. Verified on the careful-scan images with the
     owner's approval, each attached twice and read-only; the images stayed
     unchanged. Script: `experiments/careful-scan/identity_check.py`.
3. **Storage, registry:** a `disks` module with load (missing file means
   empty), atomic save, the short lock, and short-ID derivation with collision
   handling. Unit tests.
   - Done: `diskdrift_storage::disks`. `Registry::load` (a missing file is
     empty and nothing is created), `save` (atomic) and `update` (lock, load,
     change, save; a failed change writes nothing). `observe` records a
     sighting: a known disk is updated, a new one added, and a possible clone
     (same identity, different device serial) or a `weak` match is returned as
     a question, with nothing changed. Labels are checked here: unique ignoring
     case, no `:` or `/`, not shaped like a short ID, not `latest`. SHA-256 is
     implemented in the module (checked against the FIPS test vectors), so the
     store needs no new dependency.
4. **Storage, metadata and comparability:** the new fields
   (`META_FORMAT_VERSION` 2), the new `incompatibility`, and the core
   `with_root_name` adapter wired into diff, delta and rebase. Tests:
   - the same disk at two mount points is comparable, its delta stays small,
     and the diff is empty;
   - two disks with the same mount point are not comparable;
   - a missing UUID against a present one is not comparable.
   - Done: `disk` and `rootInVolume` in `SnapshotMeta` (metadata version 2;
     absent in older documents, and nothing sets them until phase 6), the
     new `incompatibility`, and `meta::root_in_volume`.
     `diskdrift_core::diff::with_root_name` renames the root record, and
     `Store::records_as(id, root)` reads a snapshot as if scanned at `root`.
   - One rule keeps deltas correct across mount points: the ops of a delta
     are always built against its base renamed to the delta snapshot's own
     root. Reading a delta therefore renames the base first, then applies the
     ops. This holds for reverse deltas (rebase), forward deltas (`begin` with
     a base) and the refolding in `remove`. Deltas written before this change
     have equal roots, so the rename is a no-op for them; when the roots are
     equal the stream is not touched at all, so the root's exact bytes are
     kept.
   - Compare (`diff`, `diff_browse`) renames the older scan to the newer
     root only when the two are comparable. With `--allow-incompatible` they
     are compared as they are.
   - Tests: one disk at two mount points (empty delta, empty diff, the older
     scan still reads under its own root), at three (removing the middle
     scan, a forward delta), two disks at one mount point, and the rules.
5. **Storage, store version 3:** the new stem, the migration, in-memory
   derivation for version 2 stores, and interruption tests at each step.
   `snapshots verify` passes after the migration on a copy of a real store.
   - Done. `STORE_VERSION` 3, stems `<disk>-YYMMDD-HHMMSS`. One `migrate()`
     takes version 1 and 2 stores to 3: the registry first (snapshots in id
     order, so first and last seen are right), then per snapshot a hard link
     of the data, the new metadata, the removal of the old metadata, the
     removal of the old data; `store.json` last. A rerun after an
     interruption picks the same stems (they depend only on the metadata) and
     finishes; recording a sighting older than "last seen" changes only
     "first seen", so a rerun does not move "last seen" back.
   - Unmigrated stores are read as they are: `list` fills in `disk` and
     `rootInVolume` in memory, and `Store::registry()` derives the registry
     when `disks.json` does not exist. Nothing is written.
   - `begin` gives a snapshot without a disk the disk of its volume and
     records it in the registry, so every snapshot in a version 3 store has
     one. Phase 6 passes the disk from `disk_info` instead.
   - `rootInVolume` on the System+Data volume group is the root itself (the
     recorded mount point there is `/System/Volumes/Data`, the root is seen
     from `/`).
   - Weak identities from metadata are filesystem, volume name and mount
     point (as section 2.8 says), not capacity: older scans without a space
     sample would otherwise split one disk in two.
   - Tests: a version 2 store read without migrating (nothing written), then
     migrated, then migrated again (no change); a migration interrupted after
     each of its steps in turn, each time readable in between and
     finished by the next lock; the version 1 store; an exFAT volume becoming
     a `weak` disk.
   - On a copy of a real store (11 snapshots, 30.9 MB, delta chains, a
     PARTIAL and two CANCELLED scans): migrated by the next scan in 0.5 s
     including the scan; `snapshots verify` gives the same record counts
     before and after for every snapshot, and a diff of snapshots 1 and 9 is
     unchanged. No data file was rewritten (hard links).
6. **App:** `scan` records `disk` and `rootInVolume` and updates the registry.
   - Snapshot references accept a disk: `backupy-2014:latest`,
     `d1a2b3c4:latest~1`.
   - `snapshots --disk <ref>`.
   - `diskdrift disks` (list), `disks label|describe|set|forget`.
   - JSON output for all of them.
   - `comparable` / `previous_comparable` / `latest_pair` now go through
     lineages.
   - Done. `scan` takes the identity from `disk_info` (Disk Arbitration's
     UUID; `serial` for FAT/exFAT, `weak` without a UUID), with the device
     facts, and records it before the walk. A doubtful match (a known
     identity with another device serial, or a weak one) stops the scan with
     a usage error naming the known disk; `scan --disk same|new` answers it.
     "New" for a clone is the identity plus the device serial; for a weak
     one, the identity plus the time of the scan. The scan result says which
     disk it was and whether it was new.
   - References: `Store::resolve` takes `<disk>:<ref>` (label, ID or ID
     prefix; nothing after the colon means `latest`), so every command that
     takes a snapshot accepts it.
   - `diskdrift disks` (connected: the identities of the mounted volumes,
     from `disk_info` of each mount), `label`, `describe`, `set add|remove`,
     `merge`, `forget`, all with `--json`; `snapshots --disk`. Edits on a
     store without `disks.json` start from the registry derived from the
     metadata (`Store::update_registry`), so IDs do not change.
   - `disks merge` (not in the original plan): the way to join a FAT
     stick's migrated `weak` history to its new `serial` identity. It
     re-points the metadata, then the migration renames the files: a
     snapshot whose disk is in the registry keeps it.
   - Not yet: the TUI asks nothing (a doubtful match shows the error in the
     scan form until phase 7), and FAT/exFAT were not scanned end to end on
     disk images in this phase (the identity rule is unit-tested; phase 2
     verified the UUIDs on the images).
7. **TUI, My disks:** a screen next to History, one row per disk:
   - columns: label or volume name, volume, fs, capacity, connected, last
     scan, number of scans, sets;
   - Enter opens History filtered to the disk (the filter shown in the header,
     one key clears it);
   - actions: rename, describe, set, scan now when connected, forget (asks,
     says the space freed).

   Compare, Largest files and the scan form follow the filter.
   - Done. `K` opens My disks (in the menu next to History). The filter
     lives in the App (`disk_filter`); `history()` and `usable()` apply it,
     so Compare, Largest files and the scan form's suggested folder follow
     without changes of their own; setting or clearing it resets their
     chosen pairs (indices into the usable list change meaning). Edits and
     forget call the app services directly (small, local writes); a refused
     label keeps the typed text. The doubtful-disk question is a scan stage
     (`AskDisk`): `AppError::DiskDoubt` from the scan becomes
     `Msg::ScanDoubt`, and `s`/`n` start the same scan again with the
     answer.
   - Checked in a pseudo-terminal at 80×24 on a copy of a real store plus
     a registry-only disconnected disk: list, detail panel, filter, label
     edit, forget question and refusal. The first version cut the columns
     and hid the forget question below the panel's last line; fixed (the
     volume column goes below 100 columns, a question takes the panel).
8. **Benchmarks:** confirm that the migration of a real 30-snapshot store
   and the adapter's cost on diff and rebase stay within the section 2A
   budgets. Record the numbers here.
   - Done, 2026-10-22, M2 Max, warm cache, mimalloc, each step in its own
     process (`benches/diskdrift-bench/examples/disks_cost.rs`, raw rows in
     `docs/benchmarks/2026-10-22-disks-results.jsonl`). Workload: the M1
     synthetic tree, 3 M entries, 30 scans at ~1 % change (a version 2
     store of 119.7 MB, built in 38 s, then rewritten as version 2 wrote
     it).

     | Step | Result | Budget (spec 2A) |
     |---|---|---|
     | Migration v2 → v3, 30 snapshots | 0.44 s, peak RSS 4 MB, no data file rewritten | – (no budget; well below one scan) |
     | Verify after: newest / oldest (end of the delta chain) | 0.47 s / 6.3 s, all checksums valid, every file named after its disk | – |
     | Commit + rebase, same mount point / another one | 1.28 s / 1.29 s, both deltas 30,244 items | – (the adapter adds nothing measurable) |
     | Adjacent diff with the adapter (another mount point) | 0.61 s, peak RSS 26 MB, 9,838 changes | ≤ 5 s ✓ |
     | The same diff without the adapter | 0.82 s, 6.03 M "changes" (every path differs by its root) | – (why the adapter exists) |

     A real store (the 11-snapshot copy used in phase 5, 30.9 MB) migrated in
     the time of a small scan (0.5 s including it).
   - Name search without an index (the baseline ROADMAP 3.2 asked for
     before closing this ADR): a case-insensitive substring of each entry's
     own name, streamed through `Store::records`.
     - The newest snapshot, 3 M entries: 0.24 s (820 hits for `abc`).
     - Every snapshot of the history, each reconstructed on its own:
       101 s for 90.5 M records. Reconstructing each snapshot is the wrong
       way to search history; search-index.md section 3 reads the delta
       ops instead (113 ms over 9 deltas in its prototype).
     - The real store, 9 usable snapshots, 8.7 M records: 0.97 s.
   - What that means for the format. The search design recommended in
     search-index.md is a derived cache under `<store>/index/` (a name
     arena built from the records, a persisted sidecar), which needs no
     change to the snapshot data. A split names frame in the data files
     (its E5) would only make building that cache faster, and would be a
     data-file format change of its own, separate from this ADR's store
     version 3 (file names and metadata). ROADMAP had asked for both to go
     into one format change; that did not happen, because store version 3
     shipped before E5 was measured. If E5 later favours the names frame,
     its migration is a rewrite of every data file (about the cost of one
     scan per full snapshot), not a rename.

## 6. Owner's decisions (2026-10-22)

1. File names: **a disk prefix** (`d1a2b3c4-261021-121552`), as proposed.
2. Labels: **unique** in a store, so they can be used in references.
3. Clones (a known UUID with a different serial): **ask every time**.
4. Sets: **several per disk**; they work like tags.
5. The macOS system disk: **one disk**.
