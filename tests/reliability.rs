mod common;
use common::*;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    time::{Duration, Instant},
};

#[test]
fn busy_timeout_is_finite_and_keeps_existing_images() {
    let h = Harness::new();
    h.init();
    h.put();
    let c = rusqlite::Connection::open(h.root.join("index.sqlite3")).unwrap();
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    let start = Instant::now();
    h.error(&["put", "--file", h.input.to_str().unwrap()], "E_BUSY");
    assert!(start.elapsed() >= Duration::from_secs(4));
    assert!(start.elapsed() < Duration::from_secs(15));
    c.execute_batch("ROLLBACK").unwrap();
    assert_eq!(h.call(&["verify"])["images_checked"], 1);
}
#[test]
fn permission_error_keeps_source_and_existing_store() {
    // Root bypasses permission bits; this test is meaningful for normal users only.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let h = Harness::new();
    h.init();
    let p = h.put();
    let original = fs::read(&h.input).unwrap();
    let tmp = h.root.join("tmp");
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o500)).unwrap();
    h.error(
        &["put", "--file", h.input.to_str().unwrap()],
        "E_PERMISSION",
    );
    fs::set_permissions(tmp, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(fs::read(&h.input).unwrap(), original);
    h.call(&["get", p["ref"].as_str().unwrap()]);
}
#[test]
fn managed_symlinks_are_rejected_and_store_id_must_match() {
    let h = Harness::new();
    h.init();
    let db = h.root.join("index.sqlite3");
    let external = h.temp.path().join("external.sqlite3");
    fs::rename(&db, &external).unwrap();
    symlink(&external, &db).unwrap();
    h.error(&["list"], "E_INTEGRITY");
    let h = Harness::new();
    h.init();
    let path = h.root.join("store.json");
    let mut m: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    m["store_id"] = uuid::Uuid::new_v4().to_string().into();
    fs::write(path, serde_json::to_vec(&m).unwrap()).unwrap();
    h.error(&["list"], "E_INTEGRITY");
}

#[cfg(feature = "fault-injection")]
#[test]
fn source_change_during_snapshot_is_detected() {
    let h = Harness::new();
    h.init();
    let out = h
        .command()
        .args(["put", "--file"])
        .arg(&h.input)
        .env("VSTORE_TEST_MUTATE_SOURCE", "1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["error"]["code"], "E_SOURCE_CHANGED");
    assert_eq!(h.call(&["verify"])["images_checked"], 0);
}

#[cfg(feature = "fault-injection")]
#[test]
fn process_crashes_at_every_commit_boundary_preserve_committed_images() {
    for point in [
        "before_blob_publish",
        "after_blob_publish",
        "before_db_commit",
        "during_db_commit",
        "after_db_commit",
    ] {
        let h = Harness::new();
        h.init();
        let existing = h.put();
        fs::write(&h.input, fixture(33, 24, true, false)).unwrap();
        let out = h
            .command()
            .args(["put", "--file"])
            .arg(&h.input)
            .args(["--operation-id", "crash-op"])
            .env("VSTORE_TEST_CRASH", point)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(91), "{point}");
        assert!(out.stdout.is_empty());
        let v = h.call(&["verify"]);
        assert_eq!(v["valid"], true);
        assert_eq!(
            v["images_checked"],
            if point == "after_db_commit" { 2 } else { 1 }
        );
        h.call(&["get", existing["ref"].as_str().unwrap()]);
        let p = h.call(&[
            "put",
            "--file",
            h.input.to_str().unwrap(),
            "--operation-id",
            "crash-op",
        ]);
        assert_eq!(p["record_reused"], point == "after_db_commit");
        assert_eq!(h.call(&["verify"])["images_checked"], 2);
    }
}
#[cfg(feature = "fault-injection")]
#[test]
fn injected_disk_full_never_removes_committed_data() {
    for point in [
        "blob_write",
        "before_blob_publish",
        "after_blob_publish",
        "during_db_commit",
    ] {
        let h = Harness::new();
        h.init();
        let p = h.put();
        let original = fs::read(&h.input).unwrap();
        let out = h
            .command()
            .args(["put", "--file"])
            .arg(&h.input)
            .env("VSTORE_TEST_DISK_FULL", point)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(6));
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["error"]["code"], "E_DISK_FULL");
        assert_eq!(fs::read(&h.input).unwrap(), original);
        h.call(&["get", p["ref"].as_str().unwrap()]);
        assert_eq!(h.call(&["verify"])["images_checked"], 1);
    }
}
#[cfg(feature = "fault-injection")]
#[test]
fn partial_initialization_is_reported_without_deleting_files() {
    let h = Harness::new();
    let out = h
        .command()
        .arg("init")
        .env("VSTORE_TEST_CRASH", "init_before_manifest")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(91));
    h.error(&["init"], "E_INTEGRITY");
    assert!(h.root.join("index.sqlite3").exists());
}
