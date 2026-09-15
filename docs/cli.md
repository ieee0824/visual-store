# CLI contract

The stable command form is:

```text
vstore [GLOBAL_OPTIONS] COMMAND [OPTIONS]
```

Store selection precedence is `--store PATH`, `VSTORE_ROOT`, then `$CWD/.visual-store`. Parent directories are not searched. `init` accepts a missing directory, an empty directory, or an existing valid store. Other non-empty directories and partial stores fail without deleting their contents.

Except for `--help` and `--version`, stdout contains one UTF-8 JSON object followed by a newline. Successful commands use:

```json
{"schema_version":2,"ok":true,"data":{}}
```

Failures set a nonzero exit status and use:

```json
{"schema_version":2,"ok":false,"error":{"code":"E_...","message":"...","retryable":false}}
```

`verify` includes its bounded report under `data` when integrity problems are found. `pack` likewise includes a compact partial report under `data` if codec or persistence work fails. The machine-readable contract is [cli.schema.json](cli.schema.json). The previous response contract remains archived as [cli.schema.v1.json](cli.schema.v1.json); clients must select the schema by the envelope's `schema_version`.

## Commands

### init

Initializes the selected store and returns its store UUID, schema version, and whether it was already initialized. Reinitializing a valid store preserves its ID.

### put

Required: `--file PATH`.

Optional metadata: `--run`, `--stream`, `--label`, `--note`, repeatable `--tag`, and `--captured-at RFC3339`. Storage options are `--keep-source`, `--operation-id`, and `--compression-level 0..9`.

`--stream` requires `--run`. When a run is provided without a stream, the stream is `default`; observations without a run have no stream or frame number. Stream names are 1 through 128 UTF-8 bytes, and an explicitly empty value is rejected. Each successful registration receives the next `frame_no`, starting at zero, independently for each `(run, stream)`. Allocation and insertion occur in the same immediate SQLite transaction, so concurrent writers cannot publish duplicate frame numbers.

Tags are sorted and deduplicated. Empty run, label, and note values are normalized to absent. Captured timestamps are stored in UTC. A versioned operation fingerprint covers the source hash and normalized explicit registration options, including stream, compression level, and `keep-source`. Migrated version-1 fingerprints remain retryable with their original semantics. The input path, generated image ID, registration time, and allocated frame number are excluded.

The input is opened once, bounded, copied into a store temporary file, and checked for identity and metadata changes before registration. The input is never modified or removed.

### info

Accepts a full `visual://STORE_UUID/images/IMAGE_UUID` reference or an image UUID in the selected store. Returns registered metadata and hashes without reading the blob into stdout. Use `verify` when current blob integrity matters.

### list

Options: `--run`, `--limit` from 1 through 100, and `--cursor`. Default limit is 20. Results are ordered by descending registration sequence.

The opaque cursor binds store ID, run filter, the first page's maximum sequence, and the previous page boundary. A cursor from another store or filter fails with `E_INVALID_CURSOR`. Registrations made during paging do not enter that cursor series.

Each successful `list` response is at most 16 KiB on stdout, including the JSON envelope and trailing newline. The byte budget is applied after JSON escaping. A page may therefore contain fewer items than `--limit`; when more matching items remain, `next_cursor` resumes after the last item actually returned without changing stored metadata.

### get

Accepts a reference, optional `--variant stored|source`, and optional `--output PATH`. The default destination is a unique file under the store's `exports/` directory. `source` requires `--keep-source` at registration.

The source blob is size- and SHA-256-checked before publication. Existing files and symlinks are never overwritten. The returned path is absolute and the JSON always reports `displayed: false`.

For an active VP9 representation, stored retrieval reads exactly its indexed segment,
decodes from `decode_start_index` through `frame_index`, reconstructs the PNG, and
validates dimensions plus pixel, scanline, non-IDAT, and complete PNG hashes before
no-clobber publication. The result records `backend`, `segment_id`, `frame_index`, and
the decoded range. `info` reports these representation fields and shared byte/image
counts without decoding.

### get-frame

`get-frame --run RUN [--stream default] --frame N` selects one observation through
the `(run, stream, frame_no)` index and otherwise accepts the same `--variant` and
`--output` options as `get`. A missing exact frame returns `E_NOT_FOUND`; adjacent
content is never substituted and unrelated segments are not scanned.

### verify

Runs SQLite integrity and foreign-key checks, validates the manifest/store ID, verifies every indexed typed object's path, size, and SHA-256, and enumerates fixed-depth object paths without following symlinks. Each shared segment is decoded once; color/alpha containers, mappings, reconstructed dimensions, pixels, scanlines, non-IDAT metadata, and final PNG validity are checked.

The stdout report contains counts and at most 20 examples. `--report NEW_FILE` writes every issue to a newly created JSON file. Integrity errors produce exit status 5. Files not referenced by the database are reported as `unreferenced_candidate`; they are not deleted or treated as corruption on that fact alone.

