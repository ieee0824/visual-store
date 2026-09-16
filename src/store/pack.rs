use super::{RetiredPngEncoding, Store, now};
use crate::{
    Error, Result,
    codec::vp9::{self, PixelLayout},
    error::{integrity, invalid},
    fault,
    image::{
        ImageMeta, Limits,
        reconstruction::{self, ReconstructionMetadata},
    },
    segment::{self, SegmentLimits, StoredSegment},
    sha256,
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    time::{Duration, Instant},
};

const RESULT_EXAMPLES: usize = 20;

#[derive(Debug, Clone)]
pub struct PackOptions {
    pub run: String,
    pub stream: Option<String>,
    pub segment_frames: usize,
    pub dry_run: bool,
    pub max_segment_bytes: usize,
    pub max_segment_packets: usize,
    pub max_reconstruction_bytes: usize,
    pub max_pack_images: usize,
    pub max_encode_seconds: u64,
    pub limits: Limits,
}

impl PackOptions {
    fn normalize(&mut self) -> Result<()> {
        if self.run.is_empty() {
            return Err(invalid("Run requires 1–128 UTF-8 bytes."));
        }
        if self.run.len() > 128 {
            return Err(Error::new(
                "E_LIMIT_EXCEEDED",
                "Run exceeds the 128-byte limit.",
            ));
        }
        if self.stream.as_ref().is_some_and(|stream| stream.is_empty()) {
            return Err(invalid("Stream requires 1–128 UTF-8 bytes."));
        }
        if self
            .stream
            .as_ref()
            .is_some_and(|stream| stream.len() > 128)
        {
            return Err(Error::new(
                "E_LIMIT_EXCEEDED",
                "Stream exceeds the 128-byte limit.",
            ));
        }
        if !(2..=128).contains(&self.segment_frames) {
            return Err(invalid("Segment frames must be 2 through 128."));
        }
        if self.max_segment_bytes == 0
            || self.max_segment_packets == 0
            || self.max_reconstruction_bytes == 0
            || self.max_pack_images == 0
            || self.max_encode_seconds == 0
        {
            return Err(invalid("Pack limits must be positive and finite."));
        }
        self.limits.validate()
    }

    fn segment_limits(&self) -> SegmentLimits {
        SegmentLimits {
            max_bytes: self.max_segment_bytes,
            max_packets: self.max_segment_packets,
            max_descriptor_bytes: 16 * 1024,
        }
    }
}

pub struct PackOutcome {
    pub data: Value,
    pub error: Option<Error>,
}

#[derive(Clone, Debug)]
struct Candidate {
    image_id: String,
    stream: String,
    frame_no: u64,
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: u8,
    scanline_sha256: String,
    non_idat_sha256: String,
    pixel_sha256: String,
    representation_version: u32,
    png_hash: String,
    png_bytes: u64,
    encoding_version: String,
    compression_level: u32,
    compression_applied: bool,
}

impl Candidate {
    fn compatible_with(&self, other: &Self) -> bool {
        self.stream == other.stream
            && self.width == other.width
            && self.height == other.height
            && self.bit_depth == other.bit_depth
            && self.color_type == other.color_type
            && self.frame_no.checked_add(1) == Some(other.frame_no)
    }

    fn verification(&self) -> ImageMeta {
        ImageMeta {
            width: self.width,
            height: self.height,
            bit_depth: self.bit_depth,
            color_type: self.color_type,
            scanline_sha256: self.scanline_sha256.clone(),
            non_idat_sha256: self.non_idat_sha256.clone(),
            pixel_sha256: self.pixel_sha256.clone(),
        }
    }
}

struct PreparedFrame {
    candidate: Candidate,
    reconstruction_hash: String,
    reconstruction_bytes: Vec<u8>,
}

struct PreparedSegment {
    frames: Vec<PreparedFrame>,
    stored: StoredSegment,
    color_hash: String,
    alpha_hash: Option<String>,
    segment_id: String,
    png_distinct_bytes: u64,
    candidate_bytes: u64,
    reconstruction_distinct_bytes: u64,
    encode_millis: u64,
    inter_predicted: bool,
}

