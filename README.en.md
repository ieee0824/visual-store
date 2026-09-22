# Visual Store

[日本語](README.md) | [English](README.en.md)

Visual Store is a local PNG store for coding agents. It saves an image as an opaque `visual://` reference, returns compact JSON from the CLI, and materializes image files only when requested. Saving and retrieving never display an image or write its bytes to stdout.

The MVP supports non-interlaced, static, 8-bit RGB and RGBA PNG files. It losslessly recompresses the existing filtered scanlines with zlib level 6, keeps the smaller valid representation, and deduplicates identical stored blobs by SHA-256. Image records remain separate so repeated observations are retained.

## Why Rust

This repository started as a Rust 2024 project. Rust gives the parser checked arithmetic and bounded buffers, supports a single local CLI binary, and has maintained libraries for PNG, zlib, SQLite, SHA-256, UUID, JSON, and CLI parsing. SQLite is built from bundled source. The temporal codec links directly to the system libvpx. FFmpeg, ImageMagick, network access, and API keys are not used at runtime.

## Install

Requirements: a current stable Rust toolchain, a C compiler, `pkg-config`, and the libvpx development package. The supported system libvpx range is 1.12.0 through 1.16.0.

```bash
# macOS
brew install libvpx pkg-config

# Ubuntu/Debian
sudo apt-get update
sudo apt-get install -y libvpx-dev pkg-config
```

```bash
cargo install --path . --locked
vstore --version
```

For development, replace `vstore` with `cargo run --locked --` in the examples below.

## Quick start

```bash
vstore --store "$PWD/.visual-store" init

vstore --store "$PWD/.visual-store" put \
  --file artifacts/render.png \
  --run ui-check-20260915-a \
  --stream browser-main \
  --label input-border \
  --note "Saved for later inspection; not viewed yet."

vstore --store "$PWD/.visual-store" list --run ui-check-20260915-a --limit 20
vstore --store "$PWD/.visual-store" pack --run ui-check-20260915-a --dry-run
vstore --store "$PWD/.visual-store" pack --run ui-check-20260915-a
vstore --store "$PWD/.visual-store" get-frame --run ui-check-20260915-a --stream browser-main --frame 0
vstore --store "$PWD/.visual-store" prune --dry-run
# Explicitly apply only after reviewing the report on an isolated store.
vstore --store "$PWD/.visual-store" prune --apply
vstore --store "$PWD/.visual-store" info 'visual://STORE_ID/images/IMAGE_ID'
vstore --store "$PWD/.visual-store" features 'visual://STORE_ID/images/IMAGE_ID'
vstore --store "$PWD/.visual-store" judgment list 'visual://STORE_ID/images/IMAGE_ID'
vstore --store "$PWD/.visual-store" get 'visual://STORE_ID/images/IMAGE_ID'
vstore --store "$PWD/.visual-store" verify
```

Every command except `--help` and `--version` emits one JSON value to stdout. Diagnostics do not contain image bytes. `get` returns an absolute local path and `displayed: false`; pass that path to an image viewer only when image content is needed.

Temporal compression reduces local storage capacity. Reference-based retrieval keeps
unneeded images out of the conversation; the Skill cannot remove an image that was
already displayed from model history. Packed byte savings are not image-token savings.

Store selection uses `--store PATH`, then `VSTORE_ROOT`, then `$CWD/.visual-store`. It never searches parent directories. Keep `.visual-store/` out of Git; this repository's `.gitignore` already excludes its local store.

An observation with `--run` receives an immutable, zero-based `frame_no` within its `(run, stream)`; the stream defaults to `default`. Use `put --keep-source` when byte-for-byte retrieval of the input is required. Otherwise, only the validated lossless stored representation is retained. Use `--operation-id` for a retryable registration operation. Reusing an operation ID with different source bytes or metadata fails with `E_CONFLICT`.

## Judgment layer

Storage format version 3 can append multiple external judgments to each image. Each judgment independently preserves producer, model, producer schema version, JSON value, probability, confidence, metadata, and creation time. Rules, classical algorithms, Jev, vision models, and humans use the same storage model.

```bash
vstore judgment add 'visual://STORE/images/IMAGE' \
  --kind needs_visual_inspection \
  --producer jev \
  --value false \
  --confidence 0.96

vstore judgment search --kind needs_visual_inspection --value true
vstore judgment search --producer jev --confidence-below 0.70
```

