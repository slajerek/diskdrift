# DiskDrift

Fast, **read-only** disk-space investigation for macOS (APFS): where did my free
space go?

DiskDrift reports what the OS says about free space (including purgeable space
and local snapshots), measures the usual suspects, and — with saved metadata
scans — shows exactly what grew between two points in time. It never modifies,
moves or deletes your files; the only things it writes are its own config and
snapshot store.

> **Work in progress.** This is a development snapshot, not a release: features are
> unfinished and the storage format may still change.

Storage decision: [`docs/adr/0001-storage-engine.md`](docs/adr/0001-storage-engine.md) ·
benchmarks: [`docs/benchmarks/`](docs/benchmarks/)

## Build

```bash
./build.sh                     # release build (--debug, --test: tests + clippy)
./run.sh [command ...]         # rebuild if needed, then run (no command: the TUI)

cargo build --release          # binary: target/release/diskdrift
cargo test                     # all tests
scripts/tui_smoke.py target/release/diskdrift   # drives the TUI in a pseudo-terminal
```

Rust stable (edition 2024), macOS 11+.

## Use

```bash
# Today's problem, no baseline needed (creates no files)
diskdrift usage                      # free/available/purgeable per volume, APFS containers
diskdrift info                       # topology, Full Disk Access, effective paths
diskdrift doctor --quick --reference-free 120GB

# Record a baseline, look at it, repeat later and compare
diskdrift scan / --name baseline
diskdrift snapshots
diskdrift top --snapshot latest --directories --limit 30
diskdrift browse ~/Library                     # one folder level of the latest scan, largest first
diskdrift scan / --name followup
diskdrift diff baseline followup --sort growth --limit 50
diskdrift diff baseline followup --path /Users --min-size 1GB
diskdrift browse ~/Library --compare baseline  # what changed in that folder since baseline

# Disks: every scan records the disk it was taken on (external ones too)
diskdrift disks                                # known disks, connected or not, last scan
diskdrift disks label d1a2 backupy-2014        # your own name for a disk
diskdrift diff backupy-2014:latest~1 backupy-2014:latest
diskdrift snapshots --disk backupy-2014
diskdrift disks label /Volumes/STICK stick     # a new disk, by its mount point (records it, does not scan)

# Find a file on any disk, connected or not; when it is gone, when it was deleted
diskdrift find wazna                           # part of a name; case and Polish letters ignored
diskdrift find '*.pdf' --disk backupy-2014     # a glob over the whole name, one disk
diskdrift find 'Docs/plan' --history           # a path; also when it appeared, and older copies

# Old disks: careful mode (nothing is written to a disk you connect)
diskdrift careful                              # until Ctrl-C: hold back each disk, ask, scan read-only, eject
diskdrift careful --scan                       # scan every disk connected, without asking
diskdrift disks set add d1a2 stare-dyski       # put a disk in a set
diskdrift find wazna --set stare-dyski         # search only that set, all of it unplugged

# Consolidated, read-only advice
diskdrift doctor --deep
diskdrift recover
diskdrift analyze "~/Library/Application Support"   # what each subfolder is and whether it matters

# Full-screen UI
diskdrift            # (or: diskdrift tui)
```

In the TUI, history, a finished scan, the largest-files list and comparisons all
open the same browser: ↑↓ moves the cursor, Enter opens a folder one level
deeper, Esc goes back up (on to the scan root, then back where you came from);
`a` analyses the selected folder as it is now (what each subfolder is and
whether it matters); `c` shows what changed in that folder since the previous
scan (and back).
Compare (D) opens straight on your newest scan against the previous one of the
same folder; ↑ at the top (or `p`) lists the scans to pick another pair, in two
steps: from which scan, then to which later one.
Largest files (T) works the same way: ←→ switch files/folders, ↑ at the top (or
`p`) picks another scan, `m` sorts by space on disk or by file size.
Scan (S) takes any folder (`/` the whole disk, `~` your home, `~/Library`) and
says when it was last scanned and how long that took; the result says what
changed since the previous scan of it (`c` shows the changes).
Connect a disk while the TUI runs and DiskDrift notices: a disk it knows is
rescanned in the background when its last scan is old (`x` stops it;
Settings: "Rescan a disk you connect"), and afterwards the dashboard says
what changed and that you can eject it. A disk it has never seen is only
listed, never scanned on its own. The dashboard's Disks panel shows every
disk, connected or not, with its last scan.
My disks (K) lists every disk the store knows, connected or not: Enter shows
its scans (History, Compare and Largest files then follow that disk; `*` in
History shows every disk again), `l` gives it a label, `o` a description,
`+`/`-` sets, `g` scans it now, `f` forgets it (asks; removes its scans, never
files).
Find (F or /) searches the newest scan of every disk for a name, a glob
(`*.pdf`), `re:` regex or a path (`Docs/plan`), ignoring case and diacritics;
Enter opens the result in its scan. When nothing is there now it searches the
history by itself ("last seen in scan #12, gone in #13"; `x` stops it), and
`p` searches it also when something was found. No index: it reads the
saved scans and their deltas as they are.
Careful mode (C) is for old, possibly failing disks: while it is on (`o`), an
external disk you connect is not mounted at all, so macOS does not repair it,
replay its journal or put Finder and Spotlight files on it. `g` mounts it
read-only, scans it with one thread and ejects it; `m` mounts it read-only for
Finder (to copy a file off); `j` ejects it. The header shows CAREFUL MODE
while it is on; quitting turns it off. A dirty volume is left unmounted, with
the command to mount it read-only yourself. A hardware write blocker is still
the only real guarantee for a disk that matters. `w` in My disks limits
History and Find to the selected disk's set.
Doctor (I) opens with its conclusion; Tab or Enter switch to the recovery
suggestions (R), where Enter opens a location: a folder analysis, or for recent
growth what changed there. F5 measures again.

