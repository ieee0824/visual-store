# Performance measurement

Build once with `cargo build --locked --release`. Use a fresh store for each image class and keep the machine otherwise idle. Measure at least 20 warm-cache repetitions after 3 warmups, then report median and p95 rather than a single run.

Fixtures should cover a simple GUI, text-heavy GUI, photographic screen, incompressible synthetic image, 800×600, 1920×1080, and a supported image near configured limits. Do not commit private screenshots or session artifacts as fixtures.

For each class, record:

- CPU, OS, Rust version, release commit, compression level, cache state, and concurrency;
- input bytes, stored bytes, and whether recompression was selected;
- wall time for `put` and `get`, including median and p95;
- peak resident memory reported by the platform (`/usr/bin/time -l` on macOS or `/usr/bin/time -v` on Linux).

Use unique operation IDs or omit them so repeated `put` calls create separate event records. Confirm that 100 registrations create 100 images and one shared blob for identical stored bytes. Run the same procedure at concurrency 1 and 4. Compression ratio is data-dependent and has no universal pass threshold.
