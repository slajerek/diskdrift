# DiskDrift storage & scanner benchmarks — `full` suite

Run 2026-10-21 02:10:22 → 2026-10-21 03:22:16  
Machine: Mac14,6 · Apple M2 Max · 12 CPUs · 69 GB RAM · macOS 15.6.1 (24G90)  
Storage: apfs on  (solid state: Yes, protocol: Apple Fabric) · rustc 1.99.1 (58cf8f9c3 2026-10-13)  
Cache: Filesystem cache state cannot be controlled without root (`purge`). Storage reads directly follow their writes (warm). Scanner 'scan-first' is the first traversal of that tree in this benchmark session; 'scan' rows are repeated warm runs.

Times are the operation itself measured inside a fresh child process (median, p95 by nearest rank; with ≤5 trials p95 is the maximum). RSS is the child's peak resident memory, which includes the synthetic generator where it feeds the operation. Footprint is allocated bytes on disk.

## Synthetic generator baseline

| entries | generated | median | p95 | rate | peak RSS MB |
|---:|---:|---:|---:|---:|---:|
| 100000 | 100442 | 17.4 ms | 17.4 ms | 5.78 M/s | 4.3 |
| 1000000 | 1004508 | 178 ms | 179 ms | 5.64 M/s | 10.5 |
| 5000000 | 5023313 | 921 ms | 932 ms | 5.45 M/s | 33.5 |
| 10000000 | 10047147 | 1882 ms | 1886 ms | 5.34 M/s | 42.4 |

## Single snapshot: import, read, queries

### 100000 entries

| backend | import median (p95) | import rate | import RSS MB | footprint MB | bytes/entry | full read median (p95) | read rate | top-100 | native top-100 | prefix query |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| binary | 52.8 ms (54.3 ms) | 1.90 M/s | 12.3 | 3.5 | 34.6 | 10.7 ms (10.9 ms) | 9.39 M/s | 11.0 ms (11.2 ms) | - | 2.5 ms (2.6 ms) |
| binary-columnar | 45.0 ms (45.3 ms) | 2.23 M/s | 11.8 | 2.8 | 28.0 | 8.2 ms (8.3 ms) | 12.24 M/s | 8.7 ms (8.9 ms) | - | 1.3 ms (1.3 ms) |
| sqlite-rows | 104 ms (106 ms) | 965 k/s | 14.7 | 6.5 | 64.5 | 26.5 ms (27.8 ms) | 3.79 M/s | 29.4 ms (30.1 ms) | - | 11.8 ms (12.7 ms) |
| sqlite-indexed | 191 ms (194 ms) | 527 k/s | 19.5 | 10.7 | 107.0 | 26.7 ms (27.4 ms) | 3.76 M/s | 29.2 ms (29.4 ms) | 0.7 ms (0.7 ms) | 0.8 ms (0.9 ms) |
| sqlite-blocks | 48.7 ms (49.7 ms) | 2.06 M/s | 12.0 | 3.5 | 34.9 | 12.8 ms (12.8 ms) | 7.86 M/s | 15.4 ms (15.5 ms) | - | 6.2 ms (6.2 ms) |
| lmdb | 40.0 ms (43.0 ms) | 2.51 M/s | 13.2 | 6.4 | 63.4 | 11.8 ms (12.1 ms) | 8.49 M/s | 13.7 ms (13.8 ms) | - | 5.6 ms (5.7 ms) |

### 1000000 entries

