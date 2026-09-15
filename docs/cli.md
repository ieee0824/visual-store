# CLI contract

The stable command form is:

```text
vstore [GLOBAL_OPTIONS] COMMAND [OPTIONS]
```

Store selection precedence is `--store PATH`, `VSTORE_ROOT`, then `$CWD/.visual-store`. Parent directories are not searched. `init` accepts a missing directory, an empty directory, or an existing valid store. Other non-empty directories and partial stores fail without deleting their contents.

Except for `--help` and `--version`, stdout contains one UTF-8 JSON object followed by a newline. Successful commands use:

```json
{"schema_version":1,"ok":true,"data":{}}
```

Failures set a nonzero exit status and use:

```json
{"schema_version":1,"ok":false,"error":{"code":"E_...","message":"...","retryable":false}}
```

`verify` includes its bounded report under `data` when integrity problems are found. The machine-readable contract is [cli.schema.json](cli.schema.json).

## Commands

### init

Initializes the selected store and returns its store UUID, schema version, and whether it was already initialized. Reinitializing a valid store preserves its ID.

### put

Required: `--file PATH`.

Optional metadata: `--run`, `--label`, `--note`, repeatable `--tag`, and `--captured-at RFC3339`. Storage options are `--keep-source`, `--operation-id`, and `--compression-level 0..9`.

Tags are sorted and deduplicated. Empty run, label, and note values are normalized to absent. Captured timestamps are stored in UTC. An operation fingerprint covers the source hash and normalized explicit registration options, including the compression level and `keep-source`. The input path, generated image ID, and registration time are excluded.

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

### verify

Runs SQLite integrity and foreign-key checks, validates the manifest/store ID, verifies every indexed blob's path, size, SHA-256, PNG decoding, dimensions, and stored verification hashes, and enumerates fixed-depth object paths without following symlinks.

The stdout report contains counts and at most 20 examples. `--report NEW_FILE` writes every issue to a newly created JSON file. Integrity errors produce exit status 5. Files not referenced by the database are reported as `unreferenced_candidate`; they are not deleted or treated as corruption on that fact alone.

## Global resource limits

| Option | Default |
| --- | ---: |
| `--max-source-bytes` | 67,108,864 |
| `--max-edge` | 16,384 |
| `--max-pixels` | 16,777,216 |
| `--max-inflated-bytes` | 134,217,728 |
| `--max-memory-bytes` | 268,435,456 |

All values must be positive and finite. The memory bound is enforced with a conservative pre-allocation estimate covering input/candidate container copies, filtered scanlines, decoded samples, and headroom. Increasing limits is an explicit caller decision. The same increased bounds may be needed for a later `verify`.

Metadata limits are run 128 bytes, label 256 bytes, note 2,048 bytes, and at most 16 nonempty tags of 64 bytes each. Operation IDs are 1 through 128 bytes. Escaped metadata is also capped to preserve the 8 KiB `info` response budget.

## Exit statuses

| Status | Meaning | Representative codes |
| ---: | --- | --- |
| 0 | success, including empty lists and candidate-only verification | — |
| 2 | input or usage | `E_INVALID_ARGUMENT`, `E_INVALID_IMAGE`, `E_LIMIT_EXCEEDED`, `E_INVALID_CURSOR` |
| 3 | missing or unsupported | `E_NOT_FOUND`, `E_STORE_NOT_INITIALIZED`, `E_UNSUPPORTED_IMAGE`, `E_UNSUPPORTED_METADATA`, `E_SOURCE_NOT_RETAINED` |
| 4 | conflict or temporary failure | `E_CONFLICT`, `E_BUSY`, `E_OUTPUT_EXISTS`, `E_SOURCE_CHANGED` |
| 5 | integrity or version | `E_INTEGRITY`, `E_SCHEMA_VERSION`, `E_STORE_MISMATCH` |
| 6 | I/O or environment | `E_IO`, `E_PERMISSION`, `E_DISK_FULL` |

Only `E_BUSY` and `E_SOURCE_CHANGED` report `retryable: true`. This means a bounded retry may succeed; it does not direct an automatic unbounded retry.
