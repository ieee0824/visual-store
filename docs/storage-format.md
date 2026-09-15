# Storage format version 1

```text
STORE/
  store.json
  index.sqlite3
  index.sqlite3-wal       # may exist while in use
  index.sqlite3-shm       # may exist while in use
  objects/sha256/ab/cd/FULL_SHA256.png
  tmp/
  exports/
```

`store.json` contains only `store_id` and `format_version`. SQLite stores the same ID in `store_meta`; disagreement is an integrity error. Database migrations set `PRAGMA user_version`, currently 1. The schema source is [001.sql](../migrations/001.sql).

`images` represents observation events. Its UUID stays stable independently of physical encoding. `blobs` represents immutable byte strings addressed by lowercase SHA-256. Multiple image records can point at one stored blob. A source blob is referenced only when `put --keep-source` was used; it can be the same as the stored blob.

The stored PNG preserves IHDR values, the byte-exact decompressed filtered scanlines, decoded RGB/RGBA samples including hidden color under transparent pixels, and every accepted non-IDAT chunk in content and relative order. IDAT boundaries and compressed bytes may change. Recompression uses the selected zlib level and is adopted only when the candidate is smaller and round-trip validation succeeds.

Version 1 accepts static, non-interlaced, 8-bit RGB/RGBA PNG. It validates the PNG signature, chunk lengths and types, reserved bits, CRCs, critical ordering, contiguous IDAT stream, bounded zlib termination, filter bytes, and decoded samples. Unknown critical chunks and unknown ancillary chunks marked unsafe to copy are rejected. Accepted ancillary chunks are preserved and never interpreted as instructions.

## Publication order

1. Read and freeze the source under configured bounds; detect source changes.
2. Validate and repack outside a database write transaction.
3. Write the candidate to a mode-0600 temporary file and sync it.
4. Atomically publish the immutable blob with a same-filesystem hard-link operation that cannot overwrite an existing path.
5. Verify a concurrent winner by size and SHA-256 and sync the blob directory.
6. In a short `BEGIN IMMEDIATE` transaction, insert or validate the blob row and insert the image record.
7. Return success only after the database commit.

This order permits an unreferenced complete blob after a crash. It prevents a successfully committed image from pointing to a partially written blob. `verify` reports unreferenced files without deleting them because another process may be registering the same blob.

All managed path components are opened relative to directory file descriptors with no-follow flags. Blob paths derive only from validated hashes. Exports and reports are copies published without overwrite; writable hard links to stored blobs are never returned.

## Compatibility

Readers reject unknown format and database schema versions. Encoding changes require a database migration while preserving image UUIDs and `visual://` references. Keep `Cargo.lock` with releases and run the existing-blob compatibility suite before updating PNG, zlib, SQLite, or hashing dependencies.
