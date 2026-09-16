# Temporal compression final report

Date: 2026-09-15
Scope: issue #8 and its implementation sub-issues #14, #10, and #12

## Outcome

Visual Store format version 2 retains stable `visual://` observation references while
allowing active storage to switch from an immediately retrievable PNG to an immutable
multi-frame VP9 segment. `put` remains a batch ingest boundary; `pack` is explicit.
`get` and indexed `get-frame` reconstruct exactly one requested PNG, and `prune` is
the only operation allowed to remove verified retired PNG objects.

The required backend is implemented with direct libvpx encoder/decoder calls. It uses
lossless VP9 profile 1 with a reversible full-resolution RGB plane mapping, plus a
separate lossless alpha stream for RGBA. Multiple frames share one encoder context;
tests prove a later non-key packet needs prior decoder state. PNG reconstruction
metadata preserves accepted chunks, ordering, row filters, and all validation hashes.
FFmpeg executables and libav libraries are not used. AV1 is deliberately not
implemented: explicit `--codec av1` returns `E_CODEC_UNAVAILABLE`.

## Persistence and compatibility

- Format v1 stores remain readable without mutation; writes require an explicit,
  resumable `migrate --to 2`.
- Logical observations, active representations, segments, frame locations,
  reconstruction descriptors, typed objects, and retired representations are
separate database concepts.
- Pack submits one expanded frame at a time to a stateful encoder, and retrieval plus
  verification consume decoded output one frame at a time; whole expanded segments
  are not retained in Rust memory.
- Segment objects are content-addressed, verified, published before a short database
  transaction, and never appended after commit.
- Frame numbers are immutable registration order within `(run, stream)`; physical
  `frame_index` may differ and is not a durable user reference.
- Prune uses an exclusive lock and durable `retained → pending → deleted` tombstones.
  It validates replacements before deletion, counts shared hashes once, and resumes
  every tested interruption point.

## Measured result

The committed benchmark uses 32 deterministic 640×360 changing UI frames. On an
Apple M2 MacBook Air (8 cores, 24 GB), macOS 26.5.1 (25F80), Rust 1.95.0, libvpx
1.16.0, release mode, one codec thread, and a 32-frame segment, it recorded:

| Representation or operation | Result |
| --- | ---: |
| A: input uncompressed PNGs | 22,135,456 bytes |
| B: distinct repacked PNG objects | 89,272 bytes |
| C: complete inter-frame VP9 + reconstruction + index | 30,558 bytes |
| D: complete all-intra VP9 + reconstruction + index | 62,858 bytes |
| Pack encode and full verification | 652 ms |
| Warm frame-17 retrieval median / p95 | 24.611 / 29.369 ms |
| Required decode range | frame 0 through 17 in one segment |
| Process peak RSS | 105,398,272 bytes |
| Object file lengths before / after prune | 99,059 / 9,787 bytes |
| Allocated object blocks before / after prune | 147,456 / 16,384 bytes |
| Reported and observed reclaimed file length | 89,272 bytes |

C is 65.8% smaller than B and 51.4% smaller than D for this fixture. These values are
not generalized compression claims. Full machine-readable data and the exact method
are in [the benchmark record](benchmarks/temporal-macos-m2.json) and
[benchmark procedure](benchmark.md). CI reruns the same pass/fail invariants on macOS
and Linux, although runner timings and byte counts are not treated as stable golden
values.

## Commands for an isolated trial

Copy a closed store before experimenting; do not point these commands at the only
copy of user data. For a new isolated store, the complete path is:

```bash
trial_store="$PWD/.visual-store-trial"
vstore --store "$trial_store" init
vstore --store "$trial_store" put \
  --file artifacts/frame-000.png --run ui-check-001 --stream main-window
vstore --store "$trial_store" pack \
  --run ui-check-001 --stream main-window --codec vp9 --dry-run
vstore --store "$trial_store" pack \
  --run ui-check-001 --stream main-window --codec vp9
vstore --store "$trial_store" get-frame \
  --run ui-check-001 --stream main-window --frame 0
vstore --store "$trial_store" verify
vstore --store "$trial_store" prune --dry-run
```

Only after reviewing the isolated dry-run and confirming every reference still
materializes should a caller explicitly choose `prune --apply`. A copied v1 store must
first be backed up while closed and then explicitly upgraded with
`vstore --store PATH migrate --to 2`; opening it never migrates automatically.

## Verification and residual limits

The final audit is [V01–V26](acceptance.md). It covers frame/sample identity,
segment boundaries, retry/restart/copy behavior, schema migration, concurrency,
corruption, resource limits, codec-free behavior, A/B/C/D accounting, and crash-safe
pruning. Output validates against schema v2, archived schema v1 remains present, and
command budgets prevent image data from entering JSON responses.

One host-dependent test is intentionally unperformed: a paid fresh Codex session was
not launched to observe real Skill tool choice. Repository validation covers the
Skill structure and its required instructions, but cannot guarantee host discovery or
erase images already displayed in conversation history. The Skill therefore says to
pack only a clearly completed run, retrieve/display only the requested frame, and
never prune or migrate without an explicit request.

Codec ABI/version, build reproduction, plane layout, and license obligations are in
[ADR 0001](adr/0001-vp9-lossless-codec.md). The project remains MIT; the system
libvpx dependency is BSD 3-Clause and pinned `libvpx-native-sys` is MPL-2.0. Binary
redistributors must include notices corresponding to the actual bundled artifacts.