struct Report {
    run: String,
    requested_stream: Option<String>,
    frozen_through_seq: i64,
    dry_run: bool,
    segment_frames: u64,
    candidate_images: u64,
    groups_considered: u64,
    segments_packed: u64,
    images_packed: u64,
    segments_not_beneficial: u64,
    images_not_beneficial: u64,
    images_skipped: u64,
    segments_failed: u64,
    png_distinct_bytes: u64,
    temporal_candidate_bytes: u64,
    published_object_bytes: u64,
    result_count: u64,
    results: Vec<Value>,
}

impl Report {
    fn push(&mut self, value: Value) {
        self.result_count += 1;
        if self.results.len() < RESULT_EXAMPLES {
            self.results.push(value);
        }
    }

    fn value(self) -> Value {
        json!({
            "run": self.run,
            "requested_stream": self.requested_stream,
            "codec": "vp9",
            "segment_frames": self.segment_frames,
            "frozen_through_seq": self.frozen_through_seq,
            "dry_run": self.dry_run,
            "candidate_images": self.candidate_images,
            "groups_considered": self.groups_considered,
            "segments_packed": self.segments_packed,
            "images_packed": self.images_packed,
            "segments_not_beneficial": self.segments_not_beneficial,
            "images_not_beneficial": self.images_not_beneficial,
            "images_skipped": self.images_skipped,
            "segments_failed": self.segments_failed,
            "png_distinct_bytes": self.png_distinct_bytes,
            "temporal_candidate_bytes": self.temporal_candidate_bytes,
            "published_object_bytes": self.published_object_bytes,
            "result_count": self.result_count,
            "results": self.results,
        })
    }
}

fn checked_sum<I: IntoIterator<Item = u64>>(values: I, message: &str) -> Result<u64> {
    values
        .into_iter()
        .try_fold(0u64, |total, value| total.checked_add(value))
        .ok_or_else(|| Error::new("E_LIMIT_EXCEEDED", message))
}

fn codec_error(error: vp9::CodecError) -> Error {
    if error.is_unavailable() {
        Error::new(
            "E_CODEC_UNAVAILABLE",
            "VP9 codec support is unavailable in this build.",
        )
    } else if error.is_limit_exceeded() {
        Error::new(
            "E_LIMIT_EXCEEDED",
            "VP9 encoding exceeded --max-encode-seconds.",
        )
    } else {
        Error::new("E_CODEC_FAILURE", format!("VP9 operation failed: {error}"))
    }
}