| backend | import median (p95) | import rate | import RSS MB | footprint MB | bytes/entry | full read median (p95) | read rate | top-100 | native top-100 | prefix query |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| binary | 388 ms (391 ms) | 2.59 M/s | 18.5 | 34.5 | 34.4 | 107 ms (107 ms) | 9.43 M/s | 105 ms (106 ms) | - | 2.7 ms (2.7 ms) |
| binary-columnar | 317 ms (319 ms) | 3.16 M/s | 16.8 | 28.0 | 27.9 | 77.4 ms (77.6 ms) | 12.98 M/s | 79.2 ms (79.6 ms) | - | 2.0 ms (2.0 ms) |
| sqlite-rows | 1027 ms (1030 ms) | 978 k/s | 95.6 | 65.7 | 65.4 | 266 ms (267 ms) | 3.77 M/s | 289 ms (290 ms) | - | 137 ms (139 ms) |
| sqlite-indexed | 1965 ms (1988 ms) | 511 k/s | 96.9 | 117.5 | 117.0 | 267 ms (278 ms) | 3.77 M/s | 293 ms (307 ms) | 0.9 ms (1.0 ms) | 3.5 ms (3.5 ms) |
| sqlite-blocks | 472 ms (483 ms) | 2.13 M/s | 57.2 | 34.7 | 34.5 | 125 ms (126 ms) | 8.02 M/s | 147 ms (148 ms) | - | 68.8 ms (70.4 ms) |
| lmdb | 318 ms (321 ms) | 3.16 M/s | 78.1 | 62.6 | 62.3 | 109 ms (110 ms) | 9.26 M/s | 124 ms (125 ms) | - | 58.8 ms (60.5 ms) |

### 5000000 entries

| backend | import median (p95) | import rate | import RSS MB | footprint MB | bytes/entry | full read median (p95) | read rate | top-100 | native top-100 | prefix query |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| binary | 1902 ms (1909 ms) | 2.64 M/s | 37.8 | 174.5 | 34.7 | 534 ms (538 ms) | 9.40 M/s | 526 ms (534 ms) | - | 7.9 ms (7.9 ms) |
| binary-columnar | 1570 ms (1572 ms) | 3.20 M/s | 35.1 | 156.1 | 31.1 | 389 ms (389 ms) | 12.93 M/s | 395 ms (398 ms) | - | 6.0 ms (6.1 ms) |
| sqlite-rows | 5088 ms (5128 ms) | 987 k/s | 114.0 | 335.6 | 66.8 | 1300 ms (1300 ms) | 3.86 M/s | 1410 ms (1412 ms) | - | 669 ms (670 ms) |
| sqlite-indexed | 10.3 s (10.4 s) | 486 k/s | 121.5 | 553.7 | 110.2 | 1314 ms (1321 ms) | 3.82 M/s | 1413 ms (1413 ms) | 1.1 ms (1.2 ms) | 15.5 ms (15.6 ms) |
| sqlite-blocks | 2205 ms (2206 ms) | 2.28 M/s | 105.5 | 184.6 | 36.8 | 617 ms (623 ms) | 8.14 M/s | 714 ms (717 ms) | - | 341 ms (341 ms) |
| lmdb | 1603 ms (1604 ms) | 3.13 M/s | 354.9 | 318.8 | 63.5 | 543 ms (543 ms) | 9.26 M/s | 612 ms (655 ms) | - | 290 ms (304 ms) |

### 10000000 entries

| backend | import median (p95) | import rate | import RSS MB | footprint MB | bytes/entry | full read median (p95) | read rate | top-100 | native top-100 | prefix query |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| binary | 3837 ms (3861 ms) | 2.62 M/s | 42.5 | 358.0 | 35.6 | 1061 ms (1065 ms) | 9.47 M/s | 1071 ms (1081 ms) | - | 13.3 ms (13.3 ms) |
| binary-columnar | 3156 ms (3158 ms) | 3.18 M/s | 45.9 | 289.1 | 28.8 | 773 ms (775 ms) | 12.99 M/s | 786 ms (792 ms) | - | 10.4 ms (10.6 ms) |
| sqlite-rows | 10.3 s (10.4 s) | 972 k/s | 130.4 | 671.2 | 66.8 | 2601 ms (2608 ms) | 3.86 M/s | 2799 ms (2803 ms) | - | 1323 ms (1334 ms) |
| sqlite-indexed | 21.5 s (21.6 s) | 468 k/s | 149.1 | 1107.4 | 110.2 | 2620 ms (2632 ms) | 3.83 M/s | 2806 ms (2829 ms) | 1.0 ms (1.0 ms) | 29.4 ms (29.6 ms) |
| sqlite-blocks | 4533 ms (4538 ms) | 2.22 M/s | 123.2 | 352.4 | 35.1 | 1238 ms (1239 ms) | 8.11 M/s | 1424 ms (1424 ms) | - | 676 ms (681 ms) |
| lmdb | 3247 ms (3248 ms) | 3.09 M/s | 696.8 | 637.6 | 63.5 | 1088 ms (1094 ms) | 9.24 M/s | 1212 ms (1220 ms) | - | 579 ms (586 ms) |

