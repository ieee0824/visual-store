mod common;

use common::{Harness, copy_tree, decode, from_scan};
use rusqlite::Connection;
use std::{fs, process::Stdio};

fn frame(width: u32, height: u32, index: u8) -> Vec<u8> {
    let mut scan = Vec::new();
    for y in 0..height {
        scan.push(0);
        for x in 0..width {
            let moving = (u32::from(index) * 3..u32::from(index) * 3 + 12).contains(&x)
                && (20..32).contains(&y);
            let v = if moving {
                240
            } else {
                ((x / 16 + y / 16) % 2 * 24 + 32) as u8
            };
            scan.extend_from_slice(&[v, v.saturating_add(5), v.saturating_add(11)]);
        }
    }
    from_scan(width, height, 2, 8, 0, &scan)
}

fn noisy_frame(width: u32, height: u32, rgba: bool, index: u8, independent: bool) -> Vec<u8> {
    let mut scan = Vec::new();
    let mut seed = if independent {
        0x9e37_79b9 ^ u32::from(index)
    } else {
        0x9e37_79b9
    };
    for y in 0..height {
        scan.push(0);
        for x in 0..width {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let changed = (10 + u32::from(index)..18 + u32::from(index)).contains(&x)
                && (10..18).contains(&y);
            scan.extend_from_slice(&[
                if changed { 250 } else { seed as u8 },
                if changed { 30 } else { (seed >> 8) as u8 },
                if changed { 90 } else { (seed >> 16) as u8 },
            ]);
            if rgba {
                scan.push(if changed { 180 } else { 255 });
            }
        }
    }
    from_scan(width, height, if rgba { 6 } else { 2 }, 8, 0, &scan)
}

fn photographic_frame(index: u8) -> Vec<u8> {
    let (width, height) = (160u32, 120u32);
    let mut scan = Vec::new();
    let mut seed = 0xa341_316c ^ u32::from(index).wrapping_mul(0x9e37_79b9);
    for y in 0..height {
        scan.push(0);
        for x in 0..width {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let grain = (seed as u8) & 31;
            scan.extend_from_slice(&[
                ((x * 190 / width) as u8).saturating_add(grain),
                ((y * 170 / height) as u8).saturating_add(grain / 2),
                (((x + y) * 120 / (width + height)) as u8).saturating_add(grain),
            ]);
        }
    }
    from_scan(width, height, 2, 8, 0, &scan)
}

fn put_sequence(h: &Harness, run: &str, count: usize, identical: bool) {
    for index in 0..count {
        let path = h.temp.path().join(format!("frame-{run}-{index}.png"));
        fs::write(
            &path,
            frame(256, 128, if identical { 0 } else { index as u8 }),
        )
        .unwrap();
        h.call(&[
            "put",
            "--file",
            path.to_str().unwrap(),
            "--run",
            run,
            "--stream",
            "screen",
        ]);
    }
}

#[test]
fn dry_run_then_pack_is_atomic_retryable_and_readable() {
    let h = Harness::new();
    h.init();
    h.call(&["migrate", "--to", "3"]);
    h.error(
        &["pack", "--run", "demo", "--codec", "av1", "--dry-run"],
        "E_CODEC_UNAVAILABLE",
    );
    put_sequence(&h, "demo", 8, false);
    let dry = h.validated_call(
        &[
            "pack",
            "--run",
            "demo",
            "--dry-run",
            "--segment-frames",
            "8",
        ],
        16 * 1024,
    );
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["segments_packed"], 0);

    let packed = h.call(&["pack", "--run", "demo", "--segment-frames", "8"]);
    assert_eq!(packed["segments_packed"], 1, "{packed}");
    assert_eq!(packed["images_packed"], 8);
    let db = Connection::open(h.root.join("index.sqlite3")).unwrap();
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM segments", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM representations WHERE representation_kind='vp9_segment'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        8
    );

    let retry = h.call(&["pack", "--run", "demo"]);
    assert_eq!(retry["segments_packed"], 0);
    let listed = h.call(&["list", "--run", "demo"]);
    let reference = listed["items"][0]["ref"].as_str().unwrap();
    h.validated_call(&["info", reference], 8 * 1024);
    h.validated_call(&["get", reference], 8 * 1024);
    let middle = h.validated_call(
        &[
            "get-frame",
            "--run",
            "demo",
            "--stream",
            "screen",
            "--frame",
            "4",
        ],
        8 * 1024,
    );
    assert_eq!(middle["backend"], "vp9_segment");
    assert_eq!(middle["decoded_from_frame"], 0);
    assert_eq!(middle["decoded_through_frame"], 4);
    assert_eq!(
        decode(&fs::read(middle["path"].as_str().unwrap()).unwrap()),
        decode(&frame(256, 128, 4))
    );
    h.error(
        &[
            "get-frame",
            "--run",
            "demo",
            "--stream",
            "screen",
            "--frame",
            "99",
        ],
        "E_NOT_FOUND",
    );
    let verified = h.call(&["verify"]);
    assert_eq!(verified["valid"], true, "{verified}");
    assert_eq!(verified["segments_checked"], 1);

    let copied = Harness::new();
    copy_tree(&h.root, &copied.root);
    let final_frame = copied.call(&[
        "get-frame",
        "--run",
        "demo",
        "--stream",
        "screen",
        "--frame",
        "7",
    ]);
    assert_eq!(final_frame["decoded_through_frame"], 7);
    assert_eq!(copied.call(&["verify"])["valid"], true);
}

