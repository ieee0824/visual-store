//! Metadata-compatible VP9 boundary for builds without the native codec.

use serde::{Deserialize, Serialize};
use std::{fmt, time::Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PixelLayout {
    Rgb8,
    Rgba8,
}

#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    pub layout: PixelLayout,
    pub samples: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub layout: PixelLayout,
    pub samples: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodecDescriptor {
    pub version: u8,
    pub codec: String,
    pub profile: u8,
    pub bit_depth: u8,
    pub pixel_layout: PixelLayout,
    pub color_layout: String,
    pub alpha_layout: Option<String>,
    pub color_space: String,
    pub lossless: bool,
    pub libvpx_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub data: Vec<u8>,
    pub pts: i64,
    pub duration: u64,
    pub keyframe: bool,
    pub invisible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedSequence {
    pub descriptor: CodecDescriptor,
    pub width: u32,
    pub height: u32,
    pub frame_count: usize,
    pub color_packets: Vec<Packet>,
    pub alpha_packets: Option<Vec<Packet>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadByteLengths {
    pub color: u64,
    pub alpha: u64,
    pub total: u64,
}

impl EncodedSequence {
    pub fn payload_byte_lengths(&self) -> Result<PayloadByteLengths> {
        Err(CodecError::unavailable())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketInfo {
    pub keyframe: bool,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecError;
impl CodecError {
    fn unavailable() -> Self {
        Self
    }
    pub fn is_limit_exceeded(&self) -> bool {
        false
    }
    pub fn is_unavailable(&self) -> bool {
        true
    }
}
impl fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("VP9 codec support is not available in this build")
    }
}
impl std::error::Error for CodecError {}
pub type Result<T> = std::result::Result<T, CodecError>;

pub fn encode(_: &[Frame<'_>]) -> Result<EncodedSequence> {
    Err(CodecError::unavailable())
}
pub fn encode_with_time_limit(_: &[Frame<'_>], _: Duration) -> Result<EncodedSequence> {
    Err(CodecError::unavailable())
}
pub fn decode(_: &EncodedSequence) -> Result<Vec<DecodedFrame>> {
    Err(CodecError::unavailable())
}
pub fn decode_prefix(_: &EncodedSequence, _: usize) -> Result<Vec<DecodedFrame>> {
    Err(CodecError::unavailable())
}
pub fn inspect_packet(_: &[u8]) -> Result<PacketInfo> {
    Err(CodecError::unavailable())
}
pub fn libvpx_version() -> String {
    "unavailable".into()
}
pub fn libvpx_build_config() -> String {
    "unavailable".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_operations_are_explicitly_unavailable() {
        let samples = [0u8; 12];
        let frames = [
            Frame {
                width: 2,
                height: 2,
                layout: PixelLayout::Rgb8,
                samples: &samples,
            },
            Frame {
                width: 2,
                height: 2,
                layout: PixelLayout::Rgb8,
                samples: &samples,
            },
        ];
        assert!(encode(&frames).unwrap_err().is_unavailable());
    }
}