## Subsequent snapshot at 1000000 entries by change rate

Delta = complete comparison against the previous snapshot + write of the changes only. Full = write the whole new snapshot. Diff = full comparison report (via delta reconstruction / of two full snapshots).

| backend | change | ops | delta write | delta MB | full write | full MB | diff (delta) | diff (full/full) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| binary | 0% | 0 | 358 ms | 0.0 | 405 ms | 34.1 | 154 ms | 132 ms |
| binary | 0.1% | 1054 | 357 ms | 0.1 | 404 ms | 34.2 | 209 ms | 132 ms |
| binary | 1% | 9936 | 370 ms | 0.4 | 404 ms | 34.1 | 221 ms | 135 ms |
| binary | 10% | 99444 | 469 ms | 3.0 | 407 ms | 34.0 | 257 ms | 145 ms |
| binary | 50% | 473782 | 878 ms | 13.4 | 414 ms | 33.1 | 387 ms | 185 ms |
| binary-columnar | 0% | 0 | 356 ms | 0.0 | 331 ms | 27.8 | 153 ms | 132 ms |
| binary-columnar | 0.1% | 1054 | 360 ms | 0.1 | 329 ms | 27.8 | 210 ms | 130 ms |
| binary-columnar | 1% | 9936 | 372 ms | 0.4 | 334 ms | 27.8 | 220 ms | 131 ms |
| binary-columnar | 10% | 99444 | 466 ms | 3.0 | 333 ms | 27.8 | 255 ms | 143 ms |
| binary-columnar | 50% | 473782 | 871 ms | 13.4 | 342 ms | 27.7 | 398 ms | 184 ms |
| lmdb | 0% | 0 | 448 ms | 0.0 | 331 ms | 72.4 | 341 ms | 317 ms |
| lmdb | 0.1% | 1054 | 449 ms | 0.2 | 325 ms | 71.3 | 389 ms | 318 ms |
| lmdb | 1% | 9936 | 460 ms | 0.8 | 328 ms | 71.3 | 402 ms | 319 ms |
| lmdb | 10% | 99444 | 551 ms | 6.5 | 330 ms | 67.1 | 459 ms | 333 ms |
| lmdb | 50% | 473782 | 933 ms | 38.8 | 337 ms | 67.1 | 684 ms | 378 ms |
| sqlite-blocks | 0% | 0 | 456 ms | 0.0 | 467 ms | 50.3 | 368 ms | 345 ms |
| sqlite-blocks | 0.1% | 1054 | 456 ms | 0.1 | 466 ms | 50.3 | 424 ms | 345 ms |
| sqlite-blocks | 1% | 9936 | 471 ms | 0.4 | 473 ms | 49.3 | 439 ms | 345 ms |
| sqlite-blocks | 10% | 99444 | 574 ms | 3.1 | 472 ms | 47.2 | 498 ms | 354 ms |
| sqlite-blocks | 50% | 473782 | 1004 ms | 13.6 | 472 ms | 36.7 | 724 ms | 402 ms |
| sqlite-indexed | 0% | 0 | 594 ms | 0.0 | 2076 ms | 117.4 | 628 ms | 629 ms |
| sqlite-indexed | 0.1% | 1054 | 598 ms | 0.0 | 2076 ms | 117.4 | 695 ms | 626 ms |
| sqlite-indexed | 1% | 9936 | 616 ms | 0.0 | 2090 ms | 117.4 | 710 ms | 642 ms |
| sqlite-indexed | 10% | 99444 | 749 ms | 0.0 | 2071 ms | 117.4 | 767 ms | 637 ms |
| sqlite-indexed | 50% | 473782 | 1304 ms | 33.6 | 2094 ms | 100.7 | 1004 ms | 680 ms |
| sqlite-rows | 0% | 0 | 595 ms | 0.0 | 1045 ms | 69.2 | 624 ms | 621 ms |
| sqlite-rows | 0.1% | 1054 | 598 ms | 0.0 | 1046 ms | 68.2 | 690 ms | 626 ms |
| sqlite-rows | 1% | 9936 | 617 ms | 0.0 | 1054 ms | 68.2 | 707 ms | 634 ms |
| sqlite-rows | 10% | 99444 | 747 ms | 17.8 | 1050 ms | 67.1 | 762 ms | 634 ms |
| sqlite-rows | 50% | 473782 | 1297 ms | 34.6 | 1061 ms | 67.1 | 1000 ms | 681 ms |

