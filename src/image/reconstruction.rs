//! Deterministic metadata and reconstruction for PNGs stored as decoded frames.
//!
//! The descriptor contains only information that a video frame cannot retain:
//! the original non-IDAT chunks, their position around the contiguous IDAT run,
//! and one original PNG filter type per row. Pixel and verification hashes stay
//! on the observation so identical descriptors can be content-shared.

use super::{ImageMeta, Limits, SIGNATURE, be32, limited, parse, pixels, scanlines, write_chunk};
use crate::{Error, Result, error::integrity, sha256};
use flate2::{Compression, write::ZlibEncoder};
use sha2::{Digest, Sha256};
use std::io::Write;

const MAGIC: &[u8; 8] = b"VSPNGR\0\0";
/// SQLite `blobs.object_kind` value for this descriptor.
pub const OBJECT_KIND: &str = "png_reconstruction";
/// Media type stored beside an immutable descriptor blob.
pub const MEDIA_TYPE: &str = "application/vnd.visual-store.png-reconstruction-v1";
/// Managed-object filename extension.
pub const FILE_EXTENSION: &str = "pngr";
/// Canonical binary descriptor format version.
pub const FORMAT_VERSION: u16 = 1;
const HEADER_BYTES: usize = 8 + 2 + 4 + 4;
const CHUNK_HEADER_BYTES: usize = 1 + 4 + 4;
const MAX_CHUNKS: usize = 65_536;
const OUTPUT_HEADROOM: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Placement {
    BeforeIdat,
    AfterIdat,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PreservedChunk {
    placement: Placement,
    kind: [u8; 4],
    data: Vec<u8>,
}

/// Canonical, versioned metadata needed to rebuild one supported PNG container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconstructionMetadata {
    chunks: Vec<PreservedChunk>,
    filters: Vec<u8>,
}

/// Strictly validated PNG data prepared for temporal encoding.
pub struct ExtractedReconstruction {
    /// Descriptor that is independent of the image samples and their hashes.
    pub metadata: ReconstructionMetadata,
    /// Packed RGB or RGBA samples, including RGB beneath fully transparent alpha.
    pub samples: Vec<u8>,
    /// Existing observation hash definitions, unchanged.
    pub verification: ImageMeta,
}

fn corrupt(message: &str) -> Error {
    integrity(message)
}

fn checked_add(left: usize, right: usize) -> Result<usize> {
    left.checked_add(right).ok_or_else(limited)
}

fn descriptor_budget(limits: &Limits) -> Result<usize> {
    checked_add(
        checked_add(limits.source_bytes, limits.max_edge as usize)?,
        64,
    )
}

fn enforce_descriptor_budget(length: usize, limits: &Limits) -> Result<()> {
    limits.validate()?;
    // Covers the encoded input, owned chunk copies, vector entries, and output.
    let resident = length.checked_mul(4).ok_or_else(limited)?;
    if length > descriptor_budget(limits)? || resident > limits.memory_bytes {
        return Err(limited());
    }
    Ok(())
}

fn read_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32> {
    let end = checked_add(*cursor, 4)?;
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| corrupt("Truncated PNG reconstruction metadata."))?;
    *cursor = end;
    Ok(u32::from_be_bytes(value.try_into().unwrap()))
}

impl ReconstructionMetadata {
    /// Descriptor format version.
    pub fn version(&self) -> u16 {
        FORMAT_VERSION
    }

    /// Original filter byte for every image row.
    pub fn filter_types(&self) -> &[u8] {
        &self.filters
    }