#[test]
fn retained_source_is_byte_exact_after_temporal_pack() {
    let h = Harness::new();
    h.init();
    h.call(&["migrate", "--to", "3"]);
    let mut originals = Vec::new();
    for index in 0..4 {
        let bytes = frame(256, 128, index);
        let path = h.temp.path().join(format!("retained-{index}.png"));
        fs::write(&path, &bytes).unwrap();
        h.call(&[
            "put",
            "--file",
            path.to_str().unwrap(),
            "--run",
            "retained",
            "--keep-source",
        ]);
        originals.push(bytes);
    }
    let packed = h.call(&["pack", "--run", "retained", "--segment-frames", "4"]);
    assert_eq!(packed["segments_packed"], 1, "{packed}");
    let source = h.call(&[
        "get-frame",
        "--run",
        "retained",
        "--frame",
        "2",
        "--variant",
        "source",
    ]);
    assert_eq!(source["backend"], "source_png");
    assert_eq!(
        fs::read(source["path"].as_str().unwrap()).unwrap(),
        originals[2]
    );
}

#[test]
fn corrupt_temporal_objects_and_mappings_never_publish_output() {
    let base = Harness::new();
    base.init();
    base.call(&["migrate", "--to", "3"]);
    put_sequence(&base, "corrupt", 4, false);
    assert_eq!(
        base.call(&["pack", "--run", "corrupt", "--segment-frames", "4"])["segments_packed"],
        1
    );

    for (case, object_kind) in [
        ("video", Some("vp9_bitstream")),
        ("reconstruction", Some("png_reconstruction")),
        ("mapping", None),
    ] {
        let h = Harness::new();
        copy_tree(&base.root, &h.root);
        let db_path = h.root.join("index.sqlite3");
        let db = Connection::open(&db_path).unwrap();
        if let Some(kind) = object_kind {
            let relative: String = db
                .query_row(
                    "SELECT relative_path FROM blobs WHERE object_kind=?1 LIMIT 1",
                    [kind],
                    |row| row.get(0),
                )
                .unwrap();
            let path = h.root.join(relative);
            let mut bytes = fs::read(&path).unwrap();
            let last = bytes.len() - 1;
            bytes[last] ^= 1;
            fs::write(path, bytes).unwrap();
        } else {
            db.execute(
                "UPDATE frame_locations SET decode_start_index=1 WHERE frame_index=2",
                [],
            )
            .unwrap();
        }
        let image_id: String = db.query_row(
            "SELECT image_id FROM images WHERE run='corrupt' AND stream='screen' AND frame_no=2",
            [], |row| row.get(0),
        ).unwrap();
        drop(db);
        let info = h.call(&["info", &image_id]);
        assert_eq!(info["representation_kind"], "vp9_segment");
        let output = h.temp.path().join(format!("should-not-exist-{case}.png"));
        h.error(
            &[
                "get-frame",
                "--run",
                "corrupt",
                "--stream",
                "screen",
                "--frame",
                "2",
                "--output",
                output.to_str().unwrap(),
            ],
            "E_INTEGRITY",
        );
        assert!(!output.exists(), "{case}");
        h.error(&["verify"], "E_INTEGRITY");
    }
}

#[test]
fn identical_frames_are_retained_when_distinct_png_is_smaller() {
    let h = Harness::new();
    h.init();
    h.call(&["migrate", "--to", "3"]);
    put_sequence(&h, "same", 4, true);
    let report = h.call(&["pack", "--run", "same", "--segment-frames", "4"]);
    assert_eq!(report["segments_packed"], 0, "{report}");
    assert_eq!(report["segments_not_beneficial"], 1);
    let db = Connection::open(h.root.join("index.sqlite3")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM representations WHERE representation_kind='png'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        4
    );
}

