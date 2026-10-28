# Portability Phase 1 Acceptance

This document is the public acceptance contract for DiskDrift portability
phase 1. It intentionally contains no private implementation notes or links.

## Acceptance IDs

- **F1-A1:** Native names and paths round-trip losslessly on Unix and Windows,
  including invalid Unix bytes and lone UTF-16 surrogates.
- **F1-A2:** Roots, identity values, volume descriptors and registry values use
  explicit tagged schemas with deterministic ordering and unknown-field rules.
- **F1-A3:** Legacy DDS1 and store versions 1 through 3 remain readable and
  verify without writing; the older reader rejects the new store marker.
- **F1-A4:** Diff, delta, top, browse and find compare exact component keys,
  not host separator strings, and preserve coverage uncertainty.
- **F1-A5:** Native filesystem operations provide the documented lock,
  flush, publication and recovery guarantees on supported hosts.
- **F1-A6:** The new record codec has deterministic goldens, corruption
  detection, bounded memory use and no production writer before activation.
- **F1-A7:** Store-v4 migration is staged and idempotent; injected failures
  leave the previous readable store intact and recovery can finish later.
- **F1-A8:** Linux and Windows baseline adapters run their required native
  workflow smoke tests without changing user data or filesystem state.
- **F1-A9:** CLI and TUI outputs preserve observed, estimated and unknown
  measurement quality across supported hosts.
- **F1-A10:** Required macOS, Linux and Windows CI jobs pass the workspace
  checks, migration/fault tests and read-only audit from the filtered public
  tree.

## Evidence

Each acceptance ID is linked to public tests, CI jobs or benchmark artifacts
when its owning work item completes. A missing native capability is a failed
required job, not a silently skipped test.

The phase remains incomplete until all ten IDs have recorded green evidence.
