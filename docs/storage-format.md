# Storage format version 3

```text
STORE/
  store.json
  index.sqlite3
  index.sqlite3-wal       # may exist while in use
  index.sqlite3-shm       # may exist while in use
  objects/sha256/ab/cd/FULL_SHA256.png
  objects/sha256/ab/cd/FULL_SHA256.vpxs
  objects/sha256/ab/cd/FULL_SHA256.pngr
  tmp/
  exports/
  migration-v1-to-v2.json        # only while migration is incomplete
  migration-v1-backup.sqlite3    # only while migration is incomplete
  migration-v2-to-v3.json        # only while migration is incomplete
  migration-v2-backup.sqlite3    # only while migration is incomplete
```

`store.json` contains only `store_id` and `format_version`. SQLite stores the same ID in `store_meta`; disagreement is an integrity error. Version 3 requires both the manifest version and `PRAGMA user_version` to equal 3. A fresh schema is composed from [002.sql](../migrations/002.sql) and additive [003.sql](../migrations/003.sql). The v1-to-v2 sources remain [001-to-002-prepare.sql](../migrations/001-to-002-prepare.sql) and [001-to-002-copy.sql](../migrations/001-to-002-copy.sql); version 1 is documented by immutable [001.sql](../migrations/001.sql).

## Logical and physical model

The schema deliberately separates a captured observation from its current storage representation:

| Relation | Role |
| --- | --- |
| `images` | Stable observation identity, metadata, source hashes, run/stream/frame identity, and versioned operation fingerprint. |
| `representations` | Exactly one active physical representation for an observation. Version 2 migrations create PNG representation version 1. |
| `segments` | Immutable multi-frame codec objects and their codec descriptor. |
| `frame_locations` | An observation's frame index and decode start within a segment. Bounds are enforced by checks and triggers. |
| `png_reconstruction` | Versioned reconstruction metadata needed to reproduce a PNG from a non-PNG representation. The descriptor is an immutable typed blob and may be shared by content hash. |
| `blobs` | Immutable SHA-256-addressed bytes with an explicit object kind: PNG, VP9 bitstream, or PNG reconstruction descriptor. |
| `retired_representations` | Superseded physical representations and their pruning state; observation rows are not replaced. |
| `judgments` | Append-only external decisions for an image, including producer/model provenance, typed JSON value, optional probability/confidence, metadata, and creation time. |
| `schema_migrations` | Durable history of explicit schema migrations. |

`images.image_id`, `seq`, user metadata, source identity hashes, and `(run, stream, frame_no)` are observation properties. Repacking or changing the active representation must not change them. A database uniqueness constraint protects `(run, stream, frame_no)`. Registrations with a run allocate the next frame number in the same `BEGIN IMMEDIATE` transaction that inserts the observation. Registrations without a run have null stream and frame number.

`judgments.seq` is an internal append sequence used for stable pagination; `judgment_id` is the external UUID. The foreign key binds each judgment to one image while permitting any number of kinds and producers per image. `value_json` and `metadata_json` are canonical compact JSON written by the application. Exact value search compares that canonical encoding. `probability` and `confidence` are independently nullable and constrained to 0 through 1; the producer defines their semantics. The image and blob relations contain no Jev-specific column or trigger.

The stored PNG preserves IHDR values, the byte-exact decompressed filtered scanlines, decoded RGB/RGBA samples including hidden color under transparent pixels, and every accepted non-IDAT chunk in content and relative order. IDAT boundaries and compressed bytes may change. Recompression uses the selected zlib level and is adopted only when the candidate is smaller and round-trip validation succeeds.

Version 2 accepts static, non-interlaced, 8-bit RGB/RGBA PNG. It validates the PNG signature, chunk lengths and types, reserved bits, CRCs, critical ordering, contiguous IDAT stream, bounded zlib termination, filter bytes, and decoded samples. Unknown critical chunks and unknown ancillary chunks marked unsafe to copy are rejected. Accepted ancillary chunks are preserved and never interpreted as instructions.

The canonical reconstruction descriptor is specified in [ADR 0002](adr/0002-png-reconstruction.md). It preserves original IHDR, every accepted non-IDAT chunk in order and on the same side of IDAT, plus one original filter type per row. After codec decode, Visual Store recreates filtered scanlines from packed samples and those filters, verifies the unchanged pixel, scanline, and non-IDAT hashes, rebuilds a PNG, and passes it through the same strict validator before publication. RGBA reconstruction includes alpha and nonzero hidden RGB beneath alpha zero. Rebuilt zlib bytes and IDAT boundaries are not required to match the pre-pack file; a retained source blob remains byte-identical.

Temporal packing stores each color or alpha stream in the bounded `VSVP9` packet
container specified by [ADR 0003](adr/0003-temporal-segments.md). A complete candidate
is encoded and decoded outside a write transaction. Immutable objects are synced
before one short transaction inserts typed blobs, segment and frame mappings, retires
the old PNG representation, and activates the new representation. A crash before
commit can leave only complete unreferenced candidates; a crash after commit leaves
the entire segment active. Retired PNG objects are not deleted by pack.

