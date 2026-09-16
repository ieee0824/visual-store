mod common;

use common::{Harness, copy_tree, decode, from_scan};
use rusqlite::Connection;
use std::{fs, process::Stdio, thread, time::Duration};
use visual_store::Store;

fn frame(index: u8) -> Vec<u8> {
    let (width, height) = (256u32, 128u32);
    let mut scan = Vec::new();
    for y in 0..height {
        scan.push(0);
        for x in 0..width {
            let moving = (u32::from(index) * 3..u32::from(index) * 3 + 12).contains(&x)
                && (20..32).contains(&y);
            let value = if moving {
                240
            } else {
                ((x / 16 + y / 16) % 2 * 24 + 32) as u8
            };
            scan.extend_from_slice(&[value, value.saturating_add(5), value.saturating_add(11)]);
        }
    }
    from_scan(width, height, 2, 8, 0, &scan)
}

fn packed_store(run: &str, keep_source: bool) -> (Harness, Vec<Vec<u8>>) {
    let h = Harness::new();
    h.init();
    h.call(&["migrate", "--to", "2"]);
    let mut originals = Vec::new();
    for index in 0..8 {
        let bytes = frame(index);
        let path = h.temp.path().join(format!("prune-{index}.png"));
        fs::write(&path, &bytes).unwrap();
        let mut args = vec!["put", "--file", path.to_str().unwrap(), "--run", run];
        if keep_source {
            args.push("--keep-source");
        }
        h.call(&args);
        originals.push(bytes);
    }
    assert_eq!(
        h.call(&["pack", "--run", run, "--segment-frames", "8"])["segments_packed"],
        1
    );
    (h, originals)
}

#[test]
fn dry_run_apply_capacity_and_restart_retrieval_are_exact() {
    let (h, originals) = packed_store("prune", true);
    let db = Connection::open(h.root.join("index.sqlite3")).unwrap();
    let retired_bytes: u64 = db.query_row(
        "SELECT SUM(byte_length) FROM blobs WHERE sha256 IN (SELECT png_blob_sha256 FROM retired_representations WHERE prune_state='retained')",
        [], |row| row.get(0),
    ).unwrap();
    drop(db);

    let dry = h.validated_call(&["prune", "--dry-run"], 16 * 1024);
    assert_eq!(dry["retired_candidate_bytes"], retired_bytes);
    assert_eq!(dry["reclaimable_bytes"], retired_bytes);
    assert_eq!(dry["reclaimed_bytes"], 0);
    let applied = h.call(&["prune", "--apply"]);
    assert_eq!(applied["reclaimed_bytes"], retired_bytes, "{applied}");
    assert_eq!(
        applied["physical_object_bytes_before"].as_u64().unwrap()
            - applied["physical_object_bytes_after"].as_u64().unwrap(),
        applied["reclaimed_bytes"].as_u64().unwrap()
    );
    assert_eq!(h.call(&["prune", "--apply"])["reclaimed_bytes"], 0);
    assert_eq!(h.call(&["verify"])["valid"], true);
    for index in [0usize, 4, 7] {
        let output = h.call(&["get-frame", "--run", "prune", "--frame", &index.to_string()]);
        assert_eq!(
            decode(&fs::read(output["path"].as_str().unwrap()).unwrap()),
            decode(&originals[index])
        );
    }
    let source = h.call(&[
        "get-frame",
        "--run",
        "prune",
        "--frame",
        "3",
        "--variant",
        "source",
    ]);
    assert_eq!(
        fs::read(source["path"].as_str().unwrap()).unwrap(),
        originals[3]
    );
}

#[test]
fn corrupt_replacement_refuses_to_delete_recoverable_pngs() {
    let (h, _) = packed_store("corrupt-prune", false);
    let db = Connection::open(h.root.join("index.sqlite3")).unwrap();
    let video: String = db
        .query_row(
            "SELECT relative_path FROM blobs WHERE object_kind='vp9_bitstream' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let pngs = db.prepare("SELECT relative_path FROM blobs WHERE sha256 IN (SELECT png_blob_sha256 FROM retired_representations)").unwrap()
        .query_map([], |row| row.get::<_, String>(0)).unwrap()
        .collect::<Result<Vec<_>, _>>().unwrap();
    drop(db);
    let video_path = h.root.join(video);
    let mut bytes = fs::read(&video_path).unwrap();
    bytes[0] ^= 1;
    fs::write(video_path, bytes).unwrap();
    h.error(&["prune", "--apply"], "E_INTEGRITY");
    assert!(pngs.iter().all(|path| h.root.join(path).is_file()));
}

#[test]
fn shared_retired_pngs_are_counted_and_removed_once() {
    let h = Harness::new();
    h.init();
    h.call(&["migrate", "--to", "2"]);
    for run in ["shared-a", "shared-b"] {
        for index in 0..8 {
            let path = h.temp.path().join(format!("shared-{index}.png"));
            if !path.exists() {
                fs::write(&path, frame(index)).unwrap();
            }
            h.call(&["put", "--file", path.to_str().unwrap(), "--run", run]);
        }
        assert_eq!(
            h.call(&["pack", "--run", run, "--segment-frames", "8"])["segments_packed"],
            1
        );
    }
    let db = Connection::open(h.root.join("index.sqlite3")).unwrap();
    let (rows, distinct): (u64, u64) = db.query_row(
        "SELECT COUNT(*),COUNT(DISTINCT png_blob_sha256) FROM retired_representations WHERE prune_state='retained'",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert_eq!((rows, distinct), (16, 8));
    drop(db);
    let report = h.call(&["prune", "--apply"]);
    assert_eq!(report["candidate_objects"], 8);
    assert_eq!(report["pruned_objects"], 8);
    for run in ["shared-a", "shared-b"] {
        h.call(&["get-frame", "--run", run, "--frame", "7"]);
    }
}

#[test]
fn prune_waits_for_readers_instead_of_upgrading_a_shared_lock() {
    let (h, _) = packed_store("lock", false);
    let reader = Store::open(&h.root, false).unwrap();
    let mut child = h
        .command()
        .args(["prune", "--dry-run"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(250));
    assert!(child.try_wait().unwrap().is_none());
    drop(reader);
    assert!(child.wait().unwrap().success());
}

#[cfg(feature = "fault-injection")]
#[test]
fn every_tombstone_and_delete_boundary_resumes_safely() {
    let (base, _) = packed_store("fault-prune", false);
    for point in [
        "prune_before_tombstone",
        "prune_after_tombstone",
        "prune_before_delete",
        "prune_after_delete",
        "prune_before_finalize",
        "prune_after_finalize",
    ] {
        let h = Harness::new();
        copy_tree(&base.root, &h.root);
        let crashed = h
            .command()
            .args(["prune", "--apply"])
            .env("VSTORE_TEST_CRASH", point)
            .output()
            .unwrap();
        assert_eq!(crashed.status.code(), Some(91), "{point}");
        h.call(&["prune", "--apply"]);
        assert_eq!(h.call(&["verify"])["valid"], true, "{point}");
        h.call(&["get-frame", "--run", "fault-prune", "--frame", "7"]);
    }
}
