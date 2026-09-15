mod common;
use common::*;
use visual_store::image::{Limits, repack, validate};

#[test]
fn uncompressed_800x600_shrinks_and_preserves_samples() {
    let original = fixture(800, 600, false, false);
    let out = repack(&original, 6, &Limits::default()).unwrap();
    assert!(out.compression_applied);
    assert!(out.bytes.len() < original.len());
    assert_eq!(decode(&out.bytes), samples(800, 600, false, false));
    assert_eq!(validate(&original, &Limits::default()).unwrap(), out.meta);
}
#[test]
fn noise_never_grows_and_is_deterministic() {
    let original = fixture(64, 63, false, true);
    let a = repack(&original, 6, &Limits::default()).unwrap();
    let b = repack(&original, 6, &Limits::default()).unwrap();
    assert!(a.bytes.len() <= original.len());
    assert_eq!(a.bytes, b.bytes);
    assert_eq!(decode(&a.bytes), decode(&original));
    let c = repack(&a.bytes, 0, &Limits::default()).unwrap();
    assert_eq!(c.bytes, a.bytes);
    assert!(!c.compression_applied);
}
#[test]
fn rgb_rgba_hidden_colors_and_all_filters_with_split_idat() {
    for rgba in [false, true] {
        for filter in [
            png::Filter::NoFilter,
            png::Filter::Sub,
            png::Filter::Up,
            png::Filter::Avg,
            png::Filter::Paeth,
        ] {
            let samples = samples(31, 29, rgba, true);
            let mut bytes = Vec::new();
            {
                let mut enc = png::Encoder::new(&mut bytes, 31, 29);
                enc.set_color(if rgba {
                    png::ColorType::Rgba
                } else {
                    png::ColorType::Rgb
                });
                enc.set_filter(filter);
                let mut w = enc.write_header().unwrap();
                w.write_image_data(&samples).unwrap();
            }
            let mut parts = Vec::new();
            for (k, d) in chunks(&bytes) {
                if k == *b"IDAT" {
                    for p in d.chunks(7) {
                        parts.push((k, p.to_vec()));
                    }
                } else {
                    parts.push((k, d));
                }
            }
            let split = rebuild(&parts);
            let packed = repack(&split, 6, &Limits::default()).unwrap();
            assert_eq!(decode(&packed.bytes), samples);
            assert_eq!(validate(&split, &Limits::default()).unwrap(), packed.meta);
        }
    }
}
#[test]
fn metadata_preserved_and_unsafe_metadata_rejected() {
    let mut b = fixture(20, 20, true, false);
    for (k, d) in [
        (*b"gAMA", 45455u32.to_be_bytes().to_vec()),
        (*b"sRGB", vec![0]),
        (*b"pHYs", vec![0, 0, 0, 72, 0, 0, 0, 72, 0]),
        (*b"tEXt", b"Note\0untrusted instructions".to_vec()),
        (*b"vpAg", b"safe unknown".to_vec()),
    ] {
        b = add_chunk(&b, k, d);
    }
    let out = repack(&b, 6, &Limits::default()).unwrap();
    let others = |x: &[u8]| {
        chunks(x)
            .into_iter()
            .filter(|(k, _)| k != b"IDAT")
            .collect::<Vec<_>>()
    };
    assert_eq!(others(&b), others(&out.bytes));
    for k in [*b"vpAG", *b"ABCD"] {
        let bad = add_chunk(&b, k, vec![]);
        assert_eq!(
            validate(&bad, &Limits::default()).unwrap_err().code,
            "E_UNSUPPORTED_METADATA"
        );
    }
}
#[test]
fn rejects_unsupported_png_variants_and_apng() {
    for (c, d, i) in [(0, 8, 0), (3, 8, 0), (4, 8, 0), (2, 16, 0), (6, 8, 1)] {
        let b = from_scan(1, 1, c, d, i, &[0, 0, 0, 0, 0]);
        assert_eq!(
            validate(&b, &Limits::default()).unwrap_err().code,
            "E_UNSUPPORTED_IMAGE"
        );
    }
    let b = add_chunk(
        &fixture(1, 1, true, false),
        *b"acTL",
        vec![0, 0, 0, 1, 0, 0, 0, 0],
    );
    assert_eq!(
        validate(&b, &Limits::default()).unwrap_err().code,
        "E_UNSUPPORTED_IMAGE"
    );
}
#[test]
fn malformed_crc_truncation_bombs_and_chunk_order_fail() {
    let b = fixture(8, 8, true, false);
    let mut crc = b.clone();
    crc[29] ^= 1;
    let mut trailing = b.clone();
    trailing.push(0);
    let mut parts = chunks(&b);
    parts.insert(2, (*b"tEXt", b"n\0text".to_vec()));
    parts.insert(3, (*b"IDAT", vec![]));
    for invalid in [
        crc,
        b[..b.len() - 4].to_vec(),
        trailing,
        rebuild(&parts),
        from_scan(1, 1, 6, 8, 0, &vec![0; 65536]),
        from_scan(1, 1, 6, 8, 0, &[5, 0, 0, 0, 0]),
    ] {
        assert_eq!(
            validate(&invalid, &Limits::default()).unwrap_err().code,
            "E_INVALID_IMAGE"
        );
    }
    for limits in [
        Limits {
            source_bytes: 10,
            ..Limits::default()
        },
        Limits {
            max_edge: 4,
            ..Limits::default()
        },
        Limits {
            pixels: 63,
            ..Limits::default()
        },
        Limits {
            inflated_bytes: 5,
            ..Limits::default()
        },
        Limits {
            memory_bytes: 1024,
            ..Limits::default()
        },
    ] {
        assert_eq!(validate(&b, &limits).unwrap_err().code, "E_LIMIT_EXCEEDED");
    }
}
#[test]
fn malformed_inputs_do_not_panic() {
    let valid = fixture(4, 4, true, false);
    for n in 0..valid.len() {
        assert!(validate(&valid[..n], &Limits::default()).is_err());
    }
    for i in 0..valid.len() {
        let mut b = valid.clone();
        b[i] ^= 0xff;
        let _ = validate(&b, &Limits::default());
    }
}