    /// Number of preserved non-IDAT chunks, including IHDR and IEND.
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Serialize to the canonical immutable-object representation.
    pub fn to_bytes(&self, limits: &Limits) -> Result<Vec<u8>> {
        self.validate_structure(limits)?;
        let length = self.encoded_length()?;
        enforce_descriptor_budget(length, limits)?;
        let mut output = Vec::with_capacity(length);
        output.extend_from_slice(MAGIC);
        output.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
        output.extend_from_slice(
            &u32::try_from(self.chunks.len())
                .map_err(|_| limited())?
                .to_be_bytes(),
        );
        output.extend_from_slice(
            &u32::try_from(self.filters.len())
                .map_err(|_| limited())?
                .to_be_bytes(),
        );
        for chunk in &self.chunks {
            output.push(match chunk.placement {
                Placement::BeforeIdat => 0,
                Placement::AfterIdat => 1,
            });
            output.extend_from_slice(&chunk.kind);
            output.extend_from_slice(
                &u32::try_from(chunk.data.len())
                    .map_err(|_| limited())?
                    .to_be_bytes(),
            );
            output.extend_from_slice(&chunk.data);
        }
        output.extend_from_slice(&self.filters);
        debug_assert_eq!(output.len(), length);
        Ok(output)
    }

    /// Parse the canonical form without trusting lengths, counts, or dimensions.
    pub fn from_bytes(bytes: &[u8], limits: &Limits) -> Result<Self> {
        enforce_descriptor_budget(bytes.len(), limits)?;
        if bytes.len() < HEADER_BYTES || bytes.get(..8) != Some(MAGIC) {
            return Err(corrupt("Invalid PNG reconstruction metadata header."));
        }
        let version = u16::from_be_bytes(bytes[8..10].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(Error::new(
                "E_SCHEMA_VERSION",
                "Unsupported PNG reconstruction metadata version.",
            ));
        }
        let mut cursor = 10;
        let chunk_count = read_u32(bytes, &mut cursor)? as usize;
        let filter_count = read_u32(bytes, &mut cursor)? as usize;
        if chunk_count > MAX_CHUNKS || filter_count > limits.max_edge as usize {
            return Err(limited());
        }
        let minimum = chunk_count
            .checked_mul(CHUNK_HEADER_BYTES)
            .and_then(|length| length.checked_add(filter_count))
            .and_then(|length| length.checked_add(cursor))
            .ok_or_else(limited)?;
        if minimum > bytes.len() {
            return Err(corrupt("Truncated PNG reconstruction metadata."));
        }

        let mut chunks = Vec::with_capacity(chunk_count);
        for _ in 0..chunk_count {
            let placement = match bytes.get(cursor) {
                Some(0) => Placement::BeforeIdat,
                Some(1) => Placement::AfterIdat,
                _ => return Err(corrupt("Invalid IDAT-relative chunk placement.")),
            };
            cursor += 1;
            let kind_end = checked_add(cursor, 4)?;
            let kind: [u8; 4] = bytes
                .get(cursor..kind_end)
                .ok_or_else(|| corrupt("Truncated PNG reconstruction metadata."))?
                .try_into()
                .unwrap();
            cursor = kind_end;
            let data_length = read_u32(bytes, &mut cursor)? as usize;
            if data_length > 0x7fff_ffff {
                return Err(corrupt("Invalid preserved PNG chunk length."));
            }
            let data_end = checked_add(cursor, data_length)?;
            let data = bytes
                .get(cursor..data_end)
                .ok_or_else(|| corrupt("Truncated PNG reconstruction metadata."))?
                .to_vec();
            cursor = data_end;
            chunks.push(PreservedChunk {
                placement,
                kind,
                data,
            });
        }
        let filter_end = checked_add(cursor, filter_count)?;
        if filter_end != bytes.len() {
            return Err(corrupt("PNG reconstruction metadata has trailing bytes."));
        }
        let metadata = Self {
            chunks,
            filters: bytes[cursor..filter_end].to_vec(),
        };
        metadata.validate_structure(limits)?;
        Ok(metadata)
    }

    fn encoded_length(&self) -> Result<usize> {
        self.chunks.iter().try_fold(
            checked_add(HEADER_BYTES, self.filters.len())?,
            |length, chunk| checked_add(checked_add(length, CHUNK_HEADER_BYTES)?, chunk.data.len()),
        )
    }

