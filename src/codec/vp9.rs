//! Direct libvpx VP9 lossless encoding and decoding.
//!
//! RGB samples are placed directly into the three full-resolution I444 planes:
//! R in plane 0, G in plane 1, and B in plane 2. RGBA alpha is encoded in a
//! separate lossless I444 stream's plane 0; its other planes are fixed at zero.
//! This is an internal reversible storage mapping, not conventional YUV video.

#![deny(unsafe_op_in_unsafe_fn)]

use serde::{Deserialize, Serialize};
use std::{
    ffi::CStr,
    fmt,
    os::raw::{c_int, c_ulong},
    ptr,
};
use vpx_sys as ffi;

const DESCRIPTOR_VERSION: u8 = 1;
const VP9_PROFILE_I444_8_BIT: u8 = 1;
const COLOR_LAYOUT: &str = "rgb_planar_i444_direct_v1";
const ALPHA_LAYOUT: &str = "alpha_in_i444_plane0_v1";

/// Pixel layout accepted by the temporal codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PixelLayout {
    /// Packed R, G, B bytes.
    Rgb8,
    /// Packed R, G, B, A bytes.
    Rgba8,
}

impl PixelLayout {
    fn channels(self) -> usize {
        match self {
            Self::Rgb8 => 3,
            Self::Rgba8 => 4,
        }
    }
}

/// Borrowed image frame passed to the encoder.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Packed sample layout.
    pub layout: PixelLayout,
    /// Packed samples in row-major order.
    pub samples: &'a [u8],
}

/// Owned frame returned by the decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Packed sample layout.
    pub layout: PixelLayout,
    /// Packed samples in row-major order.
    pub samples: Vec<u8>,
}

/// Versioned description required to interpret the VP9 streams.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodecDescriptor {
    /// Descriptor schema version.
    pub version: u8,
    /// Codec name.
    pub codec: String,
    /// VP9 profile used for 8-bit I444.
    pub profile: u8,
    /// Sample bit depth.
    pub bit_depth: u8,
    /// Original packed pixel layout.
    pub pixel_layout: PixelLayout,
    /// Reversible mapping of RGB samples to codec planes.
    pub color_layout: String,
    /// Reversible alpha mapping, when present.
    pub alpha_layout: Option<String>,
    /// Declared color metadata; no RGB-to-YUV conversion is performed.
    pub color_space: String,
    /// Whether the libvpx lossless control was required.
    pub lossless: bool,
    /// Runtime libvpx version used to encode the sequence.
    pub libvpx_version: String,
}

/// One encoded VP9 packet copied out of libvpx-owned memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// Encoded bytes in decode order.
    pub data: Vec<u8>,
    /// Presentation timestamp assigned from the input frame index.
    pub pts: i64,
    /// Packet duration in codec timebase units.
    pub duration: u64,
    /// Packet flag reported by libvpx.
    pub keyframe: bool,
    /// Whether the packet itself is not displayed.
    pub invisible: bool,
}

/// Encoded multi-frame sequence. Container storage is added by the segment task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedSequence {
    /// Versioned interpretation metadata.
    pub descriptor: CodecDescriptor,
    /// Frame width.
    pub width: u32,
    /// Frame height.
    pub height: u32,
    /// Number of input display frames.
    pub frame_count: usize,
    /// RGB VP9 packets in decode order.
    pub color_packets: Vec<Packet>,
    /// Alpha VP9 packets in decode order for RGBA input.
    pub alpha_packets: Option<Vec<Packet>>,
}

/// Header information parsed by libvpx's decoder interface from a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketInfo {
    /// Whether the compressed packet is a key frame.
    pub keyframe: bool,
    /// Width reported for headers that carry dimensions.
    pub width: u32,
    /// Height reported for headers that carry dimensions.
    pub height: u32,
}

/// Codec failure with a bounded diagnostic string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecError {
    operation: &'static str,
    message: String,
}

impl CodecError {
    fn input(message: impl Into<String>) -> Self {
        Self {
            operation: "input validation",
            message: message.into(),
        }
    }

    fn unsupported(message: impl Into<String>) -> Self {
        Self {
            operation: "descriptor validation",
            message: message.into(),
        }
    }

