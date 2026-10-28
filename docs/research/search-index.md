# Research: a file-name search index across all disks and all snapshots

- Status: **research, not a decision.** Input for a future ADR 0002 (search
  index). File search across disks is a deferred roadmap item (spec section 12,
  ROADMAP 3.2); per CLAUDE.md nothing here is to be implemented before M0-M2
  are done.
- Date: 2026-10-22. Machine for every measurement below: Mac14,6 (Apple M2 Max,
  12 cores, 64 GB), macOS 15.6.1, APFS internal SSD, rustc 1.99.1, release
  build. Warm page cache unless stated (no root, so cold cache cannot be forced).
- Question (product owner): *"We must be the fastest file-search program in the
  world. How do we load the snapshot information (into RAM?) so that searching
  a file name across ALL snapshots is extremely fast? Is the incremental
  (delta-based) storage OK for that?"*

Labels used throughout: **measured** (run on the machine above, prototype
described in section 6), **published** (number from a cited source),
**estimate** (derived from the two; treat as a hypothesis for the benchmark
plan in section 7).

## Outcome (2026-10-22, roadmap step 5)

Search shipped **without** the index recommended below (spec 3.5): the newest
scan of each disk and folder is streamed (0.24 s for 3 M synthetic entries,
0.17 s for the 1.77 M-entry real store, one thread per scan), and the history
is read from the stored delta ops as section 3.2 proposes (29 deltas of a
3 M-entry history in 0.05 s on 12 cores; a pair without a delta is compared
record by record). Folding follows section 2.5 (NFD, marks dropped, the
letters without a decomposition mapped). Numbers:
[`../benchmarks/2026-10-22-search-results.jsonl`](../benchmarks/2026-10-22-search-results.jsonl).
The index remains the design for search as you type and for stores far
larger than the reference workload.

## 0. Summary

1. **Keep the snapshot format as it is.** The reverse deltas already contain
   exactly the facts a history search needs: for every pair of adjacent scans,
   an `Add` op for each path that existed in the older scan and is gone in the
   newer one, and a `Remove` op for each path that appeared in the newer one.
   Verified with the production store on real names (section 3.2). No snapshot
   format change is required for search.
2. **The index is a derived, disposable cache** under `<store>/index/`, with its
   own version, rebuildable from the snapshots at any time. Two parts per
   lineage (one disk + root): a **live name column** for the newest scan, and
   **lifetime segments** (graveyard of gone paths plus births) written from the
   same op stream the reverse delta is written from.
3. **The query engine is a flat, case-folded name arena in RAM scanned in
   parallel with SIMD substring search** (`memchr::memmem`, `regex-automata`
   for globs and regex). Measured: **53.5 M real-world names (1.02 GB) in
   8.5 ms** on 12 cores (≈120 GB/s, memory-bandwidth bound), 31 ms on one core.
   A trigram index was measured too: it doubles memory (+19 B/entry) and is
   not faster than the parallel scan except for very selective queries, so it
   is not recommended as the primary structure.
4. **Memory:** ≈ 24 MB per million entries hot (19 B folded name + 4 B parent
   id + ≈0.1 B sparse offsets), or ≈ 4-7 MB per million in a
   compressed-resident mode with ≈ 60 ms queries at 53 M (measured). **Disk:**
   ≈ 3.5-4 B per entry for the persisted name column (measured 3.3 B for
   names in 256 KiB zstd blocks) on top of today's 9.7 B/entry snapshots.
5. **History search** becomes a scan of the (much smaller) graveyard arena,
   with lifetimes stored as snapshot-id intervals that are resolved against
   the current snapshot list at query time. Snapshot removal and compaction
   therefore never make the index wrong; they only leave garbage that
   compaction drops.

## 1. How existing tools do it

B/file values marked *derived* are divisions of published totals, not numbers
the source states. Hardware and cache state are rarely given; compare orders
of magnitude only.

