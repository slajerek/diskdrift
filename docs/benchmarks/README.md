# Benchmarks (M1)

Reproduce:

```bash
cargo build --release -p diskdrift-bench
./target/release/diskdrift-bench run --suite full --out docs/benchmarks/<date>-full-results.json
./target/release/diskdrift-bench report docs/benchmarks/<date>-full-results.json --out docs/benchmarks/<date>-full-report.md
# subsets: --only scan,ops,change,history,robustness   (--append merges into an existing file)
```

- Every measured operation runs in its own child process; peak RSS comes from
  `wait4(2)` for exactly that process.
- Scratch data lives in `target/bench-work/` and is deleted at the end; scanner
  benchmarks only *read* real trees.
- Synthetic trees (`benches/diskdrift-bench/src/synth.rs`) are deterministic:
  log-normal fan-out and file sizes, random names (which compress worse than real
  names — real-tree end-to-end rows show the difference), and versioned changes
  (hot files grow/shrink, deletions, creations, in-place renames, ~0.5% hardlinks)
  calibrated to a target change fraction.
- Cache state cannot be controlled without root; see the note in each report.

Helper experiments (`examples/`): `lister_only` (lister cost without the
walker), `codec_levels` (row vs columnar codec across zstd levels on synthetic
and real data), `production_history` (the shipped store's history policy),
`disks_cost` (ADR 0002: store version 3 migration, the root-rename adapter in
diff and rebase, name search without an index; results in
[`2026-10-22-disks-results.jsonl`](2026-10-22-disks-results.jsonl)),
`search_cost` (roadmap step 5: `diskdrift find` through the shipped search
service, the newest scan and the history from the deltas, on a store built by
`disks_cost build`; results in
[`2026-10-22-search-results.jsonl`](2026-10-22-search-results.jsonl)).
Removal and compaction (roadmap step 7) were timed with the release CLI on a
copy of that store (`diskdrift --db <copy> snapshots remove 10 … 19 --yes`,
then the same one call per scan on a fresh copy, `snapshots remove 30`,
`snapshots compact`); results in
[`2026-10-23-prune-results.jsonl`](2026-10-23-prune-results.jsonl).

```bash
cargo build --release -p diskdrift-bench --examples
B=target/release/examples/disks_cost W=target/bench-work/disks
$B build 3000000 30 $W/s && $B migrate $W/s && $B verify $W/s && $B diff $W/s
$B adapter 3000000 $W/a && $B search $W/s abc
target/release/examples/search_cost $W/s abc history   # also: zzzzqqq, 're:^ab[0-9]', '*.x*', e
```

Real-world acceptance run (whole disk, this Mac): 8.72M entries in 82 s, stored
in 84.9 MB, peak RSS 225 MB — details in the ADR.

Results for review:

- [`2026-10-21-full-report.md`](2026-10-21-full-report.md) — generated tables
- [`2026-10-21-full-results.json`](2026-10-21-full-results.json) — raw rows
- Decision and analysis: [`../adr/0001-storage-engine.md`](../adr/0001-storage-engine.md)