    fn vpx(
        operation: &'static str,
        code: ffi::vpx_codec_err_t,
        context: Option<&ffi::vpx_codec_ctx_t>,
    ) -> Self {
        let mut message = format!("libvpx returned {code:?}");
        if let Some(context) = context {
            // SAFETY: libvpx owns both NUL-terminated strings for the lifetime
            // of the initialized context. We copy immediately.
            unsafe {
                for value in [
                    ffi::vpx_codec_error(context),
                    ffi::vpx_codec_error_detail(context),
                ] {
                    if value.is_null() {
                        continue;
                    }
                    let Ok(value) = CStr::from_ptr(value).to_str() else {
                        continue;
                    };
                    if !value.is_empty() && !message.contains(value) {
                        message.push_str(": ");
                        message.push_str(value);
                    }
                }
            }
        }
        Self { operation, message }
    }
}

impl fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.operation, self.message)
    }
}

impl std::error::Error for CodecError {}

type Result<T> = std::result::Result<T, CodecError>;

struct Encoder {
    context: ffi::vpx_codec_ctx_t,
}

impl Encoder {
    fn new(width: u32, height: u32) -> Result<Self> {
        // SAFETY: the interface pointer and ABI version come from the linked
        // libvpx. Config and context are initialized before use.
        unsafe {
            let interface = ffi::vpx_codec_vp9_cx();
            if interface.is_null() {
                return Err(CodecError::input("VP9 encoder interface is unavailable"));
            }
            let mut config = ffi::vpx_codec_enc_cfg_t::default();
            let status = ffi::vpx_codec_enc_config_default(interface, &mut config, 0);
            check("vpx_codec_enc_config_default", status, None)?;
            config.g_w = width;
            config.g_h = height;
            config.g_threads = 1;
            config.g_profile = u32::from(VP9_PROFILE_I444_8_BIT);
            config.g_bit_depth = ffi::vpx_bit_depth::VPX_BITS_8;
            config.g_input_bit_depth = 8;
            config.g_timebase.num = 1;
            config.g_timebase.den = 30;
            config.g_lag_in_frames = 0;
            config.rc_dropframe_thresh = 0;
            config.rc_resize_allowed = 0;
            config.rc_end_usage = ffi::vpx_rc_mode::VPX_Q;
            config.rc_min_quantizer = 0;
            config.rc_max_quantizer = 0;
            config.kf_mode = ffi::vpx_kf_mode::VPX_KF_AUTO;
            config.kf_min_dist = 0;
            config.kf_max_dist = 128;

            let mut context = ffi::vpx_codec_ctx_t::default();
            let status = ffi::vpx_codec_enc_init_ver(
                &mut context,
                interface,
                &config,
                0,
                ffi::VPX_ENCODER_ABI_VERSION as c_int,
            );
            check("vpx_codec_enc_init_ver", status, Some(&context))?;
            let mut encoder = Self { context };
            encoder.control(
                ffi::vp8e_enc_control_id::VP9E_SET_LOSSLESS as c_int,
                1,
                "VP9E_SET_LOSSLESS",
            )?;
            encoder.control(
                ffi::vp8e_enc_control_id::VP8E_SET_CPUUSED as c_int,
                4,
                "VP8E_SET_CPUUSED",
            )?;
            encoder.control(
                ffi::vp8e_enc_control_id::VP9E_SET_COLOR_SPACE as c_int,
                ffi::vpx_color_space::VPX_CS_SRGB as c_int,
                "VP9E_SET_COLOR_SPACE",
            )?;
            encoder.control(
                ffi::vp8e_enc_control_id::VP9E_SET_COLOR_RANGE as c_int,
                ffi::vpx_color_range::VPX_CR_FULL_RANGE as c_int,
                "VP9E_SET_COLOR_RANGE",
            )?;
            Ok(encoder)
        }
    }

    fn control(&mut self, id: c_int, value: c_int, operation: &'static str) -> Result<()> {
        // SAFETY: these libvpx controls all accept one integer argument.
        let status = unsafe { ffi::vpx_codec_control_(&mut self.context, id, value) };
        check(operation, status, Some(&self.context))
    }