## History of snapshots at 1000000 entries, ~1% change per snapshot

| backend | strategy | snapshots | total write | median step | p95 step | footprint MB | × one full | distant diff (first↔last) | adjacent diff | read latest | list |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| binary | checkpoint+chained-delta(k=10) | 100 | 62.1 s | 621 ms | 919 ms | 384.7 | 11.16 | 835 ms | 1230 ms | 626 ms | 1.2 ms |
| binary | checkpoint+cumulative-delta(k=10) | 100 | 37.4 s | 373 ms | 395 ms | 420.3 | 12.19 | 308 ms | 300 ms | 117 ms | 1.1 ms |
| binary | checkpoint+cumulative-delta(k=30) | 100 | 40.3 s | 400 ms | 448 ms | 293.7 | 8.52 | 310 ms | 304 ms | 118 ms | 1.1 ms |
| binary | full | 100 | 39.8 s | 397 ms | 404 ms | 3474.3 | 100.78 | 228 ms | 131 ms | 109 ms | 1.1 ms |
| binary-columnar | checkpoint+chained-delta(k=10) | 100 | 61.4 s | 611 ms | 918 ms | 318.7 | 11.41 | 839 ms | 1230 ms | 618 ms | 1.2 ms |
| binary-columnar | checkpoint+cumulative-delta(k=10) | 100 | 36.6 s | 368 ms | 385 ms | 354.2 | 12.68 | 305 ms | 299 ms | 101 ms | 1.1 ms |
| binary-columnar | checkpoint+cumulative-delta(k=30) | 100 | 39.8 s | 395 ms | 444 ms | 267.2 | 9.57 | 305 ms | 301 ms | 103 ms | 1.1 ms |
| binary-columnar | full | 100 | 33.2 s | 331 ms | 340 ms | 2813.9 | 100.75 | 227 ms | 131 ms | 79.7 ms | 1.0 ms |
| lmdb | checkpoint+chained-delta(k=10) | 100 | 68.5 s | 700 ms | 994 ms | 705.3 | 11.25 | 1034 ms | 1426 ms | 714 ms | 1.1 ms |
| lmdb | checkpoint+cumulative-delta(k=10) | 100 | 44.2 s | 454 ms | 467 ms | 772.4 | 12.32 | 510 ms | 483 ms | 194 ms | 1.1 ms |
| lmdb | checkpoint+cumulative-delta(k=30) | 100 | 47.7 s | 481 ms | 526 ms | 571.0 | 9.11 | 505 ms | 482 ms | 193 ms | 1.1 ms |
| sqlite-blocks | checkpoint+chained-delta(k=10) | 100 | 72.2 s | 724 ms | 1032 ms | 386.3 | 11.13 | 1082 ms | 1507 ms | 753 ms | 0.6 ms |
| sqlite-blocks | checkpoint+cumulative-delta(k=10) | 100 | 46.5 s | 467 ms | 483 ms | 436.6 | 12.58 | 534 ms | 524 ms | 213 ms | 0.5 ms |
| sqlite-blocks | checkpoint+cumulative-delta(k=30) | 100 | 49.7 s | 495 ms | 546 ms | 302.4 | 8.71 | 533 ms | 524 ms | 215 ms | 0.6 ms |
| sqlite-blocks | full | 100 | 43.9 s | 438 ms | 446 ms | 3491.7 | 100.61 | 443 ms | 344 ms | 127 ms | 0.5 ms |
| sqlite-indexed | checkpoint+chained-delta(k=10) | 100 | 101.1 s | 938 ms | 2081 ms | 1208.0 | 10.26 | 1364 ms | 1760 ms | 892 ms | 0.6 ms |
| sqlite-indexed | checkpoint+cumulative-delta(k=10) | 100 | 75.6 s | 617 ms | 2042 ms | 1275.1 | 10.83 | 822 ms | 793 ms | 361 ms | 0.6 ms |
| sqlite-indexed | checkpoint+cumulative-delta(k=30) | 100 | 71.4 s | 663 ms | 730 ms | 755.0 | 6.41 | 826 ms | 796 ms | 362 ms | 0.6 ms |
| sqlite-rows | checkpoint+chained-delta(k=10) | 100 | 89.9 s | 932 ms | 1163 ms | 738.3 | 11.14 | 1358 ms | 1755 ms | 891 ms | 0.6 ms |
| sqlite-rows | checkpoint+cumulative-delta(k=10) | 100 | 65.1 s | 615 ms | 1001 ms | 805.6 | 12.11 | 812 ms | 796 ms | 364 ms | 0.6 ms |
| sqlite-rows | checkpoint+cumulative-delta(k=30) | 100 | 67.1 s | 658 ms | 726 ms | 554.7 | 8.38 | 812 ms | 797 ms | 360 ms | 0.5 ms |