#[test]
fn random_and_photographic_sequences_retain_png_when_accounting_is_worse() {
    let h = Harness::new();
    h.init();
    for run in ["random", "photographic"] {
        for index in 0..4u8 {
            let bytes = if run == "random" {
                noisy_frame(160, 120, false, index, true)
            } else {
                photographic_frame(index % 2)
            };
            let path = h.temp.path().join(format!("{run}-{index}.png"));
            fs::write(&path, bytes).unwrap();
            h.call(&["put", "--file", path.to_str().unwrap(), "--run", run]);
        }
        let report = h.call(&["pack", "--run", run, "--segment-frames", "4"]);
        assert_eq!(report["segments_packed"], 0, "{run}: {report}");
        assert_eq!(report["segments_not_beneficial"], 1, "{run}: {report}");
        assert!(
            report["results"][0]["candidate_bytes"].as_u64().unwrap()
                >= report["results"][0]["png_distinct_bytes"].as_u64().unwrap(),
            "{run}: {report}"
        );
    }
}

#[test]
fn segment_frame_boundaries_leave_a_single_tail_as_png() {
    let h = Harness::new();
    h.init();
    h.call(&["migrate", "--to", "3"]);
    put_sequence(&h, "boundary", 9, false);
    let report = h.call(&["pack", "--run", "boundary", "--segment-frames", "4"]);
    assert_eq!(report["segments_packed"], 2, "{report}");
    assert_eq!(report["images_packed"], 8);
    assert_eq!(report["images_skipped"], 1);
}

#[test]
fn exact_default_boundaries_retrieve_both_sides_and_final_frame() {
    for count in [31usize, 32, 33, 64, 65] {
        let h = Harness::new();
        h.init();
        put_sequence(&h, &format!("boundary-{count}"), count, false);
        let run = format!("boundary-{count}");
        let report = h.call(&["pack", "--run", &run]);
        let expected_segments = count / 32 + usize::from(count % 32 >= 2);
        assert_eq!(
            report["segments_packed"], expected_segments,
            "{count}: {report}"
        );
        for index in [0usize, 31.min(count - 1), 32.min(count - 1), count - 1] {
            let output = h.call(&[
                "get-frame",
                "--run",
                &run,
                "--stream",
                "screen",
                "--frame",
                &index.to_string(),
            ]);
            assert_eq!(
                decode(&fs::read(output["path"].as_str().unwrap()).unwrap()),
                decode(&frame(256, 128, index as u8)),
                "count={count} frame={index}"
            );
        }
    }
}

