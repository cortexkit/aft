use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::helpers::AftProcess;

fn offline_aft(root: &Path, storage: &Path) -> AftProcess {
    let transport = storage.join("missing-downloads");
    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_TEST_GRAMMAR_STORAGE", storage.as_os_str()),
        ("AFT_TEST_GRAMMAR_TRANSPORT_DIR", transport.as_os_str()),
    ]);
    let config = aft.send(&json!({
        "id":"configure", "command":"configure", "harness":"opencode",
        "project_root":root, "storage_dir":storage,
        "config":[{"tier":"user","source":storage.join("aft.jsonc"),
            "doc":json!({"storage":{"cache_dir":null},"indexes":{"semantic":false}}).to_string()}],
    }).to_string());
    assert_eq!(config["success"], true, "{config:#}");
    aft
}

fn request(aft: &mut AftProcess, name: &str, arguments: Value, retry: bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(45);
    for id in 0.. {
        let reply = aft.send(
            &json!({"id":id.to_string(), "command":"tool_call", "name":name, "arguments":arguments})
                .to_string(),
        );
        if !retry || grammar_gap(&reply).is_none() || Instant::now() >= deadline {
            return reply;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    unreachable!()
}

fn grammar_gap(reply: &Value) -> Option<&Value> {
    if reply["code"] == "grammar_unavailable" {
        return Some(reply);
    }
    reply["gaps"]
        .as_array()?
        .iter()
        .find(|gap| gap["code"] == "grammar_unavailable")
}

#[test]
fn systemverilog_reading_reports_unavailable_grammar() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let source = root.path().join("example.sv");
    fs::write(&source, "module example; endmodule\n").unwrap();
    let mut aft = offline_aft(root.path(), storage.path());
    for (tool, args) in [
        ("outline", json!({"target":source})),
        (
            "zoom",
            json!({"batch":[{"target":source,"symbols":"example"}]}),
        ),
    ] {
        let reply = request(&mut aft, tool, args, false);
        let gap = grammar_gap(&reply).unwrap_or_else(|| panic!("{reply:#}"));
        assert!(
            gap.to_string().contains("SystemVerilog grammar"),
            "{reply:#}"
        );
        assert!(
            reply["success"] == false || reply["complete"] == false,
            "{reply:#}"
        );
    }
}

#[test]
#[ignore = "requires AFT_TEST_SYSTEMVERILOG_WASM pointing to the built or published artifact"]
fn systemverilog_cached_grammar_outlines_and_zooms_the_design_offline() {
    let artifact = std::env::var_os("AFT_TEST_SYSTEMVERILOG_WASM").expect("path to test artifact");
    let entry = aft::grammar::manifest("systemverilog").unwrap();
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let cache = storage.path().join("grammars");
    fs::create_dir(&cache).unwrap();
    fs::copy(artifact, cache.join(format!("{}.wasm", entry.sha256))).unwrap();
    let fixtures = super::test_helpers::manifest_dir().join("tests/fixtures/systemverilog");
    for file in fs::read_dir(fixtures).unwrap() {
        let file = file.unwrap();
        fs::copy(file.path(), root.path().join(file.file_name())).unwrap();
    }
    // Check all advertised extensions without treating them as built-in languages.
    for extension in ["svh", "v", "vh"] {
        fs::write(
            root.path().join(format!("example.{extension}")),
            "module example; endmodule\n",
        )
        .unwrap();
    }
    let mut aft = offline_aft(root.path(), storage.path());
    for (file, expected) in [
        (
            "types_pkg.sv",
            &[
                "package types_pkg",
                "data_t;",
                "function automatic data_t increment",
                "task automatic report",
            ][..],
        ),
        ("stream_if.sv", &["interface stream_if"][..]),
        ("register_slice.sv", &["module register_slice"][..]),
        ("top.sv", &["module top"][..]),
        ("example.svh", &["module example"][..]),
        ("example.v", &["module example"][..]),
        ("example.vh", &["module example"][..]),
    ] {
        let result = request(
            &mut aft,
            "outline",
            json!({"target":root.path().join(file)}),
            true,
        );
        assert_eq!(result["success"], true, "{result:#}");
        let text = result["text"].as_str().unwrap();
        for item in expected {
            assert!(text.lines().any(|line| line.contains(item)), "{text}");
        }
    }
    drop(aft);
    // The new process has no in-memory grammar and its transport cannot fetch it.
    let mut aft = offline_aft(root.path(), storage.path());
    let reply = request(
        &mut aft,
        "zoom",
        json!({
            "batch":[{"target":root.path().join("types_pkg.sv"),"symbols":"types_pkg::increment"}],
            "contextLines":1,
        }),
        true,
    );
    assert_eq!(reply["success"], true, "{reply:#}");
    let text = reply["text"].as_str().unwrap();
    assert!(text.contains("return value + 1'b1;"), "{text}");
    assert!(!text.contains("$display"), "{text}");

    let typedef = root.path().join("typedef.sv");
    fs::write(
        &typedef,
        "package p; typedef struct packed { logic first; logic second; } pair_t; endpackage\n",
    )
    .unwrap();
    let reply = request(&mut aft, "outline", json!({"target": typedef}), false);
    assert_eq!(reply["success"], true, "{reply:#}");
    let text = reply["text"].as_str().unwrap();
    assert!(
        text.contains("typedef struct packed { logic first; logic second; } pair_t;"),
        "{text}"
    );
}
