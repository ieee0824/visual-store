# ADR 0001: Lossless VP9 codec foundation with libvpx

- Status: Accepted
- Date: 2026-09-15
- Scope: temporal codec foundation v1

## Decision

Use the VP9 encoder and decoder APIs in the system-linked libvpx library for the first temporal codec. Pin the Rust binding to `libvpx-native-sys` 5.0.17 and enable it in normal builds. The supported system libvpx range is 1.12.0 through 1.16.0. Validation includes Debian 12 ARM64 with libvpx 1.12.0 and the official libvpx tag `v1.16.0` (runtime version `v1.16.0`). Obtain the actual linked version and build configuration with `libvpx_version()` and `libvpx_build_config()`, and record the runtime version in the encoder descriptor.

The Homebrew build used for macOS arm64 acceptance testing reported this runtime configuration: `--prefix=/opt/homebrew/Cellar/libvpx/1.16.0 --disable-dependency-tracking --disable-examples --disable-unit-tests --enable-pic --enable-runtime-cpu-detect --enable-shared --enable-vp9-highbitdepth --target=arm64-darwin25-gcc`. CI records each runner's configuration in the `vp9_roundtrip` example JSON.

Confine FFI types, variadic control calls, `unsafe`, codec context lifetimes, image strides, and copies of libvpx-owned packets to `src/codec/vp9.rs`. Do not use `vpxenc`, `vpxdec`, the FFmpeg CLI, `libavcodec`, `libavformat`, or `libswscale`.

## Lossless plane layout v1

Do not perform the RGB/YUV conversion used for ordinary video. Rearrange 8-bit packed RGB into full-resolution I444 in VP9 profile 1 as follows:

| Input sample | VP9 color stream |
| --- | --- |
| R | plane 0 |
| G | plane 1 |
| B | plane 2 |

For RGBA, store RGB in the same color stream and alpha in plane 0 of a separate lossless I444 stream. Set planes 1 and 2 of the alpha stream to zero and verify they remain zero on decode. Do not premultiply, subsample, convert to limited range, resize, or denoise. This preserves nonzero RGB values even in pixels with alpha 0.

Descriptor version 1 fixes these values:

- codec: `vp9`
- profile: `1` (8-bit I444)
- bit depth: `8`
- color layout: `rgb_planar_i444_direct_v1`
- alpha layout: none for RGB; `alpha_in_i444_plane0_v1` for RGBA
- color metadata: `srgb_full_range_no_conversion`
- lossless: `true`

This is an internal, lossless Visual Store sample layout. A general video player will not display its RGB colors correctly without conversion. Later segment work handles the container, persistence, and PNG reconstruction information.

## Encoder settings and inter-frame verification

Feed all frames in one stream to the same encoder context in order. Use `VP9E_SET_LOSSLESS=1`, quantizer 0, lag 0, one thread, a 1/30 timebase, and a maximum keyframe interval of 128; explicitly mark only the first frame as a keyframe. Tests must not rely solely on packet flags. Use the decoder API's `vpx_codec_peek_stream_info` to verify that a compressed packet header is non-key. Require that the packet decodes exactly in a decoder context that has received the preceding frames, but fails to decode on its own in a fresh decoder. This demonstrates a bitstream that actually depends on prior decoder state, beyond encoder settings or packet flags.

The Rust `SequenceEncoder` accepts one frame's samples at a time and does not retain the input buffer after the call returns. Feed color and alpha sequentially to their two contexts in the same frame order. Retrieval and full verification also assemble decoder output one frame at a time rather than holding decompressed RGB/RGBA for an entire segment. Libvpx reference surfaces cannot be given an allocator-level hard cap. Conservatively reject work in advance based on dimensions, stream count, a bounded number of reference surfaces, working buffers, container, and auxiliary information, and record actual peak RSS in benchmarks.

## Build approach

Development builds link dynamically against the system libvpx found by `pkg-config`. Tagged release binaries set `VPX_STATIC=1` to link the system-provided static libvpx and include its BSD 3-Clause license in the archive; they do not vendor libvpx source. Standard setup for macOS and Linux:

```bash
# macOS (Homebrew)
brew install libvpx pkg-config

# Ubuntu/Debian
sudo apt-get update
sudo apt-get install -y libvpx-dev pkg-config

pkg-config --modversion vpx
cargo test --locked
```

To reproduce the libvpx 1.16.0 validation baseline from source, check out the official tag, build a shared library, and pass its `.pc` file to the Rust build.

```bash
git clone --depth 1 --branch v1.16.0 https://chromium.googlesource.com/webm/libvpx
cd libvpx
./configure \
  --prefix="$PWD/out" \
  --enable-shared \
  --enable-pic \
  --enable-runtime-cpu-detect \
  --enable-vp9-encoder \
  --enable-vp9-decoder \
  --enable-vp9-highbitdepth \
  --disable-examples \
  --disable-tools \
  --disable-docs \
  --disable-unit-tests
make -j2
make install
PKG_CONFIG_PATH="$PWD/out/lib/pkgconfig" cargo test --manifest-path ../Cargo.toml --locked
```

On macOS, a source build also requires `DYLD_LIBRARY_PATH=$PWD/out/lib` at runtime. On Linux, set `LD_LIBRARY_PATH=$PWD/out/lib`. Development CI installs system packages on both platforms, runs the round-trip example with an empty `PATH`, and checks that the artifact dynamically depends on libvpx but not libav or FFmpeg. Release CI instead verifies that its binary has no dynamic codec dependency.

## Versions and licenses

- libvpx: 1.16.0 baseline, BSD 3-Clause
- `libvpx-native-sys`: pinned to 5.0.17, MPL-2.0
- Visual Store codec code: MIT, like the rest of the repository

When redistributing the system libvpx binary, include the copyright and license notices for that binary. When redistributing `libvpx-native-sys` or other Rust dependencies, audit notices for the versions resolved in `Cargo.lock`.

## AV1

The AV1 backend is not implemented at this stage. The CLI parses `--codec av1` to keep future selection unambiguous, but rejects it explicitly with `E_CODEC_UNAVAILABLE` rather than substituting VP9 or PNG. Help text, CLI documentation, and tests expose this behavior.
