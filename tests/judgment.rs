mod common;

use common::Harness;
use serde_json::{Value, json};
use std::{fs, io::Write, process::Stdio};

fn add(h: &Harness, reference: &str, kind: &str, producer: &str, value: &str) -> Value {
    h.call(&[
        "judgment",
        "add",
        reference,
        "--kind",
        kind,
        "--producer",
        producer,
        "--value",
        value,
    ])
}

#[test]
fn judgment_add_list_and_filters_preserve_typed_values() {
    let h = Harness::new();
    h.init();
    let image = h.put();
    let reference = image["ref"].as_str().unwrap();

    let first = h.call(&[
        "judgment",
        "add",
        reference,
        "--kind",
        "needs_visual_inspection",
        "--producer",
        "jev",
        "--model",
        "jev-1.13.0",
        "--value",
        "false",
        "--probability",
        "0.04",
        "--confidence",
        "0.96",
        "--metadata",
        r#"{"question_type":"choice"}"#,
    ]);
    assert_eq!(first["value"], false);
    assert_eq!(first["probability"], 0.04);
    assert_eq!(first["confidence"], 0.96);
    assert_eq!(first["schema_version"], 1);
    assert_eq!(first["metadata"]["question_type"], "choice");

    let input = h.temp.path().join("judgment.json");
    fs::write(
        &input,
        serde_json::to_vec(&json!({
            "kind": "visual_change",
            "producer": "rule",
            "value": {"class": "unexpected", "regions": 2},
            "confidence": 0.87,
            "metadata": {"rule_version": 3}
        }))
        .unwrap(),
    )
    .unwrap();
    let second = h.call(&[
        "judgment",
        "add",
        reference,
        "--json",
        input.to_str().unwrap(),
    ]);
    assert_eq!(second["value"]["class"], "unexpected");

    let all = h.call(&["judgment", "list", reference]);
    assert_eq!(all["items"].as_array().unwrap().len(), 2);
    let by_kind = h.call(&["judgment", "list", reference, "--kind", "visual_change"]);
    assert_eq!(by_kind["items"].as_array().unwrap().len(), 1);
    assert_eq!(by_kind["items"][0]["producer"], "rule");
    assert_eq!(by_kind["items"][0]["value"], second["value"]);
    let by_producer = h.call(&["judgment", "list", reference, "--producer", "jev"]);
    assert_eq!(by_producer["items"].as_array().unwrap().len(), 1);
    assert_eq!(by_producer["items"][0]["confidence"], 0.96);
}

#[test]
fn judgment_stdin_search_and_pagination_are_machine_readable() {
    let h = Harness::new();
    h.init();
    let first_image = h.put();
    let second_image = h.put();
    let first_ref = first_image["ref"].as_str().unwrap();
    let second_ref = second_image["ref"].as_str().unwrap();

    add(&h, first_ref, "needs_visual_inspection", "jev", "true");
    add(&h, second_ref, "needs_visual_inspection", "rule", "false");

    let mut child = h
        .command()
        .args(["judgment", "add", second_ref, "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            br#"{"kind":"likely_error_state","producer":"vision-llm","value":true,"confidence":0.42}"#,
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );

    let needs_inspection = h.call(&[
        "judgment",
        "search",
        "--kind",
        "needs_visual_inspection",
        "--value",
        "true",
    ]);
    assert_eq!(needs_inspection["items"].as_array().unwrap().len(), 1);
    assert_eq!(needs_inspection["items"][0]["ref"], first_ref);

    let escalated = h.call(&["judgment", "search", "--producer", "vision-llm"]);
    assert_eq!(escalated["items"].as_array().unwrap().len(), 1);
    assert_eq!(escalated["items"][0]["kind"], "likely_error_state");

    let low_confidence = h.call(&["judgment", "search", "--confidence-below", "0.5"]);
    assert_eq!(low_confidence["items"].as_array().unwrap().len(), 1);
    assert_eq!(low_confidence["items"][0]["confidence"], 0.42);

    let page = h.call(&["judgment", "search", "--limit", "2"]);
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    let next = page["next_cursor"].as_str().unwrap();
    let last = h.call(&["judgment", "search", "--limit", "2", "--cursor", next]);
    assert_eq!(last["items"].as_array().unwrap().len(), 1);
}

#[test]
fn judgment_rejects_invalid_references_missing_images_and_invalid_input() {
    let h = Harness::new();
    h.init();
    let image = h.put();
    let reference = image["ref"].as_str().unwrap();

    h.error(
        &[
            "judgment",
            "add",
            "visual://bad",
            "--kind",
            "x",
            "--producer",
            "jev",
            "--value",
            "true",
        ],
        "E_INVALID_ARGUMENT",
    );
    let missing = format!(
        "visual://{}/images/00000000-0000-0000-0000-000000000000",
        image["ref"]
            .as_str()
            .unwrap()
            .strip_prefix("visual://")
            .unwrap()
            .split('/')
            .next()
            .unwrap()
    );
    h.error(
        &[
            "judgment",
            "add",
            &missing,
            "--kind",
            "x",
            "--producer",
            "jev",
            "--value",
            "true",
        ],
        "E_NOT_FOUND",
    );
    h.error(
        &[
            "judgment",
            "add",
            reference,
            "--kind",
            "x",
            "--producer",
            "jev",
            "--value",
            "not-json",
        ],
        "E_INVALID_ARGUMENT",
    );
    h.error(
        &[
            "judgment",
            "add",
            reference,
            "--kind",
            "x",
            "--producer",
            "jev",
            "--value",
            "true",
            "--confidence",
            "1.1",
        ],
        "E_INVALID_ARGUMENT",
    );

    let invalid = h.temp.path().join("invalid.json");
    fs::write(
        &invalid,
        br#"{"kind":"x","producer":"jev","value":true,"unknown":1}"#,
    )
    .unwrap();
    h.error(
        &[
            "judgment",
            "add",
            reference,
            "--json",
            invalid.to_str().unwrap(),
        ],
        "E_INVALID_ARGUMENT",
    );
}

#[test]
fn lightweight_features_use_metadata_without_materializing_images() {
    let h = Harness::new();
    h.init();
    let first = h.call(&[
        "put",
        "--file",
        h.input.to_str().unwrap(),
        "--run",
        "feature-run",
        "--stream",
        "browser",
    ]);
    let second = h.call(&[
        "put",
        "--file",
        h.input.to_str().unwrap(),
        "--run",
        "feature-run",
        "--stream",
        "browser",
    ]);
    let features = h.call(&["features", second["ref"].as_str().unwrap()]);
    assert_eq!(features["width"], 32);
    assert_eq!(features["height"], 24);
    assert_eq!(features["previous"]["ref"], first["ref"]);
    assert_eq!(features["previous"]["same_pixels"], true);
    assert!(features["sha256"]["source"].as_str().is_some());
    assert!(features["sha256"]["pixels"].as_str().is_some());
    assert_eq!(
        features["representation_bytes_semantics"],
        "shared_and_not_additive_across_images"
    );
}