    fn encode(
        &mut self,
        data: &mut [u8],
        width: u32,
        height: u32,
        pts: i64,
    ) -> Result<Vec<Packet>> {
        let mut image = ffi::vpx_image_t::default();
        // SAFETY: `data` contains exactly three width*height planes and remains
        // live until vpx_codec_encode returns. Alignment 1 permits tight rows.
        let wrapped = unsafe {
            ffi::vpx_img_wrap(
                &mut image,
                ffi::vpx_img_fmt_t::VPX_IMG_FMT_I444,
                width,
                height,
                1,
                data.as_mut_ptr(),
            )
        };
        if wrapped.is_null() {
            return Err(CodecError::input("libvpx rejected the I444 frame buffer"));
        }
        image.cs = ffi::vpx_color_space::VPX_CS_SRGB;
        image.range = ffi::vpx_color_range::VPX_CR_FULL_RANGE;
        let flags = if pts == 0 {
            ffi::VPX_EFLAG_FORCE_KF as ffi::vpx_enc_frame_flags_t
        } else {
            0
        };
        // SAFETY: context and wrapped image are valid for the duration of the call.
        let status = unsafe {
            ffi::vpx_codec_encode(
                &mut self.context,
                &image,
                pts,
                1,
                flags,
                ffi::VPX_DL_GOOD_QUALITY as c_ulong,
            )
        };
        check("vpx_codec_encode", status, Some(&self.context))?;
        self.drain()
    }

    fn finish(&mut self) -> Result<Vec<Packet>> {
        let mut packets = Vec::new();
        loop {
            // SAFETY: a null image flushes delayed packets from an initialized encoder.
            let status = unsafe {
                ffi::vpx_codec_encode(
                    &mut self.context,
                    ptr::null(),
                    -1,
                    1,
                    0,
                    ffi::VPX_DL_GOOD_QUALITY as c_ulong,
                )
            };
            check("vpx_codec_encode(flush)", status, Some(&self.context))?;
            let batch = self.drain()?;
            if batch.is_empty() {
                return Ok(packets);
            }
            packets.extend(batch);
        }
    }

    fn drain(&mut self) -> Result<Vec<Packet>> {
        let mut packets = Vec::new();
        let mut iterator: ffi::vpx_codec_iter_t = ptr::null();
        loop {
            // SAFETY: iterator follows libvpx's get-cx-data protocol and packet
            // memory is copied before the next call can invalidate it.
            let packet = unsafe { ffi::vpx_codec_get_cx_data(&mut self.context, &mut iterator) };
            if packet.is_null() {
                return Ok(packets);
            }
            // SAFETY: non-null packet is valid until the next codec call.
            let packet = unsafe { &*packet };
            if packet.kind != ffi::vpx_codec_cx_pkt_kind::VPX_CODEC_CX_FRAME_PKT {
                continue;
            }
            // SAFETY: the active union member is `frame` for a frame packet.
            let frame = unsafe { packet.data.frame };
            if frame.buf.is_null() || frame.sz == 0 {
                return Err(CodecError::input("libvpx returned an empty frame packet"));
            }
            // SAFETY: libvpx guarantees frame.buf spans frame.sz bytes for this packet.
            let data = unsafe { std::slice::from_raw_parts(frame.buf.cast::<u8>(), frame.sz) };
            packets.push(Packet {
                data: data.to_vec(),
                pts: frame.pts,
                duration: frame.duration as u64,
                keyframe: frame.flags & ffi::VPX_FRAME_IS_KEY != 0,
                invisible: frame.flags & ffi::VPX_FRAME_IS_INVISIBLE != 0,
            });
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: context was successfully initialized and is destroyed once.
        unsafe {
            let _ = ffi::vpx_codec_destroy(&mut self.context);
        }
    }
}

struct Decoder {
    context: ffi::vpx_codec_ctx_t,
}

impl Decoder {
    fn new(width: u32, height: u32) -> Result<Self> {
        // SAFETY: interface and ABI version come from linked libvpx; config is initialized.
        unsafe {
            let interface = ffi::vpx_codec_vp9_dx();
            if interface.is_null() {
                return Err(CodecError::input("VP9 decoder interface is unavailable"));
            }
            let config = ffi::vpx_codec_dec_cfg_t {
                threads: 1,
                w: width,
                h: height,
            };
            let mut context = ffi::vpx_codec_ctx_t::default();
            let status = ffi::vpx_codec_dec_init_ver(
                &mut context,
                interface,
                &config,
                0,
                ffi::VPX_DECODER_ABI_VERSION as c_int,
            );
            check("vpx_codec_dec_init_ver", status, Some(&context))?;
            Ok(Self { context })
        }
    }

