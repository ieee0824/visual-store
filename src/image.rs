//! PNG container repacking; png handles sample decoding, flate2 handles zlib,
//! and crc32fast handles CRC. No pixel transforms or filter selection happen here.
pub mod reconstruction;

use crate::{Error, Result, sha256};
use flate2::{Compression, Decompress, FlushDecompress, Status, write::ZlibEncoder};
use serde::{Deserialize, Serialize};
use std::{
    io::{Cursor, Write},
    ops::Range,
};

const MIB: usize = 1024 * 1024;
const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Limits {
    pub source_bytes: usize,
    pub max_edge: u32,
    pub pixels: u64,
    pub inflated_bytes: usize,
    pub memory_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            source_bytes: 64 * MIB,
            max_edge: 16384,
            pixels: 16777216,
            inflated_bytes: 128 * MIB,
            memory_bytes: 256 * MIB,
        }
    }
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        if self.source_bytes == 0
            || self.max_edge == 0
            || self.pixels == 0
            || self.inflated_bytes == 0
            || self.memory_bytes == 0
        {
            return Err(crate::error::invalid("Limits must be positive and finite."));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImageMeta {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub color_type: u8,
    pub scanline_sha256: String,
    pub non_idat_sha256: String,
    pub pixel_sha256: String,
}
pub struct Repacked {
    pub bytes: Vec<u8>,
    pub meta: ImageMeta,
    pub compression_applied: bool,
}
struct Chunk {
    kind: [u8; 4],
    full: Range<usize>,
    data: Range<usize>,
}
struct Parsed {
    chunks: Vec<Chunk>,
    width: u32,
    height: u32,
    color: u8,
    expected: usize,
}

fn bad(msg: &str) -> Error {
    Error::new("E_INVALID_IMAGE", msg)
}
fn limited() -> Error {
    Error::new(
        "E_LIMIT_EXCEEDED",
        "PNG exceeds a configured resource limit.",
    )
}
fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b.try_into().expect("checked length"))
}

