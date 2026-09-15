# ADR 0003: immutable bounded temporal segments

## Decision

Visual Store persists lossless VP9 packets in a small custom container rather than
invoking FFmpeg. Each `VSVP9` version-1 object has a fixed header (dimensions,
display-frame count, packet count) followed by bounded packet records containing byte
length, presentation timestamp, duration, keyframe and invisible flags, and bytes.
Color and alpha use separate typed objects; versioned JSON records the reversible
libvpx layout.

Each segment contains 2–128 compatible observations and begins with a real keyframe.
Encoder output is serialized, parsed, decoded through flush, and compared byte-for-byte
with every input sample before publication. Parsing checks arithmetic, counts, flags,
bounds, truncation, and trailing bytes before codec use.

Packing compares color and alpha containers, distinct reconstruction descriptors, and
the codec descriptor with distinct active PNG bytes. PNG remains active when the full
candidate is not smaller or contains no inter-frame dependency.

## Publication and recovery

Encoding and verification occur outside a write transaction. Complete immutable
objects are synced first. A short immediate transaction rechecks all active PNG
versions, registers typed objects and mappings, retains old PNG representations, and
atomically activates VP9 for every member. Interruption therefore leaves either an
unreferenced complete object or a fully registered segment. Retry does not change
image identity or frame numbering and ignores finalized segments.

## Consequences

The format is narrow and independently decodable, with no runtime media-tool
dependency. Unknown versions fail explicitly. Source/image memory, candidate count,
packet count, object bytes, reconstruction bytes, and encode time are bounded.
