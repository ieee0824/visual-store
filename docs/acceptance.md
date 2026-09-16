# Acceptance status

Checked on 2026-09-15 with Rust 1.95.0 and system libvpx 1.16.0 on macOS.
The principal local command is:

```bash
cargo test --locked --features fault-injection
```

CI repeats the normal VP9 path on macOS and Linux, runs the temporal benchmark with
an empty `PATH`, checks the linked libraries, and exercises a codec-free build.

## Original MVP regression audit

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
| T09 | Passed | CRC corruption, truncation, trailing data, invalid filter, noncontiguous IDAT, oversized expansion, and every configured limit fail without an image record. |
| T10 | Passed | Repeated registration creates separate image rows and one physical blob. |
| T11 | Passed | Sequential and four-process retries with one operation ID reuse the same image; changed content fails with `E_CONFLICT`. |
| T12 | Passed | Source bytes are retrievable only after `--keep-source`; stored and source blobs share when identical. |
| T13 | Passed | Input bytes and mtime remain unchanged; a test-only concurrent mutation is detected as `E_SOURCE_CHANGED`. |
| T14 | Passed | Four child processes register concurrently without missing references and share one blob. |
| T15 | Passed | Child processes are forcibly terminated around blob publication and database commit; committed records remain retrievable and incomplete records are absent. |
| T16 | Passed with simulation | A real SQLite lock covers timeout. Permission bits are tested as a non-root user. `ENOSPC` is injected at persistence boundaries; a physically full volume was not created. |
| T17 | Passed | Cursor paging excludes newly registered rows and uses `images_by_run_seq` for filtered paging. |
| T18 | Passed | Cross-store references/cursors, mismatched filters, malformed cursors, and invalid limits fail explicitly. |
| T19 | Passed | Deleted and corrupted blobs make get/verify fail without repair or deletion. |
| T20 | Passed | Existing files and symlinks are not overwritten or followed. |
| T21 | Passed | CLI output validates against schema, stays within per-command budgets, and contains no Base64 PNG or data URL. |
| T22 | Passed | A closed-store copy resolves references, verifies, and materializes an image. |
| T23 | Passed | Verification succeeds with an empty `PATH`; no external image command is invoked. |
| T24 | Passed | The fixed version-1 fixture retains its stable ref, blob hash, PNG, and SQLite behavior. |

## Temporal V01–V26 final audit

