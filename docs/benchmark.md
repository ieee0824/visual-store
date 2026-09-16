# Performance measurement

Build once with `cargo build --locked --release`. Use a fresh store for each image class and keep the machine otherwise idle. Measure at least 20 warm-cache repetitions after 3 warmups, then report median and p95 rather than a single run.

Fixtures should cover a simple GUI, text-heavy GUI, photographic screen, incompressible synthetic image, 800×600, 1920×1080, and a supported image near configured limits. Do not commit private screenshots or session artifacts as fixtures.

For each class, record:

- CPU, OS, Rust version, release commit, compression level, cache state, and concurrency;
- input bytes, stored bytes, and whether recompression was selected;
- wall time for `put` and `get`, including median and p95;
- peak resident memory reported by the platform (`/usr/bin/time -l` on macOS or `/usr/bin/time -v` on Linux).

Use unique operation IDs or omit them so repeated `put` calls create separate event records. Confirm that 100 registrations create 100 images and one shared blob for identical stored bytes. Run the same procedure at concurrency 1 and 4. Compression ratio is data-dependent and has no universal pass threshold.

## Temporal A/B/C/D benchmark

`examples/temporal_benchmark.rs` generates 32 public, deterministic 640×360 RGB
frames with a static application shell, scrolling line pattern, and moving cursor. It
creates a fresh store and records these non-overlapping quantities:

- A: original uncompressed fixture PNG bytes;
- B: distinct validated level-6 PNG objects before packing;
- C: lossless inter-frame VP9 color/alpha containers, distinct reconstruction
  descriptors, codec descriptor, and physical SQLite index growth caused by pack;
- D: the same samples and overhead, but with every VP9 frame forced intra.

It also performs three warmups and twenty warm-cache random retrievals, verifies the
packed store, runs prune dry-run/apply, checks that reported reclaimed bytes match the
physical object-tree delta, and records process peak RSS. Run it on an otherwise idle
machine from a clean checkout:

```bash
export VSTORE_BENCH_CPU='record the CPU model'
export VSTORE_BENCH_OS_VERSION='record the OS release/build'
export VSTORE_BENCH_RUST_VERSION="$(rustc --version)"
export VSTORE_BENCH_COMMIT="$(git rev-parse HEAD)"
cargo run --locked --release --example temporal_benchmark -- \
  temporal-benchmark.json
```

The example fails unless C is smaller than both B and D, so CI tests the claimed
temporal benefit rather than merely recording it. `c_index_increment_bytes` is the
change in the checkpointed and vacuumed SQLite file; the JSON also keeps the object
breakdown separate. Logical object bytes, SQLite bytes, and filesystem allocated
blocks are different measures. The prune `physical_object_bytes_*` fields use actual
regular-file lengths under the fixed-depth object tree, while
`allocated_object_bytes_*` separately use the platform's 512-byte block count.

### Recorded macOS result

Measured 2026-09-15 on an Apple M2 MacBook Air (8 cores, 24 GB), macOS 26.5.1
(25F80), Rust 1.95.0, system libvpx 1.16.0, release build, one codec thread,
segment size 32, compression level 6, concurrency 1. The committed JSON record is
[temporal-macos-m2.json](benchmarks/temporal-macos-m2.json).

| Measure | Bytes / time |
| --- | ---: |
| A input PNGs | 22,135,456 bytes |
| B distinct repacked PNGs | 89,272 bytes |
| C inter VP9 color container | 9,378 bytes |
| C reconstruction descriptors | 409 bytes |
| C codec descriptor/container overhead | 291 bytes |
| C SQLite index increment | 20,480 bytes |
| C complete temporal representation | 30,558 bytes |
| D complete all-intra representation | 62,858 bytes |
| Pack encode and verify | 652 ms |
| Random frame 17, warm median / p95 | 24.611 / 29.369 ms |
| Decoded range | frame 0 through 17 of one segment |
| Process peak RSS | 105,398,272 bytes |
| Object bytes before / after prune | 99,059 / 9,787 bytes |
| Allocated object blocks before / after prune | 147,456 / 16,384 bytes |
| Reported and observed reclaimed bytes | 89,272 bytes |

C is 65.8% smaller than B including measured SQLite growth, and 51.4% smaller than
D. These are fixture- and environment-specific results, not universal ratios. The
benchmark uses a cold fresh store for packing and a documented warm-cache retrieval
phase; it does not flush OS caches or estimate model/image-token usage.
