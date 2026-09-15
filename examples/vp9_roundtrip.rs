use visual_store::codec::vp9::{
    Frame, PixelLayout, decode, encode, inspect_packet, libvpx_build_config,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (width, height) = (17, 13);
    let samples = (0..3)
        .map(|frame| {
            let mut samples = vec![0; width as usize * height as usize * 3];
            for y in 3..8 {
                for x in 2 + frame..6 + frame {
                    let offset = (y as usize * width as usize + x as usize) * 3;
                    samples[offset..offset + 3].copy_from_slice(&[255, 31, 127]);
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
    let encoded = encode(&frames)?;
    let decoded = decode(&encoded)?;
    if decoded
        .iter()
        .zip(&samples)
        .any(|(decoded, original)| decoded.samples != *original)
    {
        return Err("VP9 lossless round-trip mismatch".into());
    }
    let inter_frames = encoded
        .color_packets
        .iter()
        .map(|packet| inspect_packet(&packet.data))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|packet| !packet.keyframe)
        .count();
    if inter_frames == 0 {
        return Err("VP9 sequence contained no inter frames".into());
    }
    println!(
        "{}",
        serde_json::json!({
            "codec": "vp9",
            "frames": decoded.len(),
            "inter_frames": inter_frames,
            "libvpx": encoded.descriptor.libvpx_version,
            "libvpx_build_config": libvpx_build_config(),
        })
    );
    Ok(())
}
