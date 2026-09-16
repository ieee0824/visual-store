use flate2::{Compression, write::ZlibEncoder};
use rusqlite::Connection;
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::{
    collections::HashMap,
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    time::Instant,
};
use tempfile::TempDir;
use visual_store::{
    PutOptions, Store,
    codec::vp9::{self, Frame, PixelLayout},
    image::{self, Limits, reconstruction},
    segment::{SegmentLimits, StoredSegment},
    sha256,
    store::{PackOptions, PruneOptions},
};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;
const FRAMES: usize = 32;

fn samples(index: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(WIDTH as usize * HEIGHT as usize * 3);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let panel = x < 144;
            let content_line = (184..572).contains(&x)
                && (44..316).contains(&y)
                && (y + index as u32 * 3) % 19 < 3;
            let cursor = (40 + index as u32 * 5..48 + index as u32 * 5).contains(&x)
                && (136..164).contains(&y);
            let value = if cursor {
                [245, 180, 32]
            } else if content_line {
                [72, 110, 168]
            } else if panel {
                [28, 32, 40]
            } else {
                [238, 241, 246]
            };
            output.extend_from_slice(&value);
        }
    }
    output
}

fn png(samples: &[u8]) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend_from_slice(&WIDTH.to_be_bytes());
    header.extend_from_slice(&HEIGHT.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    image::write_chunk(&mut bytes, b"IHDR", &header);
    let mut scanlines = Vec::with_capacity(samples.len() + HEIGHT as usize);
    for row in samples.chunks_exact(WIDTH as usize * 3) {
        scanlines.push(0);
        scanlines.extend_from_slice(row);
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::none());
    encoder.write_all(&scanlines).unwrap();
    image::write_chunk(&mut bytes, b"IDAT", &encoder.finish().unwrap());
    image::write_chunk(&mut bytes, b"IEND", &[]);
    bytes
}

fn sum_distinct(map: &HashMap<String, usize>) -> u64 {
    map.values().map(|value| *value as u64).sum()
}

fn temporal_bytes(encoded: &vp9::EncodedSequence, reconstruction_bytes: u64) -> (u64, u64, u64) {
    let stored = StoredSegment::from_sequence(
        encoded,
        &SegmentLimits {
            max_bytes: 128 * 1024 * 1024,
            max_packets: 1024,
            max_descriptor_bytes: 16 * 1024,
        },
    )
    .unwrap();
    let color = stored.color.len() as u64;
    let alpha = stored.alpha.as_ref().map_or(0, |bytes| bytes.len() as u64);
    let total = color + alpha + reconstruction_bytes + stored.descriptor_json.len() as u64;
    (color, alpha, total)
}

fn checkpoint_and_vacuum(root: &Path) -> u64 {
    let connection = Connection::open(root.join("index.sqlite3")).unwrap();
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")
        .unwrap();
    drop(connection);
    fs::metadata(root.join("index.sqlite3")).unwrap().len()
}

