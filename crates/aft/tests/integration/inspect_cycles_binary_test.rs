use std::fs;

use serde_json::{json, Value};

use super::helpers::AftProcess;

fn inspect_cycles(files: &[(&str, &str)]) -> Value {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");
    let storage = temp.path().join("storage");
    fs::create_dir_all(&project).expect("create project");
    for (relative, source) in files {
        let path = project.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create source directory");
        }
        fs::write(path, source).expect("write source fixture");
    }

    let mut aft = AftProcess::spawn();
    let configured = aft.send(
        &json!({
            "id": "configure-cycles",
            "command": "configure",
            "harness": "opencode",
            "project_root": project,
            "storage_dir": storage,
        })
        .to_string(),
    );
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:#}"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let response = loop {
        let response = aft.send(
            &json!({
                "id": "inspect-cycles",
                "command": "inspect",
                "sections": ["cycles"],
                "topK": 20,
            })
            .to_string(),
        );
        assert_eq!(response["success"], true, "inspect failed: {response:#}");
        if response["inspect_terminal"] == "fresh" {
            break response;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "cycle inspection did not become fresh: {response:#}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert_cycle_scan_complete(&response);
    assert!(aft.shutdown().success());
    response
}

fn cycle_details(response: &Value) -> &[Value] {
    response["details"]["cycles"]
        .as_array()
        .unwrap_or_else(|| panic!("missing cycle details: {response:#}"))
}

fn assert_cycle_scan_complete(response: &Value) {
    assert_eq!(
        response["success"], true,
        "a successful inspect response must be complete: {response:#}"
    );
    assert_eq!(
        response["inspect_terminal"], "fresh",
        "successful cycle inspection must reach the fresh terminal: {response:#}"
    );
}

fn cycle_named<'a>(response: &'a Value, cycle: &str) -> &'a Value {
    cycle_details(response)
        .iter()
        .find(|item| item["cycle"] == cycle)
        .unwrap_or_else(|| panic!("missing cycle {cycle:?}: {response:#}"))
}

#[test]
fn inspect_binary_reports_direct_self_import_cycle() {
    let response = inspect_cycles(&[("self.ts", "import \"./self\";\nexport const value = 1;\n")]);

    assert_eq!(response["summary"]["cycles"]["count"], 1);
    assert_eq!(response["summary"]["cycles"]["largest"], 1);
    let cycle = cycle_named(&response, "self.ts -> self.ts");
    assert_eq!(cycle["size"], 1);
    assert_eq!(cycle["files"], json!(["self.ts"]));
    assert_eq!(cycle["chain"], json!(["self.ts", "self.ts"]));
    assert_eq!(cycle["edge_kind"], "static");
    assert_eq!(cycle["edges"][0]["from"], "self.ts");
    assert_eq!(cycle["edges"][0]["to"], "self.ts");
    assert_eq!(cycle["edges"][0]["edge_kind"], "static");
    assert_eq!(cycle["edges"][0]["imports"][0]["specifier"], "./self");
    assert_eq!(
        cycle["edges"][0]["imports"][0]["kind"],
        "import::SideEffect"
    );
    assert!(response["text"]
        .as_str()
        .is_some_and(|text| text.contains("largest: 1 file")));
}

#[test]
fn inspect_binary_reports_dynamic_self_import_kind() {
    let response = inspect_cycles(&[(
        "dynamic_self.ts",
        "export const load = () => import('./dynamic_self');\n",
    )]);

    assert_eq!(response["summary"]["cycles"]["count"], 1);
    assert_eq!(response["summary"]["cycles"]["largest"], 1);
    let cycle = cycle_named(&response, "dynamic_self.ts -> dynamic_self.ts");
    assert_eq!(cycle["edge_kind"], "dynamic-only");
    assert_eq!(cycle["edges"][0]["edge_kind"], "dynamic-only");
    assert_eq!(
        cycle["edges"][0]["imports"][0]["specifier"],
        "./dynamic_self"
    );
    assert_eq!(cycle["edges"][0]["imports"][0]["kind"], "dynamic_import");
}

#[test]
fn inspect_binary_reports_self_and_multifile_cycles_together() {
    let response = inspect_cycles(&[
        ("self.ts", "import \"./self\";\nexport const value = 1;\n"),
        (
            "a.ts",
            "import { b } from './b';\nexport const a = b + 1;\n",
        ),
        (
            "b.ts",
            "import { a } from './a';\nexport const b = a + 1;\n",
        ),
    ]);

    assert_eq!(response["summary"]["cycles"]["count"], 2);
    assert_eq!(response["summary"]["cycles"]["largest"], 2);
    assert_eq!(cycle_details(&response).len(), 2);
    cycle_named(&response, "a.ts -> b.ts -> a.ts");
    cycle_named(&response, "self.ts -> self.ts");
}

#[test]
fn inspect_binary_excludes_type_only_self_import() {
    let response = inspect_cycles(&[(
        "self_type.ts",
        "import type { SelfType as ImportedSelf } from './self_type';\nexport type SelfType = { next?: ImportedSelf };\n",
    )]);

    assert_eq!(response["summary"]["cycles"]["count"], 0);
    assert_eq!(response["summary"]["cycles"]["largest"], 0);
    assert!(cycle_details(&response).is_empty());
}

#[test]
fn inspect_binary_excludes_singleton_without_self_edge() {
    let response = inspect_cycles(&[("plain.ts", "export const value = 1;\n")]);

    assert_eq!(response["summary"]["cycles"]["count"], 0);
    assert_eq!(response["summary"]["cycles"]["largest"], 0);
    assert!(cycle_details(&response).is_empty());
}