    fn validate_structure(&self, limits: &Limits) -> Result<()> {
        limits.validate()?;
        if self.chunks.len() < 2 || self.chunks.len() > MAX_CHUNKS {
            return Err(corrupt("Invalid preserved PNG chunk count."));
        }
        let first = &self.chunks[0];
        let last = self.chunks.last().unwrap();
        if first.placement != Placement::BeforeIdat
            || first.kind != *b"IHDR"
            || first.data.len() != 13
            || last.placement != Placement::AfterIdat
            || last.kind != *b"IEND"
            || !last.data.is_empty()
        {
            return Err(corrupt(
                "PNG reconstruction metadata lacks valid IHDR/IEND.",
            ));
        }
        let mut after_idat = false;
        for chunk in &self.chunks {
            if chunk.kind == *b"IDAT"
                || !chunk.kind.iter().all(u8::is_ascii_alphabetic)
                || !chunk.kind[2].is_ascii_uppercase()
            {
                return Err(corrupt("Invalid preserved non-IDAT PNG chunk."));
            }
            match chunk.placement {
                Placement::BeforeIdat if after_idat => {
                    return Err(corrupt("Preserved PNG chunk order crosses IDAT."));
                }
                Placement::BeforeIdat => {}
                Placement::AfterIdat => after_idat = true,
            }
        }
        let header = &first.data;
        let width = be32(&header[0..4]);
        let height = be32(&header[4..8]);
        if width == 0 || height == 0 {
            return Err(corrupt("Invalid preserved PNG dimensions."));
        }
        if width > limits.max_edge
            || height > limits.max_edge
            || u64::from(width)
                .checked_mul(u64::from(height))
                .is_none_or(|pixels| pixels > limits.pixels)
        {
            return Err(limited());
        }
        if header[8] != 8
            || !matches!(header[9], 2 | 6)
            || header[10] != 0
            || header[11] != 0
            || header[12] != 0
        {
            return Err(corrupt("Unsupported or invalid preserved IHDR."));
        }
        if self.filters.len() != height as usize || self.filters.iter().any(|filter| *filter > 4) {
            return Err(corrupt("Invalid preserved PNG row filters."));
        }
        let channels = if header[9] == 2 { 3usize } else { 4usize };
        let scanline_length = (width as usize)
            .checked_mul(channels)
            .and_then(|row| row.checked_add(1))
            .and_then(|row| row.checked_mul(height as usize))
            .ok_or_else(limited)?;
        if scanline_length > limits.inflated_bytes {
            return Err(limited());
        }
        Ok(())
    }

    fn shape(&self, limits: &Limits) -> Result<(u32, u32, usize, usize, usize)> {
        self.validate_structure(limits)?;
        let header = &self.chunks[0].data;
        let width = be32(&header[0..4]);
        let height = be32(&header[4..8]);
        let channels = if header[9] == 2 { 3usize } else { 4usize };
        let row_bytes = (width as usize).checked_mul(channels).ok_or_else(limited)?;
        let sample_bytes = row_bytes.checked_mul(height as usize).ok_or_else(limited)?;
        let scanline_bytes = checked_add(row_bytes, 1)?
            .checked_mul(height as usize)
            .ok_or_else(limited)?;
        Ok((width, height, channels, sample_bytes, scanline_bytes))
    }
}

/// Extract reconstruction metadata and packed samples through the existing
/// strict parser and unchanged verification-hash definitions.
pub fn extract(bytes: &[u8], limits: &Limits) -> Result<ExtractedReconstruction> {
    let parsed = parse(bytes, limits)?;
    let filtered = scanlines(bytes, &parsed)?;
    let samples = pixels(bytes, limits)?;
    let verification = super::metadata(bytes, &parsed, &filtered, &samples);
    let stride = parsed.expected / parsed.height as usize;
    let filters = filtered
        .chunks_exact(stride)
        .map(|row| row[0])
        .collect::<Vec<_>>();
    let descriptor_length = parsed
        .chunks
        .iter()
        .filter(|chunk| chunk.kind != *b"IDAT")
        .try_fold(
            checked_add(HEADER_BYTES, filters.len())?,
            |length, chunk| checked_add(checked_add(length, CHUNK_HEADER_BYTES)?, chunk.data.len()),
        )?;
    enforce_descriptor_budget(descriptor_length, limits)?;
    let mut encountered_idat = false;
    let chunks = parsed
        .chunks
        .iter()
        .filter_map(|chunk| {
            if chunk.kind == *b"IDAT" {
                encountered_idat = true;
                return None;
            }
            Some(PreservedChunk {
                placement: if encountered_idat {
                    Placement::AfterIdat
                } else {
                    Placement::BeforeIdat
                },
                kind: chunk.kind,
                data: bytes[chunk.data.clone()].to_vec(),
            })
        })
        .collect();
    let metadata = ReconstructionMetadata { chunks, filters };
    metadata.validate_structure(limits)?;
    // Prove that the extracted descriptor is representable under the same limits.
    metadata.to_bytes(limits)?;
    Ok(ExtractedReconstruction {
        metadata,
        samples,
        verification,
    })
}

