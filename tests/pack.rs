mod common;

use common::{Harness, copy_tree, decode, from_scan};
use rusqlite::Connection;
use std::fs;

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
    h.call(&["migrate", "--to", "2"]);
    put_sequence(&h, "demo", 8, false);
    let dry = h.call(&[
        "pack",
        "--run",
        "demo",
        "--dry-run",
        "--segment-frames",
        "8",
    ]);
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
    h.call(&["info", reference]);
    h.call(&["get", reference]);
    let middle = h.call(&[
        "get-frame",
        "--run",
        "demo",
        "--stream",
        "screen",
        "--frame",
        "4",
    ]);
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
    h.call(&["migrate", "--to", "2"]);
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
    base.call(&["migrate", "--to", "2"]);
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
    h.call(&["migrate", "--to", "2"]);
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
fn segment_frame_boundaries_leave_a_single_tail_as_png() {
    let h = Harness::new();
    h.init();
    h.call(&["migrate", "--to", "2"]);
    put_sequence(&h, "boundary", 9, false);
    let report = h.call(&["pack", "--run", "boundary", "--segment-frames", "4"]);
    assert_eq!(report["segments_packed"], 2, "{report}");
    assert_eq!(report["images_packed"], 8);
    assert_eq!(report["images_skipped"], 1);
}

#[cfg(feature = "fault-injection")]
#[test]
fn crashes_at_every_pack_publication_boundary_are_retryable() {
    let source = Harness::new();
    source.init();
    source.call(&["migrate", "--to", "2"]);
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
