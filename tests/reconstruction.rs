mod common;

use common::{chunks, decode, fixture, rebuild, samples};
use std::io::Write;
use visual_store::{
    codec::vp9::{Frame, PixelLayout, decode as decode_vp9, encode as encode_vp9},
    image::{
        Limits, reconstruction::ReconstructionMetadata, reconstruction::extract,
        reconstruction::rebuild_png, validate,
    },
};

fn filtered_png(width: u32, height: u32, rgba: bool, filter: png::Filter) -> Vec<u8> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(if rgba {
            png::ColorType::Rgba
        } else {
            png::ColorType::Rgb
        });
        encoder.set_filter(filter);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&samples(width, height, rgba, true))
            .unwrap();
    }
    output
}

fn with_metadata_and_split_idat(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    for (kind, data) in chunks(bytes) {
        match &kind {
            b"IHDR" => {
                output.push((kind, data));
                output.extend([
                    (*b"gAMA", 45_455u32.to_be_bytes().to_vec()),
                    (*b"sRGB", vec![0]),
                    (*b"pHYs", vec![0, 0, 0, 72, 0, 0, 0, 72, 0]),
                    (*b"tEXt", b"Before\0preserved before IDAT".to_vec()),
                    (*b"vpAg", b"unknown safe-to-copy".to_vec()),
                ]);
            }
            b"IDAT" => {
                output.extend(data.chunks(7).map(|part| (kind, part.to_vec())));
            }
            b"IEND" => {
                output.push((*b"tEXt", b"After\0preserved after IDAT".to_vec()));
                output.push((kind, data));
            }
            _ => output.push((kind, data)),
        }
    }
    rebuild(&output)
}

fn non_idat(bytes: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
    chunks(bytes)
        .into_iter()
        .filter(|(kind, _)| kind != b"IDAT")
        .collect()
}

fn non_idat_layout(bytes: &[u8]) -> Vec<(bool, [u8; 4], Vec<u8>)> {
    let mut after_idat = false;
    chunks(bytes)
        .into_iter()
        .filter_map(|(kind, data)| {
            if kind == *b"IDAT" {
                after_idat = true;
                None
            } else {
                Some((after_idat, kind, data))
            }
        })
        .collect()
}

#[test]
fn every_filter_split_idat_and_chunk_placement_reconstruct_exact_hashes() {
    for rgba in [false, true] {
        for (filter, filter_byte) in [
            (png::Filter::NoFilter, 0),
            (png::Filter::Sub, 1),
            (png::Filter::Up, 2),
            (png::Filter::Avg, 3),
            (png::Filter::Paeth, 4),
        ] {
            let input = with_metadata_and_split_idat(&filtered_png(31, 29, rgba, filter));
            assert!(
                chunks(&input)
                    .iter()
                    .filter(|(kind, _)| kind == b"IDAT")
                    .count()
                    > 1
            );
            let unchanged = input.clone();
            let extracted = extract(&input, &Limits::default()).unwrap();
            assert!(
                extracted
                    .metadata
                    .filter_types()
                    .iter()
                    .all(|value| *value == filter_byte)
            );
            let descriptor = extracted.metadata.to_bytes(&Limits::default()).unwrap();
            let decoded =
                ReconstructionMetadata::from_bytes(&descriptor, &Limits::default()).unwrap();
            assert_eq!(decoded.to_bytes(&Limits::default()).unwrap(), descriptor);
            let rebuilt = rebuild_png(
                &decoded,
                &extracted.samples,
                &extracted.verification,
                6,
                &Limits::default(),
            )
            .unwrap();
            let rebuilt_again = rebuild_png(
                &decoded,
                &extracted.samples,
                &extracted.verification,
                6,
                &Limits::default(),
            )
            .unwrap();
            assert_eq!(input, unchanged, "the original input was modified");
            assert_eq!(rebuilt, rebuilt_again);
            assert_eq!(decode(&rebuilt), extracted.samples);
            assert_eq!(non_idat(&rebuilt), non_idat(&input));
            assert_eq!(non_idat_layout(&rebuilt), non_idat_layout(&input));
            assert_eq!(
                validate(&rebuilt, &Limits::default()).unwrap(),
                extracted.verification
            );
        }
    }
}