fn split_groups(candidates: Vec<Candidate>, size: usize) -> Vec<Vec<Candidate>> {
    let mut groups = Vec::new();
    let mut current = Vec::new();
    for candidate in candidates {
        let boundary = current.last().is_some_and(|previous: &Candidate| {
            current.len() == size || !previous.compatible_with(&candidate)
        });
        if boundary {
            groups.push(std::mem::take(&mut current));
        }
        current.push(candidate);
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

impl Store {
    fn pack_candidates(&self, options: &PackOptions) -> Result<(i64, Vec<Candidate>)> {
        let frozen: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM images WHERE run=?1 AND (?2 IS NULL OR stream=?2)",
            params![options.run, options.stream],
            |row| row.get(0),
        )?;
        let count: u64 = self.conn.query_row(
            "SELECT COUNT(*) FROM images i JOIN representations r ON r.image_id=i.image_id WHERE i.run=?1 AND (?2 IS NULL OR i.stream=?2) AND i.seq<=?3 AND r.representation_kind='png'",
            params![options.run, options.stream, frozen],
            |row| row.get(0),
        )?;
        if count > options.max_pack_images as u64 {
            return Err(Error::new(
                "E_LIMIT_EXCEEDED",
                "Pack candidate count exceeds --max-pack-images.",
            ));
        }
        let candidate_memory = usize::try_from(count)
            .ok()
            .and_then(|count| count.checked_mul(1024))
            .and_then(|bytes| bytes.checked_add(16 * 1024 * 1024))
            .ok_or_else(|| Error::new("E_LIMIT_EXCEEDED", "Pack candidate memory overflow."))?;
        if candidate_memory > options.limits.memory_bytes {
            return Err(Error::new(
                "E_LIMIT_EXCEEDED",
                "Pack candidate metadata exceeds the memory limit.",
            ));
        }
        let mut statement = self.conn.prepare(
            "SELECT i.image_id,i.seq,i.stream,i.frame_no,i.width,i.height,i.bit_depth,i.color_type,i.scanline_sha256,i.non_idat_sha256,i.pixel_sha256,r.representation_version,r.png_blob_sha256,b.byte_length,r.encoding_version,r.compression_level,r.compression_applied FROM images i JOIN representations r ON r.image_id=i.image_id AND r.representation_kind='png' JOIN blobs b ON b.sha256=r.png_blob_sha256 WHERE i.run=?1 AND (?2 IS NULL OR i.stream=?2) AND i.seq<=?3 ORDER BY i.stream,i.frame_no",
        )?;
        let candidates = statement
            .query_map(params![options.run, options.stream, frozen], |row| {
                Ok(Candidate {
                    image_id: row.get(0)?,
                    stream: row.get(2)?,
                    frame_no: row.get(3)?,
                    width: row.get(4)?,
                    height: row.get(5)?,
                    bit_depth: row.get(6)?,
                    color_type: row.get(7)?,
                    scanline_sha256: row.get(8)?,
                    non_idat_sha256: row.get(9)?,
                    pixel_sha256: row.get(10)?,
                    representation_version: row.get(11)?,
                    png_hash: row.get(12)?,
                    png_bytes: row.get(13)?,
                    encoding_version: row.get(14)?,
                    compression_level: row.get(15)?,
                    compression_applied: row.get(16)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok((frozen, candidates))
    }

    fn prepare_segment(
        &self,
        candidates: Vec<Candidate>,
        options: &PackOptions,
    ) -> Result<PreparedSegment> {
        let started = Instant::now();
        let first = candidates
            .first()
            .ok_or_else(|| invalid("A temporal segment requires at least two frames."))?;
        let layout = if first.color_type == 2 {
            PixelLayout::Rgb8
        } else {
            PixelLayout::Rgba8
        };
        let pixels = usize::try_from(first.width)
            .ok()
            .and_then(|width| {
                usize::try_from(first.height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .ok_or_else(|| Error::new("E_LIMIT_EXCEEDED", "Pack dimensions overflow."))?;
        let codec_streams = if layout == PixelLayout::Rgba8 { 2 } else { 1 };
        let codec_surface_estimate = pixels
            .checked_mul(3)
            .and_then(|bytes| bytes.checked_mul(codec_streams))
            .and_then(|bytes| bytes.checked_mul(12))
            .ok_or_else(|| Error::new("E_LIMIT_EXCEEDED", "Pack memory estimate overflow."))?;
        let frame_samples = pixels
            .checked_mul(if layout == PixelLayout::Rgba8 { 4 } else { 3 })
            .ok_or_else(|| Error::new("E_LIMIT_EXCEEDED", "Pack sample size overflow."))?;
        let largest_png = candidates
            .iter()
            .map(|candidate| usize::try_from(candidate.png_bytes).unwrap_or(usize::MAX))
            .max()
            .unwrap_or(0);
        let fixed_memory_estimate =
            codec_surface_estimate
                .checked_add(frame_samples.checked_mul(4).ok_or_else(|| {
                    Error::new("E_LIMIT_EXCEEDED", "Pack memory estimate overflow.")
                })?)
                .and_then(|bytes| {
                    largest_png
                        .checked_mul(3)
                        .and_then(|png| bytes.checked_add(png))
                })
                .and_then(|bytes| bytes.checked_add(32 * 1024 * 1024))
                .ok_or_else(|| Error::new("E_LIMIT_EXCEEDED", "Pack memory estimate overflow."))?;
        if fixed_memory_estimate > options.limits.memory_bytes {
            return Err(Error::new(
                "E_LIMIT_EXCEEDED",
                "Temporal encoding exceeds the configured memory estimate.",
            ));
        }
        let mut encoder = vp9::SequenceEncoder::with_time_limit(
            first.width,
            first.height,
            layout,
            Duration::from_secs(options.max_encode_seconds),
        )
        .map_err(codec_error)?;
        let mut frames = Vec::with_capacity(candidates.len());
        let mut png_sizes = HashMap::new();
        let mut reconstruction_sizes = HashMap::new();
        let mut reconstruction_bytes = 0usize;
        for candidate in candidates {
            let png = self.object_bytes(
                &candidate.png_hash,
                candidate.png_bytes,
                "png",
                options.limits.source_bytes,
            )?;
            let extracted = reconstruction::extract(&png, &options.limits)?;
            if extracted.verification != candidate.verification() {
                return Err(integrity(
                    "Active PNG differs from its observation verification hashes.",
                ));
            }
            let descriptor = extracted.metadata.to_bytes(&options.limits)?;
            let descriptor_hash = sha256(&descriptor);
            encoder.push(&extracted.samples).map_err(codec_error)?;
            reconstruction_bytes = reconstruction_bytes
                .checked_add(descriptor.len())
                .ok_or_else(|| {
                    Error::new("E_LIMIT_EXCEEDED", "Reconstruction metadata size overflow.")
                })?;
            png_sizes
                .entry(candidate.png_hash.clone())
                .or_insert(candidate.png_bytes);
            reconstruction_sizes
                .entry(descriptor_hash.clone())
                .or_insert(descriptor.len() as u64);
            frames.push(PreparedFrame {
                candidate,
                reconstruction_hash: descriptor_hash,
                reconstruction_bytes: descriptor,
            });
        }
        let reconstruction_distinct_bytes = checked_sum(
            reconstruction_sizes.values().copied(),
            "Reconstruction byte total overflow.",
        )?;
        if reconstruction_distinct_bytes > options.max_reconstruction_bytes as u64 {
            return Err(Error::new(
                "E_LIMIT_EXCEEDED",
                "Reconstruction metadata exceeds --max-reconstruction-bytes.",
            ));
        }
        let memory_estimate =
            fixed_memory_estimate
                .checked_add(reconstruction_bytes.checked_mul(2).ok_or_else(|| {
                    Error::new("E_LIMIT_EXCEEDED", "Pack memory estimate overflow.")
                })?)
                .ok_or_else(|| Error::new("E_LIMIT_EXCEEDED", "Pack memory estimate overflow."))?;
        if memory_estimate > options.limits.memory_bytes {
            return Err(Error::new(
                "E_LIMIT_EXCEEDED",
                "Temporal encoding exceeds the configured memory estimate.",
            ));
        }

        let sequence = encoder.finish().map_err(codec_error)?;
        if started.elapsed() > Duration::from_secs(options.max_encode_seconds) {
            return Err(Error::new(
                "E_LIMIT_EXCEEDED",
                "VP9 encoding exceeded --max-encode-seconds.",
            ));
        }
        let first = sequence
            .color_packets
            .first()
            .ok_or_else(|| integrity("VP9 encoder returned no color packets."))?;
        if !vp9::inspect_packet(&first.data)
            .map_err(codec_error)?
            .keyframe
        {
            return Err(integrity(
                "VP9 segment does not begin at an independently decodable frame.",
            ));
        }
        let inter_predicted = sequence
            .color_packets
            .iter()
            .skip(1)
            .filter_map(|packet| vp9::inspect_packet(&packet.data).ok())
            .any(|packet| !packet.keyframe);
        let stored = StoredSegment::from_sequence(&sequence, &options.segment_limits())?;
        let persisted = stored.to_sequence(&options.segment_limits())?;
        let mut verification_error = None;
        let decoded = vp9::decode_each(&persisted, |index, decoded| {
            let Some(frame) = frames.get(index) else {
                return false;
            };
            let result =
                ReconstructionMetadata::from_bytes(&frame.reconstruction_bytes, &options.limits)
                    .and_then(|metadata| {
                        reconstruction::rebuild_png(
                            &metadata,
                            &decoded.samples,
                            &frame.candidate.verification(),
                            6,
                            &options.limits,
                        )
                    });
            if let Err(error) = result {
                verification_error = Some(error);
                false
            } else {
                true
            }
        });
        if let Some(error) = verification_error {
            return Err(error);
        }
        decoded.map_err(codec_error)?;
        let encode_millis = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        if started.elapsed() > Duration::from_secs(options.max_encode_seconds) {
            return Err(Error::new(
                "E_LIMIT_EXCEEDED",
                "VP9 verification exceeded --max-encode-seconds.",
            ));
        }
        let color_hash = sha256(&stored.color);
        let alpha_hash = stored.alpha.as_ref().map(|bytes| sha256(bytes));
        let png_distinct_bytes = checked_sum(
            png_sizes.values().copied(),
            "Distinct PNG byte total overflow.",
        )?;
        let candidate_bytes = checked_sum(
            [
                stored.color.len() as u64,
                stored.alpha.as_ref().map_or(0, |bytes| bytes.len() as u64),
                reconstruction_distinct_bytes,
                stored.descriptor_json.len() as u64,
            ],
            "Temporal candidate byte total overflow.",
        )?;
        let mut identity = Vec::new();
        for value in std::iter::once(stored.descriptor_json.as_bytes())
            .chain(std::iter::once(color_hash.as_bytes()))
            .chain(alpha_hash.as_deref().map(str::as_bytes))
            .chain(
                frames
                    .iter()
                    .map(|frame| frame.candidate.image_id.as_bytes()),
            )
        {
            identity.extend_from_slice(&(value.len() as u64).to_be_bytes());
            identity.extend_from_slice(value);
        }
        let segment_id = sha256(&identity);
        Ok(PreparedSegment {
            frames,
            stored,
            color_hash,
            alpha_hash,
            segment_id,
            png_distinct_bytes,
            candidate_bytes,
            reconstruction_distinct_bytes,
            encode_millis,
            inter_predicted,
        })
    }

    fn activate_segment(&mut self, prepared: &PreparedSegment) -> Result<bool> {
        fault("pack_before_object_publish")?;
        self.save_object(&prepared.stored.color, segment::FILE_EXTENSION)?;
        if let Some(alpha) = &prepared.stored.alpha {
            self.save_object(alpha, segment::FILE_EXTENSION)?;
        }
        let mut descriptors = BTreeMap::new();
        for frame in &prepared.frames {
            descriptors
                .entry(frame.reconstruction_hash.clone())
                .or_insert(&frame.reconstruction_bytes);
        }
        for (hash, bytes) in &descriptors {
            let (stored_hash, _) = self.save_object(bytes, reconstruction::FILE_EXTENSION)?;
            if &stored_hash != hash {
                return Err(integrity("Reconstruction object hash changed."));
            }
        }
        fault("pack_after_object_publish")?;
        fault("pack_before_db_commit")?;
        let created = now();
        let transaction = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for frame in &prepared.frames {
            let active: Option<(u32, String, String)> = transaction
                .query_row(
                    "SELECT representation_version,representation_kind,png_blob_sha256 FROM representations WHERE image_id=?1",
                    [&frame.candidate.image_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            if active.as_ref()
                != Some(&(
                    frame.candidate.representation_version,
                    "png".into(),
                    frame.candidate.png_hash.clone(),
                ))
            {
                transaction.rollback()?;
                return Ok(false);
            }
        }
        let object_path = |hash: &str, extension: &str| Store::object_path(hash, extension);
        let insert_blob = |hash: &str,
                           bytes: usize,
                           media: &str,
                           kind: &str,
                           extension: &str|
         -> Result<()> {
            let relative = object_path(hash, extension);
            transaction.execute(
                "INSERT INTO blobs(sha256,relative_path,byte_length,media_type,object_kind,created_at) VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(sha256) DO NOTHING",
                params![hash,relative,bytes as u64,media,kind,created],
            )?;
            let valid: bool = transaction.query_row(
                "SELECT relative_path=?2 AND byte_length=?3 AND media_type=?4 AND object_kind=?5 FROM blobs WHERE sha256=?1",
                params![hash,relative,bytes as u64,media,kind],
                |row| row.get(0),
            )?;
            if !valid {
                return Err(integrity("Existing temporal object metadata differs."));
            }
            Ok(())
        };
        insert_blob(
            &prepared.color_hash,
            prepared.stored.color.len(),
            segment::MEDIA_TYPE,
            segment::OBJECT_KIND,
            segment::FILE_EXTENSION,
        )?;
        if let (Some(hash), Some(bytes)) = (&prepared.alpha_hash, &prepared.stored.alpha) {
            insert_blob(
                hash,
                bytes.len(),
                segment::MEDIA_TYPE,
                segment::OBJECT_KIND,
                segment::FILE_EXTENSION,
            )?;
        }
        for (hash, bytes) in &descriptors {
            insert_blob(
                hash,
                bytes.len(),
                reconstruction::MEDIA_TYPE,
                reconstruction::OBJECT_KIND,
                reconstruction::FILE_EXTENSION,
            )?;
        }
        let first = &prepared.frames[0].candidate;
        transaction.execute(
            "INSERT INTO segments(segment_id,codec,codec_descriptor_json,width,height,bit_depth,pixel_layout,frame_count,color_blob_sha256,alpha_blob_sha256,created_at) VALUES (?1,'vp9',?2,?3,?4,8,?5,?6,?7,?8,?9)",
            params![prepared.segment_id,prepared.stored.descriptor_json,first.width,first.height,if first.color_type==2 {"rgb8"} else {"rgba8"},prepared.frames.len() as u32,prepared.color_hash,prepared.alpha_hash,created],
        )?;
        for (index, frame) in prepared.frames.iter().enumerate() {
            transaction.execute(
                "INSERT INTO png_reconstruction(image_id,reconstruction_version,descriptor_blob_sha256,descriptor_byte_length,created_at) VALUES (?1,1,?2,?3,?4)",
                params![frame.candidate.image_id,frame.reconstruction_hash,frame.reconstruction_bytes.len() as u64,created],
            )?;
            transaction.execute(
                "INSERT INTO frame_locations(image_id,segment_id,frame_index,decode_start_index) VALUES (?1,?2,?3,0)",
                params![frame.candidate.image_id,prepared.segment_id,index as u32],
            )?;
            let retired_encoding = serde_json::to_string(&RetiredPngEncoding {
                version: 1,
                encoding_version: frame.candidate.encoding_version.clone(),
                compression_level: frame.candidate.compression_level,
                compression_applied: frame.candidate.compression_applied,
            })?;
            transaction.execute(
                "INSERT INTO retired_representations(image_id,representation_version,representation_kind,png_blob_sha256,segment_id,encoding_version,retired_at,replacement_version,prune_state) VALUES (?1,?2,'png',?3,NULL,?4,?5,?6,'retained')",
                params![frame.candidate.image_id,frame.candidate.representation_version,frame.candidate.png_hash,retired_encoding,created,frame.candidate.representation_version+1],
            )?;
            let updated = transaction.execute(
                "UPDATE representations SET representation_version=?2,representation_kind='vp9_segment',png_blob_sha256=NULL,segment_id=?3,encoding_version=?4,compression_level=NULL,compression_applied=NULL,created_at=?5,verified_at=?5 WHERE image_id=?1 AND representation_version=?6 AND representation_kind='png' AND png_blob_sha256=?7",
                params![frame.candidate.image_id,frame.candidate.representation_version+1,prepared.segment_id,segment::ENCODING_VERSION,created,frame.candidate.representation_version,frame.candidate.png_hash],
            )?;
            if updated != 1 {
                return Err(integrity("Active representation changed during pack."));
            }
        }
        fault("pack_during_db_commit")?;
        transaction.commit()?;
        fault("pack_after_db_commit")?;
        Ok(true)
    }

    pub fn pack(&mut self, mut options: PackOptions) -> Result<PackOutcome> {
        options.normalize()?;
        if self.format_version != 2 {
            return Err(Error::new(
                "E_SCHEMA_VERSION",
                "Pack requires a version 2 store.",
            ));
        }
        let (frozen, candidates) = self.pack_candidates(&options)?;
        let groups = split_groups(candidates, options.segment_frames);
        let candidate_images = groups.iter().map(Vec::len).sum::<usize>() as u64;
        let mut report = Report {
            run: options.run.clone(),
            requested_stream: options.stream.clone(),
            frozen_through_seq: frozen,
            dry_run: options.dry_run,
            segment_frames: options.segment_frames as u64,
            candidate_images,
            groups_considered: groups.len() as u64,
            segments_packed: 0,
            images_packed: 0,
            segments_not_beneficial: 0,
            images_not_beneficial: 0,
            images_skipped: 0,
            segments_failed: 0,
            png_distinct_bytes: 0,
            temporal_candidate_bytes: 0,
            published_object_bytes: 0,
            result_count: 0,
            results: Vec::new(),
        };
        if groups.is_empty() {
            report.push(json!({"status":"no_op","reason":"no_active_png_candidates"}));
            return Ok(PackOutcome {
                data: report.value(),
                error: None,
            });
        }
        for group in groups {
            let stream = group[0].stream.clone();
            let first_frame = group[0].frame_no;
            let last_frame = group.last().unwrap().frame_no;
            if group.len() < 2 {
                report.images_skipped += group.len() as u64;
                report.push(json!({"stream":stream,"first_frame_no":first_frame,"last_frame_no":last_frame,"image_count":group.len(),"status":"skipped","reason":"insufficient_compatible_frames"}));
                continue;
            }
            let prepared = match self.prepare_segment(group, &options) {
                Ok(prepared) => prepared,
                Err(error) => {
                    report.segments_failed += 1;
                    report.push(json!({"stream":stream,"first_frame_no":first_frame,"last_frame_no":last_frame,"status":"failed","reason":error.code}));
                    return Ok(PackOutcome {
                        data: report.value(),
                        error: Some(error),
                    });
                }
            };
            report.png_distinct_bytes = report
                .png_distinct_bytes
                .checked_add(prepared.png_distinct_bytes)
                .ok_or_else(|| Error::new("E_LIMIT_EXCEEDED", "Pack byte total overflow."))?;
            report.temporal_candidate_bytes = report
                .temporal_candidate_bytes
                .checked_add(prepared.candidate_bytes)
                .ok_or_else(|| Error::new("E_LIMIT_EXCEEDED", "Pack byte total overflow."))?;
            if !prepared.inter_predicted {
                report.segments_not_beneficial += 1;
                report.images_not_beneficial += prepared.frames.len() as u64;
                report.push(json!({"stream":stream,"first_frame_no":first_frame,"last_frame_no":last_frame,"image_count":prepared.frames.len(),"status":"not_beneficial","reason":"no_inter_prediction","png_distinct_bytes":prepared.png_distinct_bytes,"candidate_bytes":prepared.candidate_bytes,"encode_millis":prepared.encode_millis}));
                continue;
            }
            if prepared.candidate_bytes >= prepared.png_distinct_bytes {
                report.segments_not_beneficial += 1;
                report.images_not_beneficial += prepared.frames.len() as u64;
                report.push(json!({"stream":stream,"first_frame_no":first_frame,"last_frame_no":last_frame,"image_count":prepared.frames.len(),"status":"not_beneficial","reason":"temporal_bytes_not_smaller_than_distinct_png_bytes","png_distinct_bytes":prepared.png_distinct_bytes,"candidate_bytes":prepared.candidate_bytes,"encode_millis":prepared.encode_millis}));
                continue;
            }
            if options.dry_run {
                report.push(json!({"stream":stream,"first_frame_no":first_frame,"last_frame_no":last_frame,"image_count":prepared.frames.len(),"status":"would_pack","reason":"temporal_bytes_smaller_than_distinct_png_bytes","png_distinct_bytes":prepared.png_distinct_bytes,"candidate_bytes":prepared.candidate_bytes,"reconstruction_distinct_bytes":prepared.reconstruction_distinct_bytes,"encode_millis":prepared.encode_millis}));
                continue;
            }
            match self.activate_segment(&prepared) {
                Ok(true) => {
                    report.segments_packed += 1;
                    report.images_packed += prepared.frames.len() as u64;
                    report.published_object_bytes = report
                        .published_object_bytes
                        .checked_add(
                            prepared.candidate_bytes - prepared.stored.descriptor_json.len() as u64,
                        )
                        .ok_or_else(|| {
                            Error::new("E_LIMIT_EXCEEDED", "Pack byte total overflow.")
                        })?;
                    report.push(json!({"stream":stream,"first_frame_no":first_frame,"last_frame_no":last_frame,"image_count":prepared.frames.len(),"status":"packed","reason":"temporal_bytes_smaller_than_distinct_png_bytes","segment_id":prepared.segment_id,"png_distinct_bytes":prepared.png_distinct_bytes,"candidate_bytes":prepared.candidate_bytes,"reconstruction_distinct_bytes":prepared.reconstruction_distinct_bytes,"encode_millis":prepared.encode_millis}));
                }
                Ok(false) => {
                    report.images_skipped += prepared.frames.len() as u64;
                    report.push(json!({"stream":stream,"first_frame_no":first_frame,"last_frame_no":last_frame,"image_count":prepared.frames.len(),"status":"skipped","reason":"concurrent_representation_change"}));
                }
                Err(error) => {
                    report.segments_failed += 1;
                    report.push(json!({"stream":stream,"first_frame_no":first_frame,"last_frame_no":last_frame,"image_count":prepared.frames.len(),"status":"failed","reason":error.code}));
                    return Ok(PackOutcome {
                        data: report.value(),
                        error: Some(error),
                    });
                }
            }
        }
        Ok(PackOutcome {
            data: report.value(),
            error: None,
        })
    }
}