| tool | mechanism | memory / disk per file | query latency | source |
|---|---|---|---|---|
| **voidtools Everything** (Windows) | Reads the NTFS MFT, follows the USN journal. In-memory list of UTF-8 names, size, mtime and a *parent pointer* (no path strings). Search = "optimized multi-threaded strstr on every single filename", compiled to byte code. Separate presorted indexes per sort column | ≈ 100 B/file RAM ("around 100MB per 1 million files"), 45 MB disk per 1 M files; + 8 B/file per search window and per fast-sort index; indexing can double RAM | "about 15 ms per 1 million files" for a normal search, ≈ 50 ms per 1 M when matching paths; build ≈ 1 min per 1 M files | [FAQ](https://www.voidtools.com/faq/), [p=60262](https://www.voidtools.com/forum/viewtopic.php?p=60262), [t=11234](https://www.voidtools.com/forum/viewtopic.php?t=11234), [t=9024](https://www.voidtools.com/forum/viewtopic.php?t=9024), [t=8848](https://www.voidtools.com/forum/viewtopic.php?t=8848), [indexes](https://www.voidtools.com/support/everything/indexes/) |
| **plocate** (Linux) | Trigram posting lists (delta + TurboPFor, blocks of 128) over blocks of 32 zstd-compressed (level 6, 1 KB dictionary) file names; candidates are verified by decompressing their block; io_uring for asynchronous reads | 466 MB for 27 M files ≈ **17 B/file on disk** (derived); mlocate 1.1 GB ≈ 41 B/file | "two files out of 27 million in just a few milliseconds" (8 ms vs 20.1 s mlocate). Patterns < 3 bytes and `--regexp` fall back to a linear scan "which is slow" | [plocate.sesse.net](https://plocate.sesse.net/), [man page](https://manpages.debian.org/testing/plocate/plocate.1.en.html), [database-builder.cpp](https://codeberg.org/RivuletCedar/plocate/raw/branch/main/database-builder.cpp), [plocate-build.cpp](https://codeberg.org/RivuletCedar/plocate/raw/branch/main/plocate-build.cpp) |
| **mlocate** | Per directory: timestamp + path, then sorted names; full linear scan per query; `updatedb` reuses unchanged directories | ≈ 41 B/file (above) | 20.1 s for 27 M files (plocate's example) | [mlocate.db(5)](https://linux.die.net/man/5/mlocate.db) |
| **GNU / BSD locate** (also macOS) | Front compression (`frcode`, 4-5× smaller), BSD adds bigram coding; linear scan. On macOS the weekly `locate.updatedb` job is disabled by default | small (front-coded) | linear | [locatedb(5)](https://manpages.debian.org/testing/locate/locatedb.5.en.html), [locate.code.c](https://raw.githubusercontent.com/freebsd/freebsd-src/main/usr.bin/locate/code/locate.code.c), [com.apple.locate.plist](https://opensource.apple.com/source/shell_cmds/shell_cmds-175/locate/locate/com.apple.locate.plist.auto.html), [eclecticlight](https://eclecticlight.co/2018/03/24/find-files-fast-with-locate/) |
| **fd** (no index) | Parallel directory walk + regex | none | 0.85 s for ≈ 4.75 M entries, warm (≈ 5.5 M entries/s, derived) | [fd README](https://github.com/sharkdp/fd) |
| **ripgrep** (no index) | Parallel walk + SIMD regex over contents | none | 0.06-0.08 s on the Linux kernel tree (i9-12900K) | [ripgrep README](https://github.com/BurntSushi/ripgrep) |
| **macOS Spotlight** | Per-volume `.Spotlight-V100` store, proprietary; postings in two-level tables (frequent / rare tokens) plus transient tables merged later; names tokenized at word boundaries (ICU: spaces, `_`, `-`, CamelCase), not trigrams. Substring needs `*paris*` wildcards; `[cd]` modifiers for case and diacritics | GB-scale indexes reported by users (anecdotal) | no primary latency benchmark found; updates "within a second or two" | [eclecticlight 2025](https://eclecticlight.co/2025/07/30/a-deeper-dive-into-spotlight-indexes/), [eclecticlight 2021](https://eclecticlight.co/2021/01/28/spotlight-on-search-how-spotlight-works/), [query format](https://developer.apple.com/library/archive/documentation/Carbon/Conceptual/SpotlightQuery/Concepts/QueryFormat.html) |
| **FSearch** (Linux, Everything clone) | In-memory entries (8 B parent pointer, flags, name length, inline NUL-terminated name), multi-threaded linear scan split across all cores | ≈ 16 B + name per entry in RAM (derived from the struct); ≈ 50 B/file on disk (derived) | no published benchmark | [entry struct](https://github.com/cboxdoerfer/fsearch/blob/c20e6b17bf4b56ddf8b56bf021429c5088346ff8/src/fsearch_database_entry.c), [parallel search](https://github.com/cboxdoerfer/fsearch/blob/c20e6b17bf4b56ddf8b56bf021429c5088346ff8/src/fsearch_database_index_store.c), [wiki](https://github.com/cboxdoerfer/fsearch/wiki/Database-update) |
| Alfred, HoudahSpot | Spotlight's index | — | Spotlight's | [Alfred](https://www.alfredapp.com/help/troubleshooting/indexing/spotlight/), [HoudahSpot](https://www.houdah.com/houdahSpot/) |
| Raycast | Spotlight before; a 2026 release builds its own indexer ("scans your entire disk in seconds"), no numbers | — | — | [manual](https://manual.raycast.com/troubleshooting/spotlight), [blog](https://raycast.com/blog/the-new-raycast) |
| Find Any File | No database: catalog search (`searchfs`) on the volume, "a few seconds" for millions of files; `FSCatalogSearch` does not work on APFS | none | seconds | [tempel.org](https://blog.tempel.org/2017/07/apfs-and-fast-catalog-search.html), [findanyfile.app](https://findanyfile.app/) |

Lessons for DiskDrift:

- **The fastest interactive tool (Everything) does no clever indexing for
  substring search**: names plus parent pointers in RAM, scanned in parallel on
  every keystroke. Its cost is RAM (≈ 100 B/file). plocate is the opposite
  trade-off: tiny on disk, no resident memory, trigram lookups, and a slow
  linear fallback for short patterns and regexes. Spotlight is word-token based
  and is not built for arbitrary substrings of file names.
- DiskDrift's case differs from both in two ways: the data is **already
  captured** (snapshots, so no live file-system access is needed and
  disconnected disks work), and it needs **history**, which none of these
  tools has.
- The measured flat arena (section 2.2) uses ≈ 24 B/entry hot, about a quarter
  of Everything's published figure. Its scan (≈ 0.16 ms per million names on 12
  cores) is far below Everything's published "15 ms per million" (different
  hardware and a per-entry rather than bulk scan, so this is not a fair
  head-to-head; it only shows the approach has headroom).

## 2. Candidate index structures

### 2.1 What the names look like (measured)

Corpus: every path under `/System/Library /Applications /Library /usr /opt`
on this Mac (`find -xdev -print0`, read-only): **2,672,958 entries**,
48.4 MB of name bytes, **average name 18.1 bytes**, **32.4 % distinct names**
(867k distinct; `Info.plist`, `Contents`, `Resources`, `*.lproj` repeat
endlessly), only 213 names with non-ASCII bytes (user document trees in Polish
will have many more). To reach archive-disk scale the corpus was replicated
20× (53.5 M entries, last byte of longer names perturbed per replica so the
copies are not byte-identical).

### 2.2 Flat name arena + SIMD scan (measured)

Layout: all case-folded names concatenated, each preceded by a NUL
(`\0name\0name...`), plus a `u32` start offset per entry. A substring match can
never cross a NUL, so the arena is searched as one byte string and every hit
is mapped back to its entry by binary search on the offsets. Globs and regexes
are compiled with `[^\x00]` in place of `.`/`*`.

| 53.5 M names, 1.02 GB arena | hits | 1 thread | 12 threads (64 chunks) |
|---|---:|---:|---:|
| `waznasprawa10.txt` | 0 | 30.7 ms (33 GB/s) | **8.5 ms** (120 GB/s) |
| `wazna` | 0 | 31.3 ms | 8.5 ms |
| `info.plist` | 36,516 | 47.5 ms | 8.9 ms |
| `readme` | 94,976 | 43.3 ms | 9.2 ms |
| `.txt` | 44,169 | 36.3 ms | 8.7 ms |
| `zq` (2 chars) | 2,304 | 30.7 ms | 8.8 ms |
| `e` (1 char) | 34.4 M | 2,535 ms | 331 ms |
| regex `wazna[^\x00]*\.txt` | 0 | 29.9 ms | 8.6 ms |
| regex `report[^\x00]*20[0-9]{2}` | 21 | 50.2 ms | 8.5 ms |
| regex `(?i)waznasprawa` (no pre-folding) | 0 | 109 ms | 13.3 ms |

At 2.67 M names (51 MB, fits in the caches) the same queries take 1.5-3 ms on
one core and ≈0.5 ms on 12.

Observations:

- The scan is **memory-bandwidth bound** in parallel (≈120 GB/s of the M2
  Max's 400 GB/s theoretical) and SIMD-compute bound on one core (≈33 GB/s).
  On a base M1/M2 (4 performance cores, ≈68-100 GB/s) the same 1 GB should
  take roughly 15-30 ms (**estimate**; to be measured, section 7).
- The 1-character query is slow only because 34 M hits are mapped back to
  entries one by one. A real UI caps results (count, then first N in display
  order), so the mapping work stops early; the scan itself is the same 8.5 ms.
- Pre-folding the arena (lower case, normalization) matters: an `(?i)` regex
  over unfolded names is 3.5× slower single-threaded.
- **Type-ahead refinement:** when the new query contains the previous one
  (`wazn` → `wazna`) and the previous result set was complete, only the
  previous hits need checking. After the first keystroke, typing is nearly
  free. This is an engine rule, not a data structure.

### 2.3 Trigram index with compressed posting lists (measured)

Built the way Google Code Search and plocate do it (one posting list of entry
ids per distinct 3-byte sequence; a query intersects the lists of its
trigrams, starting from the shortest, then verifies candidates against the
arena):

| 53.5 M names | value |
|---|---:|
| distinct trigrams | 64,187 |
| postings | 851 M (15.9 per entry) |
| build (sort-based, parallel sort) | 11.8 s |
| size, raw `u32` | 3.4 GB (64 B/entry) |
| size, delta + varint | 1.02 GB (**19.1 B/entry**) |
| size, Roaring bitmaps | 1.63 GB (30.6 B/entry) |
| longest list | 8.3 M |

Query latency (single thread, lists in RAM, uncompressed): 0.00 ms for
`waznasprawa10.txt` (a trigram that does not exist ends the query), 0.79 ms
`.txt`, 2.5 ms `readme`, 7.0 ms `info.plist`, **13.3 ms `localizable`**
(337k hits; slower than the 8.7 ms parallel scan).

Conclusion: at 50 M names a trigram index costs as much memory as the names
themselves (delta+varint) or more (Roaring, which is designed for dense sets
and loses on sparse id lists), takes seconds to build, cannot answer 1-2
character queries, and is only faster than a parallel scan for selective
queries. It would pay off at ≥ 10× the data or on a much weaker CPU. Keep it
as a fallback experiment (section 7, E6), not as the design.

### 2.4 Other structures

- **FST (BurntSushi `fst`)**, measured: the 862k distinct folded names compress
  to 5.7 MB (6.6 B per distinct name, from 18.2 MB raw), built in 0.36 s; a
  prefix query (`info*`) returns 442 names in 0.06 ms. Excellent for
  *prefix/fuzzy on whole names* and as a name dictionary, but it does not
  answer *substring* queries (the main use case: `wazna` must find
  `bardzo_wazna_sprawa.txt`) without indexing every suffix.
- **Suffix arrays / FM-index:** a plain suffix array stores one 32-bit position
  per *character* (textbook; ≈ 76 B/entry at 19 B names), i.e. 4× the arena.
  Compressed suffix arrays (sdsl) are smaller: 5.3 bits/char on enwiki
  (published), so ≈ 12-13 B/entry, comparable to the trigram index. They are
  complex, slow to build, and their locate step is slower than the bandwidth
  numbers above. Not worth it at this scale.
- Published context for the numbers above: Stream VByte decodes more than 4
  billion delta-coded integers/s; SIMD binary packing reaches 0.7 cycles per
  integer; Roaring stores at most 16 bits per value in array containers.
  Russ Cox's trigram index for Code Search is ≈ 20 % of the indexed text,
  but source files are long, while file names are ≈ 18 bytes with ≈ 16
  trigrams each. That is why the index here is as large as the text.
  memchr has had aarch64 SIMD routines since 2.6.
- **Name dictionary (deduplication):** with only 32 % distinct names, storing
  each distinct name once plus a `u32` name id per entry would scan ~2.7× fewer
  bytes (**estimate**; the hit → entries mapping then needs an id column scan or
  an inverted list). Worth measuring on real multi-disk data (archive disks
  often hold copies of each other), section 7 E6.
- **Compressed-resident arena** (measured): the same names in 256 KiB zstd-3
  blocks take **3.3 B/name**; decompressing every block and scanning it in
  parallel takes **56-63 ms for 53 M names** (16-18 GB/s of raw names). A
  mode for "many disks, little RAM" or for a one-shot CLI process.

### 2.5 Case folding and Unicode normalization

- APFS is normalization-preserving and (on macOS) normalization-insensitive;
  HFS+ stores names decomposed (a variant of NFD). A Polish name `Ważna` can
  therefore arrive as NFC (`z` + U+017C) or NFD (`z` + U+0307); external disks
  (exFAT, FAT32, NTFS) are case-insensitive and UTF-16 underneath (see
  sources). Matching on raw bytes would miss files.
- The arena holds a **search key**, never the display name: NFC (or NFD;
  either, consistently) → Unicode full case fold → by default also strip
  combining marks, with a small table for letters that have no decomposition
  (`ł→l`, `ø→o`, `đ→d`, `ß→ss`, `æ→ae`, ...; verified that `ł` and `ø` do not
  decompose). The default is then diacritic-insensitive: `wazna` finds
  `Ważna sprawa.txt` and `ważna` finds `wazna.txt`, which is what a Polish user
  typing without diacritics expects. Invalid UTF-8 names are kept as bytes.
- Exact or case-sensitive matching is a verification step on the original
  names (read from the snapshot for candidate hits only).
- Folding only changes non-ASCII names (and ASCII to lower case), so it costs
  almost nothing at build time (measured arena build: 139 ms for 2.67 M names,
  folding included).

## 3. History search

### 3.1 What a history query needs

"Not on any disk now; was it ever?" needs, per path that matches the query,
the scans in which it existed: *last seen in #12, gone in #13*, and for "when
did it appear", *first seen in #4*. Equivalently, a lifetime **interval list**
per path, in snapshot ids of its lineage.

### 3.2 The reverse-delta format already has it (checked against the code)

`Store::rebase_previous` (crates/diskdrift-storage/src/store.rs) writes the
previous snapshot P as a delta against the new snapshot N with
`DeltaBuilder` over `merge(N, P)`. Reading `diskdrift_core::delta`:

| op in the delta file of P (against N) | meaning | lifetime fact |
|---|---|---|
| `Add` (full record, parent path) | path in P, not in N | **last seen in P, gone in N** |
| `Remove` (name only) | path in N, not in P | **first seen in N** |
| `Replace` (full record) | path in both, record differs | size/mtime/kind changed between P and N |

Ops are in canonical path order and each ops block's index entry carries the
path of its first op, so one path can be looked up in a delta by binary search
over the block index plus one 64 KiB block decode.

**Measured with the production store** (prototype `hist`, section 6): the real
2.67 M-entry tree as 10 snapshots with ≈0.5 % of files deleted and ≈0.5 % new
per scan, plus one planted file `Users/Docs/WaznaSprawa10.txt` created before
snapshot 4 and deleted before snapshot 7:

- write + rebase per snapshot: 0.63-0.73 s (2.68 M entries); newest full
  54 MB (synthetic sizes and inodes compress worse than real ones), each
  reverse delta ≈ 0.7 MB, 61 MB for all 10;
- per delta ≈ 11.8k `Add` and 13.4k `Remove` ops;
- scanning **all 9 deltas** for `waznasprawa` without any index, decoding every
  op: **113 ms on one thread** (231k ops, 6.5 MB on disk; ≈0.49 µs per op).
  It found exactly two ops: `Remove` in the delta of #3 (= first seen in #4)
  and `Add` in the delta of #6 (= last seen in #6, gone in #7). That is the
  answer the ROADMAP asks for, straight from the existing files.

So the delta format is fine. Without an index, a history search costs
≈0.5 µs per op of history (single thread); scaled to 10 disks × 10 M entries ×
30 deltas at 1 % change = 30 M ops, that is ≈15 s on one core, ≈1.5-2 s on 8
(**estimate**). Usable as the fallback and as the rebuild procedure for the
index, not as the instant path.

### 3.3 Gaps the history index must handle (none needs a format change)

1. **Keyframes.** When `rebase_previous` keeps P full (chain longer than
   `MAX_CHAIN` = 30, delta > half the full size, or P is not comparable), no
   delta between P and N exists. The index builder must then run the
   comparison itself (one merge pass, published in ADR 0001 at 0.13-0.19 s per
   1 M entries) or, in the "delta too large" case, keep the ops it already
   computed. Incompatible snapshots (other root, volume, exclusions or
   filesystem-boundary setting, `SnapshotMeta::incompatibility`) start a new
   lineage; linking lineages of the same disk waits for the disk ID
   (ROADMAP 3.1b).
2. **Partial coverage is not deletion.** A directory that was readable in P and
   `INACCESSIBLE`/`EXCLUDED`/`SKIPPED_MOUNT` in N produces `Add` ops for its
   whole subtree. The builder must mark those as *unknown in N* (no false
   precision), using the `CONTENTS_UNKNOWN` directories of N (a few hundred on
   a whole-disk scan: 583 in the acceptance run).
3. **Moves.** A moved directory shows up as its whole subtree gone plus a whole
   subtree new. The index stores both; "probably moved" (same folded name and
   size elsewhere in N) is a query-time heuristic, labelled as such.
4. **Cancelled and failed scans** have no data and are not lineage members.

### 3.4 Lifetime index design

Per lineage:

- **Live part:** the newest snapshot's entries (section 4) plus a
  `first_seen` snapshot id per entry. It is constant for most entries (the
  lineage's first scan), so it run-length-encodes to almost nothing.
- **Graveyard:** one row per (path, lifetime interval) that is not alive in
  the newest scan: folded name, parent path (front-coded, canonical order),
  kind, size, mtime of the last-seen version, `first_seen`, `last_seen`,
  flags (`unknown` for coverage loss). A path that disappears and comes back
  has several rows.
- **Built incrementally from each scan's op stream:** `Add` → close the live
  interval (row `first_seen..P`); `Remove` → new live entry with
  `first_seen = N`. The work is O(changes), done inside the rebase that
  already produces the ops (a tee on `DeltaBuilder`'s emit callback).
- **Stored as segments:** one small immutable segment per scan
  (`index/<lineage>/gone.<snapshot>.ddx`), merged tiered into one when there
  are more than about 8 (LSM style), so the per-scan cost never grows with
  history size. Same durability rules as snapshots (temp, fsync, rename).

Growth (**estimate**, from the measured churn): at 1 % change per scan, ≈0.44 %
of entries die per scan, i.e. 44k graveyard rows per scan per 10 M entries.
At ≈ 4-10 B/row compressed that is 0.2-0.4 MB per scan per 10 M entries on
disk, and ≈ 30 B/row in RAM (≈ 1.3 MB per scan per 10 M entries). After 100
scans: 4.4 M rows, ≈ 130 MB RAM if fully loaded, ≈ 20-40 MB on disk. Churny
trees (caches with hash names, `node_modules`, build output) can be far worse:
100k dead files per day on one disk is 10 M rows after 100 scans. Mitigations:
graveyard rows follow snapshot retention (dropped when every snapshot of their
interval is removed); per-lineage budget with the oldest rows dropped first
and reported; optional "collapse" of a deleted subtree under configured
cache paths into one row (then names inside it are not searchable; must be
explicit).

### 3.5 Snapshot removal and compaction (ROADMAP 4)

Intervals are stored as **snapshot ids**, never as positions, and resolved at
query time against the lineage's *current* snapshot list:

- *existed in* = the existing snapshots with ids in `[first_seen, last_seen]`;
  if there are none, the row is unprovable and hidden (garbage for
  compaction);
- *last seen* = the newest of those; *gone in* = the first existing snapshot
  after `last_seen`;
- two rows of one path whose gap contains no existing snapshot merge into one.

Example: a path seen in #10-#12, gone in #13. Remove #12: the answer becomes
"last seen #11, gone #13", which is exactly what the remaining data supports.
So `snapshots remove` (any position) needs no index rewrite at all and stays as
fast and crash-safe as today. **Removing the tip** (newest) needs the live
part rebuilt from the new newest snapshot, and graveyard rows with
`last_seen` ≥ the new newest id are alive again (filtered at query time, fixed
at the next merge). **Compaction** (`snapshots compact`) rewrites segments
without unprovable rows and merges intervals. Each index file records the
snapshot ids and data files it was built from; a mismatch with the store
metadata triggers a rebuild of that lineage from the deltas (≈0.5 µs per op,
measured) instead of a wrong answer.

## 4. RAM vs mmap vs rebuild

Measured on a 6.2 GB arena file in the page cache (warm): `mmap` itself 0.02 ms;
**first scan through the mapping 168-177 ms** (page-fault cost ≈ 27 ms/GB),
later scans 50 ms; `read()` into the heap **763-793 ms** (≈ 124 ms/GB), then
50 ms per scan. The very first run after writing (partly cold) took 690 ms
through `mmap`. Reading 51 MB of arena back: 9 ms.

Startup alternatives for the live part (per million entries):

| source | startup cost | RAM | extra disk |
|---|---|---|---|
| rebuild from the newest full snapshot (decodes all columns) | **78 ms/M single reader thread** (measured, 209 ms for 2.69 M) | 24 MB/M hot | 0 |
| persisted zstd name column, decompressed into RAM | ≈ 1.8 GB/s per core of names (measured), ≈ 11 ms/M one core, ≈ 1-2 ms/M parallel | 24 MB/M hot | ≈ 3.5-4 MB/M |
| persisted uncompressed arena, `mmap` | ≈ 0 (first query pays ≈ 27 ms/GB of page faults) | page cache, evictable | ≈ 23 MB/M |
| compressed-resident (decompress on every query) | 0 | ≈ 3.5-7 MB/M | ≈ 3.5-4 MB/M |

Trade-offs:

- An uncompressed `mmap`ed arena is the "fastest start" and its pages are
  clean and evictable (good on a struggling machine), but it doubles the store
  (23 B/entry vs 9.7 B/entry for a real snapshot), which the footprint budget
  and the "store must not become the next space problem" rule argue against.
- A persisted zstd name column costs ≈ 35-40 % of a real snapshot on disk
  (**estimate**: 3.3 B names + depth/first_seen/flags) and gives a 50 M-name
  start in ≈ 60-100 ms in parallel (**estimate** from the measured
  decompression rates). A one-shot `diskdrift find` can stream-decompress and
  scan it without loading anything: ≈ 60 ms for 53 M names (measured), plus
  reading ≈ 3.5 B/entry from disk.
- Rebuilding from snapshots needs no new files, but at 78 ns/entry a 50 M-entry
  store costs ≈ 4 s CPU (≈ 0.5 s on 8 threads, **estimate**). Acceptable for
  the TUI in the background, not for an instant CLI query. It stays the
  recovery path when the index is missing or stale.
- Lazy loading per disk: lineages are independent, so the TUI can load the
  connected disks first and the disconnected archives after, and drop
  rarely-used archives to the compressed-resident mode under memory pressure.

Spec tension: spec section 8 says *"Never require an in-memory map of every
file."* A hot search arena is one. The rule should be clarified (spec change,
same commit as the ADR) to: scan, diff and storage never require it; search
may keep a bounded, optional name index with a streaming fallback (the
compressed-resident mode, or the plain scan of snapshots and deltas). A memory
budget for search should be added to section 2A (proposal below).

## 5. Recommended design

### 5.1 Data structures (in RAM)

Per lineage (disk + root + scan options):

```text
LiveIndex   names: Vec<u8>        folded keys, NUL-separated, canonical order   ~19 B/entry
            parent: Vec<u32>      entry ordinal of the parent directory          4 B/entry
            sparse: Vec<u32>      arena offset of every 64th entry               ~0.06 B/entry
            first_seen: RLE<u32>  snapshot id                                    ~0
            unknown_dirs: Vec<u32> ordinals of CONTENTS_UNKNOWN directories       few
            snapshot: id + data file + per-block record counts (from the .dds index)
GoneIndex   same columns for graveyard rows + parent path ids into a small dir table,
            kind, size, mtime, first_seen, last_seen, flags                       ~30 B/row
```

- Entry ordinal = record ordinal in the newest snapshot, so a hit is turned
  into a full record (original name, size, mtime, flags) by locating its block
  from the per-block counts and decoding one 256 KiB block. The first screen of
  results (≈ 50 hits) costs a handful of block decodes (**estimate** ≤ 10 ms).
- The hit → entry mapping uses the sparse offsets plus a NUL count within at
  most 64 names, instead of a 4 B offset per entry.
- Path queries (`docs/wazna`, `**/2014/*.pdf`) match the last component in
  the arena, then walk `parent` to verify the directory components.

### 5.2 Query engine

1. Fold the query exactly like the keys. Plain text → `memmem`; glob → regex
   with `[^\x00]`; regex → `regex-automata` meta regex (which extracts literal
   prefilters itself).
2. Split all live arenas into ≈ 64 chunks at NUL boundaries, scan in parallel
   (rayon), stop collecting at the result cap but keep counting.
3. Nothing found → scan the graveyard arenas the same way (they are small),
   group rows by path, resolve intervals against the snapshot list (3.5), and
   answer "last seen in #12 (3 Oct 14:20), gone in #13 (10 Oct 09:05)". With no
   index on disk: fall back to scanning deltas newest first, stoppable, as the
   ROADMAP describes.
4. Type-ahead refinement (2.2) and a per-query cancellation token, so a stale
   keystroke never blocks the next.

### 5.3 On-disk layout (derived cache, not snapshot format)

```text
<store>/index/index.json                         format marker + index version
<store>/index/<lineage>/live.<snapshot-stem>.ddx  name column (folded), depth deltas,
                                                  first_seen RLE, flags; zstd 256 KiB
                                                  blocks aligned with the .dds blocks;
                                                  header: snapshot id, data file, items, CRC
<store>/index/<lineage>/gone.<snapshot-stem>.ddx  graveyard segment(s), same container
```

- Same container conventions as `.dds` (magic, kind byte, per-block CRC,
  block index, trailer), temp → fsync → rename.
- It is a cache: deleting `index/` loses nothing; a version mismatch or a
  stale header rebuilds it from the snapshots. The `.dds`/`store.json` formats
  stay at version 2, so no snapshot migration. **It still needs an ADR, a
  version for `index/`, benchmarks against the 2A budgets, and these store
  changes:** orphan cleanup in `lock()` extended to `index/`; `size_on_disk`
  and `info` must count `index/` (the store reports its own size, spec 2A);
  `remove`/compact/tip-removal hooks as in 3.5.
- Alternative kept for the benchmark (E5): a snapshot format v3 that stores the
  names column (and depth) as a separate zstd frame inside each block, so names
  can be read without decoding the other columns and the live index needs no
  extra disk. That one *is* a format change (version bump; old v2 files stay
  readable; files are rewritten naturally at the next rebase). Prefer it over
  the sidecar only if it costs < 3 % snapshot size and the sidecar overhead
  turns out to matter.

### 5.4 Build cost at scan time

- Live part: the writer already sees every record in canonical order
  (`SnapshotWriter::push`); folding plus zstd of the names costs ≈ 33 ns/entry
  (measured: 51 MB of names compressed in 83-89 ms for 2.67 M entries), i.e.
  ≈ 0.3 s per 10 M entries against a scan of ≈ 80-100 s per 10 M (ADR 0001
  acceptance run: 8.7 M in 82 s): well under the 1.3× ingest budget. The
  writer's RSS grows by one block buffer, not by the index.
- Lifetime segment: O(changes), from the rebase op stream; in keyframe cases
  one extra merge pass (0.13-0.19 s per 1 M, ADR 0001).
- Initial build for an existing store (migration): one pass over each newest
  snapshot (78 ns/entry) plus one pass over every delta (0.5 µs/op).

### 5.5 Expected numbers (to be confirmed by section 7)

| | 10 M entries | 50 M entries |
|---|---:|---:|
| RAM, hot live index | ≈ 240 MB | ≈ 1.2 GB |
| RAM, compressed-resident | ≈ 40-70 MB | ≈ 200-350 MB |
| disk, live index | ≈ 35-40 MB | ≈ 175-200 MB |
| query, hot, M2 Max 12 cores | ≈ 2 ms | **≈ 8-9 ms (measured at 53 M)** |
| query, hot, base M1/M2 | ≈ 4-6 ms | ≈ 15-30 ms |
| query, compressed-resident | ≈ 12 ms | ≈ 60 ms (measured at 53 M) |
| CLI one-shot `find`, warm | ≈ 20-30 ms | ≈ 80-120 ms |
| history query, graveyard of 100 scans at 1 % | ≈ 1-2 ms | ≈ 5-10 ms |

Proposed spec 2A budgets (to be validated like the others): search query p95
≤ 10 ms at the 3 M-entry reference workload and ≤ 50 ms at 50 M entries across
all disks; index on disk ≤ 5 B/entry; a ceiling for the hot index of
≤ 25 B/entry and ≤ 1 GB or 10 % of physical RAM, whichever is smaller (beyond
it, disconnected disks switch to compressed-resident). "Fastest in the world" is a
marketing claim; what can be promised is a measured number on named hardware.

## 6. Prototype (throwaway)

Lives only in the session scratchpad (not in the repo), to be ported into
`benches/diskdrift-bench` when the work starts:

- `srchbench` (main): arena build, sequential/parallel `memmem`, regex,
  trigram index (sort-based build, delta+varint and Roaring sizes, intersect +
  verify queries), FST of distinct names, zstd of names.
- `hist`: uses `diskdrift-storage` itself (path dependency, store in the
  scratchpad): writes the real tree as 10 snapshots with `commit_and_rebase`,
  rebuilds the arena from the newest snapshot, scans the reverse deltas'
  ops for a name.
- `mm`: `mmap` vs `read()` startup and first-touch cost.
- `zs`: parallel decompress-and-scan of zstd name blocks (block size, level).

## 7. Benchmark plan

A new group `search` in `benches/diskdrift-bench` (same harness: fresh child
process per operation, median and p95 over ≥ 5 trials, peak RSS from
`wait4`). Corpora: the real-name corpus from the ADR's real trees
(`/System/Library`, `/Applications`, `~/Library`) replicated to 1/10/50 M with
per-replica perturbation; a synthetic Polish/diacritics corpus (NFC and NFD);
a cache-churn history (hash names).

| id | experiment | decides | threshold |
|---|---|---|---|
| E1 | arena scan: substring, glob, regex; 1/2/3/5/10-char queries; 1..N threads; 1/10/50 M | whether the flat arena is enough | p95 ≤ 10 ms at 50 M on the reference Mac, ≤ 30 ms on a base M-series; first keystroke with result cap ≤ 50 ms |
| E2 | hit → record: block lookup + decode for the first 50/500 hits | result rendering cost | ≤ 20 ms for the first screen |
| E3 | build at scan time: writer tee, folding, zstd | scan overhead | ≤ 3 % end-to-end; ≤ 5 B/entry on real trees |
| E4 | startup: rebuild vs zstd sidecar vs mmap; warm, and cold after reboot | persistence choice | searchable ≤ 0.5 s warm at 50 M (TUI); CLI `find` ≤ 150 ms warm at 50 M |
| E5 | snapshot v3 with split names frame vs sidecar | format change or not | adopt v3 only if ≤ 3 % larger snapshots and names-only decode ≥ 3× faster than full decode |
| E6 | trigram (delta+varint, streamvbyte), name dictionary, FST | secondary structures | adopt only if ≥ 3× better p95 for ≤ +25 % memory where E1 misses |
| E7 | history: 100 snapshots × 1 % churn at 1/10 M, plus cache churn; segment merge; removal of interior and tip snapshots; keyframes; partial scans | graveyard size and correctness | ≤ 0.5 MB/scan per 10 M on disk at 1 % churn; query ≤ 20 ms; exact lifetimes vs a brute-force reconstruction of every snapshot |
| E8 | Unicode: NFC/NFD, case, diacritics (`ł`, `ø`, `ß`), invalid UTF-8, exFAT/NTFS-style names | folding rules | no misses on the test vectors |
| E9 | memory: RSS of the TUI with 10/50 M entries hot and compressed-resident | budget | within the proposed 2A search budget |

Re-run E1 and E4 after any change to the arena layout or folding; record the
numbers in the ADR like ADR 0001 does.

## 8. Open questions

- Persisted sidecar (≈ 3.5-4 B/entry more disk) or snapshot format v3 (split
  names frame, no extra disk, a real format change)? E4/E5 decide.
- Diacritic-insensitive matching by default? (Recommended yes; needs a product
  decision and a toggle.)
- Search memory budget in spec 2A, and the clarification of spec 8's "never
  require an in-memory map of every file".
- Graveyard retention for churny trees: follow snapshot retention only, or a
  per-lineage cap, or collapse configured cache paths?
- Linking lineages of the same disk across option changes (needs the disk ID,
  ROADMAP 3.1b) so history does not restart when exclusions change.
- Live search on connected disks for files newer than the last scan: bounded
  how (time, depth), and whether FSEvents (deferred) changes the answer.
- Duplicate results from overlapping roots of the same disk (`/` and
  `~/Projects` scanned separately): dedupe by disk + path, prefer the newest.
- Does a resident index belong in a long-running process (TUI, a future
  menu-bar agent) only, with the CLI always using the compressed one-shot
  path?

## Sources

Repository: [ADR 0001](../adr/0001-storage-engine.md),
[spec](../../specs/DiskDrift_Spec_v0.1.md) sections 2A, 7, 8, 12,
[ROADMAP](../../ROADMAP.md) sections 3-4,
`crates/diskdrift-storage/src/{store,codec,container}.rs`,
`crates/diskdrift-core/src/{record,delta}.rs`.

Tools:

- [voidtools Everything FAQ](https://www.voidtools.com/faq/)
- [Everything forum: database structure, strstr search (p=60262)](https://www.voidtools.com/forum/viewtopic.php?p=60262)
- [Everything forum: byte-code search (p=6515)](https://www.voidtools.com/forum/viewtopic.php?p=6515)
- [Everything forum: parent pointers, RAM per file (t=11234)](https://www.voidtools.com/forum/viewtopic.php?t=11234)
- [Everything forum: RAM per index and window (t=9024)](https://www.voidtools.com/forum/viewtopic.php?t=9024)
- [Everything forum: search time per million files (t=8848)](https://www.voidtools.com/forum/viewtopic.php?t=8848)
- [Everything forum: presorted indexes (p=51415)](https://www.voidtools.com/forum/viewtopic.php?p=51415)
- [Everything indexes](https://www.voidtools.com/support/everything/indexes/)
- [Everything 1.5 content indexing (t=10471)](https://www.voidtools.com/forum/viewtopic.php?t=10471)
- [plocate homepage](https://plocate.sesse.net/)
- [plocate(1)](https://manpages.debian.org/testing/plocate/plocate.1.en.html)
- [plocate README](https://sources.debian.org/src/plocate/1.1.25-1/README/)
- [plocate database-builder.cpp](https://codeberg.org/RivuletCedar/plocate/raw/branch/main/database-builder.cpp)
- [plocate-build.cpp](https://codeberg.org/RivuletCedar/plocate/raw/branch/main/plocate-build.cpp)
- [mlocate.db(5)](https://linux.die.net/man/5/mlocate.db)
- [GNU locatedb(5)](https://manpages.debian.org/testing/locate/locatedb.5.en.html)
- [GNU findutils manual, database format](https://ftp.gnu.org/old-gnu/Manuals/findutils-4.1/html_chapter/find_5.html)
- [FreeBSD locate.code.c](https://raw.githubusercontent.com/freebsd/freebsd-src/main/usr.bin/locate/code/locate.code.c)
- [FreeBSD locate(1)](https://man.freebsd.org/cgi/man.cgi?query=locate&sektion=1)
- [macOS com.apple.locate.plist](https://opensource.apple.com/source/shell_cmds/shell_cmds-175/locate/locate/com.apple.locate.plist.auto.html)
- [Eclectic Light: locate on macOS](https://eclecticlight.co/2018/03/24/find-files-fast-with-locate/)
- [fd](https://github.com/sharkdp/fd)
- [ripgrep](https://github.com/BurntSushi/ripgrep)
- [Eclectic Light: a deeper dive into Spotlight indexes (2025)](https://eclecticlight.co/2025/07/30/a-deeper-dive-into-spotlight-indexes/)
- [Eclectic Light: how Spotlight works (2021)](https://eclecticlight.co/2021/01/28/spotlight-on-search-how-spotlight-works/)
- [spotlight_parser](https://github.com/ydkhatri/spotlight_parser)
- [Apple: Spotlight query format](https://developer.apple.com/library/archive/documentation/Carbon/Conceptual/SpotlightQuery/Concepts/QueryFormat.html)
- [FSearch](https://github.com/cboxdoerfer/fsearch)
- [FSearch entry struct](https://github.com/cboxdoerfer/fsearch/blob/c20e6b17bf4b56ddf8b56bf021429c5088346ff8/src/fsearch_database_entry.c)
- [FSearch parallel search](https://github.com/cboxdoerfer/fsearch/blob/c20e6b17bf4b56ddf8b56bf021429c5088346ff8/src/fsearch_database_index_store.c)
- [FSearch wiki: database update](https://github.com/cboxdoerfer/fsearch/wiki/Database-update)
- [Alfred and Spotlight](https://www.alfredapp.com/help/troubleshooting/indexing/spotlight/)
- [HoudahSpot](https://www.houdah.com/houdahSpot/)
- [Raycast manual: Spotlight](https://manual.raycast.com/troubleshooting/spotlight)
- [Raycast blog: the new Raycast](https://raycast.com/blog/the-new-raycast)
- [Find Any File](https://findanyfile.app/)
- [Thomas Tempel: APFS and fast catalog search](https://blog.tempel.org/2017/07/apfs-and-fast-catalog-search.html)

Data structures and algorithms:

- [Andrew Gallant: Index 1,600,000,000 keys with automata and Rust (fst)](https://burntsushi.net/transducers/)
- [Chambi et al.: Better bitmap performance with Roaring bitmaps](https://arxiv.org/abs/1402.6407)
- [Lemire et al.: Consistently faster and smaller compressed bitmaps with Roaring](https://arxiv.org/abs/1603.06549)
- [Lemire et al.: Stream VByte](https://arxiv.org/abs/1709.08990)
- [Lemire, Boytsov: Decoding billions of integers per second through vectorization](https://arxiv.org/abs/1401.6399)
- [TurboPFor](https://github.com/powturbo/TurboPFor-Integer-Compression)
- [Gog et al.: compressed suffix arrays and document retrieval (sdsl)](https://ar5iv.arxiv.org/html/1311.1249)
- [Russ Cox: Regular expression matching with a trigram index](https://swtch.com/~rsc/regexp/regexp4.html)
- [memchr crate](https://github.com/BurntSushi/memchr)
- [memchr vs StringZilla (third-party throughput)](https://github.com/ashvardanian/memchr_vs_stringzilla)

Unicode and file systems:

- [Apple QA1173: HFS+ decomposition](https://developer.apple.com/library/archive/qa/qa1173/_index.html)
- [Apple TN1150: HFS Plus volume format](https://developer.apple.com/library/archive/technotes/tn/tn1150.html)
- [Apple: APFS FAQ (normalization)](https://developer.apple.com/library/archive/documentation/FileManagement/Conceptual/APFS_Guide/FAQ/FAQ.html)
- [Apple File System Reference (PDF)](https://developer.apple.com/support/downloads/Apple-File-System-Reference.pdf)
- [Microsoft: exFAT specification](https://learn.microsoft.com/en-us/windows/win32/fileio/exfat-specification)
- [Microsoft: naming files](https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file)
- [Microsoft: case sensitivity](https://learn.microsoft.com/en-us/windows/wsl/case-sensitivity)

Not verified (no primary source found): Spotlight/`mdfind` query latency and
typical index size; FSearch latency and RAM benchmarks; absolute memchr GB/s
from its author; plocate benchmarks beyond its homepage (hardware and cache
state unstated).