#[test]
fn every_currently_accepted_ancillary_chunk_reconstructs() {
    let mut compressed_profile =
        flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    compressed_profile
        .write_all(b"synthetic ICC profile")
        .unwrap();
    let compressed_profile = compressed_profile.finish().unwrap();
    let mut compressed_text =
        flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    compressed_text.write_all(b"compressed text").unwrap();
    let compressed_text = compressed_text.finish().unwrap();

    let before = [
        (*b"cHRM", vec![0; 32]),
        (*b"gAMA", 45_455u32.to_be_bytes().to_vec()),
        (
            *b"iCCP",
            [b"Profile\0\0".as_slice(), compressed_profile.as_slice()].concat(),
        ),
        (*b"sBIT", vec![8, 8, 8, 8]),
        (*b"sRGB", vec![0]),
        (*b"bKGD", vec![0; 6]),
        (*b"pHYs", vec![0, 0, 0, 1, 0, 0, 0, 1, 0]),
        (*b"cICP", vec![1, 13, 0, 1]),
        (*b"mDCV", vec![0; 24]),
        (*b"cLLI", vec![0; 8]),
    ];
    let after = [
        (*b"tIME", vec![0x07, 0xea, 9, 15, 12, 0, 0]),
        (*b"tEXt", b"Key\0plain text".to_vec()),
        (
            *b"zTXt",
            [b"Key\0\0".as_slice(), compressed_text.as_slice()].concat(),
        ),
        (*b"iTXt", b"Key\0\0\0\0\0international text".to_vec()),
        (*b"eXIf", b"MM\0*\0\0\0\x08\0\0".to_vec()),
        (*b"vpAg", b"safe private chunk".to_vec()),
    ];

    for (kind, data, before_idat) in before
        .into_iter()
        .map(|(kind, data)| (kind, data, true))
        .chain(after.into_iter().map(|(kind, data)| (kind, data, false)))
    {
        let parts = chunks(&fixture(9, 7, true, false));
        let idat = parts.iter().position(|(name, _)| name == b"IDAT").unwrap();
        let iend = parts.iter().position(|(name, _)| name == b"IEND").unwrap();
        let mut parts = parts;
        parts.insert(if before_idat { idat } else { iend }, (kind, data));
        let input = rebuild(&parts);
        let extracted = extract(&input, &Limits::default()).unwrap();
        let descriptor = extracted.metadata.to_bytes(&Limits::default()).unwrap();
        let metadata = ReconstructionMetadata::from_bytes(&descriptor, &Limits::default()).unwrap();
        let output = rebuild_png(
            &metadata,
            &extracted.samples,
            &extracted.verification,
            6,
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(non_idat(&output), non_idat(&input), "chunk {kind:?}");
        assert_eq!(
            non_idat_layout(&output),
            non_idat_layout(&input),
            "chunk {kind:?}"
        );
        assert_eq!(
            validate(&output, &Limits::default()).unwrap(),
            extracted.verification
        );
    }

    // tRNS is valid for RGB but forbidden for RGBA by the PNG format.
    let mut parts = chunks(&fixture(9, 7, false, false));
    parts.insert(1, (*b"tRNS", vec![0; 6]));
    let input = rebuild(&parts);
    let extracted = extract(&input, &Limits::default()).unwrap();
    let output = rebuild_png(
        &extracted.metadata,
        &extracted.samples,
        &extracted.verification,
        6,
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(non_idat(&output), non_idat(&input));
}

#[test]
fn vp9_samples_rebuild_rgb_rgba_alpha_hidden_rgb_and_boundary_detail() {
    for rgba in [false, true] {
        let (width, height) = (257, 5);
        let frames = (0..2)
            .map(|frame| {
                let channels = if rgba { 4 } else { 3 };
                let mut packed = Vec::with_capacity(width as usize * height as usize * channels);
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
                        packed.extend_from_slice(&rgb);
                        if rgba {
                            packed.push(match x % 4 {
                                0 => 0,
                                1 => 1,
                                2 => 127,
                                _ => 255,
                            });
                        }
                    }
                }
                packed
            })
            .collect::<Vec<_>>();
        let pngs = frames
            .iter()
            .map(|samples| {
                let mut output = Vec::new();
                {
                    let mut encoder = png::Encoder::new(&mut output, width, height);
                    encoder.set_color(if rgba {
                        png::ColorType::Rgba
                    } else {
                        png::ColorType::Rgb
                    });
                    encoder.set_filter(png::Filter::Paeth);
                    let mut writer = encoder.write_header().unwrap();
                    writer.write_image_data(samples).unwrap();
                }
                output
            })
            .collect::<Vec<_>>();
        let extracted = pngs
            .iter()
            .map(|png| extract(png, &Limits::default()).unwrap())
            .collect::<Vec<_>>();
        let layout = if rgba {
            PixelLayout::Rgba8
        } else {
            PixelLayout::Rgb8
        };
        let codec_frames = extracted
            .iter()
            .map(|frame| Frame {
                width,
                height,
                layout,
                samples: &frame.samples,
            })
            .collect::<Vec<_>>();
        let encoded = encode_vp9(&codec_frames).unwrap();
        let payload = encoded.payload_byte_lengths().unwrap();
        assert_eq!(payload.total, payload.color + payload.alpha);
        assert_eq!(payload.alpha > 0, rgba);
        let decoded = decode_vp9(&encoded).unwrap();
        for ((decoded, extracted), original) in decoded.iter().zip(&extracted).zip(&pngs) {
            let rebuilt = rebuild_png(
                &extracted.metadata,
                &decoded.samples,
                &extracted.verification,
                6,
                &Limits::default(),
            )
            .unwrap();
            assert_eq!(decoded.samples, extracted.samples);
            assert_eq!(decode(&rebuilt), decode(original));
            assert_eq!(
                validate(&rebuilt, &Limits::default()).unwrap(),
                extracted.verification
            );
        }
    }
}