fn paeth(left: u8, above: u8, upper_left: u8) -> u8 {
    let left = i32::from(left);
    let above = i32::from(above);
    let upper_left = i32::from(upper_left);
    let prediction = left + above - upper_left;
    let left_distance = (prediction - left).abs();
    let above_distance = (prediction - above).abs();
    let upper_left_distance = (prediction - upper_left).abs();
    if left_distance <= above_distance && left_distance <= upper_left_distance {
        left as u8
    } else if above_distance <= upper_left_distance {
        above as u8
    } else {
        upper_left as u8
    }
}

fn rebuild_scanlines(samples: &[u8], filters: &[u8], row_bytes: usize, channels: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(samples.len() + filters.len());
    for (row_index, row) in samples.chunks_exact(row_bytes).enumerate() {
        let filter = filters[row_index];
        output.push(filter);
        let previous = row_index
            .checked_sub(1)
            .map(|index| &samples[index * row_bytes..(index + 1) * row_bytes]);
        for (column, &sample) in row.iter().enumerate() {
            let left = column.checked_sub(channels).map_or(0, |index| row[index]);
            let above = previous.map_or(0, |row| row[column]);
            let upper_left = previous.map_or(0, |row| {
                column.checked_sub(channels).map_or(0, |index| row[index])
            });
            let predictor = match filter {
                0 => 0,
                1 => left,
                2 => above,
                3 => ((u16::from(left) + u16::from(above)) / 2) as u8,
                4 => paeth(left, above, upper_left),
                _ => unreachable!("filter types were validated"),
            };
            output.push(sample.wrapping_sub(predictor));
        }
    }
    output
}

fn non_idat_hash(chunks: &[PreservedChunk]) -> Result<String> {
    let mut hash = Sha256::new();
    for chunk in chunks {
        hash.update(
            u32::try_from(chunk.data.len())
                .map_err(|_| limited())?
                .to_be_bytes(),
        );
        hash.update(chunk.kind);
        hash.update(&chunk.data);
    }
    Ok(crate::hex(&hash.finalize()))
}

