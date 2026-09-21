mod common;

use common::{Harness, copy_tree};
use rusqlite::Connection;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::TempDir;
use visual_store::{PutOptions, Store};

const V1_REFERENCE: &str =
    "visual://c3829740-9e12-4f8a-b723-f69b76a29a05/images/c3d40628-ebe0-4e4e-b768-ac1e2a6279fa";
const V1_HASH: &str = "fe9614fd5f645c8fe6e4dddb9d0bf075fa7bb6305651da42bc8063c3e18e2f97";

fn v1_copy() -> (TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("store");
    copy_tree(Path::new("tests/fixtures/v1-store"), &root);
    (temp, root)
}

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vstore"));
    command.arg("--store").arg(root).env_remove("VSTORE_ROOT");
    command
}

fn call(root: &Path, args: &[&str]) -> Value {
    let output = command(root).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice::<Value>(&output.stdout).unwrap()["data"].clone()
}

#[test]
fn stream_frame_numbers_are_stable_and_allocated_per_stream() {
    let h = Harness::new();
    h.init();

    let ungrouped = h.put();
    assert!(ungrouped["stream"].is_null());
    assert!(ungrouped["frame_no"].is_null());

    let a = h.call(&[
        "put",
        "--file",
        h.input.to_str().unwrap(),
        "--run",
        "run-a",
        "--captured-at",
        "2026-09-15T12:00:00Z",
    ]);
    let b = h.call(&[
        "put",
        "--file",
        h.input.to_str().unwrap(),
        "--run",
        "run-a",
        "--captured-at",
        "2026-09-15T12:00:00Z",
    ]);
    let c = h.call(&[
        "put",
        "--file",
        h.input.to_str().unwrap(),
        "--run",
        "run-a",
        "--stream",
        "camera-b",
    ]);
    let earlier = h.call(&[
        "put",
        "--file",
        h.input.to_str().unwrap(),
        "--run",
        "run-a",
        "--captured-at",
        "2000-01-01T00:00:00Z",
    ]);
    assert_eq!(
        (&a["stream"], &a["frame_no"]),
        (&Value::from("default"), &Value::from(0))
    );
    assert_eq!(
        (&b["stream"], &b["frame_no"]),
        (&Value::from("default"), &Value::from(1))
    );
    assert_eq!(
        (&c["stream"], &c["frame_no"]),
        (&Value::from("camera-b"), &Value::from(0))
    );
    assert_eq!(earlier["frame_no"], 2);
    assert_ne!(earlier["image_id"], a["image_id"]);

    let operation = [
        "put",
        "--file",
        h.input.to_str().unwrap(),
        "--run",
        "retry-run",
        "--stream",
        "camera",
        "--operation-id",
        "same-observation",
    ];
    let first_operation = h.call(&operation);
    let retried_operation = h.call(&operation);
    assert_eq!(first_operation["image_id"], retried_operation["image_id"]);
    assert_eq!(first_operation["frame_no"], retried_operation["frame_no"]);
    assert_eq!(retried_operation["record_reused"], true);
    h.error(
        &[
            "put",
            "--file",
            h.input.to_str().unwrap(),
            "--run",
            "retry-run",
            "--stream",
            "different-camera",
            "--operation-id",
            "same-observation",
        ],
        "E_CONFLICT",
    );

    let mut children = Vec::new();
    for index in 0..8 {
        children.push(
            command(&h.root)
                .args(["put", "--file"])
                .arg(&h.input)
                .args([
                    "--run",
                    "concurrent",
                    "--stream",
                    "camera",
                    "--operation-id",
                    &format!("concurrent-{index}"),
                ])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    let mut frames = BTreeSet::new();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        frames.insert(result["data"]["frame_no"].as_u64().unwrap());
    }
    assert_eq!(frames, (0..8).collect());

    let mut store = Store::open(&h.root, true).unwrap();
    assert_eq!(
        store
            .put(
                &h.input,
                PutOptions {
                    run: Some("run-a".into()),
                    stream: Some(String::new()),
                    ..PutOptions::default()
                },
            )
            .err()
            .unwrap()
            .code,
        "E_INVALID_ARGUMENT"
    );
    assert_eq!(
        store
            .put(
                &h.input,
                PutOptions {
                    run: Some("run-a".into()),
                    stream: Some("界".repeat(43)),
                    ..PutOptions::default()
                },
            )
            .err()
            .unwrap()
            .code,
        "E_LIMIT_EXCEEDED"
    );
    assert_eq!(
        store
            .put(
                &h.input,
                PutOptions {
                    stream: Some("camera".into()),
                    ..PutOptions::default()
                },
            )
            .err()
            .unwrap()
            .code,
        "E_INVALID_ARGUMENT"
    );
    drop(store);

    let connection = Connection::open(h.root.join("index.sqlite3")).unwrap();
    assert!(
        connection
            .execute(
                "UPDATE images SET frame_no=0 WHERE image_id=?1",
                [b["image_id"].as_str().unwrap()],
            )
            .is_err()
    );
    assert!(
        connection
            .execute(
                "UPDATE images SET stream='' WHERE image_id=?1",
                [a["image_id"].as_str().unwrap()],
            )
            .is_err()
    );
}

#[test]
fn explicit_v1_migration_preserves_identity_metadata_and_legacy_retry() {
    let (temp, root) = v1_copy();
    let input = temp.path().join("fixture.png");
    fs::copy(
        root.join(format!("objects/sha256/fe/96/{V1_HASH}.png")),
        &input,
    )
    .unwrap();
    let connection = Connection::open(root.join("index.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE images SET operation_id='legacy-operation' WHERE seq=1",
            [],
        )
        .unwrap();
    drop(connection);

    let before = Store::open(&root, false)
        .unwrap()
        .info(V1_REFERENCE)
        .unwrap();
    assert_eq!(before["stored_sha256"], V1_HASH);
    assert_eq!(
        Store::open(&root, true).err().unwrap().code,
        "E_SCHEMA_VERSION"
    );

    let result = call(&root, &["migrate", "--to", "2"]);
    assert_eq!(result["migrated"], true);
    assert_eq!(result["restored"], false);
    assert!(!root.join("migration-v1-to-v2.json").exists());
    assert!(!root.join("migration-v1-backup.sqlite3").exists());

    let mut store = Store::open(&root, true).unwrap();
    let after = store.info(V1_REFERENCE).unwrap();
    for field in [
        "ref",
        "image_id",
        "seq",
        "run",
        "created_at",
        "captured_at",
        "label",
        "note",
        "tags",
        "source_sha256",
        "source_bytes",
        "stored_sha256",
        "stored_bytes",
        "source_retained",
        "scanline_sha256",
        "non_idat_sha256",
        "pixel_sha256",
    ] {
        assert_eq!(after[field], before[field], "field {field}");
    }
    assert_eq!(after["stream"], "default");
    assert_eq!(after["frame_no"], 0);
    assert_eq!(store.verify(None).unwrap()["valid"], true);
    let stored = store.materialize(V1_REFERENCE, false, None).unwrap();
    let source = store.materialize(V1_REFERENCE, true, None).unwrap();
    assert_eq!(
        fs::read(stored["path"].as_str().unwrap()).unwrap(),
        fs::read(&input).unwrap()
    );
    assert_eq!(
        fs::read(source["path"].as_str().unwrap()).unwrap(),
        fs::read(&input).unwrap()
    );

    let retry = store
        .put(
            &input,
            PutOptions {
                run: Some("compatibility-v1".into()),
                label: Some("fixed-fixture".into()),
                keep_source: true,
                operation_id: Some("legacy-operation".into()),
                ..PutOptions::default()
            },
        )
        .unwrap();
    assert_eq!(retry["ref"], V1_REFERENCE);
    assert_eq!(retry["record_reused"], true);
    drop(store);

    let connection = Connection::open(root.join("index.sqlite3")).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        2
    );
    let migration: (u32, u32, Option<String>) = connection
        .query_row(
            "SELECT from_version,to_version,completed_at FROM schema_migrations",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!((migration.0, migration.1), (1, 2));
    assert!(migration.2.is_some());
}

#[test]
fn explicit_v2_to_v3_migration_preserves_images_and_blobs() {
    let (_temp, root) = v1_copy();
    let v2 = call(&root, &["migrate", "--to", "2"]);
    assert_eq!(v2["to_version"], 2);

    let before = Store::open(&root, false)
        .unwrap()
        .info(V1_REFERENCE)
        .unwrap();
    let connection = Connection::open(root.join("index.sqlite3")).unwrap();
    let image_count: u64 = connection
        .query_row("SELECT COUNT(*) FROM images", [], |row| row.get(0))
        .unwrap();
    let blob_count: u64 = connection
        .query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get(0))
        .unwrap();
    drop(connection);

    let old_judgment = command(&root)
        .args([
            "judgment",
            "add",
            V1_REFERENCE,
            "--kind",
            "needs_visual_inspection",
            "--producer",
            "jev",
            "--value",
            "false",
        ])
        .output()
        .unwrap();
    assert!(!old_judgment.status.success());
    let old_error: Value = serde_json::from_slice(&old_judgment.stdout).unwrap();
    assert_eq!(old_error["error"]["code"], "E_SCHEMA_VERSION");

    let migrated = call(&root, &["migrate", "--to", "3"]);
    assert_eq!(migrated["from_version"], 2);
    assert_eq!(migrated["to_version"], 3);
    assert_eq!(migrated["migrated"], true);
    assert!(!root.join("migration-v2-to-v3.json").exists());
    assert!(!root.join("migration-v2-backup.sqlite3").exists());

    let after = Store::open(&root, false)
        .unwrap()
        .info(V1_REFERENCE)
        .unwrap();
    assert_eq!(after, before);
    let connection = Connection::open(root.join("index.sqlite3")).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM images", [], |row| row
                .get::<_, u64>(0))
            .unwrap(),
        image_count
    );
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get::<_, u64>(0))
            .unwrap(),
        blob_count
    );
    let tables: u64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='judgments'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 1);
    let migrations: Vec<(u32, u32)> = connection
        .prepare("SELECT from_version,to_version FROM schema_migrations ORDER BY migration_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(migrations, vec![(1, 2), (2, 3)]);
    drop(connection);

    let judgment = call(
        &root,
        &[
            "judgment",
            "add",
            V1_REFERENCE,
            "--kind",
            "needs_visual_inspection",
            "--producer",
            "jev",
            "--value",
            "false",
            "--confidence",
            "0.96",
        ],
    );
    assert_eq!(judgment["value"], false);
}

