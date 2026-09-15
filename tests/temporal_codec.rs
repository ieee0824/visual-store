use visual_store::codec::vp9::{
    CodecDescriptor, Frame, PixelLayout, decode, encode, inspect_packet,
};

fn rgb_frames(width: u32, height: u32, count: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|frame| {
            let mut samples = Vec::with_capacity(width as usize * height as usize * 3);
            for y in 0..height {
                for x in 0..width {
                    let changed =
                        (2 + frame as u32..6 + frame as u32).contains(&x) && (3..8).contains(&y);
                    samples.extend_from_slice(&[
                        if changed { 255 } else { (x * 13) as u8 },
                        if changed { 0 } else { (y * 17) as u8 },
                        if changed { 31 } else { ((x + y) * 7) as u8 },
                    ]);
                }
            }
            samples
        })
        .collect()
}

#[test]
fn vp9_lossless_round_trip_uses_inter_frames() {
    let (width, height) = (17, 13);
    let samples = rgb_frames(width, height, 5);
    let frames = samples
        .iter()
        .map(|samples| Frame {
            width,
            height,
            layout: PixelLayout::Rgb8,
            samples,
        })
        .collect::<Vec<_>>();
    let encoded = encode(&frames).unwrap();
    assert_eq!(encoded.frame_count, frames.len());
    assert_eq!(encoded.descriptor.codec, "vp9");
    assert!(encoded.descriptor.lossless);
    let descriptor_json = serde_json::to_vec(&encoded.descriptor).unwrap();
    let restored_descriptor: CodecDescriptor = serde_json::from_slice(&descriptor_json).unwrap();
    assert_eq!(restored_descriptor, encoded.descriptor);

    let parsed = encoded
        .color_packets
        .iter()
        .map(|packet| inspect_packet(&packet.data).unwrap())
        .collect::<Vec<_>>();
    assert!(parsed.first().unwrap().keyframe);
    assert!(
        parsed.iter().skip(1).any(|packet| !packet.keyframe),
        "libvpx decoder metadata did not report an inter frame"
    );
    assert_eq!(
        encoded
            .color_packets
            .iter()
            .map(|packet| packet.keyframe)
            .collect::<Vec<_>>(),
        parsed
            .iter()
            .map(|packet| packet.keyframe)
            .collect::<Vec<_>>()
    );
    let decoded = decode(&encoded).unwrap();
    assert_eq!(decoded.len(), samples.len());
    for (decoded, original) in decoded.iter().zip(&samples) {
        assert_eq!(decoded.width, width);
        assert_eq!(decoded.height, height);
        assert_eq!(decoded.layout, PixelLayout::Rgb8);
        assert_eq!(&decoded.samples, original);
    }

    let inter_index = parsed
        .iter()
        .position(|packet| !packet.keyframe)
        .expect("encoded bitstream had no non-key packet");
    let mut isolated_inter_packet = encoded.clone();
    isolated_inter_packet.frame_count = 1;
    isolated_inter_packet.color_packets = vec![encoded.color_packets[inter_index].clone()];
    assert!(
        decode(&isolated_inter_packet).is_err(),
        "non-key packet decoded without the preceding reference-frame state"
    );
}

#[test]
fn rgba_alpha_and_hidden_rgb_round_trip_exactly() {
    let (width, height) = (15, 11);
    let samples = (0..4)
        .map(|frame| {
            let mut samples = Vec::with_capacity(width as usize * height as usize * 4);
            for y in 0..height {
                for x in 0..width {
                    let index = (y * width + x) as u8;
                    let rgba = match (x + y + frame as u32) % 7 {
                        0 => [255, 0, 0, 0],
                        1 => [0, 255, 0, 1],
                        2 => [0, 0, 255, 127],
                        3 => [254, 253, 252, 254],
                        _ => [index, 255 - index, index.wrapping_mul(17), 255],
                    };
                    samples.extend_from_slice(&rgba);
                }
            }
            samples
        })
        .collect::<Vec<_>>();
    let frames = samples
        .iter()
        .map(|samples| Frame {
            width,
            height,
            layout: PixelLayout::Rgba8,
            samples,
        })
        .collect::<Vec<_>>();
    let encoded = encode(&frames).unwrap();
    assert!(encoded.alpha_packets.is_some());
    assert_eq!(
        encoded.descriptor.alpha_layout.as_deref(),
        Some("alpha_in_i444_plane0_v1")
    );
    assert!(
        encoded
            .alpha_packets
            .as_ref()
            .unwrap()
            .iter()
            .skip(1)
            .map(|packet| inspect_packet(&packet.data).unwrap())
            .any(|packet| !packet.keyframe)
    );
    let decoded = decode(&encoded).unwrap();
    for (decoded, original) in decoded.iter().zip(&samples) {
        assert_eq!(decoded.layout, PixelLayout::Rgba8);
        assert_eq!(&decoded.samples, original);
    }
}

#[test]
fn every_byte_value_primary_one_pixel_lines_and_odd_dimensions_round_trip() {
    let (width, height) = (257, 5);
    let samples = (0..2)
        .map(|frame| {
            let mut samples = Vec::with_capacity(width as usize * height as usize * 3);
            for y in 0..height {
                for x in 0..width {
                    let value = (x % 256) as u8;
                    let rgb = match y {
                        0 => [value, value, value],
                        1 => [value, 0, 0],
                        2 => [0, value, 0],
                        3 => [0, 0, value],
                        _ if x == 128 + frame => [255, 1, 254],
                        _ => [127, 128, 129],
                    };
                    samples.extend_from_slice(&rgb);
                }
            }
            samples
        })
        .collect::<Vec<_>>();
    let frames = samples
        .iter()
        .map(|samples| Frame {
            width,
            height,
            layout: PixelLayout::Rgb8,
            samples,
        })
        .collect::<Vec<_>>();

    let encoded = encode(&frames).unwrap();
    let decoded = decode(&encoded).unwrap();
    assert_eq!(decoded.len(), samples.len());
    for (decoded, original) in decoded.iter().zip(&samples) {
        assert_eq!(&decoded.samples, original);
    }
}

#[test]
fn invalid_sequences_are_rejected_before_ffi() {
    let samples = vec![0; 3 * 3 * 3];
    let one = [Frame {
        width: 3,
        height: 3,
        layout: PixelLayout::Rgb8,
        samples: &samples,
    }];
    assert!(encode(&one).is_err());
    let wrong = vec![0; samples.len() - 1];
    let frames = [
        one[0],
        Frame {
            samples: &wrong,
            ..one[0]
        },
    ];
    assert!(encode(&frames).is_err());
}

#[test]
fn unknown_descriptor_and_truncated_packet_are_rejected() {
    let samples = rgb_frames(9, 7, 2);
    let frames = samples
        .iter()
        .map(|samples| Frame {
            width: 9,
            height: 7,
            layout: PixelLayout::Rgb8,
            samples,
        })
        .collect::<Vec<_>>();
    let encoded = encode(&frames).unwrap();

    let mut unknown_descriptor = encoded.clone();
    unknown_descriptor.descriptor.version += 1;
    assert!(decode(&unknown_descriptor).is_err());

    let mut truncated = encoded;
    truncated.color_packets[0].data.truncate(1);
    assert!(decode(&truncated).is_err());
}