`features` returns stored hashes, dimensions, sizes, and pixel identity with the preceding frame without retrieving or displaying an image. The [Skill adapter workflow](skills/visual-store/references/jev-adapter.md), not Visual Store core, calls the separately configured `jev-mcp`. Core commands including `put` need no network, Jev installation, or API key. An external agent can inspect metadata, features, and prior judgments first, and call `get` for vision only when inspection is requested or confidence is below its policy threshold. See the [Judgment Layer ADR](docs/adr/0005-judgment-layer.md) for boundaries and deferred features.

See [the CLI reference](docs/cli.md), [JSON Schema v2](docs/cli.schema.json), and [storage format](docs/storage-format.md) for the complete contract. Consumers of the archived schema v1 must update for representation, pack, get-frame, and prune fields.

The default build includes the VP9 backend through system libvpx directly. AV1 is
not implemented: `pack --codec av1` returns `E_CODEC_UNAVAILABLE`. FFmpeg is not
required.

## Codex skill

The repository includes [the Visual Store skill](skills/visual-store/SKILL.md). Codex scans project skills under `.agents/skills`; either copy the folder into this repository or symlink it while developing:

```bash
mkdir -p .agents/skills
ln -s ../../skills/visual-store .agents/skills/visual-store
```

For use across repositories, copy `skills/visual-store` to `$HOME/.agents/skills/visual-store`. The CLI installation is separate from the Skill installation. Codex detects skill changes automatically in current releases; restart Codex if it does not appear. Invoke it explicitly with `$visual-store`, or let Codex select it for matching PNG storage tasks. These locations and invocation methods follow the [official OpenAI skill documentation](https://developers.openai.com/codex/skills/).

The integration was prepared against `codex-cli 0.154.0`. The automated tests verify the CLI and Skill files, but a fresh Codex session's implicit selection and host image-viewer behavior still require a manual integration test.

## Backup and maintenance

Stop every `vstore` process, copy the entire store directory, then run `vstore --store COPY verify` on the copy. Do not copy only `index.sqlite3` while the store is active. The MVP does not delete records, blobs, exports, or temporary orphan candidates automatically.

Format-version-1 stores remain readable but are read-only. Before writing, back up the complete store and explicitly run `vstore --store STORE migrate --to 2`. Upgrade an existing version-2 store to the judgment schema with `migrate --to 3`. If either migration is interrupted, ordinary commands stop with `E_MIGRATION_INCOMPLETE`; repeat the same target with `--resume` to roll forward or `--restore` to return to the preceding version.

The default store directory and files are created with POSIX modes `0700` and `0600`. Existing owners and permissions are not changed. Stores on network filesystems and multi-host concurrent use are unsupported.

## Development

```bash
cargo test --locked --features fault-injection
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo build --locked --release
cargo run --locked --example vp9_roundtrip
```

The `fault-injection` feature exists only for isolated tests that terminate child processes at persistence boundaries or inject `ENOSPC`. Do not enable it in installed builds.

Performance depends on image contents and hardware. Use [the benchmark procedure](docs/benchmark.md) on representative GUI, text-heavy, photographic, and incompressible fixtures. Record OS, CPU, release build, compression level, cache state, and concurrency with each result.

## Dependencies and licensing

Visual Store is licensed under MIT. The VP9 backend calls the BSD 3-Clause system libvpx through `libvpx-native-sys` 5.0.17, which is MPL-2.0. Other direct runtime dependencies are `base64`, `chrono`, `clap`, `crc32fast`, `flate2`, `libc`, `png`, `rusqlite`, `serde`, `serde_json`, `sha2`, and `uuid`. They use MIT, Apache-2.0, or compatible terms; `rusqlite` is MIT and its bundled SQLite library is public domain. Test-only dependencies are `tempfile` and `jsonschema`. Exact resolved Rust crate versions are committed in `Cargo.lock`. The selected versions, reversible plane mapping, and system-library reproduction steps are recorded in the [VP9 codec ADR](docs/adr/0001-vp9-lossless-codec.md).

Before redistribution, audit the complete transitive dependency graph and notices for the target artifact. The project does not vendor third-party source or license files.

## Platform status

The implementation targets local filesystems on macOS and Linux and intentionally fails to compile elsewhere. CI checks the default VP9 build against libvpx 1.12.0 on Debian 12 ARM64 and the system libvpx packages on Ubuntu and macOS. Codec tests in this development environment ran on macOS with Rust 1.95.0 and libvpx 1.16.0.
