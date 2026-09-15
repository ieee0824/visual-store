use super::{GET_FRAME_SQL, ImageRecord, Store};
use crate::{
    Error, Result,
    codec::vp9,
    error::integrity,
    image::{
        ImageMeta,
        reconstruction::{self, ReconstructionMetadata},
    },
    segment::{self, SegmentLimits, StoredSegment},
    sha256,
};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

pub(super) struct MaterializedBytes {
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub backend: &'static str,
    pub segment_id: Option<String>,
    pub frame_index: Option<u32>,
    pub decoded_from_frame: Option<u32>,
    pub decoded_through_frame: Option<u32>,
}

struct TemporalLocation {
    segment_id: String,
    descriptor: String,
    width: u32,
    height: u32,
    frame_count: u32,
    pixel_layout: String,
    color_hash: String,
    color_bytes: u64,
    alpha_hash: Option<String>,
    alpha_bytes: Option<u64>,
    frame_index: u32,
    decode_start_index: u32,
    reconstruction_hash: String,
    reconstruction_bytes: u64,
}

impl Store {
    fn temporal_location(&self, image_id: &str) -> Result<TemporalLocation> {
        self.conn.query_row(
            "SELECT s.segment_id,s.codec_descriptor_json,s.width,s.height,s.frame_count,s.pixel_layout,s.color_blob_sha256,cb.byte_length,s.alpha_blob_sha256,ab.byte_length,fl.frame_index,fl.decode_start_index,pr.descriptor_blob_sha256,pr.descriptor_byte_length FROM representations r JOIN frame_locations fl ON fl.image_id=r.image_id AND fl.segment_id=r.segment_id JOIN segments s ON s.segment_id=r.segment_id AND s.codec='vp9' JOIN blobs cb ON cb.sha256=s.color_blob_sha256 LEFT JOIN blobs ab ON ab.sha256=s.alpha_blob_sha256 JOIN png_reconstruction pr ON pr.image_id=r.image_id WHERE r.image_id=?1 AND r.representation_kind='vp9_segment'",
            [image_id],
            |row| Ok(TemporalLocation {
                segment_id: row.get(0)?, descriptor: row.get(1)?, width: row.get(2)?,
                height: row.get(3)?, frame_count: row.get(4)?, pixel_layout: row.get(5)?,
                color_hash: row.get(6)?, color_bytes: row.get(7)?, alpha_hash: row.get(8)?,
                alpha_bytes: row.get(9)?, frame_index: row.get(10)?, decode_start_index: row.get(11)?,
                reconstruction_hash: row.get(12)?, reconstruction_bytes: row.get(13)?,
            }),
        ).optional()?.ok_or_else(|| integrity("Temporal representation mapping is incomplete."))
    }

    fn segment_limits(&self) -> SegmentLimits {
        SegmentLimits {
            max_bytes: self.limits.memory_bytes,
            max_packets: 1024,
            max_descriptor_bytes: 16 * 1024,
        }
    }