## Crash safety and write failure

Crash: SIGKILL the writer half-way through importing snapshot 1, then verify snapshot 0 record-by-record against the generator and write another snapshot. Write failure: `RLIMIT_FSIZE` makes writes fail with EFBIG part-way (a stand-in for a full disk that does not touch the real volume).

| backend | step | detail | outcome |
|---|---|---|---|
| binary | crash-setup | import snapshot 0 | ok |
| binary | crash-kill | SIGKILL after 198 ms | killed |
| binary | crash-verify | snapshot 0 intact after kill | ok (listed: [0]) |
| binary | crash-reuse | write snapshot 2 after crash | ok |
| binary | crash-verify-2 | snapshot 2 correct | ok (listed: [0,2]) |
| binary | diskfull-setup | import snapshot 0 | ok |
| binary | diskfull-write | RLIMIT_FSIZE=17 MB | failed as intended: File too large (os error 27) |
| binary | diskfull-verify | snapshot 0 intact after failed write | ok (listed: [0]) |
| binary | diskfull-reuse | write snapshot 2 afterwards | ok |
| binary-columnar | crash-setup | import snapshot 0 | ok |
| binary-columnar | crash-kill | SIGKILL after 161 ms | killed |
| binary-columnar | crash-verify | snapshot 0 intact after kill | ok (listed: [0]) |
| binary-columnar | crash-reuse | write snapshot 2 after crash | ok |
| binary-columnar | crash-verify-2 | snapshot 2 correct | ok (listed: [0,2]) |
| binary-columnar | diskfull-setup | import snapshot 0 | ok |
| binary-columnar | diskfull-write | RLIMIT_FSIZE=13 MB | failed as intended: File too large (os error 27) |
| binary-columnar | diskfull-verify | snapshot 0 intact after failed write | ok (listed: [0]) |
| binary-columnar | diskfull-reuse | write snapshot 2 afterwards | ok |
| sqlite-rows | crash-setup | import snapshot 0 | ok |
| sqlite-rows | crash-kill | SIGKILL after 511 ms | killed |
| sqlite-rows | crash-verify | snapshot 0 intact after kill | ok (listed: [0]) |
| sqlite-rows | crash-reuse | write snapshot 2 after crash | ok |
| sqlite-rows | crash-verify-2 | snapshot 2 correct | ok (listed: [0,2]) |
| sqlite-rows | diskfull-setup | import snapshot 0 | ok |
| sqlite-rows | diskfull-write | RLIMIT_FSIZE=82 MB | failed as intended: disk I/O error |
| sqlite-rows | diskfull-verify | snapshot 0 intact after failed write | ok (listed: [0,1]) |
| sqlite-rows | diskfull-reuse | write snapshot 2 afterwards | ok |
| sqlite-indexed | crash-setup | import snapshot 0 | ok |
| sqlite-indexed | crash-kill | SIGKILL after 977 ms | killed |
| sqlite-indexed | crash-verify | snapshot 0 intact after kill | ok (listed: [0]) |
| sqlite-indexed | crash-reuse | write snapshot 2 after crash | ok |
| sqlite-indexed | crash-verify-2 | snapshot 2 correct | ok (listed: [0,2]) |
| sqlite-indexed | diskfull-setup | import snapshot 0 | ok |
| sqlite-indexed | diskfull-write | RLIMIT_FSIZE=139 MB | failed as intended: disk I/O error |
| sqlite-indexed | diskfull-verify | snapshot 0 intact after failed write | ok (listed: [0,1]) |
| sqlite-indexed | diskfull-reuse | write snapshot 2 afterwards | ok |
| sqlite-blocks | crash-setup | import snapshot 0 | ok |
| sqlite-blocks | crash-kill | SIGKILL after 235 ms | killed |
| sqlite-blocks | crash-verify | snapshot 0 intact after kill | ok (listed: [0]) |
| sqlite-blocks | crash-reuse | write snapshot 2 after crash | ok |
| sqlite-blocks | crash-verify-2 | snapshot 2 correct | ok (listed: [0,2]) |
| sqlite-blocks | diskfull-setup | import snapshot 0 | ok |
| sqlite-blocks | diskfull-write | RLIMIT_FSIZE=43 MB | failed as intended: disk I/O error |
| sqlite-blocks | diskfull-verify | snapshot 0 intact after failed write | ok (listed: [0,1]) |
| sqlite-blocks | diskfull-reuse | write snapshot 2 afterwards | ok |
| lmdb | crash-setup | import snapshot 0 | ok |
| lmdb | crash-kill | SIGKILL after 163 ms | killed |
| lmdb | crash-verify | snapshot 0 intact after kill | ok (listed: [0]) |
| lmdb | crash-reuse | write snapshot 2 after crash | ok |
| lmdb | crash-verify-2 | snapshot 2 correct | ok (listed: [0,2]) |
| lmdb | diskfull-setup | import snapshot 0 | ok |
| lmdb | diskfull-write | RLIMIT_FSIZE=78 MB | failed as intended: Input/output error (os error 5) |
| lmdb | diskfull-verify | snapshot 0 intact after failed write | ok (listed: [0]) |
| lmdb | diskfull-reuse | write snapshot 2 afterwards | ok |

