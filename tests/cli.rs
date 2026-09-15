mod common;
use common::*;
use serde_json::Value;
use visual_store::{PutOptions, Store};

const LIST_STDOUT_BUDGET_BYTES: usize = 16 * 1024;

fn list_page(h: &Harness, run: &str, limit: u32, cursor: Option<&str>) -> (Value, usize) {
    let mut command = h.command();
    command
        .arg("list")
        .args(["--run", run, "--limit", &limit.to_string()]);
    if let Some(cursor) = cursor {
        command.args(["--cursor", cursor]);
    }
    let out = command.output().unwrap();
    assert!(
        out.status.success(),
        "list failed: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        out.stdout.len() <= LIST_STDOUT_BUDGET_BYTES,
        "list JSON is {} bytes",
        out.stdout.len()
    );
    let bytes = out.stdout.len();
    let envelope: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(envelope["ok"], true);
    (envelope["data"].clone(), bytes)
}

fn collect_list(h: &Harness, run: &str, limit: u32) -> Vec<i64> {
    let mut cursor = None;
    let mut sequences = Vec::new();
    loop {
        let (page, _) = list_page(h, run, limit, cursor.as_deref());
        sequences.extend(
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["seq"].as_i64().unwrap()),
        );
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            return sequences;
        }
    }
}

#[test]
fn stdout_obeys_json_schema_and_size_budgets() {
    let schema: Value = serde_json::from_str(include_str!("../docs/cli.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let h = Harness::new();
    let check = |args: &[&str], budget: usize| -> Value {
        let out = h.raw(args);
        assert!(out.stdout.len() <= budget, "JSON exceeds {budget} bytes");
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        if let Err(e) = validator.validate(&v) {
            panic!("{args:?}: {e}: {v}");
        }
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(!text.contains("data:image"));
        assert!(!text.contains("iVBORw0KGgo"));
        v
    };
    check(&["list"], 4096);
    check(&["init"], 4096);
    check(&["migrate", "--to", "2"], 4096);
    let p = check(
        &[
            "put",
            "--file",
            h.input.to_str().unwrap(),
            "--label",
            "日本語",
            "--note",
            &"メモ".repeat(300),
        ],
        4096,
    );
    let r = p["data"]["ref"].as_str().unwrap();
    check(&["info", r], 8192);
    check(&["get", r], 4096);
    check(&["list"], 16384);
    check(&["verify"], 8192);
    check(&["get", r, "--variant", "source"], 4096);
    check(&["unknown-command"], 4096);
    check(
        &[
            "put",
            "--file",
            h.input.to_str().unwrap(),
            "--compression-level",
            "12",
        ],
        4096,
    );
    h.error(
        &[
            "put",
            "--file",
            h.input.to_str().unwrap(),
            "--note",
            &"\u{1}".repeat(2048),
        ],
        "E_LIMIT_EXCEEDED",
    );
}

#[test]
fn list_budget_pages_twenty_hundred_and_escaped_metadata_without_loss() {
    let h = Harness::new();
    h.init();
    let mut store = Store::open(&h.root, true).unwrap();
    for i in 0..100 {
        store
            .put(
                &h.input,
                PutOptions {
                    run: Some("short-metadata".into()),
                    label: Some(format!("image-{i}")),
                    ..PutOptions::default()
                },
            )
            .unwrap();
    }

    let (default_page, default_bytes) = list_page(&h, "short-metadata", 20, None);
    assert_eq!(default_page["items"].as_array().unwrap().len(), 20);
    assert!(default_page.get("next_cursor").is_some());
    assert!(default_bytes <= LIST_STDOUT_BUDGET_BYTES);

    let (hundred_page, hundred_bytes) = list_page(&h, "short-metadata", 100, None);
    assert!(hundred_page["items"].as_array().unwrap().len() < 100);
    assert!(hundred_page.get("next_cursor").is_some());
    assert!(hundred_bytes <= LIST_STDOUT_BUDGET_BYTES);
    let short_sequences = collect_list(&h, "short-metadata", 100);
    assert_eq!(short_sequences.len(), 100);
    assert!(short_sequences.windows(2).all(|pair| pair[0] > pair[1]));

    let escaped_run = "\u{1}".repeat(128);
    let escaped_label = "\u{2}".repeat(256);
    for _ in 0..20 {
        store
            .put(
                &h.input,
                PutOptions {
                    run: Some(escaped_run.clone()),
                    label: Some(escaped_label.clone()),
                    ..PutOptions::default()
                },
            )
            .unwrap();
    }
    drop(store);

    let (escaped_page, escaped_bytes) = list_page(&h, &escaped_run, 20, None);
    assert!(escaped_page["items"].as_array().unwrap().len() < 20);
    assert!(escaped_page.get("next_cursor").is_some());
    assert!(escaped_bytes <= LIST_STDOUT_BUDGET_BYTES);
    for item in escaped_page["items"].as_array().unwrap() {
        assert_eq!(item["run"], escaped_run);
        assert_eq!(item["label"], escaped_label);
    }
    let escaped_sequences = collect_list(&h, &escaped_run, 20);
    assert_eq!(escaped_sequences.len(), 20);
    assert!(escaped_sequences.windows(2).all(|pair| pair[0] > pair[1]));
}
#[test]
fn environment_store_selection_and_no_implicit_ancestor_search() {
    let h = Harness::new();
    h.init();
    let binary = env!("CARGO_BIN_EXE_vstore");
    let out = std::process::Command::new(binary)
        .arg("list")
        .env("VSTORE_ROOT", &h.root)
        .output()
        .unwrap();
    assert!(out.status.success());
    let out = std::process::Command::new(binary)
        .args(["--store", "absent", "list"])
        .env("VSTORE_ROOT", &h.root)
        .current_dir(h.temp.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let out = std::process::Command::new(binary)
        .arg("list")
        .env_remove("VSTORE_ROOT")
        .current_dir(h.temp.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
}
#[test]
fn help_and_version_are_text_exceptions() {
    let h = Harness::new();
    for args in [["--help"], ["--version"]] {
        let out = h.raw(&args);
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("vstore"));
    }
}