fn parse(bytes: &[u8], limits: &Limits) -> Result<Parsed> {
    limits.validate()?;
    if bytes.len() > limits.source_bytes {
        return Err(limited());
    }
    if !bytes.starts_with(SIGNATURE) {
        return Err(bad("Expected a PNG signature."));
    }
    let mut chunks = Vec::new();
    let mut p = 8usize;
    let mut idat = false;
    let mut idat_ended = false;
    let mut iend = false;
    let mut singleton = std::collections::HashSet::new();
    while p < bytes.len() {
        if chunks.len() >= 65536 {
            return Err(limited());
        }
        if bytes.len() - p < 12 {
            return Err(bad("Truncated PNG chunk."));
        }
        let n = be32(&bytes[p..p + 4]) as usize;
        let end = p
            .checked_add(n)
            .and_then(|v| v.checked_add(12))
            .ok_or_else(limited)?;
        if n > 0x7fff_ffff || end > bytes.len() {
            return Err(bad("Invalid PNG chunk length."));
        }
        let kind: [u8; 4] = bytes[p + 4..p + 8].try_into().unwrap();
        if !kind.iter().all(u8::is_ascii_alphabetic) || !kind[2].is_ascii_uppercase() {
            return Err(bad("Invalid PNG chunk type."));
        }
        if crc32fast::hash(&bytes[p + 4..end - 4]) != be32(&bytes[end - 4..end]) {
            return Err(bad("PNG CRC mismatch."));
        }
        if chunks.is_empty() && kind != *b"IHDR" {
            return Err(bad("IHDR must be first."));
        }
        if matches!(&kind, b"acTL" | b"fcTL" | b"fdAT") {
            return Err(Error::new("E_UNSUPPORTED_IMAGE", "APNG is not supported."));
        }
        let known = matches!(
            &kind,
            b"IHDR"
                | b"PLTE"
                | b"IDAT"
                | b"IEND"
                | b"cHRM"
                | b"gAMA"
                | b"iCCP"
                | b"sBIT"
                | b"sRGB"
                | b"bKGD"
                | b"tRNS"
                | b"pHYs"
                | b"tIME"
                | b"tEXt"
                | b"zTXt"
                | b"iTXt"
                | b"eXIf"
                | b"cICP"
                | b"mDCV"
                | b"cLLI"
        );
        if !known && (kind[0].is_ascii_uppercase() || kind[3].is_ascii_uppercase()) {
            return Err(Error::new(
                "E_UNSUPPORTED_METADATA",
                "Unsupported critical or unsafe-to-copy chunk.",
            ));
        }
        if known
            && !matches!(&kind, b"IDAT" | b"tEXt" | b"zTXt" | b"iTXt")
            && !singleton.insert(kind)
        {
            return Err(bad("Duplicate singleton PNG chunk."));
        }
        if kind == *b"IDAT" {
            if idat_ended {
                return Err(bad("IDAT chunks must be contiguous."));
            }
            idat = true;
        } else if idat {
            idat_ended = true;
        }
        if idat
            && matches!(
                &kind,
                b"PLTE"
                    | b"cHRM"
                    | b"gAMA"
                    | b"iCCP"
                    | b"sBIT"
                    | b"sRGB"
                    | b"bKGD"
                    | b"tRNS"
                    | b"pHYs"
                    | b"cICP"
                    | b"mDCV"
                    | b"cLLI"
            )
        {
            return Err(bad("PNG metadata is after IDAT."));
        }
        chunks.push(Chunk {
            kind,
            full: p..end,
            data: p + 8..end - 4,
        });
        p = end;
        if kind == *b"IEND" {
            if n != 0 || p != bytes.len() || !idat {
                return Err(bad("Invalid IEND or trailing data."));
            }
            iend = true;
            break;
        }
    }
    if !iend {
        return Err(bad("Missing IEND."));
    }
    let h = &bytes[chunks[0].data.clone()];
    if h.len() != 13 {
        return Err(bad("Invalid IHDR."));
    }
    let (width, height) = (be32(&h[0..4]), be32(&h[4..8]));
    if width == 0 || height == 0 || h[10] != 0 || h[11] != 0 || h[12] > 1 {
        return Err(bad("Invalid IHDR values."));
    }
    if h[8] != 8 || !matches!(h[9], 2 | 6) || h[12] != 0 {
        return Err(Error::new(
            "E_UNSUPPORTED_IMAGE",
            "Only non-interlaced 8-bit RGB/RGBA PNG is supported.",
        ));
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(limited)?;
    let expected = u64::from(width)
        .checked_mul(if h[9] == 2 { 3 } else { 4 })
        .and_then(|v| v.checked_add(1))
        .and_then(|v| v.checked_mul(height.into()))
        .and_then(|v| usize::try_from(v).ok())
        .ok_or_else(limited)?;
    // Includes original/candidate/container copies, scanlines, decoding buffers and headroom.
    let memory = bytes
        .len()
        .checked_mul(3)
        .and_then(|v| expected.checked_mul(4).and_then(|n| v.checked_add(n)))
        .and_then(|v| v.checked_add(16 * MIB))
        .ok_or_else(limited)?;
    if width > limits.max_edge
        || height > limits.max_edge
        || pixels > limits.pixels
        || expected > limits.inflated_bytes
        || memory > limits.memory_bytes
    {
        return Err(limited());
    }
    Ok(Parsed {
        chunks,
        width,
        height,
        color: h[9],
        expected,
    })
}

fn scanlines(bytes: &[u8], parsed: &Parsed) -> Result<Vec<u8>> {
    let compressed: Vec<u8> = parsed
        .chunks
        .iter()
        .filter(|c| c.kind == *b"IDAT")
        .flat_map(|c| bytes[c.data.clone()].iter().copied())
        .collect();
    let mut output = vec![0; parsed.expected.checked_add(1).ok_or_else(limited)?];
    let mut decoder = Decompress::new(true);
    let status = decoder
        .decompress(&compressed, &mut output, FlushDecompress::Finish)
        .map_err(|_| bad("Invalid zlib stream."))?;
    if status != Status::StreamEnd
        || decoder.total_out() != parsed.expected as u64
        || decoder.total_in() != compressed.len() as u64
    {
        return Err(bad("Unexpected decompressed length or trailing zlib data."));
    }
    output.truncate(parsed.expected);
    let stride = parsed.expected / parsed.height as usize;
    if output.chunks_exact(stride).any(|row| row[0] > 4) {
        return Err(bad("Invalid PNG filter."));
    }
    Ok(output)
}

fn pixels(bytes: &[u8], limits: &Limits) -> Result<Vec<u8>> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_limits(png::Limits {
        bytes: limits.memory_bytes,
    });
    decoder.set_transformations(png::Transformations::IDENTITY);
    // Preserve raw chunks; do not inflate ancillary text/ICC data merely to store it.
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    let mut reader = decoder
        .read_info()
        .map_err(|_| bad("PNG decoder rejected the header."))?;
    let n = reader.output_buffer_size().ok_or_else(limited)?;
    if n > limits.inflated_bytes {
        return Err(limited());
    }
    let mut buf = vec![0; n];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|_| bad("PNG decoder rejected the image."))?;
    buf.truncate(info.buffer_size());
    reader
        .finish()
        .map_err(|_| bad("PNG decoder rejected trailing chunks."))?;
    Ok(buf)
}

