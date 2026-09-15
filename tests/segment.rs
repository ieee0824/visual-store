use visual_store::{
    codec::vp9::{self, Frame, PixelLayout},
    segment::{SegmentLimits, StoredSegment},
};

fn limits() -> SegmentLimits {
    SegmentLimits {
        max_bytes: 1024 * 1024,
        max_packets: 256,
        max_descriptor_bytes: 16 * 1024,
    }
}

#[test]
fn immutable_packet_container_round_trips_and_rejects_corruption() {
    let a = vec![20u8; 8 * 8 * 3];
    let mut b = a.clone();
    b[9] = 21;
    let frames = [
        Frame {
            width: 8,
            height: 8,
            layout: PixelLayout::Rgb8,
            samples: &a,
        },
        Frame {
            width: 8,
            height: 8,
            layout: PixelLayout::Rgb8,
            samples: &b,
        },
    ];
    let encoded = vp9::encode(&frames).unwrap();
    let stored = StoredSegment::from_sequence(&encoded, &limits()).unwrap();
    assert_eq!(
        vp9::decode(&stored.to_sequence(&limits()).unwrap()).unwrap()[1].samples,
        b
    );

    let mut corrupt = stored.clone();
    corrupt.color[0] ^= 1;
    assert_eq!(
        corrupt.to_sequence(&limits()).unwrap_err().code,
        "E_INTEGRITY"
    );
    let mut trailing = stored;
    trailing.color.push(0);
    assert_eq!(
        trailing.to_sequence(&limits()).unwrap_err().code,
        "E_INTEGRITY"
    );
}

#[test]
fn packet_and_byte_limits_are_enforced_before_allocation() {
    let a = vec![0u8; 8 * 8 * 3];
    let frames = [
        Frame {
            width: 8,
            height: 8,
            layout: PixelLayout::Rgb8,
            samples: &a,
        },
        Frame {
            width: 8,
            height: 8,
            layout: PixelLayout::Rgb8,
            samples: &a,
        },
    ];
    let encoded = vp9::encode(&frames).unwrap();
    let tiny = SegmentLimits {
        max_bytes: 16,
        max_packets: 1,
        max_descriptor_bytes: 16,
    };
    assert_eq!(
        StoredSegment::from_sequence(&encoded, &tiny)
            .unwrap_err()
            .code,
        "E_LIMIT_EXCEEDED"
    );
}