#[test]
fn v2_schema_enforces_representation_and_frame_location_constraints() {
    let h = Harness::new();
    h.init();
    let first = h.call(&[
        "put",
        "--file",
        h.input.to_str().unwrap(),
        "--run",
        "constraints",
    ]);
    let second = h.call(&[
        "put",
        "--file",
        h.input.to_str().unwrap(),
        "--run",
        "constraints",
    ]);
    let connection = Connection::open(h.root.join("index.sqlite3")).unwrap();
    connection
        .pragma_update(None, "foreign_keys", true)
        .unwrap();
    assert!(connection
        .execute(
            "INSERT INTO frame_locations(image_id,segment_id,frame_index,decode_start_index) VALUES ('00000000-0000-0000-0000-000000000000','segment',1,0)",
            [],
        )
        .is_err());
    let blob: String = connection
        .query_row(
            "SELECT png_blob_sha256 FROM representations LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO segments(segment_id,codec,codec_descriptor_json,width,height,bit_depth,pixel_layout,frame_count,color_blob_sha256,created_at) VALUES ('segment','vp9','{}',32,24,8,'rgba',2,?1,'now')",
            [&blob],
        )
        .unwrap();
    assert!(connection
        .execute(
            "INSERT INTO frame_locations(image_id,segment_id,frame_index,decode_start_index) VALUES (?1,'segment',2,0)",
            [first["image_id"].as_str().unwrap()],
        )
        .is_err());
    connection
        .execute(
            "INSERT INTO frame_locations(image_id,segment_id,frame_index,decode_start_index) VALUES (?1,'segment',0,0)",
            [first["image_id"].as_str().unwrap()],
        )
        .unwrap();
    assert!(
        connection
            .execute(
                "UPDATE segments SET frame_count=3 WHERE segment_id='segment'",
                []
            )
            .is_err()
    );
    assert!(
        connection
            .execute(
                "UPDATE images SET stream='default',frame_no=0 WHERE image_id=?1",
                [second["image_id"].as_str().unwrap()],
            )
            .is_err()
    );
    let indexes: BTreeSet<String> = connection
        .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%'")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for required in [
        "images_by_run_seq",
        "images_by_stream_frame",
        "representations_by_segment",
        "frame_locations_by_segment",
    ] {
        assert!(indexes.contains(required), "missing index {required}");
    }
    let foreign_key_errors: u32 = connection
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(foreign_key_errors, 0);
}

