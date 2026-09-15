mod common;
use common::*;
use rusqlite::Connection;
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::Path,
};
use visual_store::{PutOptions, Store};

#[test]
fn initialization_preserves_id_and_unrelated_files() {
    let h = Harness::new();
    h.error(&["list"], "E_STORE_NOT_INITIALIZED");
    let a = h.call(&["init"]);
    let b = h.call(&["init"]);
    assert_eq!(a["schema_version"], 2);
    assert_eq!(a["store_id"], b["store_id"]);
    assert_eq!(b["already_initialized"], true);
    assert_eq!(
        fs::metadata(&h.root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for name in ["store.json", "index.sqlite3"] {
        assert_eq!(
            fs::metadata(h.root.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let other = Harness::new();
    fs::create_dir(&other.root).unwrap();
    fs::write(other.root.join("unrelated"), b"keep").unwrap();
    other.error(&["init"], "E_INTEGRITY");
    assert_eq!(fs::read(other.root.join("unrelated")).unwrap(), b"keep");
}
#[test]
fn put_info_get_keep_source_and_input_preservation() {
    let h = Harness::new();
    h.init();
    let before = fs::metadata(&h.input).unwrap();
    let original = fs::read(&h.input).unwrap();
    let p = h.call(&[
        "put",
        "--file",
        h.input.to_str().unwrap(),
        "--keep-source",
        "--run",
        "r",
        "--note",
        "unviewed",
        "--tag",
        "b",
        "--tag",
        "a",
        "--captured-at",
        "2026-09-15T12:00:00+09:00",
    ]);
    assert_eq!(p["source_retained"], true);
    assert!(p["stored_bytes"].as_u64().unwrap() < p["source_bytes"].as_u64().unwrap());
    let reference = p["ref"].as_str().unwrap();
    let info = h.call(&["info", reference]);
    assert_eq!(info["tags"], serde_json::json!(["a", "b"]));
    assert_eq!(info["captured_at"], "2026-09-15T03:00:00.000000000Z");
    let get = h.call(&["get", reference]);
    assert_eq!(get["displayed"], false);
    assert_eq!(
        decode(&fs::read(get["path"].as_str().unwrap()).unwrap()),
        decode(&original)
    );
    let src = h.call(&["get", reference, "--variant", "source"]);
    assert_eq!(fs::read(src["path"].as_str().unwrap()).unwrap(), original);
    fs::write(get["path"].as_str().unwrap(), b"changed exported copy").unwrap();
    assert_eq!(h.call(&["verify"])["valid"], true);
    assert_eq!(fs::read(&h.input).unwrap(), original);
    let after = fs::metadata(&h.input).unwrap();
    assert_eq!(
        (before.mtime(), before.mtime_nsec()),
        (after.mtime(), after.mtime_nsec())
    );
    let p = h.put();
    h.error(
        &["get", p["ref"].as_str().unwrap(), "--variant", "source"],
        "E_SOURCE_NOT_RETAINED",
    );
}
#[test]
fn deduplication_retains_events_and_operation_id_is_idempotent() {
    let h = Harness::new();
    h.init();
    let a = h.put();
    let b = h.put();
    assert_ne!(a["image_id"], b["image_id"]);
    assert_eq!(b["blob_reused"], true);
    let a = h.call(&[
        "put",
        "--file",
        h.input.to_str().unwrap(),
        "--operation-id",
        "once",
        "--tag",
        "b",
        "--tag",
        "a",
    ]);
    let b = h.call(&[
        "put",
        "--file",
        h.input.to_str().unwrap(),
        "--operation-id",
        "once",
        "--tag",
        "a",
        "--tag",
        "b",
    ]);
    assert_eq!(a["ref"], b["ref"]);
    assert_eq!(b["record_reused"], true);
    h.error(
        &[
            "put",
            "--file",
            h.input.to_str().unwrap(),
            "--operation-id",
            "once",
            "--note",
            "different",
        ],
        "E_CONFLICT",
    );
    let v = h.call(&["verify"]);
    assert_eq!(v["images_checked"], 3);
    assert_eq!(v["blobs_checked"], 1);
}

#[test]
fn identical_retained_source_and_stored_bytes_share_one_blob() {
    let h = Harness::new();
    h.init();
    let input = fs::read(&h.input).unwrap();
    let packed =
        visual_store::image::repack(&input, 6, &visual_store::image::Limits::default()).unwrap();
    fs::write(&h.input, packed.bytes).unwrap();
    let result = h.call(&["put", "--file", h.input.to_str().unwrap(), "--keep-source"]);
    assert_eq!(result["source_retained"], true);
    assert_eq!(result["compression_applied"], false);
    assert_eq!(h.call(&["verify"])["blobs_checked"], 1);
}
#[test]
fn pagination_is_stable_across_new_registrations_and_rejects_wrong_cursor() {
    let h = Harness::new();
    h.init();
    for _ in 0..5 {
        h.put();
    }
    let p = h.call(&["list", "--limit", "2"]);
    let cursor = p["next_cursor"].as_str().unwrap();
    h.put();
    let q = h.call(&["list", "--limit", "2", "--cursor", cursor]);
    let r = h.call(&[
        "list",
        "--limit",
        "2",
        "--cursor",
        q["next_cursor"].as_str().unwrap(),
    ]);
    let seq = p["items"]
        .as_array()
        .unwrap()
        .iter()
        .chain(q["items"].as_array().unwrap())
        .chain(r["items"].as_array().unwrap())
        .map(|x| x["seq"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(seq, vec![5, 4, 3, 2, 1]);
    assert!(r.get("next_cursor").is_none());
    h.error(
        &["list", "--run", "different", "--cursor", cursor],
        "E_INVALID_CURSOR",
    );
    h.error(&["list", "--cursor", "garbage"], "E_INVALID_CURSOR");
    h.error(&["list", "--limit", "0"], "E_INVALID_ARGUMENT");
    assert!(
        h.call(&["list", "--run", "absent"])["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let other = Harness::new();
    other.init();
    other.error(&["list", "--cursor", cursor], "E_INVALID_CURSOR");
    let a = h.put();
    other.error(&["info", a["ref"].as_str().unwrap()], "E_STORE_MISMATCH");
}

#[test]
fn sparse_old_run_pages_correctly_among_many_newer_records() {
    let h = Harness::new();
    h.init();
    let mut store = Store::open(&h.root, true).unwrap();
    let mut rare_refs = Vec::new();
    for _ in 0..3 {
        rare_refs.push(
            store
                .put(
                    &h.input,
                    PutOptions {
                        run: Some("rare".into()),
                        ..PutOptions::default()
                    },
                )
                .unwrap()["ref"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    drop(store);

    let connection = Connection::open(h.root.join("index.sqlite3")).unwrap();
    connection
        .execute_batch(
            "WITH digits(n) AS (VALUES(0),(1),(2),(3),(4),(5),(6),(7),(8),(9)),
             numbers(n) AS (
                 SELECT a.n + 10*b.n + 100*c.n + 1000*d.n + 10000*e.n
                 FROM digits a, digits b, digits c, digits d, digits e
             )
             INSERT INTO images(
                 image_id,run,stream,frame_no,created_at,captured_at,label,note,tags_json,
                 width,height,bit_depth,color_type,source_sha256,source_byte_length,
                 source_blob_sha256,scanline_sha256,non_idat_sha256,pixel_sha256,
                 operation_id,operation_fingerprint,operation_fingerprint_version,
                 validation_limits_json
             )
             SELECT printf('10000000-0000-0000-0000-%012d',numbers.n),'common',
                 'default',numbers.n-1,template.created_at,template.captured_at,
                 template.label,template.note,template.tags_json,template.width,
                 template.height,template.bit_depth,template.color_type,
                 template.source_sha256,template.source_byte_length,template.source_blob_sha256,
                 template.scanline_sha256,template.non_idat_sha256,template.pixel_sha256,
                 NULL,template.operation_fingerprint,template.operation_fingerprint_version,
                 template.validation_limits_json
             FROM numbers CROSS JOIN images AS template
             WHERE numbers.n BETWEEN 1 AND 30000 AND template.seq=1;

             WITH digits(n) AS (VALUES(0),(1),(2),(3),(4),(5),(6),(7),(8),(9)),
             numbers(n) AS (
                 SELECT a.n + 10*b.n + 100*c.n + 1000*d.n + 10000*e.n
                 FROM digits a, digits b, digits c, digits d, digits e
             )
             INSERT INTO representations(
                 image_id,representation_version,representation_kind,png_blob_sha256,
                 segment_id,encoding_version,compression_level,compression_applied,
                 created_at,verified_at
             )
             SELECT printf('10000000-0000-0000-0000-%012d',numbers.n),1,'png',
                 representation.png_blob_sha256,NULL,representation.encoding_version,
                 representation.compression_level,representation.compression_applied,
                 representation.created_at,representation.verified_at
             FROM numbers CROSS JOIN representations AS representation
             WHERE numbers.n BETWEEN 1 AND 30000
               AND representation.image_id=(SELECT image_id FROM images WHERE seq=1)",
        )
        .unwrap();
    drop(connection);

    let store = Store::open(&h.root, false).unwrap();
    let unfiltered = store.list(None, 2, None).unwrap();
    assert_eq!(unfiltered["items"].as_array().unwrap().len(), 2);
    assert!(
        unfiltered["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["run"] == "common")
    );
    assert!(unfiltered.get("next_cursor").is_some());

    let first = store.list(Some("rare".into()), 2, None).unwrap();
    assert_eq!(
        first["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["ref"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![rare_refs[2].as_str(), rare_refs[1].as_str()]
    );
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    drop(store);
    let mut store = Store::open(&h.root, true).unwrap();
    let later = store
        .put(
            &h.input,
            PutOptions {
                run: Some("rare".into()),
                ..PutOptions::default()
            },
        )
        .unwrap();
    drop(store);

    let store = Store::open(&h.root, false).unwrap();
    let second = store.list(Some("rare".into()), 2, Some(&cursor)).unwrap();
    assert_eq!(second["items"][0]["ref"], rare_refs[0]);
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    assert!(second.get("next_cursor").is_none());
    let fresh = store.list(Some("rare".into()), 1, None).unwrap();
    assert_eq!(fresh["items"][0]["ref"], later["ref"]);
}
#[test]
fn corrupt_or_missing_blob_is_detected_without_publishing_output() {
    for delete in [false, true] {
        let h = Harness::new();
        h.init();
        let p = h.put();
        let reference = p["ref"].as_str().unwrap();
        let info = h.call(&["info", reference]);
        let blob = h.blob(info["stored_sha256"].as_str().unwrap());
        if delete {
            fs::remove_file(blob).unwrap();
        } else {
            fs::write(blob, b"corrupt").unwrap();
        }
        let out = h.temp.path().join("out.png");
        h.error(
            &["get", reference, "--output", out.to_str().unwrap()],
            "E_INTEGRITY",
        );
        assert!(!out.exists());
        let report = h.temp.path().join("report.json");
        let v = h.error(
            &["verify", "--report", report.to_str().unwrap()],
            "E_INTEGRITY",
        );
        assert_eq!(v["data"]["valid"], false);
        assert!(!fs::read(&report).unwrap().is_empty());
        h.error(
            &["verify", "--report", report.to_str().unwrap()],
            "E_OUTPUT_EXISTS",
        );
    }
}
#[test]
fn symlinks_and_existing_exports_are_not_overwritten() {
    let h = Harness::new();
    h.init();
    let p = h.put();
    let reference = p["ref"].as_str().unwrap();
    let out = h.temp.path().join("out.png");
    fs::write(&out, b"keep").unwrap();
    h.error(
        &["get", reference, "--output", out.to_str().unwrap()],
        "E_OUTPUT_EXISTS",
    );
    assert_eq!(fs::read(&out).unwrap(), b"keep");
    let link = h.temp.path().join("link.png");
    symlink(&out, &link).unwrap();
    h.error(
        &["get", reference, "--output", link.to_str().unwrap()],
        "E_OUTPUT_EXISTS",
    );
    let dangling = h.temp.path().join("dangling.png");
    symlink(h.temp.path().join("absent"), &dangling).unwrap();
    h.error(
        &["get", reference, "--output", dangling.to_str().unwrap()],
        "E_OUTPUT_EXISTS",
    );
    fs::remove_dir(h.root.join("exports")).unwrap();
    symlink(h.temp.path(), h.root.join("exports")).unwrap();
    assert!(!h.raw(&["get", reference]).status.success());
}
#[test]
fn four_processes_share_blob_and_same_operation() {
    for operation in [false, true] {
        let h = Harness::new();
        h.init();
        let mut children = Vec::new();
        for _ in 0..4 {
            let mut c = h.command();
            c.args(["put", "--file"]).arg(&h.input);
            if operation {
                c.args(["--operation-id", "concurrent"]);
            }
            children.push(c.stdout(std::process::Stdio::piped()).spawn().unwrap());
        }
        let mut ids = std::collections::HashSet::new();
        for child in children {
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stdout)
            );
            let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            ids.insert(v["data"]["image_id"].to_string());
        }
        assert_eq!(ids.len(), if operation { 1 } else { 4 });
        let v = h.call(&["verify"]);
        assert_eq!(v["blobs_checked"], 1);
    }
}
#[test]
fn backup_restores_and_invalid_images_leave_no_records() {
    let h = Harness::new();
    h.init();
    let p = h.put();
    let backup = h.temp.path().join("backup");
    copy_tree(&h.root, &backup);
    let s = Store::open(&backup, false).unwrap();
    assert_eq!(s.verify(None).unwrap()["valid"], true);
    s.materialize(p["ref"].as_str().unwrap(), false, None)
        .unwrap();
    fs::write(&h.input, b"not png").unwrap();
    h.error(
        &["put", "--file", h.input.to_str().unwrap()],
        "E_INVALID_IMAGE",
    );
    assert_eq!(h.call(&["verify"])["images_checked"], 1);
}

#[test]
fn version_one_store_fixture_remains_readable() {
    const REFERENCE: &str =
        "visual://c3829740-9e12-4f8a-b723-f69b76a29a05/images/c3d40628-ebe0-4e4e-b768-ac1e2a6279fa";
    const HASH: &str = "fe9614fd5f645c8fe6e4dddb9d0bf075fa7bb6305651da42bc8063c3e18e2f97";
    let temp = tempfile::tempdir().unwrap();
    let copy = temp.path().join("v1-store");
    copy_tree(Path::new("tests/fixtures/v1-store"), &copy);
    let store = Store::open(&copy, false).unwrap();
    let info = store.info(REFERENCE).unwrap();
    assert_eq!(info["stored_sha256"], HASH);
    assert_eq!(info["width"], 4);
    assert_eq!(info["height"], 3);
    assert_eq!(store.verify(None).unwrap()["valid"], true);
    let stored = store.materialize(REFERENCE, false, None).unwrap();
    let source = store.materialize(REFERENCE, true, None).unwrap();
    assert_eq!(stored["sha256"], HASH);
    assert_eq!(source["sha256"], HASH);
}
#[test]
fn hundred_events_share_one_blob_and_no_ffmpeg_is_needed() {
    let h = Harness::new();
    h.init();
    let mut store = Store::open(&h.root, true).unwrap();
    for _ in 0..100 {
        store.put(&h.input, PutOptions::default()).unwrap();
    }
    drop(store);
    let out = h.command().arg("verify").env("PATH", "").output().unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["images_checked"], 100);
    assert_eq!(v["data"]["blobs_checked"], 1);
}