Every data command accepts `--db <path>` (snapshot store, default
`~/.diskdrift/data`) and `--config <path>` (default `~/.diskdrift/config.json`);
`--json` gives machine-readable output; `--units binary` switches to KiB/MiB/GiB.
Exit codes: 0 ok (partial coverage is a warning), 1 failure, 2 usage/config
error, 3 cancelled.

Snapshots are referenced by id, name, `latest` or `latest~N`, optionally
behind a disk (`backupy-2014:latest`, `d1a2b3c4:latest~1`). `diskdrift
snapshots remove <ref>` deletes DiskDrift's own data, `snapshots verify <ref>`
re-checks every checksum. Several at once: `snapshots remove 3 4 7`,
`--failed` (cancelled and failed scans) or `--before 2026-06-01`; `snapshots
prune --keep-last 5 --keep-weekly 8 --keep-monthly 24` keeps those per disk
and folder (and always the newest) and removes the rest. Both list what goes
and how much it frees and ask first (`--yes` skips that). `snapshots compact`
stores the history as tightly as if the remaining scans had been written one
by one, then checks every checksum. In the TUI, History: `Space` marks, `o`
older ones, `n` scans without data, `p` by the retention rules, `Del` removes
(asks), `z` compacts. A disk is known by its volume UUID, so one disk
mounted at another place keeps one history. `disks describe`, `disks set
add|remove` (sets work like tags), `disks merge <from> <into>` (one disk
recognised as two) and `disks forget` (removes its snapshots, never its files)
edit the store's disk registry.

### Full Disk Access

Without it, macOS hides Mail, Messages, Safari, iOS backups and the Trash; DiskDrift
reports those as inaccessible and marks scans PARTIAL. Grant it to your terminal
in System Settings > Privacy & Security > Full Disk Access for complete coverage.

## Config (`~/.diskdrift/config.json`, optional)

```json
{
  "schemaVersion": 1,
  "storage": { "path": "~/.diskdrift/data", "keepLast": 0, "keepWeekly": 0, "keepMonthly": 0 },
  "scan": { "defaultRoot": "/", "threads": 0, "stayOnFilesystem": true, "followSymlinks": false, "exclude": [],
            "autoOnStart": true, "autoMaxAgeHours": 6 },
  "ui": { "confirmBeforeScan": true },
  "output": { "sizeUnits": "decimal" }
}
```

`autoOnStart`: once you have scanned the default root yourself, the TUI rescans
it in the background at low priority when it starts and the latest scan is older
than `autoMaxAgeHours` (shown below the menu; S shows it, ← goes back with it still running, `x` stops it after asking).

`keepLast`, `keepWeekly`, `keepMonthly` are the retention rules for `snapshots
prune` and `p` in History (0 = off, the default); nothing is removed without asking.

In the TUI, **E Settings** edits the same file (each change is validated and saved at once).
`diskdrift config init` writes this template (never overwrites); `diskdrift config
show` prints effective values and where each comes from (command line > config
file > default).

## How it works (short)

- **Scanner**: parallel `getattrlistbulk(2)` with deterministic depth-first output
  (children sorted by name) and bounded read-ahead memory. On macOS the System and
  Data volumes are scanned as one filesystem through firmlinks, without counting
  `/System/Volumes/Data` twice. Hardlinks count once; symlinks are not followed.
- **Store**: one immutable file per snapshot, columnar zstd blocks (4–17 bytes per
  entry on real trees). The newest snapshot is full; each earlier one is rewritten
  as a small reverse delta (30 snapshots ≈ 1.5× the size of one). Atomic commit
  (temp file + fsync + rename), single-writer lock, per-block checksums.
- **Diff**: a streaming merge of two sorted snapshots — memory independent of
  their size — with coverage-aware classification (entries that merely became
  readable are not "added").
