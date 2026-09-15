mod common;

use common::{Harness, copy_tree, from_scan};
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
    let verified = h.call(&["verify"]);
    assert_eq!(verified["valid"], true, "{verified}");
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