#[cfg(feature = "fault-injection")]
#[test]
fn migration_rolls_forward_after_every_persistence_boundary() {
    for point in [
        "migration_after_backup",
        "migration_after_journal",
        "migration_before_db_commit",
        "migration_after_db_commit",
        "migration_after_db_journal",
        "migration_after_manifest",
        "migration_after_manifest_journal",
        "migration_before_cleanup",
        "migration_after_backup_cleanup",
        "migration_after_cleanup",
    ] {
        let (_temp, root) = v1_copy();
        let output = command(&root)
            .args(["migrate", "--to", "2"])
            .env("VSTORE_TEST_CRASH", point)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(91), "{point}");

        let journal_exists = root.join("migration-v1-to-v2.json").exists();
        if journal_exists {
            assert_eq!(
                Store::open(&root, false).err().unwrap().code,
                "E_MIGRATION_INCOMPLETE"
            );
            let without_resume = command(&root)
                .args(["migrate", "--to", "2"])
                .output()
                .unwrap();
            assert_eq!(without_resume.status.code(), Some(4), "{point}");
            let error: Value = serde_json::from_slice(&without_resume.stdout).unwrap();
            assert_eq!(error["error"]["code"], "E_MIGRATION_INCOMPLETE");
        }
        let resumed = call(&root, &["migrate", "--to", "2", "--resume"]);
        assert!(
            resumed["migrated"] == true || resumed["to_version"] == 2,
            "{point}"
        );
        let store = Store::open(&root, false).unwrap();
        assert_eq!(store.info(V1_REFERENCE).unwrap()["stored_sha256"], V1_HASH);
        assert_eq!(store.verify(None).unwrap()["valid"], true);
        assert!(!root.join("migration-v1-to-v2.json").exists(), "{point}");
        assert!(
            !root.join("migration-v1-backup.sqlite3").exists(),
            "{point}"
        );
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn interrupted_migration_can_be_restored_and_restore_is_resumable() {
    for restore_point in [
        "migration_restore_started",
        "migration_restore_after_db",
        "migration_restore_after_manifest",
        "migration_restore_after_journal_cleanup",
    ] {
        let (_temp, root) = v1_copy();
        let migration = command(&root)
            .args(["migrate", "--to", "2"])
            .env("VSTORE_TEST_CRASH", "migration_after_manifest")
            .output()
            .unwrap();
        assert_eq!(migration.status.code(), Some(91));
        let restore = command(&root)
            .args(["migrate", "--to", "2", "--restore"])
            .env("VSTORE_TEST_CRASH", restore_point)
            .output()
            .unwrap();
        assert_eq!(restore.status.code(), Some(91), "{restore_point}");

        let restored = call(&root, &["migrate", "--to", "2", "--restore"]);
        assert_eq!(restored["restored"], true);
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(root.join("store.json")).unwrap()).unwrap()["format_version"],
            1
        );
        assert_eq!(
            Store::open(&root, false)
                .unwrap()
                .info(V1_REFERENCE)
                .unwrap()["stored_sha256"],
            V1_HASH
        );
        assert_eq!(
            Store::open(&root, true).err().unwrap().code,
            "E_SCHEMA_VERSION"
        );
        assert!(!root.join("migration-v1-to-v2.json").exists());
        assert!(!root.join("migration-v1-backup.sqlite3").exists());
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn version_three_migration_resumes_after_persistence_boundaries() {
    for point in [
        "migration_v3_after_backup",
        "migration_v3_after_journal",
        "migration_v3_before_db_commit",
        "migration_v3_after_db_commit",
        "migration_v3_after_db_journal",
        "migration_v3_after_manifest",
        "migration_v3_after_manifest_journal",
        "migration_v3_before_cleanup",
        "migration_v3_after_backup_cleanup",
        "migration_v3_after_cleanup",
    ] {
        let (_temp, root) = v1_copy();
        call(&root, &["migrate", "--to", "2"]);
        let output = command(&root)
            .args(["migrate", "--to", "3"])
            .env("VSTORE_TEST_CRASH", point)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(91), "{point}");

        let resumed = call(&root, &["migrate", "--to", "3", "--resume"]);
        assert_eq!(resumed["to_version"], 3, "{point}");
        assert!(!root.join("migration-v2-to-v3.json").exists(), "{point}");
        assert!(
            !root.join("migration-v2-backup.sqlite3").exists(),
            "{point}"
        );
        assert_eq!(
            Store::open(&root, false)
                .unwrap()
                .info(V1_REFERENCE)
                .unwrap()["stored_sha256"],
            V1_HASH,
            "{point}"
        );
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn version_three_migration_restore_is_resumable() {
    for point in [
        "migration_v3_restore_started",
        "migration_v3_restore_after_db",
        "migration_v3_restore_after_manifest",
        "migration_v3_restore_after_journal_cleanup",
    ] {
        let (_temp, root) = v1_copy();
        call(&root, &["migrate", "--to", "2"]);
        let interrupted = command(&root)
            .args(["migrate", "--to", "3"])
            .env("VSTORE_TEST_CRASH", "migration_v3_after_manifest")
            .output()
            .unwrap();
        assert_eq!(interrupted.status.code(), Some(91));
        let restore = command(&root)
            .args(["migrate", "--to", "3", "--restore"])
            .env("VSTORE_TEST_CRASH", point)
            .output()
            .unwrap();
        assert_eq!(restore.status.code(), Some(91), "{point}");
        let restored = call(&root, &["migrate", "--to", "3", "--restore"]);
        assert_eq!(restored["to_version"], 2, "{point}");
        assert_eq!(
            Connection::open(root.join("index.sqlite3"))
                .unwrap()
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            2,
            "{point}"
        );
        assert_eq!(
            Store::open(&root, false)
                .unwrap()
                .info(V1_REFERENCE)
                .unwrap()["stored_sha256"],
            V1_HASH,
            "{point}"
        );
    }
}