### pack

Required: `--run RUN`. Optional: `--stream STREAM`, `--codec vp9`,
`--segment-frames 2..128` (default 32), and `--dry-run`. Finite safety bounds are
`--max-segment-bytes`, `--max-segment-packets`, `--max-reconstruction-bytes`,
`--max-pack-images`, and `--max-encode-seconds`.

The command freezes eligible active PNG observations at its starting sequence,
then groups only consecutive frames from the same stream with identical dimensions
and RGB/RGBA layout. Encoding and round-trip verification occur before a short
SQLite write transaction. A candidate activates only when its color/alpha packet
containers, reconstruction descriptors, and codec descriptor together are smaller
than the distinct active PNG objects they replace. Otherwise PNG remains active and
the report says `not_beneficial`. Published segments are immutable and begin with a
keyframe; retired PNGs remain available. Re-running pack never renumbers frames or
repacks finalized segments.

### prune

Choose exactly one of `prune --dry-run` or `prune --apply`. The optional
`--max-prune-objects` bound defaults to 10,000. No other command prunes as a side
effect.

Candidates are verified retired PNG objects with no active PNG, retained source,
segment, alpha, or reconstruction reference. Every active replacement segment is
decoded and reconstructed before a candidate is eligible. Prune takes an exclusive
store lock from the start instead of upgrading a reader lock, so put, pack, get,
verify, migration, and another prune cannot overlap its verification/deletion window.

Apply first commits `pending`, then removes and syncs the exact content-addressed PNG,
then commits `deleted`. Re-running resumes either interruption state. It never guesses
at unindexed files and never removes inputs, exports, backups, sources, active objects,
or temporal objects. Capacity fields are distinct within each category:
`active_representation_bytes`, `source_retained_bytes`, `retired_candidate_bytes`,
`reclaimable_bytes`, `reclaimed_bytes`, and total scoped physical object bytes before
and after. Shared blobs and segments are not multiplied per image.

### migrate

`migrate --to 2` is the only operation that upgrades a version-1 store. Merely opening an old store never changes it: read operations remain available, while `put` returns `E_SCHEMA_VERSION` until migration. Stop other writers and back up the whole store before migration.

Migration first creates and validates an on-store SQLite backup, then records a durable journal before changing the database and manifest. If interrupted, normal commands return `E_MIGRATION_INCOMPLETE`. Use `migrate --to 2 --resume` to roll forward or `migrate --to 2 --restore` to restore version 1. These are crash-recoverable ordered updates across SQLite and JSON files, not a claim of cross-file atomicity. A successful cleanup removes the migration journal and backup.

## Global resource limits

| Option | Default |
| --- | ---: |
| `--max-source-bytes` | 67,108,864 |
| `--max-edge` | 16,384 |
| `--max-pixels` | 16,777,216 |
| `--max-inflated-bytes` | 134,217,728 |
| `--max-memory-bytes` | 268,435,456 |

All values must be positive and finite. The memory bound is enforced with a conservative pre-allocation estimate covering input/candidate container copies, filtered scanlines, decoded samples, and headroom. Increasing limits is an explicit caller decision. The same increased bounds may be needed for a later `verify`.

Metadata limits are run and stream 128 bytes each, label 256 bytes, note 2,048 bytes, and at most 16 nonempty tags of 64 bytes each. Operation IDs are 1 through 128 bytes. Escaped metadata is also capped to preserve the 8 KiB `info` response budget.

## Exit statuses

| Status | Meaning | Representative codes |
| ---: | --- | --- |
| 0 | success, including empty lists and candidate-only verification | — |
| 2 | input or usage | `E_INVALID_ARGUMENT`, `E_INVALID_IMAGE`, `E_LIMIT_EXCEEDED`, `E_INVALID_CURSOR` |
| 3 | missing or unsupported | `E_NOT_FOUND`, `E_STORE_NOT_INITIALIZED`, `E_UNSUPPORTED_IMAGE`, `E_UNSUPPORTED_METADATA`, `E_SOURCE_NOT_RETAINED`, `E_CODEC_UNAVAILABLE` |
| 4 | conflict or temporary failure | `E_CONFLICT`, `E_BUSY`, `E_OUTPUT_EXISTS`, `E_SOURCE_CHANGED`, `E_MIGRATION_INCOMPLETE` |
| 5 | integrity or version | `E_INTEGRITY`, `E_SCHEMA_VERSION`, `E_STORE_MISMATCH` |
| 6 | I/O, codec, or environment | `E_IO`, `E_PERMISSION`, `E_DISK_FULL`, `E_CODEC_FAILURE` |

Only `E_BUSY` and `E_SOURCE_CHANGED` report `retryable: true`. This means a bounded retry may succeed; it does not direct an automatic unbounded retry.
