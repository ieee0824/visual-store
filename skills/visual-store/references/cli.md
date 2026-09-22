# vstore CLI v2

```text
vstore [--store PATH] COMMAND
```

Store selection follows `--store` > `VSTORE_ROOT` > `$CWD/.visual-store`. Parent directories are not searched.
Except for `--help` and `--version`, stdout contains one UTF-8 JSON object. Success has the form `{schema_version:2,ok:true,data:{...}}`; errors have the form `{schema_version:2,ok:false,error:{code,message,retryable}}`.

## Commands

| Command | Purpose and arguments |
| --- | --- |
| `init` | Initialize a new or empty dedicated directory. Preserve the ID of a valid existing store. |
| `put --file PATH` | Register a PNG. Supports `--run`, `--stream`, `--label`, `--note`, repeated `--tag`, `--captured-at RFC3339`, `--operation-id`, `--keep-source`, and `--compression-level 0..9`. |
| `info REF` | Return dimensions, notes, tags, hashes, storage sizes, and compression information. This is not an integrity check. |
| `features REF` | Return stored hashes, dimensions, size, and pixel equality with the previous frame in the same run and stream. Does not decode or display the image. |
| `list [--run RUN] [--limit N] [--cursor CURSOR]` | List newest registrations first. Default 20, maximum 100. Stdout is capped at 16 KiB; a byte limit may produce a cursor before the requested item count. Use the same run filter on the next page. |
| `judgment add REF ...` | Add an external judgment using an inline JSON value, `--json FILE`, or `--stdin`. |
| `judgment list REF [--kind K] [--producer P]` | List judgments for an image, newest first. |
| `judgment search [--kind K] [--producer P] [--value JSON] [--confidence-below N]` | Search judgments across a store. |
| `get REF [--variant stored\|source] [--output PATH]` | Copy a PNG and return its absolute path. The default destination is under exports. Does not display the image. |
| `get-frame --run RUN [--stream default] --frame N [--variant stored\|source] [--output PATH]` | Reconstruct one PNG by frame index. Returns the segment and decode range. |
| `pack --run RUN [--stream STREAM] [--codec vp9] [--segment-frames 2..128] [--dry-run]` | Compress a completed sequence into verified immutable segments. Keep PNGs when compression is unfavorable. AV1 is unimplemented and returns an explicit error. |
| `prune --dry-run\|--apply` | Explicitly remove only verified retired PNGs. Storing and packing do not trigger it. |
| `verify [--report NEW_FILE]` | Check PNG and shared-segment integrity. Return a summary and up to 20 issue examples; write all issues to the report. |
| `migrate --to 2 [--resume\|--restore]` | Explicitly migrate a v1 store. Resume an interrupted migration or restore v1. |
| `migrate --to 3 [--resume\|--restore]` | Add the judgment schema to a v2 store. Resume an interrupted migration or restore v2. |

A REF is either `visual://STORE_UUID/images/IMAGE_UUID` or an IMAGE_UUID in the selected store.
`get` and `verify --report` fail rather than overwrite an existing file or symlink.
The source variant is available only when registration used `--keep-source`.
A stream requires a run; with a run but no stream, the stream is `default`. Each `(run, stream)` has its own zero-based `frame_no`. An explicitly empty stream is rejected.
`get` and `get-frame` return a verified PNG and always report `displayed:false`, whether the active representation is PNG or VP9. A build that cannot decode video returns `E_CODEC_UNAVAILABLE`.
The `shared_representation_bytes` field from `info` may be shared by several images, so do not sum it per image.
See the [Jev adapter](jev-adapter.md) for Jev integration and vision escalation. `put` and other core commands do not call Jev.

## Retries

The same operation ID, original bytes, and registration options return the same image ID.
Tags are sorted and deduplicated; empty run, label, and note values are treated as absent.
Times are normalized to UTC, and the default compression level is included in comparisons. The input path is excluded.
An operation ID must be unique within a store and contain 1–128 UTF-8 bytes. Reusing it with changed registration content returns `E_CONFLICT`.
If the original input is gone, the operation ID alone cannot retry registration. Use `info` or `get` on the saved reference.

## Input limits

Supported inputs are static, non-interlaced, 8-bit RGB/RGBA PNGs. APNG, palette, grayscale, and 16-bit PNGs are unsupported.
Limits are 64 MiB per file, 16,384 per edge, 16,777,216 total pixels, 128 MiB inflated scanlines, and 256 MiB estimated memory.
The combined memory estimate can reject an image that meets the dimension limits.
Run and stream are limited to 128 UTF-8 bytes each; label to 256; note to 2,048; and tags to 16 entries of 64 bytes each. Total JSON-escaped metadata is also limited.

Use finite global limit options only when needed and as directed by the user:
`--max-source-bytes`, `--max-edge`, `--max-pixels`, `--max-inflated-bytes`, `--max-memory-bytes`.
Images registered above default limits require the appropriate limits on later `get` and `verify` calls too.

## Errors

| Exit status | Main codes |
| --- | --- |
| 2 | `E_INVALID_ARGUMENT`, `E_INVALID_IMAGE`, `E_LIMIT_EXCEEDED`, `E_INVALID_CURSOR` |
| 3 | `E_NOT_FOUND`, `E_STORE_NOT_INITIALIZED`, `E_UNSUPPORTED_IMAGE`, `E_UNSUPPORTED_METADATA`, `E_SOURCE_NOT_RETAINED`, `E_CODEC_UNAVAILABLE` |
| 4 | `E_CONFLICT`, `E_BUSY`, `E_OUTPUT_EXISTS`, `E_SOURCE_CHANGED`, `E_MIGRATION_INCOMPLETE` |
| 5 | `E_INTEGRITY`, `E_SCHEMA_VERSION`, `E_STORE_MISMATCH` |
| 6 | `E_IO`, `E_PERMISSION`, `E_DISK_FULL` |

Only `E_BUSY` and `E_SOURCE_CHANGED` have `retryable:true`. Do not retry indefinitely.
When `verify` detects an integrity issue, it exits with status 5 and returns `ok:false` plus inspection results in `data` alongside the error.
A partial `pack` or `prune` failure also returns a nonzero status and `data` describing completed work.
Unreferenced files are reported as `unreferenced_candidate` only; by themselves they produce exit status 0. They may belong to an in-progress registration, so do not delete them.
