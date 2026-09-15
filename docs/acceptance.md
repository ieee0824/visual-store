# MVP acceptance status

Checked on 2026-09-15 with Rust 1.97.0 on macOS. The test command is:

```bash
cargo test --locked --offline --features fault-injection
```

| ID | Status | Evidence |
| --- | --- | --- |
| T01 | Passed | Initialization is idempotent, keeps the store ID, rejects unrelated nonempty directories, and detects partial initialization. |
| T02 | Passed | Put, info, and get preserve dimensions, decoded samples, accepted metadata, and verification hashes. |
| T03 | Passed | A synthetic uncompressed 800×600 RGB fixture shrinks and round-trips exactly. |
| T04 | Passed | Incompressible synthetic data never produces a stored file larger than its input. |
| T05 | Passed | RGB/RGBA and zero-alpha pixels with nonzero hidden RGB round-trip exactly. |
| T06 | Passed | Every PNG filter and split contiguous IDAT chunks round-trip. |
| T07 | Passed | Common color/physical/text chunks and unknown safe-to-copy chunks are retained; unsafe unknown chunks fail explicitly. |
| T08 | Passed | Grayscale, palette, grayscale-alpha, 16-bit, interlaced PNG, and APNG fail with `E_UNSUPPORTED_IMAGE`. |
| T09 | Passed | CRC corruption, truncation, trailing data, invalid filter, noncontiguous IDAT, oversized expansion, and every configured limit fail without an image record. Mutation/truncation loops exercise parser boundaries. |
| T10 | Passed | Repeated registration creates separate image rows and one blob. |
| T11 | Passed | Sequential and four-process retries with one operation ID reuse the same image; changed content fails with `E_CONFLICT`. |
| T12 | Passed | Source bytes are retrievable only after `--keep-source`; stored and source blobs share when identical. |
| T13 | Passed | Input bytes and mtime remain unchanged; a test-only concurrent mutation is detected as `E_SOURCE_CHANGED`. |
| T14 | Passed | Four child processes register concurrently without missing references and share one blob. |
| T15 | Passed | Child processes are forcibly terminated before/after blob publication and before/during/after DB commit; committed records remain retrievable and incomplete records are absent. |
| T16 | Passed with simulation | A real SQLite lock exercises the five-second timeout. Permission bits are tested as a non-root user. `ENOSPC` is injected at persistence boundaries; a physically full volume was not created. Existing records and inputs remain intact. |
| T17 | Passed | Cursor paging excludes newly registered rows and returns each original row once. Filtered paging retrieves a sparse old run from 30,000 newer records, and its query plan uses `images_by_run_seq`. |
| T18 | Passed | Cross-store references and cursors, filter-mismatched cursors, malformed cursors, and invalid limits fail explicitly. |
| T19 | Passed | Deleted and corrupted blobs make get/verify fail; no repair or deletion occurs. |
| T20 | Passed | Existing files, valid symlinks, dangling symlinks, and a symlinked managed export directory are not overwritten or followed. |
| T21 | Passed | CLI output validates against `docs/cli.schema.json`, respects command budgets, and contains neither PNG signatures encoded as Base64 nor data URLs. List coverage includes 20- and 100-item limits, JSON-escaped metadata, and lossless cursor continuation under the 16 KiB stdout budget. |
| T22 | Passed | An offline copy of the entire closed store resolves references, verifies, and materializes an image. |
| T23 | Passed | Verification succeeds with an empty `PATH`; no FFmpeg or external image command is invoked. |
| T24 | Baseline established | A fixed version-1 store with a stable ref, blob hash, PNG, and SQLite index is copied and exercised by get/verify. Future dependency updates must keep this test passing. No dependency update has occurred since the baseline was created. |

## Manual integration still required

The repository Skill passes the Skill Creator structural validator. A fresh session using the actual Codex host should still verify:

1. the installed Codex version discovers the Skill;
2. a save-only prompt calls `put` and does not invoke an image display tool;
3. a prompt to inspect one reference calls `get` and then the available image viewer exactly when needed;
4. the host's captured tool results contain compact JSON for saving and contain image data only after the explicit display step.

This test is intentionally manual because it depends on host behavior and can invoke a paid remote model. No Codex session, goal, rollout, or internal database was modified by the automated suite.

Representative performance measurements are also pending. [The benchmark procedure](benchmark.md) defines the required inputs and reporting method; private working screenshots are not included in the repository.

## Temporal codec foundation acceptance

Checked on 2026-09-15 with Rust 1.95.0 and system libvpx 1.16.0 on macOS. The `VP9 required` CI job runs the same codec coverage on macOS and Linux with VP9 enabled in the normal build.

| ID | Status | Evidence |
| --- | --- | --- |
| V01 | Passed | `tests/temporal_codec.rs` passes multiple non-identical frames through one direct libvpx encoder and decoder and compares every decoded sample. |
| V02 | Passed | libvpx parses a later compressed packet as non-key. The full sequence decodes exactly in one decoder context, while that same packet fails in a fresh decoder without preceding frame state, proving a real inter-frame dependency rather than a configured label. |
| V03 | Passed | RGB and RGBA sequences round-trip exactly, including transparent and partially transparent pixels and nonzero hidden RGB at alpha 0. |
| V04 | Passed | An odd 257×5 fixture covers every byte value 0–255, one-pixel red/green/blue lines, adjacent values, and a one-pixel frame change without a changed byte after decode. |
| V08 schema/ingest boundary | Passed | `tests/migration.rs` registers repeated pixels with equal and past capture timestamps and confirms immutable, zero-based frame numbering by registration order. Eight concurrent writers receive every unique frame from 0 through 7 in one stream. |
| V09 schema/ingest boundary | Passed | A stream-aware operation retry returns the original image ID and frame number; changing only the stream conflicts. The migrated v1 fingerprint remains retryable with the same ID. Pack-transition coverage belongs to the segment publication change. |
| V13 codec boundary | Passed | The CI codec probe runs with an empty `PATH`; `ldd`/`otool` must show libvpx and must not show libavcodec, libavformat, libavutil, libswscale, or libswresample. End-to-end `pack`/`get` remains assigned to the segment and retrieval sub-issues. |
| V19 schema migration boundary | Passed | The fixed v1 fixture can be read, verified, and materialized without mutation. Explicit migration preserves the store/image UUIDs, ref, seq, source identity, metadata, hashes, original PNG bytes, and legacy operation behavior while assigning `default` stream/frame 0. |
| V20 schema migration boundary | Passed | Fault injection terminates migration after backup, journal, database, manifest, history, and cleanup boundaries—including database version 2 with manifest version 1. Normal access blocks ambiguous states; `--resume` rolls each state forward, and interrupted `--restore` returns safely to v1. |

The precise binding, baseline libvpx version, encoder configuration, reversible RGB/RGBA plane layout, licenses, and reproducible setup are recorded in [ADR 0001](adr/0001-vp9-lossless-codec.md).
