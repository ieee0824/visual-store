//! Bounded immutable container for one VP9 color or alpha packet stream.

use crate::{Error, Result, codec::vp9, error::integrity};
use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 8] = b"VSVP9S\0\0";
const CONTAINER_VERSION: u16 = 1;
const HEADER_BYTES: usize = 8 + 2 + 4 + 4 + 4 + 4;
const PACKET_HEADER_BYTES: usize = 4 + 8 + 8 + 1;

pub const OBJECT_KIND: &str = "vp9_bitstream";
pub const MEDIA_TYPE: &str = "application/vnd.visual-store.vp9-packets-v1";
pub const FILE_EXTENSION: &str = "vpxs";
pub const ENCODING_VERSION: &str = "vp9-lossless-v1";

#[derive(Debug, Clone)]
pub struct SegmentLimits {
    pub max_bytes: usize,
    pub max_packets: usize,
    pub max_descriptor_bytes: usize,
}

impl SegmentLimits {
    pub fn validate(&self) -> Result<()> {
        if self.max_bytes == 0 || self.max_packets == 0 || self.max_descriptor_bytes == 0 {
            return Err(crate::error::invalid(
                "Segment limits must be positive and finite.",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentDescriptor {
    pub version: u16,
    pub container: String,
    pub codec: vp9::CodecDescriptor,
}

#[derive(Debug, Clone)]
pub struct StoredSegment {
    pub descriptor_json: String,
    pub color: Vec<u8>,
    pub alpha: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct DecodedContainer {
    pub width: u32,
    pub height: u32,
    pub frame_count: usize,
    pub packets: Vec<vp9::Packet>,
}

fn limited(message: &str) -> Error {
    Error::new("E_LIMIT_EXCEEDED", message)
}

fn checked_add(left: usize, right: usize) -> Result<usize> {
    left.checked_add(right)
        .ok_or_else(|| limited("Segment size overflow."))
}

fn read<const N: usize>(bytes: &[u8], cursor: &mut usize) -> Result<[u8; N]> {
    let end = checked_add(*cursor, N)?;
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| integrity("Truncated VP9 segment container."))?;
    *cursor = end;
    Ok(value.try_into().unwrap())
}

fn validate_packet_metadata(
    packets: &[vp9::Packet],
    frame_count: usize,
    limits: &SegmentLimits,
) -> Result<()> {
    if packets.is_empty() || packets.len() > limits.max_packets {
        return Err(limited("VP9 packet count exceeds the segment limit."));
    }
    if !packets[0].keyframe {
        return Err(integrity(
            "An immutable VP9 segment must begin with a keyframe.",
        ));
    }
    for packet in packets {
        if packet.data.is_empty()
            || packet.duration == 0
            || packet.pts < 0
            || usize::try_from(packet.pts)
                .ok()
                .is_none_or(|pts| pts >= frame_count)
        {
            return Err(integrity("Invalid VP9 packet mapping metadata."));
        }
    }
    Ok(())
}

pub fn encode_container(
    width: u32,
    height: u32,
    frame_count: usize,
    packets: &[vp9::Packet],
    limits: &SegmentLimits,
) -> Result<Vec<u8>> {
    limits.validate()?;
    if width == 0 || height == 0 || !(2..=128).contains(&frame_count) {
        return Err(integrity("Invalid VP9 segment dimensions or frame count."));
    }
    validate_packet_metadata(packets, frame_count, limits)?;
    let length = packets.iter().try_fold(HEADER_BYTES, |length, packet| {
        checked_add(checked_add(length, PACKET_HEADER_BYTES)?, packet.data.len())
    })?;
    if length > limits.max_bytes {
        return Err(limited("VP9 segment exceeds the byte limit."));
    }
    let mut output = Vec::with_capacity(length);
    output.extend_from_slice(MAGIC);
    output.extend_from_slice(&CONTAINER_VERSION.to_be_bytes());
    output.extend_from_slice(&width.to_be_bytes());
    output.extend_from_slice(&height.to_be_bytes());
    output.extend_from_slice(
        &u32::try_from(frame_count)
            .map_err(|_| limited("VP9 frame count overflow."))?
            .to_be_bytes(),
    );
    output.extend_from_slice(
        &u32::try_from(packets.len())
            .map_err(|_| limited("VP9 packet count overflow."))?
            .to_be_bytes(),
    );
    for packet in packets {
        output.extend_from_slice(
            &u32::try_from(packet.data.len())
                .map_err(|_| limited("VP9 packet size overflow."))?
                .to_be_bytes(),
        );
        output.extend_from_slice(&packet.pts.to_be_bytes());
        output.extend_from_slice(&packet.duration.to_be_bytes());
        output.push(u8::from(packet.keyframe) | (u8::from(packet.invisible) << 1));
        output.extend_from_slice(&packet.data);
    }
    debug_assert_eq!(output.len(), length);
    Ok(output)
}

pub fn decode_container(bytes: &[u8], limits: &SegmentLimits) -> Result<DecodedContainer> {
    limits.validate()?;
    if bytes.len() > limits.max_bytes {
        return Err(limited("VP9 segment exceeds the byte limit."));
    }
    if bytes.len() < HEADER_BYTES || bytes.get(..8) != Some(MAGIC) {
        return Err(integrity("Invalid VP9 segment container header."));
    }
    let mut cursor = 8;
    let version = u16::from_be_bytes(read(bytes, &mut cursor)?);
    if version != CONTAINER_VERSION {
        return Err(Error::new(
            "E_SCHEMA_VERSION",
            "Unsupported VP9 segment container version.",
        ));
    }
    let width = u32::from_be_bytes(read(bytes, &mut cursor)?);
    let height = u32::from_be_bytes(read(bytes, &mut cursor)?);
    let frame_count = u32::from_be_bytes(read(bytes, &mut cursor)?) as usize;
    let packet_count = u32::from_be_bytes(read(bytes, &mut cursor)?) as usize;
    if width == 0 || height == 0 || !(2..=128).contains(&frame_count) {
        return Err(integrity("Invalid VP9 segment dimensions or frame count."));
    }
    if packet_count == 0 || packet_count > limits.max_packets {
        return Err(limited("VP9 packet count exceeds the segment limit."));
    }
    let minimum = packet_count
        .checked_mul(PACKET_HEADER_BYTES)
        .and_then(|length| length.checked_add(cursor))
        .ok_or_else(|| limited("VP9 packet table size overflow."))?;
    if minimum > bytes.len() {
        return Err(integrity("Truncated VP9 segment packet table."));
    }
    let mut packets = Vec::with_capacity(packet_count);
    for _ in 0..packet_count {
        let data_length = u32::from_be_bytes(read(bytes, &mut cursor)?) as usize;
        let pts = i64::from_be_bytes(read(bytes, &mut cursor)?);
        let duration = u64::from_be_bytes(read(bytes, &mut cursor)?);
        let flags = read::<1>(bytes, &mut cursor)?[0];
        if flags & !0b11 != 0 {
            return Err(integrity("Unknown VP9 segment packet flags."));
        }
        let data_end = checked_add(cursor, data_length)?;
        let data = bytes
            .get(cursor..data_end)
            .ok_or_else(|| integrity("Truncated VP9 segment packet."))?
            .to_vec();
        cursor = data_end;
        packets.push(vp9::Packet {
            data,
            pts,
            duration,
            keyframe: flags & 1 != 0,
            invisible: flags & 2 != 0,
        });
    }
    if cursor != bytes.len() {
        return Err(integrity("VP9 segment container has trailing bytes."));
    }
    validate_packet_metadata(&packets, frame_count, limits)?;
    Ok(DecodedContainer {
        width,
        height,
        frame_count,
        packets,
    })
}

impl StoredSegment {
    pub fn from_sequence(sequence: &vp9::EncodedSequence, limits: &SegmentLimits) -> Result<Self> {
        limits.validate()?;
        let descriptor = SegmentDescriptor {
            version: 1,
            container: "visual-store-vp9-packets-v1".into(),
            codec: sequence.descriptor.clone(),
        };
        let descriptor_json = serde_json::to_string(&descriptor)?;
        if descriptor_json.len() > limits.max_descriptor_bytes {
            return Err(limited("VP9 descriptor exceeds the metadata limit."));
        }
        let color = encode_container(
            sequence.width,
            sequence.height,
            sequence.frame_count,
            &sequence.color_packets,
            limits,
        )?;
        let alpha = sequence
            .alpha_packets
            .as_ref()
            .map(|packets| {
                encode_container(
                    sequence.width,
                    sequence.height,
                    sequence.frame_count,
                    packets,
                    limits,
                )
            })
            .transpose()?;
        let total = color
            .len()
            .checked_add(alpha.as_ref().map_or(0, Vec::len))
            .ok_or_else(|| limited("VP9 segment size overflow."))?;
        if total > limits.max_bytes {
            return Err(limited(
                "Combined color and alpha segment exceeds the byte limit.",
            ));
        }
        Ok(Self {
            descriptor_json,
            color,
            alpha,
        })
    }

    pub fn to_sequence(&self, limits: &SegmentLimits) -> Result<vp9::EncodedSequence> {
        limits.validate()?;
        if self.descriptor_json.len() > limits.max_descriptor_bytes {
            return Err(limited("VP9 descriptor exceeds the metadata limit."));
        }
        let descriptor: SegmentDescriptor = serde_json::from_str(&self.descriptor_json)
            .map_err(|_| integrity("Invalid VP9 segment descriptor."))?;
        if descriptor.version != 1 || descriptor.container != "visual-store-vp9-packets-v1" {
            return Err(Error::new(
                "E_SCHEMA_VERSION",
                "Unsupported VP9 segment descriptor.",
            ));
        }
        let color = decode_container(&self.color, limits)?;
        let alpha = self
            .alpha
            .as_ref()
            .map(|bytes| decode_container(bytes, limits))
            .transpose()?;
        if alpha.as_ref().is_some_and(|alpha| {
            (alpha.width, alpha.height, alpha.frame_count)
                != (color.width, color.height, color.frame_count)
        }) {
            return Err(integrity("Color and alpha segment headers differ."));
        }
        Ok(vp9::EncodedSequence {
            descriptor: descriptor.codec,
            width: color.width,
            height: color.height,
            frame_count: color.frame_count,
            color_packets: color.packets,
            alpha_packets: alpha.map(|alpha| alpha.packets),
        })
    }
}
