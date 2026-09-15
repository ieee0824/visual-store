mod common;
use common::*;
use serde_json::Value;

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
