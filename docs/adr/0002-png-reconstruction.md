# ADR 0002: Versioned PNG reconstruction metadata

- Status: Accepted
- Date: 2026-09-15

## Context

A VP9 frame preserves decoded RGB/RGBA samples, but it does not preserve the PNG container information required by Visual Store's existing losslessness contract: original IHDR values, accepted non-IDAT chunks and their position around IDAT, or the filter selected for each row. Keeping the entire filtered scanline stream beside VP9 would duplicate most image data and defeat temporal compression.

The existing observation hashes have stable meanings:

- `pixel_sha256`: packed decoded RGB or RGBA samples;
- `scanline_sha256`: decompressed PNG scanlines, including one filter byte per row;
- `non_idat_sha256`: for each non-IDAT chunk in order, SHA-256 over its big-endian length, type, and data (CRC excluded).

Those definitions must not change when the active representation changes.

## Decision

`image::reconstruction` extracts a canonical metadata object only after the existing strict PNG parser and decoder accept the input. The object contains all non-IDAT chunk type/data pairs in original order, an explicit before-IDAT or after-IDAT marker for each, and exactly one original filter type (0 through 4) per row. It includes IHDR and IEND. It excludes pixels and the three observation hashes, allowing images with the same container structure and row filters to share one content-addressed descriptor.

The immutable blob has object kind `png_reconstruction`, media type `application/vnd.visual-store.png-reconstruction-v1`, and this big-endian binary layout:

```text
8 bytes  magic: "VSPNGR\0\0"
u16      format version: 1
u32      preserved chunk count
u32      row-filter count
repeat chunk count times:
  u8     placement: 0 before IDAT, 1 after IDAT
  4 bytes PNG chunk type
  u32    data length
  bytes  chunk data
bytes    one filter type per row
```

Integers, counts, additions, dimensions, chunk lengths, row counts, encoded size, inflated scanline size, and conservative resident memory are checked before allocation or output. The format permits at most 65,536 preserved chunks and otherwise uses the caller's existing `Limits`. Unknown descriptor versions fail with `E_SCHEMA_VERSION`; corrupt stored metadata fails with `E_INTEGRITY`; exceeded bounds fail with `E_LIMIT_EXCEEDED`.

Reconstruction reapplies the PNG None, Sub, Up, Average, or Paeth filter for each row to packed decoded samples. It verifies `pixel_sha256`, rebuilds and verifies `scanline_sha256`, and verifies preserved chunks against `non_idat_sha256` before assembling a candidate. It then zlib-compresses the verified scanlines, emits a contiguous IDAT run between the preserved groups, and runs the complete result through the existing strict PNG validation path. No partial byte vector is returned on failure.

RGBA samples include alpha and every RGB value beneath alpha zero. The VP9 codec stores alpha in its separate lossless stream; the observation pixel hash covers the recombined RGBA bytes. Its checked payload accounting reports color, alpha, and their total so alpha cannot be omitted. Segment capacity accounting must add container overhead and these reconstruction descriptors.

## Consequences

The rebuilt PNG has identical dimensions, color type, samples, filtered scanlines, accepted non-IDAT chunks, ordering, placement, and all three verification hashes. Its zlib stream and IDAT splitting can differ, so whole-file identity with the former stored PNG is not promised. `get --variant source` remains a separate byte-identical path whenever `--keep-source` was used.

Segment publication will content-address the canonical descriptor bytes through the v2 `png_reconstruction` relation. Retrieval will load and bound the descriptor, decode the needed color and optional alpha frame, then call this reconstruction API before publishing a PNG.
