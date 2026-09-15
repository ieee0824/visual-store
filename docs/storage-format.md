# Storage format version 2

```text
STORE/
  store.json
  index.sqlite3
  index.sqlite3-wal       # may exist while in use
  index.sqlite3-shm       # may exist while in use
  objects/sha256/ab/cd/FULL_SHA256.png
  tmp/
  exports/
  migration-v1-to-v2.json        # only while migration is incomplete
  migration-v1-backup.sqlite3    # only while migration is incomplete
```

`store.json` contains only `store_id` and `format_version`. SQLite stores the same ID in `store_meta`; disagreement is an integrity error. Version 2 requires both the manifest version and `PRAGMA user_version` to equal 2. The schema sources are [002.sql](../migrations/002.sql), [001-to-002-prepare.sql](../migrations/001-to-002-prepare.sql), and [001-to-002-copy.sql](../migrations/001-to-002-copy.sql). Version 1 remains documented by its immutable [001.sql](../migrations/001.sql).

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
| `schema_migrations` | Durable history of explicit schema migrations. |

`images.image_id`, `seq`, user metadata, source identity hashes, and `(run, stream, frame_no)` are observation properties. Repacking or changing the active representation must not change them. A database uniqueness constraint protects `(run, stream, frame_no)`. Registrations with a run allocate the next frame number in the same `BEGIN IMMEDIATE` transaction that inserts the observation. Registrations without a run have null stream and frame number.

The stored PNG preserves IHDR values, the byte-exact decompressed filtered scanlines, decoded RGB/RGBA samples including hidden color under transparent pixels, and every accepted non-IDAT chunk in content and relative order. IDAT boundaries and compressed bytes may change. Recompression uses the selected zlib level and is adopted only when the candidate is smaller and round-trip validation succeeds.

Version 2 accepts static, non-interlaced, 8-bit RGB/RGBA PNG. It validates the PNG signature, chunk lengths and types, reserved bits, CRCs, critical ordering, contiguous IDAT stream, bounded zlib termination, filter bytes, and decoded samples. Unknown critical chunks and unknown ancillary chunks marked unsafe to copy are rejected. Accepted ancillary chunks are preserved and never interpreted as instructions.

The canonical reconstruction descriptor is specified in [ADR 0002](adr/0002-png-reconstruction.md). It preserves original IHDR, every accepted non-IDAT chunk in order and on the same side of IDAT, plus one original filter type per row. After codec decode, Visual Store recreates filtered scanlines from packed samples and those filters, verifies the unchanged pixel, scanline, and non-IDAT hashes, rebuilds a PNG, and passes it through the same strict validator before publication. RGBA reconstruction includes alpha and nonzero hidden RGB beneath alpha zero. Rebuilt zlib bytes and IDAT boundaries are not required to match the pre-pack file; a retained source blob remains byte-identical.

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

## Compatibility

Version-2 code reads fixed version-1 stores for `info`, `list`, `get`, and `verify`; writes require explicit migration. The manifest is updated to 2 only after the database commit. Version-1 binaries accept only manifest/database version 1, so a successfully migrated store is rejected rather than misread. Unknown or mismatched versions are always rejected.

Keep `Cargo.lock` with releases and run the fixed version-1 fixture, migration interruption, and existing-blob compatibility suites before updating PNG, zlib, SQLite, or hashing dependencies.