Pack retains candidate metadata but expands, validates, and submits only one PNG frame
at a time to two stateful libvpx contexts at most (color plus alpha). Full persisted
verification decodes and reconstructs one display frame at a time. Codec reference
surfaces remain native allocations; the conservative memory estimate accounts for
them but is not a hard allocator cap.

Retrieval resolves `frame_locations` by image identity (or the indexed
`images_by_stream_frame` key), loads only the named segment, and decodes its prefix
from `decode_start_index` through `frame_index`. Reconstruction must pass all stored
observation hashes and the strict PNG validator before a temporary export is
published. Verification decodes each segment once and validates all mapped frames.
Builds without the optional default `vp9` feature can still read metadata, active
PNGs, and retained sources; video retrieval reports `E_CODEC_UNAVAILABLE` and verify
marks video segments explicitly unverified.

Pruning is the only operation that removes retired representation files. It holds an
exclusive store-directory lock for the whole operation and uses the persisted
`retained → pending → deleted` state machine described in
[ADR 0004](adr/0004-prune-protocol.md). A `deleted` retired row and its blob metadata
remain as a tombstone, while the PNG object itself is expected to be absent. Capacity
queries use distinct blob hashes so shared objects are counted once per category.

## Blob publication order

1. Read and freeze the source under configured bounds; detect source changes.
2. Validate and repack outside a database write transaction.
3. Write the candidate to a mode-0600 temporary file and sync it.
4. Atomically publish the immutable blob with a same-filesystem hard-link operation that cannot overwrite an existing path.
5. Verify a concurrent winner by size and SHA-256 and sync the blob directory.
6. In a short `BEGIN IMMEDIATE` transaction, insert or validate the blob, allocate the frame number, and insert the observation and active representation.
7. Return success only after the database commit.

This order permits an unreferenced complete blob after a crash. It prevents a successfully committed image from pointing to a partially written blob. `verify` reports unreferenced files without deleting them because another process may be registering the same blob.

All managed path components are opened relative to directory file descriptors with no-follow flags. Blob paths derive only from validated hashes. Exports and reports are copies published without overwrite; writable hard links to stored blobs are never returned.

## Explicit migration from version 1

Opening version 1 is read-only and never migrates it. `migrate --to 2` takes the store lock and performs an explicit, resumable migration:

1. Create a SQLite backup in the store, sync it, and validate its schema version, store ID, and integrity.
2. Atomically publish a journal with state `started`.
3. In one SQLite transaction, rename the old tables, create version-2 relations, copy all observations/blobs, create active PNG representations, record migration history, and set `user_version=2`.
4. Advance the journal to `db_committed`.
5. Atomically replace the manifest with format version 2 and advance the journal to `manifest_updated`.
6. Complete the migration-history row, remove backup files, and finally remove the journal.

This is an ordered roll-forward/restore protocol across SQLite and JSON files; it does not claim cross-file atomicity. While a journal exists, ordinary commands fail with `E_MIGRATION_INCOMPLETE`. `--resume` observes the actual database and manifest versions and safely continues. `--restore` validates the backup, restores database version 1, restores manifest version 1, then cleans up. Fault-injection tests terminate the process at every persistence boundary and exercise both paths.

Migration keeps the store UUID, image UUID, registration sequence, source identity, accepted metadata, hashes, validation limits, and operation IDs/fingerprints. Each old run is assigned stream `default`, and its frame numbers are the zero-based order of the original `seq`; records without a run keep null stream/frame values. Legacy fingerprints are marked version 1 so an identical retry remains idempotent, while new fingerprints are stream-aware version 2.

## Explicit migration from version 2

`migrate --to 3` adds the judgment relation and generalizes migration-history constraints without rewriting image, blob, representation, segment, or reconstruction rows. It follows the same ordered protocol: validate a complete version-2 SQLite backup, publish `migration-v2-to-v3.json`, apply [003.sql](../migrations/003.sql) in one transaction, update the manifest only after the database commit, complete migration history, then remove the backup and journal.

Ordinary commands stop while the journal exists. `--resume` inspects the database and manifest versions and completes the next safe step; `--restore` validates and restores the version-2 backup. Fault-injection tests cover each persistence boundary in both paths. A version-1 store must migrate to 2 and then to 3.

## Compatibility

Version-3 code reads fixed version-1 stores for `info`, `list`, `get`, and `verify`; writes require migration to 2. Existing image operations continue on a consistent version-2 store, but judgment operations return `E_SCHEMA_VERSION` until migration to 3. Each manifest is updated only after its database commit. Older binaries reject a newer manifest rather than misread it. Unknown or mismatched versions are always rejected.

Keep `Cargo.lock` with releases and run the fixed version-1 fixture, migration interruption, and existing-blob compatibility suites before updating PNG, zlib, SQLite, or hashing dependencies.