    pub(super) fn stored_bytes(&self, record: &ImageRecord) -> Result<MaterializedBytes> {
        if record.representation_kind == "png" {
            let hash = record
                .stored_sha256
                .as_ref()
                .ok_or_else(|| integrity("Active PNG hash is missing."))?;
            let length = record
                .stored_bytes
                .ok_or_else(|| integrity("Active PNG size is missing."))?;
            let bytes = self.blob_bytes(hash, length)?;
            return Ok(MaterializedBytes {
                bytes,
                sha256: hash.clone(),
                backend: "png",
                segment_id: None,
                frame_index: None,
                decoded_from_frame: None,
                decoded_through_frame: None,
            });
        }
        if record.representation_kind != "vp9_segment" {
            return Err(Error::new(
                "E_UNSUPPORTED_IMAGE",
                "Unsupported active representation.",
            ));
        }
        let location = self.temporal_location(&record.image_id)?;
        if location.descriptor.len() > 16 * 1024 {
            return Err(Error::new(
                "E_LIMIT_EXCEEDED",
                "Temporal codec descriptor exceeds the metadata limit.",
            ));
        }
        if location.frame_index >= location.frame_count
            || location.decode_start_index > location.frame_index
            || location.decode_start_index != 0
            || (location.width, location.height) != (record.width, record.height)
            || location.segment_id != record.segment_id.as_deref().unwrap_or("")
        {
            return Err(integrity(
                "Temporal frame location or dimensions are inconsistent.",
            ));
        }
        let channels = if location.pixel_layout == "rgb8" {
            3usize
        } else if location.pixel_layout == "rgba8" {
            4
        } else {
            return Err(integrity("Temporal pixel layout is invalid."));
        };
        let prefix = usize::try_from(location.frame_index)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| Error::new("E_LIMIT_EXCEEDED", "Decode range overflow."))?;
        let resident = usize::try_from(location.width)
            .ok()
            .and_then(|w| {
                usize::try_from(location.height)
                    .ok()
                    .and_then(|h| w.checked_mul(h))
            })
            .and_then(|pixels| pixels.checked_mul(channels))
            .and_then(|frame| frame.checked_mul(prefix))
            .and_then(|samples| samples.checked_mul(8))
            .and_then(|bytes| bytes.checked_add(32 * 1024 * 1024))
            .ok_or_else(|| Error::new("E_LIMIT_EXCEEDED", "Decode memory estimate overflow."))?;
        let total_resident = resident
            .checked_add(usize::try_from(location.color_bytes).unwrap_or(usize::MAX))
            .and_then(|bytes| {
                bytes.checked_add(
                    location
                        .alpha_bytes
                        .map(|value| usize::try_from(value).unwrap_or(usize::MAX))
                        .unwrap_or(0),
                )
            })
            .and_then(|bytes| {
                bytes.checked_add(
                    usize::try_from(location.reconstruction_bytes).unwrap_or(usize::MAX),
                )
            })
            .ok_or_else(|| Error::new("E_LIMIT_EXCEEDED", "Decode memory estimate overflow."))?;
        if total_resident > self.limits.memory_bytes {
            return Err(Error::new(
                "E_LIMIT_EXCEEDED",
                "Temporal decode exceeds the memory limit.",
            ));
        }
        let color = self.object_bytes(
            &location.color_hash,
            location.color_bytes,
            segment::FILE_EXTENSION,
            self.limits.memory_bytes,
        )?;
        let alpha = match (&location.alpha_hash, location.alpha_bytes) {
            (Some(hash), Some(length)) => Some(self.object_bytes(
                hash,
                length,
                segment::FILE_EXTENSION,
                self.limits.memory_bytes,
            )?),
            (None, None) => None,
            _ => return Err(integrity("Temporal alpha metadata is incomplete.")),
        };
        let stored = StoredSegment {
            descriptor_json: location.descriptor,
            color,
            alpha,
        };
        let sequence = stored.to_sequence(&self.segment_limits())?;
        if sequence.frame_count != location.frame_count as usize
            || (sequence.width, sequence.height) != (location.width, location.height)
        {
            return Err(integrity(
                "Temporal segment descriptor disagrees with the index.",
            ));
        }
        let decoded = vp9::decode_prefix(&sequence, prefix).map_err(|error| {
            if error.is_unavailable() {
                Error::new(
                    "E_CODEC_UNAVAILABLE",
                    "VP9 codec support is unavailable in this build.",
                )
            } else {
                Error::new("E_CODEC_FAILURE", format!("VP9 decode failed: {error}"))
            }
        })?;
        let frame = decoded
            .last()
            .ok_or_else(|| integrity("VP9 produced no requested frame."))?;
        let descriptor = self.object_bytes(
            &location.reconstruction_hash,
            location.reconstruction_bytes,
            reconstruction::FILE_EXTENSION,
            self.limits.memory_bytes,
        )?;
        let metadata = ReconstructionMetadata::from_bytes(&descriptor, &self.limits)?;
        let expected = ImageMeta {
            width: record.width,
            height: record.height,
            bit_depth: record.bit_depth,
            color_type: record.color_type,
            scanline_sha256: record.scanline_sha256.clone(),
            non_idat_sha256: record.non_idat_sha256.clone(),
            pixel_sha256: record.pixel_sha256.clone(),
        };
        let bytes =
            reconstruction::rebuild_png(&metadata, &frame.samples, &expected, 6, &self.limits)?;
        let hash = sha256(&bytes);
        Ok(MaterializedBytes {
            bytes,
            sha256: hash,
            backend: "vp9_segment",
            segment_id: Some(location.segment_id),
            frame_index: Some(location.frame_index),
            decoded_from_frame: Some(location.decode_start_index),
            decoded_through_frame: Some(location.frame_index),
        })
    }

    pub fn resolve_frame(&self, run: &str, stream: &str, frame: u64) -> Result<String> {
        if self.format_version != 2 {
            return Err(Error::new(
                "E_SCHEMA_VERSION",
                "Frame lookup requires a version 2 store.",
            ));
        }
        if run.is_empty() || run.len() > 128 || stream.is_empty() || stream.len() > 128 {
            return Err(crate::error::invalid(
                "Run and stream require 1–128 UTF-8 bytes.",
            ));
        }
        self.conn
            .query_row(GET_FRAME_SQL, params![run, stream, frame], |row| row.get(0))
            .optional()?
            .ok_or_else(|| Error::new("E_NOT_FOUND", "Run, stream, and frame were not found."))
    }

    pub fn frame_info(&self, run: &str, stream: &str, frame: u64) -> Result<Value> {
        let id = self.resolve_frame(run, stream, frame)?;
        self.info(&id)
    }

    pub(super) fn verify_temporal_segment(&self, segment_id: &str) -> Result<()> {
        let (descriptor, width, height, frame_count, pixel_layout, color_hash, color_bytes,
            alpha_hash, alpha_bytes): (String, u32, u32, u32, String, String, u64, Option<String>, Option<u64>) =
            self.conn.query_row(
                "SELECT s.codec_descriptor_json,s.width,s.height,s.frame_count,s.pixel_layout,s.color_blob_sha256,cb.byte_length,s.alpha_blob_sha256,ab.byte_length FROM segments s JOIN blobs cb ON cb.sha256=s.color_blob_sha256 LEFT JOIN blobs ab ON ab.sha256=s.alpha_blob_sha256 WHERE s.segment_id=?1 AND s.codec='vp9'",
                [segment_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?,row.get(8)?)),
            )?;
        if descriptor.len() > 16 * 1024 {
            return Err(Error::new(
                "E_LIMIT_EXCEEDED",
                "Temporal codec descriptor exceeds the metadata limit.",
            ));
        }
        let color = self.object_bytes(
            &color_hash,
            color_bytes,
            segment::FILE_EXTENSION,
            self.limits.memory_bytes,
        )?;
        let alpha = match (alpha_hash, alpha_bytes) {
            (Some(hash), Some(length)) => Some(self.object_bytes(
                &hash,
                length,
                segment::FILE_EXTENSION,
                self.limits.memory_bytes,
            )?),
            (None, None) => None,
            _ => return Err(integrity("Temporal alpha metadata is incomplete.")),
        };
        let sequence = StoredSegment {
            descriptor_json: descriptor,
            color,
            alpha,
        }
        .to_sequence(&self.segment_limits())?;
        if (sequence.width, sequence.height, sequence.frame_count)
            != (width, height, frame_count as usize)
            || format!("{:?}", sequence.descriptor.pixel_layout).to_ascii_lowercase()
                != pixel_layout
        {
            return Err(integrity(
                "Temporal segment descriptor disagrees with its row.",
            ));
        }
        let decoded = vp9::decode(&sequence).map_err(|error| {
            if error.is_unavailable() {
                Error::new(
                    "E_CODEC_UNAVAILABLE",
                    "VP9 segment is unverified because codec support is unavailable.",
                )
            } else {
                integrity("VP9 segment failed decoding.")
            }
        })?;
        let mut statement = self.conn.prepare(
            "SELECT i.image_id,fl.frame_index,fl.decode_start_index,i.width,i.height,i.bit_depth,i.color_type,i.scanline_sha256,i.non_idat_sha256,i.pixel_sha256,pr.descriptor_blob_sha256,pr.descriptor_byte_length FROM frame_locations fl JOIN images i ON i.image_id=fl.image_id JOIN representations r ON r.image_id=i.image_id AND r.segment_id=fl.segment_id AND r.representation_kind='vp9_segment' JOIN png_reconstruction pr ON pr.image_id=i.image_id WHERE fl.segment_id=?1 ORDER BY fl.frame_index"
        )?;
        let rows = statement
            .query_map([segment_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, u32>(2)?,
                    row.get::<_, u32>(3)?,
                    row.get::<_, u32>(4)?,
                    row.get::<_, u8>(5)?,
                    row.get::<_, u8>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, u64>(11)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if rows.len() != frame_count as usize {
            return Err(integrity("Temporal segment mapping count differs."));
        }
        for (
            expected_index,
            (
                _id,
                frame_index,
                decode_start,
                image_width,
                image_height,
                bit_depth,
                color_type,
                scanline_hash,
                non_idat_hash,
                pixel_hash,
                reconstruction_hash,
                reconstruction_bytes,
            ),
        ) in rows.into_iter().enumerate()
        {
            if frame_index as usize != expected_index
                || decode_start > frame_index
                || decode_start != 0
                || (image_width, image_height) != (width, height)
            {
                return Err(integrity("Temporal segment frame mapping is invalid."));
            }
            let descriptor = self.object_bytes(
                &reconstruction_hash,
                reconstruction_bytes,
                reconstruction::FILE_EXTENSION,
                self.limits.memory_bytes,
            )?;
            let metadata = ReconstructionMetadata::from_bytes(&descriptor, &self.limits)?;
            reconstruction::rebuild_png(
                &metadata,
                &decoded[expected_index].samples,
                &ImageMeta {
                    width: image_width,
                    height: image_height,
                    bit_depth,
                    color_type,
                    scanline_sha256: scanline_hash,
                    non_idat_sha256: non_idat_hash,
                    pixel_sha256: pixel_hash,
                },
                6,
                &self.limits,
            )?;
        }
        Ok(())
    }
}