fn metadata(bytes: &[u8], p: &Parsed, scan: &[u8], pixel: &[u8]) -> ImageMeta {
    use sha2::{Digest, Sha256};
    let mut non_idat = Sha256::new();
    for c in &p.chunks {
        if c.kind != *b"IDAT" {
            non_idat.update(&bytes[c.full.start..c.full.end - 4]);
        }
    }
    ImageMeta {
        width: p.width,
        height: p.height,
        bit_depth: 8,
        color_type: p.color,
        scanline_sha256: sha256(scan),
        non_idat_sha256: crate::hex(&non_idat.finalize()),
        pixel_sha256: sha256(pixel),
    }
}

pub fn validate(bytes: &[u8], limits: &Limits) -> Result<ImageMeta> {
    let p = parse(bytes, limits)?;
    let scan = scanlines(bytes, &p)?;
    let pixel = pixels(bytes, limits)?;
    Ok(metadata(bytes, &p, &scan, &pixel))
}

pub fn repack(bytes: &[u8], level: u32, limits: &Limits) -> Result<Repacked> {
    if level > 9 {
        return Err(crate::error::invalid(
            "Compression level must be 0 through 9.",
        ));
    }
    let parsed = parse(bytes, limits)?;
    let scan = scanlines(bytes, &parsed)?;
    let pixel = pixels(bytes, limits)?;
    let meta = metadata(bytes, &parsed, &scan, &pixel);
    let mut z = ZlibEncoder::new(Vec::new(), Compression::new(level));
    z.write_all(&scan)?;
    let compressed = z.finish()?;
    let mut candidate = Vec::with_capacity(bytes.len());
    candidate.extend_from_slice(SIGNATURE);
    let mut inserted = false;
    for c in &parsed.chunks {
        if c.kind == *b"IDAT" {
            if !inserted {
                for part in compressed.chunks(1024 * 1024) {
                    write_chunk(&mut candidate, b"IDAT", part);
                }
                inserted = true;
            }
        } else {
            candidate.extend_from_slice(&bytes[c.full.clone()]);
        }
    }
    // A larger candidate will never be saved; avoid applying input size limits to it.
    if candidate.len() >= bytes.len() {
        return Ok(Repacked {
            bytes: bytes.to_vec(),
            meta,
            compression_applied: false,
        });
    }
    let candidate_parsed = parse(&candidate, limits)?;
    if scanlines(&candidate, &candidate_parsed)? != scan || pixels(&candidate, limits)? != pixel {
        return Err(crate::error::integrity("PNG round-trip mismatch."));
    }
    if metadata(&candidate, &candidate_parsed, &scan, &pixel) != meta {
        return Err(crate::error::integrity("PNG metadata changed."));
    }
    Ok(Repacked {
        bytes: candidate,
        meta,
        compression_applied: true,
    })
}

pub fn write_chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    output.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = output.len();
    output.extend_from_slice(kind);
    output.extend_from_slice(data);
    output.extend_from_slice(&crc32fast::hash(&output[start..]).to_be_bytes());
}