/// Rebuild a PNG from decoded packed samples and extracted metadata.
///
/// No bytes are returned until dimensions, resource bounds, all three existing
/// hashes, and the complete reconstructed PNG have passed strict validation.
pub fn rebuild_png(
    metadata: &ReconstructionMetadata,
    samples: &[u8],
    expected: &ImageMeta,
    compression_level: u32,
    limits: &Limits,
) -> Result<Vec<u8>> {
    if compression_level > 9 {
        return Err(crate::error::invalid(
            "Compression level must be 0 through 9.",
        ));
    }
    let (width, height, channels, sample_bytes, scanline_bytes) = metadata.shape(limits)?;
    if samples.len() != sample_bytes {
        return Err(corrupt("Decoded PNG sample length differs from IHDR."));
    }
    if expected.width != width
        || expected.height != height
        || expected.bit_depth != 8
        || expected.color_type != if channels == 3 { 2 } else { 6 }
        || sha256(samples) != expected.pixel_sha256
    {
        return Err(corrupt(
            "Decoded PNG samples do not match observation metadata.",
        ));
    }
    if non_idat_hash(&metadata.chunks)? != expected.non_idat_sha256 {
        return Err(corrupt(
            "Preserved PNG chunks do not match observation metadata.",
        ));
    }
    let descriptor_bytes = metadata.encoded_length()?;
    let descriptor_resident = descriptor_bytes.checked_mul(4).ok_or_else(limited)?;
    let non_idat_bytes = metadata.chunks.iter().try_fold(0usize, |length, chunk| {
        checked_add(checked_add(length, 12)?, chunk.data.len())
    })?;
    // DEFLATE expansion is far below 2x; use 2x so malformed/high-entropy input
    // cannot make the encoder grow beyond the memory estimate.
    let compressed_bound = scanline_bytes.checked_mul(2).ok_or_else(limited)?;
    let idat_header_bound = compressed_bound
        .div_ceil(1024 * 1024)
        .max(1)
        .checked_mul(12)
        .ok_or_else(limited)?;
    let output_bound = checked_add(
        checked_add(8, non_idat_bytes)?,
        checked_add(compressed_bound, idat_header_bound)?,
    )?;
    let build_resident = descriptor_resident
        .checked_add(samples.len())
        .and_then(|length| length.checked_add(scanline_bytes))
        .and_then(|length| length.checked_add(compressed_bound))
        .and_then(|length| length.checked_add(output_bound))
        .and_then(|length| length.checked_add(OUTPUT_HEADROOM))
        .ok_or_else(limited)?;
    // The strict validator temporarily holds its own container, scanline, and
    // decoded-sample buffers while the descriptor and codec samples remain live.
    let validation_resident = descriptor_resident
        .checked_add(samples.len())
        .and_then(|length| {
            output_bound
                .checked_mul(4)
                .and_then(|n| length.checked_add(n))
        })
        .and_then(|length| {
            scanline_bytes
                .checked_mul(4)
                .and_then(|n| length.checked_add(n))
        })
        .and_then(|length| length.checked_add(OUTPUT_HEADROOM))
        .ok_or_else(limited)?;
    if build_resident.max(validation_resident) > limits.memory_bytes {
        return Err(limited());
    }

    let filtered = rebuild_scanlines(
        samples,
        &metadata.filters,
        sample_bytes / height as usize,
        channels,
    );
    if sha256(&filtered) != expected.scanline_sha256 {
        return Err(corrupt(
            "Reconstructed PNG scanlines do not match observation metadata.",
        ));
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(compression_level));
    encoder.write_all(&filtered)?;
    let compressed = encoder.finish()?;
    let idat_chunks = compressed.len().div_ceil(1024 * 1024).max(1);
    let idat_header_bytes = idat_chunks.checked_mul(12).ok_or_else(limited)?;
    let output_length = checked_add(
        checked_add(8, non_idat_bytes)?,
        checked_add(compressed.len(), idat_header_bytes)?,
    )?;
    if output_length > limits.source_bytes {
        return Err(limited());
    }
    if output_length > output_bound {
        return Err(corrupt(
            "PNG compressor exceeded its bounded output estimate.",
        ));
    }

    let mut output = Vec::with_capacity(output_length);
    output.extend_from_slice(SIGNATURE);
    for chunk in metadata
        .chunks
        .iter()
        .filter(|chunk| chunk.placement == Placement::BeforeIdat)
    {
        write_chunk(&mut output, &chunk.kind, &chunk.data);
    }
    for part in compressed.chunks(1024 * 1024) {
        write_chunk(&mut output, b"IDAT", part);
    }
    for chunk in metadata
        .chunks
        .iter()
        .filter(|chunk| chunk.placement == Placement::AfterIdat)
    {
        write_chunk(&mut output, &chunk.kind, &chunk.data);
    }
    debug_assert_eq!(output.len(), output_length);
    let actual = match super::validate(&output, limits) {
        Ok(actual) => actual,
        Err(error) if error.code == "E_LIMIT_EXCEEDED" => return Err(error),
        Err(_) => return Err(corrupt("Reconstructed PNG failed strict validation.")),
    };
    if &actual != expected {
        return Err(corrupt(
            "Reconstructed PNG hashes differ from observation metadata.",
        ));
    }
    Ok(output)
}