| ID | Status | Evidence |
| --- | --- | --- |
| V01 | Passed | `tests/temporal_codec.rs` sends multiple changing frames through one direct libvpx encoder/decoder context and compares every decoded sample. |
| V02 | Passed | A later packet is parsed as non-key, decodes in the original context, and fails in a fresh decoder without preceding state, proving actual inter-frame dependency. |
| V03 | Passed | RGB and RGBA sequences round-trip exactly, including partial transparency and hidden nonzero RGB under alpha zero; alpha packets are included in accounting. |
| V04 | Passed | Odd dimensions, every byte value, one-pixel color lines, adjacent values, and a one-pixel change survive codec and PNG reconstruction byte-for-byte at the sample level. |
| V05 | Passed | Reconstruction tests cover filters 0–4, split IDAT, IHDR, accepted chunks and ordering, filter rows, and the pixel/scanline/non-IDAT hashes. |
| V06 | Passed | Exact pack boundary tests cover 31, 32, 33, 64, and 65 frames, including tail segments and retrieval at every segment boundary. |
| V07 | Passed | Segment grouping splits on run, stream, frame gap, dimensions, and RGB/RGBA layout; SQL checks forbid mixed segment properties. |
| V08 | Passed | Registration order assigns immutable zero-based `(run, stream)` frame numbers, including concurrent writers and timestamps that are equal or move backward. |
| V09 | Passed | Operation-ID retries preserve the same image/frame before and after pack; a changed stream or fingerprint conflicts. |
| V10 | Passed | `get-frame` uses `images_by_stream_frame`, selects one exact observation, reads one segment, and reports segment, frame index, and decoded prefix. |
| V11 | Passed | A complete closed-store copy retrieves and verifies packed frames in a fresh process without external state. |
| V12 | Passed | Default builds enable VP9. `--no-default-features` retains v1/PNG APIs and returns `E_CODEC_UNAVAILABLE` for packed get-frame, verify, and prune. |
| V13 | Passed | Empty-`PATH` end-to-end benchmark performs pack/get/verify/prune. Binary link inspection requires libvpx and rejects libavcodec/libavformat/libavutil/libswscale/libswresample. |
| V14 | Passed | Dry-run publishes nothing, a successful retry is a no-op, committed segments are immutable, and non-beneficial candidates stay PNG. |
| V15 | Passed | Fault points cover object publication, verified segment commit, and interruption before/after the representation switch; retry preserves committed work. |
| V16 | Passed with deterministic injection | Packet, byte, reconstruction, image-count, memory, and wall-clock bounds fail explicitly. `ENOSPC` is injected at codec publication points and timeout is forced with a test-only delay. |
| V17 | Passed | Concurrent put/pack/get preserves all observations; frozen sequence plus representation compare-and-swap prevents accidental replacement, while prune holds an exclusive lock. |
| V18 | Passed | Corrupt color video, reconstruction metadata, and valid-but-wrong offsets fail get/verify; prune refuses deletion when the replacement cannot be verified. |
| V19 | Passed | Fixed v1 stores remain readable without mutation; explicit migration preserves IDs, refs, sequence, metadata, hashes, source identity, and retry semantics. |
| V20 | Passed | Backup/journal/database/manifest/history/cleanup interruption states resume or restore safely, including database-v2/manifest-v1 ambiguity. |
| V21 | Passed | Inputs are unchanged, `--keep-source` remains byte-identical after pack/prune, and reconstruction preserves all accepted PNG semantics. |
| V22 | Passed | Schema v2 is explicit and schema v1 remains archived. Put/info/get/get-frame stay under 8 KiB; list/pack/prune/verify stay under 16 KiB; outputs contain no image bytes and report `displayed:false`. |
| V23 | Passed | Identical, deterministic random, and photographic-like independent sequences stay PNG when distinct physical PNG storage is no larger; all-intra control is larger than inter-frame VP9 on the benchmark fixture. |
| V24 | Passed | `examples/temporal_benchmark.rs` records A/B/C/D capacity, complete index/reconstruction/alpha overhead, pack time, warm random retrieval latency/range, peak RSS, environment, and prune deltas; CI requires C < B and C < D. |
| V25 | Passed | Prune is explicit dry-run/apply, counts shared candidates once, uses durable tombstones, makes reported and observed file-length deltas agree, and resumes every injected interruption. |
| V26 | Automated checks passed; host check unperformed | The repository Skill structurally validates and documents separate streams, pack only after explicit completion, requested-frame-only get/display, and no automatic prune/migrate. A fresh real Codex-host workflow was not run because it can invoke a paid remote model; this omission is explicit. |

## Manual host integration not performed

A future fresh-session host check should verify all of the following together:

1. the installed Codex version discovers the Skill;
2. a save-only request calls `put`, does not display the image, and does not infer run completion;
3. an explicitly completed run calls `pack` once, without displaying every ref;
4. inspection of one numbered frame calls `get-frame` and displays only its returned path;
5. ordinary save/inspect flows never invoke `prune` or `migrate` automatically.

The omission does not weaken codec/store tests, but it means repository tests cannot
claim control over host conversation history. The Skill also states that packed-byte
savings are not image-token savings and cannot remove an image already displayed.

The implementation, limitations, reproducible measurements, and residual risk are
summarized in [the temporal final report](temporal-final-report.md). Codec binding,
plane layout, version, build setup, and licenses are recorded in
[ADR 0001](adr/0001-vp9-lossless-codec.md).