## Scanner (metadata only, real trees, read-only)

- First traversal of `/Applications` (getattrlistbulk, 8 threads): 1237224 entries in 3393 ms (365 k/s).
- First traversal of `/System/Library` (getattrlistbulk, 8 threads): 433617 entries in 4669 ms (93 k/s).
- First traversal of `/Users/mars/Library` (getattrlistbulk, 8 threads): 1730172 entries in 33.2 s (52 k/s).

| lister | tree, threads | entries | median (p95) | rate | peak RSS MB |
|---|---|---:|---:|---:|---:|
| getattrlistbulk | /Applications threads=1 | 1237224 | 13.4 s (13.5 s) | 92 k/s | 15.8 |
| getattrlistbulk | /Applications threads=12 | 1237224 | 3918 ms (4134 ms) | 316 k/s | 42.2 |
| getattrlistbulk | /Applications threads=16 | 1237224 | 4500 ms (4664 ms) | 275 k/s | 46.7 |
| getattrlistbulk | /Applications threads=2 | 1237224 | 7108 ms (7200 ms) | 174 k/s | 20.1 |
| getattrlistbulk | /Applications threads=4 | 1237224 | 4104 ms (4109 ms) | 301 k/s | 28.3 |
| getattrlistbulk | /Applications threads=6 | 1237224 | 3382 ms (3386 ms) | 366 k/s | 33.9 |
| getattrlistbulk | /Applications threads=8 | 1237224 | 3248 ms (3266 ms) | 381 k/s | 37.1 |
| getattrlistbulk | /System/Library threads=1 | 433617 | 3634 ms (3653 ms) | 119 k/s | 9.7 |
| getattrlistbulk | /System/Library threads=12 | 433617 | 9834 ms (9947 ms) | 44 k/s | 15.0 |
| getattrlistbulk | /System/Library threads=16 | 433617 | 10.7 s (10.8 s) | 41 k/s | 15.9 |
| getattrlistbulk | /System/Library threads=2 | 433617 | 2199 ms (2201 ms) | 197 k/s | 9.8 |
| getattrlistbulk | /System/Library threads=4 | 433617 | 2311 ms (2319 ms) | 188 k/s | 11.5 |
| getattrlistbulk | /System/Library threads=6 | 433617 | 3497 ms (3515 ms) | 124 k/s | 12.1 |
| getattrlistbulk | /System/Library threads=8 | 433617 | 5299 ms (5315 ms) | 82 k/s | 13.5 |
| getattrlistbulk | /Users/mars/Library threads=1 | 1730176 | 53.1 s (53.5 s) | 33 k/s | 150.0 |
| getattrlistbulk | /Users/mars/Library threads=12 | 1730171 | 30.6 s (31.5 s) | 57 k/s | 242.4 |
| getattrlistbulk | /Users/mars/Library threads=16 | 1597763 | 33.4 s (35.7 s) | 48 k/s | 248.6 |
| getattrlistbulk | /Users/mars/Library threads=2 | 1730175 | 26.1 s (26.4 s) | 66 k/s | 163.6 |
| getattrlistbulk | /Users/mars/Library threads=4 | 1730175 | 25.8 s (26.0 s) | 67 k/s | 213.1 |
| getattrlistbulk | /Users/mars/Library threads=6 | 1730175 | 26.8 s (26.9 s) | 65 k/s | 216.2 |
| getattrlistbulk | /Users/mars/Library threads=8 | 1730175 | 28.3 s (31.4 s) | 61 k/s | 222.3 |
| readdir+lstat | /Applications threads=1 | 1237224 | 19.7 s (19.7 s) | 63 k/s | 15.8 |
| readdir+lstat | /Applications threads=12 | 1237224 | 5518 ms (5559 ms) | 224 k/s | 38.9 |
| readdir+lstat | /Applications threads=16 | 1237224 | 5946 ms (6002 ms) | 208 k/s | 44.2 |
| readdir+lstat | /Applications threads=2 | 1237224 | 11.0 s (11.0 s) | 113 k/s | 19.7 |
| readdir+lstat | /Applications threads=4 | 1237224 | 6223 ms (6312 ms) | 199 k/s | 26.5 |
| readdir+lstat | /Applications threads=6 | 1237224 | 5502 ms (5784 ms) | 225 k/s | 29.9 |
| readdir+lstat | /Applications threads=8 | 1237224 | 5055 ms (5165 ms) | 245 k/s | 31.6 |
| readdir+lstat | /System/Library threads=1 | 433617 | 5892 ms (6527 ms) | 74 k/s | 9.0 |
| readdir+lstat | /System/Library threads=12 | 433617 | 8789 ms (9003 ms) | 49 k/s | 14.0 |
| readdir+lstat | /System/Library threads=16 | 433617 | 9685 ms (9722 ms) | 45 k/s | 14.9 |
| readdir+lstat | /System/Library threads=2 | 433617 | 4453 ms (4557 ms) | 97 k/s | 11.0 |
| readdir+lstat | /System/Library threads=4 | 433617 | 3356 ms (3485 ms) | 129 k/s | 11.3 |
| readdir+lstat | /System/Library threads=6 | 433617 | 3877 ms (4050 ms) | 112 k/s | 12.1 |
| readdir+lstat | /System/Library threads=8 | 433617 | 5117 ms (5123 ms) | 85 k/s | 12.6 |
| readdir+lstat | /Users/mars/Library threads=1 | 1730170 | 61.9 s (62.3 s) | 28 k/s | 137.1 |
| readdir+lstat | /Users/mars/Library threads=12 | 761545 | 31.7 s (36.1 s) | 24 k/s | 226.5 |
| readdir+lstat | /Users/mars/Library threads=16 | 1730175 | 33.8 s (33.9 s) | 51 k/s | 243.9 |
| readdir+lstat | /Users/mars/Library threads=2 | 1730172 | 30.5 s (31.0 s) | 57 k/s | 157.3 |
| readdir+lstat | /Users/mars/Library threads=4 | 1730176 | 27.7 s (27.8 s) | 62 k/s | 208.4 |
| readdir+lstat | /Users/mars/Library threads=6 | 1730175 | 29.3 s (29.6 s) | 59 k/s | 218.3 |
| readdir+lstat | /Users/mars/Library threads=8 | 1730175 | 30.1 s (30.1 s) | 57 k/s | 223.9 |