fn copy_tree(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::create_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn allocated_tree_bytes(path: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            total = total.saturating_add(allocated_tree_bytes(&entry.path())?);
        } else if metadata.is_file() {
            #[cfg(unix)]
            {
                total = total.saturating_add(metadata.blocks().saturating_mul(512));
            }
            #[cfg(not(unix))]
            {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}

fn percentile(mut values: Vec<u128>, percentile: usize) -> u128 {
    values.sort_unstable();
    values[(values.len() * percentile).div_ceil(100).saturating_sub(1)]
}

fn cpu_name() -> String {
    if let Ok(value) = env::var("VSTORE_BENCH_CPU") {
        return value;
    }
    #[cfg(target_os = "linux")]
    {
        fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|text| {
                text.lines()
                    .find_map(|line| line.strip_prefix("model name\t: ").map(str::to_owned))
            })
            .unwrap_or_else(|| "unknown".into())
    }
    #[cfg(target_os = "macos")]
    {
        for key in ["machdep.cpu.brand_string", "hw.model"] {
            if let Some(value) = std::process::Command::new("sysctl")
                .args(["-n", key])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
            {
                return value;
            }
        }
        "unknown".into()
    }
}

fn peak_rss_bytes() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage initializes the supplied rusage structure on success.
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if result != 0 {
        return 0;
    }
    // SAFETY: checked successful initialization above.
    let value = unsafe { usage.assume_init() }.ru_maxrss as u64;
    #[cfg(target_os = "linux")]
    {
        value.saturating_mul(1024)
    }
    #[cfg(target_os = "macos")]
    {
        value
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = env::args().nth(1).map(PathBuf::from);
    let temporary = TempDir::new()?;
    let work = env::var_os("VSTORE_BENCH_WORKDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| temporary.path().to_owned());
    fs::create_dir_all(&work)?;
    let root = work.join("store");
    Store::initialize(&root)?;
    let limits = Limits::default();
    let raw_samples = (0..FRAMES).map(samples).collect::<Vec<_>>();
    let input_pngs = raw_samples
        .iter()
        .map(|samples| png(samples))
        .collect::<Vec<_>>();
    let input_bytes = input_pngs.iter().map(Vec::len).sum::<usize>() as u64;
    let mut compressed = HashMap::new();
    let mut reconstructions = HashMap::new();
    for bytes in &input_pngs {
        let packed = image::repack(bytes, 6, &limits)?;
        compressed
            .entry(sha256(&packed.bytes))
            .or_insert(packed.bytes.len());
        let extracted = reconstruction::extract(&packed.bytes, &limits)?;
        let descriptor = extracted.metadata.to_bytes(&limits)?;
        reconstructions
            .entry(sha256(&descriptor))
            .or_insert(descriptor.len());
    }
    let png_distinct_bytes = sum_distinct(&compressed);
    let reconstruction_distinct_bytes = sum_distinct(&reconstructions);
    let codec_frames = raw_samples
        .iter()
        .map(|samples| Frame {
            width: WIDTH,
            height: HEIGHT,
            layout: PixelLayout::Rgb8,
            samples,
        })
        .collect::<Vec<_>>();
    let inter = vp9::encode(&codec_frames)?;
    let all_intra = vp9::encode_all_intra(&codec_frames)?;
    let (inter_color, inter_alpha, inter_without_index) =
        temporal_bytes(&inter, reconstruction_distinct_bytes);
    let (intra_color, intra_alpha, intra_without_index) =
        temporal_bytes(&all_intra, reconstruction_distinct_bytes);

    let mut store = Store::open(&root, true)?;
    for (index, bytes) in input_pngs.iter().enumerate() {
        let path = work.join(format!("frame-{index:02}.png"));
        fs::write(&path, bytes)?;
        store.put(
            &path,
            PutOptions {
                run: Some("benchmark".into()),
                stream: Some("screen".into()),
                ..PutOptions::default()
            },
        )?;
    }
    drop(store);
    let database_bytes_before_pack = checkpoint_and_vacuum(&root);
    let mut store = Store::open(&root, true)?;
    let pack = store.pack(PackOptions {
        run: "benchmark".into(),
        stream: Some("screen".into()),
        segment_frames: 32,
        dry_run: false,
        max_segment_bytes: 128 * 1024 * 1024,
        max_segment_packets: 1024,
        max_reconstruction_bytes: 64 * 1024 * 1024,
        max_pack_images: 10_000,
        max_encode_seconds: 300,
        limits: limits.clone(),
    })?;
    if let Some(error) = pack.error {
        return Err(error.into());
    }
    if pack.data["segments_packed"] != 1 {
        return Err(format!("fixture did not pack: {}", pack.data).into());
    }
    drop(store);
    let database_bytes_after_pack = checkpoint_and_vacuum(&root);
    let index_increment_bytes =
        database_bytes_after_pack.saturating_sub(database_bytes_before_pack);
    let temporal_total_bytes = inter_without_index + index_increment_bytes;
    let all_intra_total_bytes = intra_without_index + index_increment_bytes;

    let store = Store::open(&root, false)?;
    let mut retrieval_micros = Vec::new();
    let mut retrieval = Value::Null;
    for iteration in 0..23 {
        let started = Instant::now();
        let value = store.materialize(
            &store.resolve_frame("benchmark", "screen", 17)?,
            false,
            None,
        )?;
        let elapsed = started.elapsed().as_micros();
        if iteration >= 3 {
            retrieval_micros.push(elapsed);
        }
        retrieval = value;
    }
    let verify = store.verify(None)?;
    drop(store);
    if let Some(destination) = env::var_os("VSTORE_BENCH_PREPRUNE_COPY") {
        copy_tree(&root, &PathBuf::from(destination))?;
    }
    let allocated_object_bytes_before = allocated_tree_bytes(&root.join("objects"))?;
    let prune_dry = Store::prune(
        &root,
        PruneOptions {
            dry_run: true,
            apply: false,
            max_objects: 10_000,
        },
        limits.clone(),
    )?;
    if let Some(error) = prune_dry.error {
        return Err(error.into());
    }
    let prune = Store::prune(
        &root,
        PruneOptions {
            dry_run: false,
            apply: true,
            max_objects: 10_000,
        },
        limits,
    )?;
    if let Some(error) = prune.error {
        return Err(error.into());
    }
    let allocated_object_bytes_after = allocated_tree_bytes(&root.join("objects"))?;

    let result = json!({
        "schema_version":1,
        "fixture":{"name":"deterministic-changing-ui-v1","frames":FRAMES,"width":WIDTH,"height":HEIGHT,"pixel_layout":"rgb8"},
        "environment":{
            "os":env::consts::OS,"arch":env::consts::ARCH,"cpu":cpu_name(),
            "os_version":env::var("VSTORE_BENCH_OS_VERSION").unwrap_or_else(|_| "record-with-os-version".into()),
            "rust":env::var("VSTORE_BENCH_RUST_VERSION").unwrap_or_else(|_| "record-with-rustc-version".into()),
            "commit":env::var("VSTORE_BENCH_COMMIT").unwrap_or_else(|_| "working-tree".into()),
            "build":"release","codec":"vp9","libvpx":vp9::libvpx_version(),"threads":1,
            "segment_frames":32,"compression_level":6,"concurrency":1,
            "cache":"3 warmups then 20 warm-cache retrievals"
        },
        "capacity":{
            "a_input_png_bytes":input_bytes,
            "b_distinct_repacked_png_bytes":png_distinct_bytes,
            "c_vp9_color_container_bytes":inter_color,
            "c_vp9_alpha_container_bytes":inter_alpha,
            "c_reconstruction_distinct_bytes":reconstruction_distinct_bytes,
            "c_codec_descriptor_and_container_total_before_index":inter_without_index,
            "c_index_increment_bytes":index_increment_bytes,
            "c_complete_temporal_bytes":temporal_total_bytes,
            "d_all_intra_color_container_bytes":intra_color,
            "d_all_intra_alpha_container_bytes":intra_alpha,
            "d_complete_all_intra_bytes":all_intra_total_bytes,
            "c_smaller_than_b":temporal_total_bytes < png_distinct_bytes,
            "c_smaller_than_d":temporal_total_bytes < all_intra_total_bytes
        },
        "pack":{
            "segments_packed":pack.data["segments_packed"],
            "images_packed":pack.data["images_packed"],
            "encode_millis":pack.data["results"][0]["encode_millis"],
            "candidate_bytes":pack.data["results"][0]["candidate_bytes"],
            "png_distinct_bytes":pack.data["results"][0]["png_distinct_bytes"]
        },
        "retrieval":{
            "frame_no":17,"median_micros":percentile(retrieval_micros.clone(),50),
            "p95_micros":percentile(retrieval_micros,95),
            "segment_id":retrieval["segment_id"],"frame_index":retrieval["frame_index"],
            "decoded_from_frame":retrieval["decoded_from_frame"],
            "decoded_through_frame":retrieval["decoded_through_frame"]
        },
        "peak_rss_bytes":peak_rss_bytes(),
        "verify":{"valid":verify["valid"],"images_checked":verify["images_checked"],"segments_checked":verify["segments_checked"]},
        "prune":{
            "physical_object_bytes_before":prune.data["physical_object_bytes_before"],
            "physical_object_bytes_after":prune.data["physical_object_bytes_after"],
            "allocated_object_bytes_before":allocated_object_bytes_before,
            "allocated_object_bytes_after":allocated_object_bytes_after,
            "dry_run_reclaimable_bytes":prune_dry.data["reclaimable_bytes"],
            "reclaimed_bytes":prune.data["reclaimed_bytes"],
            "pruned_objects":prune.data["pruned_objects"]
        }
    });
    if !result["capacity"]["c_smaller_than_b"]
        .as_bool()
        .unwrap_or(false)
        || !result["capacity"]["c_smaller_than_d"]
            .as_bool()
            .unwrap_or(false)
    {
        return Err(format!("temporal fixture did not demonstrate benefit: {result}").into());
    }
    if let Some(output) = output {
        fs::write(output, serde_json::to_vec_pretty(&result)?)?;
    } else {
        println!("{}", serde_json::to_string_pretty(&result)?);
    }
    Ok(())
}
