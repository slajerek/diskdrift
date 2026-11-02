# Windows and DDS2 portability benchmarks

This benchmark records production-store evidence for the DDS2/store-v4 format
freeze. It uses `diskdrift-storage::Store`, not the storage-engine candidate
adapters. On Unix, one run writes equivalent DDS1/store-v3 and DDS2/store-v4
stores and emits a row for each format. Windows emits DDS2/store-v4 only,
because DDS1 cannot represent Windows-native path components.

All generated stores and reports stay below `target/bench-results/` and
`target/bench-work/`. The harness creates output parents itself.

## Unix freeze gate

Run the full paired production workload, then gate and render it:

```bash
cargo run --release -p diskdrift-bench -- run --suite full --only production --out target/bench-results/portability-production.json
cargo run -p diskdrift-bench -- gate target/bench-results/portability-production.json --out target/bench-results/portability-gate.json
cargo run -p diskdrift-bench -- report target/bench-results/portability-production.json --out target/bench-results/portability-production.md
```

The `production-store` rows are required for both `dds1` and `dds2`, for
`import` and `read`. The gate fails if any required successful row is absent or
if DDS2 exceeds:

| Metric | Limit |
|---|---:|
| import footprint | 110% of DDS1 |
| import/read peak RSS | 110% of DDS1 |
| import/read time | 115% of DDS1 |

On macOS it also preserves ADR 0001's 256 MB RSS and 40-byte-per-entry
full-snapshot caps. The DDS2 relative limits are stricter than the existing
20% regression limit.

The JSON gate output records every compared value, calculated limit, pass/fail
status, and missing-row error. The CI job publishes the raw result, gate, and
Markdown report together with its command log.

## Windows baseline

Windows runs the production-store workload and publishes its footprint, peak
RSS, and import/read time rows with machine metadata. Windows has no fixed
performance threshold yet; these artifacts are a native store baseline and
must not be presented as Unix freeze evidence.

The portability workflow runs the paired Unix comparison and the native
Windows DDS2 workload for evidence. It does not alter any user store.

## Current evidence status

No format-freeze result is recorded in ADR 0003 until the hosted macOS Unix
gate passes. The gate fails closed when a required DDS1 or DDS2 row is absent
or any limit is exceeded; it does not waive missing evidence.