#[test]
fn dimensions_layout_and_streams_form_separate_groups() {
    let h = Harness::new();
    h.init();
    let specifications = [
        ("a", 128, 96, false),
        ("a", 96, 64, false),
        ("a", 96, 64, true),
        ("b", 128, 96, false),
    ];
    for (group, (stream, width, height, rgba)) in specifications.iter().enumerate() {
        for index in 0..2 {
            let path = h.temp.path().join(format!("compat-{group}-{index}.png"));
            fs::write(&path, noisy_frame(*width, *height, *rgba, index, false)).unwrap();
            h.call(&[
                "put",
                "--file",
                path.to_str().unwrap(),
                "--run",
                "compat",
                "--stream",
                stream,
            ]);
        }
    }
    let report = h.call(&["pack", "--run", "compat", "--segment-frames", "8"]);
    assert_eq!(report["groups_considered"], 4, "{report}");
    assert!(
        report["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|result| result["image_count"] == 2)
    );
    let db = Connection::open(h.root.join("index.sqlite3")).unwrap();
    let mixed: u64 = db.query_row(
        "SELECT COUNT(*) FROM segments s WHERE EXISTS(SELECT 1 FROM frame_locations f JOIN images i ON i.image_id=f.image_id WHERE f.segment_id=s.segment_id GROUP BY f.segment_id HAVING COUNT(DISTINCT i.stream)>1 OR COUNT(DISTINCT i.width)>1 OR COUNT(DISTINCT i.color_type)>1)",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(mixed, 0);
}

#[test]
fn pack_preserves_operation_retry_identity_and_frame_number() {
    let h = Harness::new();
    h.init();
    let mut first = None;
    for index in 0..4 {
        let path = h.temp.path().join(format!("operation-{index}.png"));
        fs::write(&path, frame(256, 128, index)).unwrap();
        let value = h.call(&[
            "put",
            "--file",
            path.to_str().unwrap(),
            "--run",
            "operation",
            "--operation-id",
            &format!("operation-{index}"),
        ]);
        if index == 0 {
            first = Some((path, value));
        }
    }
    h.call(&["pack", "--run", "operation", "--segment-frames", "4"]);
    let (path, original) = first.unwrap();
    let retry = h.call(&[
        "put",
        "--file",
        path.to_str().unwrap(),
        "--run",
        "operation",
        "--operation-id",
        "operation-0",
    ]);
    assert_eq!(retry["image_id"], original["image_id"]);
    assert_eq!(retry["frame_no"], original["frame_no"]);
    assert_eq!(retry["record_reused"], true);
}

#[test]
fn concurrent_put_pack_and_get_preserve_every_frame() {
    let h = Harness::new();
    h.init();
    put_sequence(&h, "concurrent", 8, false);
    let mut pack = h
        .command()
        .args([
            "pack",
            "--run",
            "concurrent",
            "--stream",
            "screen",
            "--segment-frames",
            "8",
        ])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let existing = h.call(&[
        "get-frame",
        "--run",
        "concurrent",
        "--stream",
        "screen",
        "--frame",
        "0",
    ]);
    assert_eq!(
        decode(&fs::read(existing["path"].as_str().unwrap()).unwrap()),
        decode(&frame(256, 128, 0))
    );
    let ninth = h.temp.path().join("concurrent-8.png");
    fs::write(&ninth, frame(256, 128, 8)).unwrap();
    h.call(&[
        "put",
        "--file",
        ninth.to_str().unwrap(),
        "--run",
        "concurrent",
        "--stream",
        "screen",
    ]);
    assert!(pack.wait().unwrap().success());
    let db = Connection::open(h.root.join("index.sqlite3")).unwrap();
    let frames = db.prepare("SELECT frame_no FROM images WHERE run='concurrent' AND stream='screen' ORDER BY frame_no").unwrap()
        .query_map([], |row| row.get::<_, u64>(0)).unwrap()
        .collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(frames, (0..9).collect::<Vec<_>>());
    drop(db);
    assert_eq!(h.call(&["verify"])["valid"], true);
    for index in 0..9usize {
        let output = h.call(&[
            "get-frame",
            "--run",
            "concurrent",
            "--stream",
            "screen",
            "--frame",
            &index.to_string(),
        ]);
        assert_eq!(
            decode(&fs::read(output["path"].as_str().unwrap()).unwrap()),
            decode(&frame(256, 128, index as u8))
        );
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn pack_resource_failures_leave_png_active_and_retrievable() {
    let base = Harness::new();
    base.init();
    put_sequence(&base, "limits", 4, false);
    for point in [
        "pack_before_object_publish",
        "pack_after_object_publish",
        "pack_before_db_commit",
    ] {
        let h = Harness::new();
        copy_tree(&base.root, &h.root);
        h.error_with_env(
            &["pack", "--run", "limits", "--segment-frames", "4"],
            "VSTORE_TEST_DISK_FULL",
            point,
            "E_DISK_FULL",
        );
        assert_eq!(h.call(&["verify"])["valid"], true);
        h.call(&[
            "get-frame",
            "--run",
            "limits",
            "--stream",
            "screen",
            "--frame",
            "3",
        ]);
    }
    let h = Harness::new();
    copy_tree(&base.root, &h.root);
    let output = h
        .command()
        .args([
            "pack",
            "--run",
            "limits",
            "--segment-frames",
            "4",
            "--max-encode-seconds",
            "1",
        ])
        .env("VSTORE_TEST_CODEC_DELAY_MS", "1100")
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["code"], "E_LIMIT_EXCEEDED", "{value}");
    let db = Connection::open(h.root.join("index.sqlite3")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM representations WHERE representation_kind='png'",
            [],
            |row| row.get::<_, u64>(0)
        )
        .unwrap(),
        4
    );
}

#[cfg(feature = "fault-injection")]
#[test]
fn crashes_at_every_pack_publication_boundary_are_retryable() {
    let source = Harness::new();
    source.init();
    source.call(&["migrate", "--to", "3"]);
    put_sequence(&source, "crash", 4, false);
    for point in [
        "pack_before_object_publish",
        "pack_after_object_publish",
        "pack_before_db_commit",
        "pack_during_db_commit",
        "pack_after_db_commit",
    ] {
        let h = Harness::new();
        copy_tree(&source.root, &h.root);
        let crashed = h
            .command()
            .args(["pack", "--run", "crash", "--segment-frames", "4"])
            .env("VSTORE_TEST_CRASH", point)
            .output()
            .unwrap();
        assert_eq!(crashed.status.code(), Some(91), "{point}");
        assert_eq!(h.call(&["verify"])["valid"], true, "{point}");
        h.call(&["pack", "--run", "crash", "--segment-frames", "4"]);
        assert_eq!(h.call(&["verify"])["valid"], true, "{point}");
    }
}