### End-to-end: scan + store (getattrlistbulk, 8 threads)

| backend | tree | entries | median | rate | peak RSS MB | footprint MB | bytes/entry |
|---|---|---:|---:|---:|---:|---:|---:|
| binary | /Applications threads=8 | 1237224 | 3252 ms | 380 k/s | 57.8 | 12.5 | 10.1 |
| binary | /System/Library threads=8 | 433617 | 5372 ms | 81 k/s | 44.0 | 3.1 | 7.2 |
| binary | /Users/mars/Library threads=8 | 1730176 | 28.3 s | 61 k/s | 244.6 | 36.7 | 21.2 |
| binary-columnar | /Applications threads=8 | 1237224 | 3227 ms | 383 k/s | 62.1 | 7.3 | 5.9 |
| binary-columnar | /System/Library threads=8 | 433617 | 5512 ms | 79 k/s | 44.3 | 1.7 | 3.9 |
| binary-columnar | /Users/mars/Library threads=8 | 1730176 | 28.1 s | 62 k/s | 260.0 | 29.2 | 16.9 |
| lmdb | /Applications threads=8 | 1237224 | 3254 ms | 380 k/s | 122.3 | 83.9 | 67.8 |
| lmdb | /System/Library threads=8 | 433617 | 5395 ms | 80 k/s | 50.0 | 28.4 | 65.4 |
| lmdb | /Users/mars/Library threads=8 | 1730177 | 27.4 s | 63 k/s | 356.9 | 134.3 | 77.6 |
| sqlite-blocks | /Applications threads=8 | 1237224 | 3239 ms | 382 k/s | 65.6 | 12.6 | 10.2 |
| sqlite-blocks | /System/Library threads=8 | 433617 | 5479 ms | 79 k/s | 45.6 | 3.2 | 7.4 |
| sqlite-blocks | /Users/mars/Library threads=8 | 1730176 | 28.1 s | 62 k/s | 291.4 | 36.8 | 21.2 |
| sqlite-rows | /Applications threads=8 | 1237224 | 3383 ms | 366 k/s | 120.8 | 100.7 | 81.4 |
| sqlite-rows | /System/Library threads=8 | 433617 | 5519 ms | 79 k/s | 53.7 | 28.4 | 65.4 |
| sqlite-rows | /Users/mars/Library threads=8 | 1730176 | 28.3 s | 61 k/s | 324.7 | 134.3 | 77.6 |