#[test]
fn canonical_metadata_is_content_shareable_across_different_pixels() {
    let flat = extract(&fixture(41, 17, true, false), &Limits::default()).unwrap();
    let noisy = extract(&fixture(41, 17, true, true), &Limits::default()).unwrap();
    assert_ne!(
        flat.verification.pixel_sha256,
        noisy.verification.pixel_sha256
    );
    assert_eq!(flat.metadata, noisy.metadata);
    assert_eq!(
        flat.metadata.to_bytes(&Limits::default()).unwrap(),
        noisy.metadata.to_bytes(&Limits::default()).unwrap()
    );
}

#[test]
fn corrupt_oversized_or_mismatched_metadata_never_returns_output() {
    let extracted = extract(&fixture(13, 11, true, true), &Limits::default()).unwrap();
    let descriptor = extracted.metadata.to_bytes(&Limits::default()).unwrap();

    for end in 0..descriptor.len() {
        assert!(
            ReconstructionMetadata::from_bytes(&descriptor[..end], &Limits::default()).is_err()
        );
    }
    let mut bad_magic = descriptor.clone();
    bad_magic[0] ^= 0xff;
    assert_eq!(
        ReconstructionMetadata::from_bytes(&bad_magic, &Limits::default())
            .err()
            .unwrap()
            .code,
        "E_INTEGRITY"
    );
    let mut unknown_version = descriptor.clone();
    unknown_version[9] = 2;
    assert_eq!(
        ReconstructionMetadata::from_bytes(&unknown_version, &Limits::default())
            .err()
            .unwrap()
            .code,
        "E_SCHEMA_VERSION"
    );
    let mut huge_count = descriptor.clone();
    huge_count[10..14].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(
        ReconstructionMetadata::from_bytes(&huge_count, &Limits::default())
            .err()
            .unwrap()
            .code,
        "E_LIMIT_EXCEEDED"
    );
    let mut invalid_placement = descriptor.clone();
    invalid_placement[18] = 2;
    assert!(ReconstructionMetadata::from_bytes(&invalid_placement, &Limits::default()).is_err());
    let mut invalid_filter = descriptor.clone();
    *invalid_filter.last_mut().unwrap() = 5;
    assert!(ReconstructionMetadata::from_bytes(&invalid_filter, &Limits::default()).is_err());
    let mut oversized_ihdr = descriptor.clone();
    oversized_ihdr[27..31].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(
        ReconstructionMetadata::from_bytes(&oversized_ihdr, &Limits::default())
            .err()
            .unwrap()
            .code,
        "E_LIMIT_EXCEEDED"
    );
    let mut trailing = descriptor.clone();
    trailing.push(0);
    assert!(ReconstructionMetadata::from_bytes(&trailing, &Limits::default()).is_err());

    let tight = Limits {
        memory_bytes: descriptor.len() * 4 - 1,
        ..Limits::default()
    };
    assert_eq!(
        ReconstructionMetadata::from_bytes(&descriptor, &tight)
            .err()
            .unwrap()
            .code,
        "E_LIMIT_EXCEEDED"
    );
    for limits in [
        Limits {
            memory_bytes: 16 * 1024 * 1024,
            ..Limits::default()
        },
        Limits {
            source_bytes: 32,
            ..Limits::default()
        },
    ] {
        assert_eq!(
            rebuild_png(
                &extracted.metadata,
                &extracted.samples,
                &extracted.verification,
                6,
                &limits,
            )
            .err()
            .unwrap()
            .code,
            "E_LIMIT_EXCEEDED"
        );
    }

    let mut changed_ihdr = descriptor.clone();
    changed_ihdr[30] = changed_ihdr[30].wrapping_add(1);
    let changed_ihdr =
        ReconstructionMetadata::from_bytes(&changed_ihdr, &Limits::default()).unwrap();
    assert_eq!(
        rebuild_png(
            &changed_ihdr,
            &extracted.samples,
            &extracted.verification,
            6,
            &Limits::default(),
        )
        .err()
        .unwrap()
        .code,
        "E_INTEGRITY"
    );

    let mut wrong_samples = extracted.samples.clone();
    wrong_samples[0] ^= 1;
    assert_eq!(
        rebuild_png(
            &extracted.metadata,
            &wrong_samples,
            &extracted.verification,
            6,
            &Limits::default(),
        )
        .err()
        .unwrap()
        .code,
        "E_INTEGRITY"
    );
    let mut wrong_hash = extracted.verification.clone();
    wrong_hash.scanline_sha256 = "0".repeat(64);
    assert_eq!(
        rebuild_png(
            &extracted.metadata,
            &extracted.samples,
            &wrong_hash,
            6,
            &Limits::default(),
        )
        .err()
        .unwrap()
        .code,
        "E_INTEGRITY"
    );
}