    fn decode(&mut self, packet: &[u8]) -> Result<Vec<PlanarFrame>> {
        let size = u32::try_from(packet.len())
            .map_err(|_| CodecError::input("VP9 packet exceeds libvpx's input size"))?;
        // SAFETY: packet is immutable and live for the complete decode call.
        let status = unsafe {
            ffi::vpx_codec_decode(&mut self.context, packet.as_ptr(), size, ptr::null_mut(), 0)
        };
        check("vpx_codec_decode", status, Some(&self.context))?;
        self.drain()
    }

    fn finish(&mut self) -> Result<Vec<PlanarFrame>> {
        // SAFETY: null input flushes an initialized decoder.
        let status =
            unsafe { ffi::vpx_codec_decode(&mut self.context, ptr::null(), 0, ptr::null_mut(), 0) };
        check("vpx_codec_decode(flush)", status, Some(&self.context))?;
        self.drain()
    }

    fn drain(&mut self) -> Result<Vec<PlanarFrame>> {
        let mut frames = Vec::new();
        let mut iterator: ffi::vpx_codec_iter_t = ptr::null();
        loop {
            // SAFETY: iterator follows the get-frame protocol. Samples are copied
            // before another codec operation invalidates the returned image.
            let image = unsafe { ffi::vpx_codec_get_frame(&mut self.context, &mut iterator) };
            if image.is_null() {
                return Ok(frames);
            }
            // SAFETY: non-null image is owned by libvpx until the next decode call.
            frames.push(unsafe { copy_i444(&*image)? });
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: context was successfully initialized and is destroyed once.
        unsafe {
            let _ = ffi::vpx_codec_destroy(&mut self.context);
        }
    }
}

struct PlanarFrame {
    width: u32,
    height: u32,
    planes: [Vec<u8>; 3],
}

fn check(
    operation: &'static str,
    status: ffi::vpx_codec_err_t,
    context: Option<&ffi::vpx_codec_ctx_t>,
) -> Result<()> {
    if status == ffi::vpx_codec_err_t::VPX_CODEC_OK {
        Ok(())
    } else {
        Err(CodecError::vpx(operation, status, context))
    }
}

fn validate_frames(frames: &[Frame<'_>]) -> Result<(u32, u32, PixelLayout, usize)> {
    let first = frames
        .first()
        .ok_or_else(|| CodecError::input("at least two frames are required"))?;
    if frames.len() < 2 {
        return Err(CodecError::input("at least two frames are required"));
    }
    if first.width == 0 || first.height == 0 {
        return Err(CodecError::input("frame dimensions must be positive"));
    }
    let pixels = usize::try_from(first.width)
        .ok()
        .and_then(|width| {
            usize::try_from(first.height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or_else(|| CodecError::input("frame dimensions overflow"))?;
    for frame in frames {
        if frame.width != first.width
            || frame.height != first.height
            || frame.layout != first.layout
        {
            return Err(CodecError::input(
                "all frames must have identical dimensions and pixel layout",
            ));
        }
        let expected = pixels
            .checked_mul(frame.layout.channels())
            .ok_or_else(|| CodecError::input("sample length overflow"))?;
        if frame.samples.len() != expected {
            return Err(CodecError::input(format!(
                "frame has {} samples; expected {expected}",
                frame.samples.len()
            )));
        }
    }
    Ok((first.width, first.height, first.layout, pixels))
}

fn color_planes(frame: Frame<'_>, pixels: usize) -> Vec<u8> {
    let channels = frame.layout.channels();
    let mut planes = vec![0; pixels * 3];
    for (index, sample) in frame.samples.chunks_exact(channels).enumerate() {
        planes[index] = sample[0];
        planes[pixels + index] = sample[1];
        planes[pixels * 2 + index] = sample[2];
    }
    planes
}

fn alpha_planes(frame: Frame<'_>, pixels: usize) -> Vec<u8> {
    let mut planes = vec![0; pixels * 3];
    for (index, sample) in frame.samples.chunks_exact(4).enumerate() {
        planes[index] = sample[3];
    }
    planes
}

fn encode_stream(
    frames: &[Frame<'_>],
    width: u32,
    height: u32,
    pixels: usize,
    alpha: bool,
) -> Result<Vec<Packet>> {
    let mut encoder = Encoder::new(width, height)?;
    let mut packets = Vec::new();
    for (index, frame) in frames.iter().copied().enumerate() {
        let mut planes = if alpha {
            alpha_planes(frame, pixels)
        } else {
            color_planes(frame, pixels)
        };
        packets.extend(encoder.encode(&mut planes, width, height, index as i64)?);
    }
    packets.extend(encoder.finish()?);
    Ok(packets)
}

/// Encode two or more frames with one lossless VP9 encoder per stored stream.
pub fn encode(frames: &[Frame<'_>]) -> Result<EncodedSequence> {
    let (width, height, layout, pixels) = validate_frames(frames)?;
    let color_packets = encode_stream(frames, width, height, pixels, false)?;
    let alpha_packets = if layout == PixelLayout::Rgba8 {
        Some(encode_stream(frames, width, height, pixels, true)?)
    } else {
        None
    };
    Ok(EncodedSequence {
        descriptor: CodecDescriptor {
            version: DESCRIPTOR_VERSION,
            codec: "vp9".into(),
            profile: VP9_PROFILE_I444_8_BIT,
            bit_depth: 8,
            pixel_layout: layout,
            color_layout: COLOR_LAYOUT.into(),
            alpha_layout: alpha_packets.as_ref().map(|_| ALPHA_LAYOUT.into()),
            color_space: "srgb_full_range_no_conversion".into(),
            lossless: true,
            libvpx_version: libvpx_version(),
        },
        width,
        height,
        frame_count: frames.len(),
        color_packets,
        alpha_packets,
    })
}

fn validate_descriptor(sequence: &EncodedSequence) -> Result<()> {
    let descriptor = &sequence.descriptor;
    if descriptor.version != DESCRIPTOR_VERSION
        || descriptor.codec != "vp9"
        || descriptor.profile != VP9_PROFILE_I444_8_BIT
        || descriptor.bit_depth != 8
        || descriptor.color_layout != COLOR_LAYOUT
        || descriptor.color_space != "srgb_full_range_no_conversion"
        || !descriptor.lossless
    {
        return Err(CodecError::unsupported("unsupported VP9 codec descriptor"));
    }
    match descriptor.pixel_layout {
        PixelLayout::Rgb8
            if descriptor.alpha_layout.is_none() && sequence.alpha_packets.is_none() =>
        {
            Ok(())
        }
        PixelLayout::Rgba8
            if descriptor.alpha_layout.as_deref() == Some(ALPHA_LAYOUT)
                && sequence.alpha_packets.is_some() =>
        {
            Ok(())
        }
        _ => Err(CodecError::unsupported(
            "pixel and alpha layouts do not match the encoded streams",
        )),
    }
}

fn decode_stream(packets: &[Packet], width: u32, height: u32) -> Result<Vec<PlanarFrame>> {
    let mut decoder = Decoder::new(width, height)?;
    let mut frames = Vec::new();
    for packet in packets {
        frames.extend(decoder.decode(&packet.data)?);
    }
    frames.extend(decoder.finish()?);
    if frames
        .iter()
        .any(|frame| frame.width != width || frame.height != height)
    {
        return Err(CodecError::input(
            "decoded frame dimensions differ from the descriptor",
        ));
    }
    Ok(frames)
}

/// Decode and reverse the internal plane mapping for every display frame.
pub fn decode(sequence: &EncodedSequence) -> Result<Vec<DecodedFrame>> {
    validate_descriptor(sequence)?;
    let colors = decode_stream(&sequence.color_packets, sequence.width, sequence.height)?;
    let alphas = sequence
        .alpha_packets
        .as_ref()
        .map(|packets| decode_stream(packets, sequence.width, sequence.height))
        .transpose()?;
    if colors.len() != sequence.frame_count
        || alphas
            .as_ref()
            .is_some_and(|frames| frames.len() != sequence.frame_count)
    {
        return Err(CodecError::input(
            "decoded display-frame count differs from the descriptor",
        ));
    }
    let pixels = usize::try_from(sequence.width)
        .ok()
        .and_then(|width| {
            usize::try_from(sequence.height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or_else(|| CodecError::input("decoded frame dimensions overflow"))?;
    let channels = sequence.descriptor.pixel_layout.channels();
    let mut decoded = Vec::with_capacity(sequence.frame_count);
    for index in 0..sequence.frame_count {
        let color = &colors[index];
        let alpha = alphas.as_ref().map(|frames| &frames[index]);
        if alpha.is_some_and(|alpha| {
            alpha.planes[1].iter().any(|&sample| sample != 0)
                || alpha.planes[2].iter().any(|&sample| sample != 0)
        }) {
            return Err(CodecError::input("alpha padding planes are not zero"));
        }
        let mut samples = Vec::with_capacity(pixels * channels);
        for pixel in 0..pixels {
            samples.push(color.planes[0][pixel]);
            samples.push(color.planes[1][pixel]);
            samples.push(color.planes[2][pixel]);
            if let Some(alpha) = alpha {
                samples.push(alpha.planes[0][pixel]);
            }
        }
        decoded.push(DecodedFrame {
            width: sequence.width,
            height: sequence.height,
            layout: sequence.descriptor.pixel_layout,
            samples,
        });
    }
    Ok(decoded)
}

/// Ask libvpx's VP9 decoder interface to parse packet header metadata.
pub fn inspect_packet(packet: &[u8]) -> Result<PacketInfo> {
    let size = u32::try_from(packet.len())
        .map_err(|_| CodecError::input("VP9 packet exceeds libvpx's input size"))?;
    let mut info = ffi::vpx_codec_stream_info_t {
        sz: std::mem::size_of::<ffi::vpx_codec_stream_info_t>() as u32,
        w: 0,
        h: 0,
        is_kf: 0,
    };
    // SAFETY: packet and initialized stream-info remain valid for the call.
    let status = unsafe {
        ffi::vpx_codec_peek_stream_info(ffi::vpx_codec_vp9_dx(), packet.as_ptr(), size, &mut info)
    };
    check("vpx_codec_peek_stream_info", status, None)?;
    Ok(PacketInfo {
        keyframe: info.is_kf != 0,
        width: info.w,
        height: info.h,
    })
}

/// Runtime libvpx version string copied from the linked library.
pub fn libvpx_version() -> String {
    // SAFETY: libvpx returns a process-lifetime NUL-terminated constant.
    unsafe {
        let value = ffi::vpx_codec_version_str();
        if value.is_null() {
            "unknown".into()
        } else {
            CStr::from_ptr(value).to_string_lossy().into_owned()
        }
    }
}

/// Runtime libvpx build configuration copied from the linked library.
pub fn libvpx_build_config() -> String {
    // SAFETY: libvpx returns a process-lifetime NUL-terminated constant.
    unsafe {
        let value = ffi::vpx_codec_build_config();
        if value.is_null() {
            "unknown".into()
        } else {
            CStr::from_ptr(value).to_string_lossy().into_owned()
        }
    }
}

unsafe fn copy_i444(image: &ffi::vpx_image_t) -> Result<PlanarFrame> {
    if image.fmt != ffi::vpx_img_fmt_t::VPX_IMG_FMT_I444 {
        return Err(CodecError::input(format!(
            "decoder returned unsupported image format {:?}",
            image.fmt
        )));
    }
    let width = image.d_w;
    let height = image.d_h;
    if width == 0 || height == 0 {
        return Err(CodecError::input("decoder returned empty dimensions"));
    }
    let width_usize = width as usize;
    let height_usize = height as usize;
    let mut planes = [Vec::new(), Vec::new(), Vec::new()];
    for (plane_index, plane) in planes.iter_mut().enumerate() {
        let stride = usize::try_from(image.stride[plane_index])
            .map_err(|_| CodecError::input("decoder returned a negative plane stride"))?;
        if stride < width_usize || image.planes[plane_index].is_null() {
            return Err(CodecError::input("decoder returned an invalid plane"));
        }
        plane.reserve(width_usize * height_usize);
        for row in 0..height_usize {
            // SAFETY: libvpx reports a non-null plane with stride bytes for each row;
            // width was checked not to exceed that stride, and we copy immediately.
            let row_start = unsafe { image.planes[plane_index].add(row * stride) };
            let samples = unsafe { std::slice::from_raw_parts(row_start, width_usize) };
            plane.extend_from_slice(samples);
        }
    }
    Ok(PlanarFrame {
        width,
        height,
        planes,
    })
}
