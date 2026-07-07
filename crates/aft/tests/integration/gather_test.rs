//! Integration tests for the `gather` command.

use crate::helpers::AftProcess;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn configure_project(aft: &mut AftProcess, root: &Path) {
    let resp = aft.send(&format!(
        r#"{{"id":"configure","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root.display())
    ));
    assert_eq!(resp["success"], true, "configure should succeed: {resp:?}");
}

#[test]
fn gather_symbol_mode_hides_and_includes_test_callers() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("src/__tests__")).unwrap();
    fs::write(
        root.join("src/target.ts"),
        r#"export function target(): number {
  return 1;
}

export function smallTarget(): number {
  return 2;
}
"#,
    )
    .unwrap();

    for idx in 0..20 {
        fs::write(
            root.join(format!("src/caller{idx:02}.ts")),
            format!(
                r#"import {{ target }} from "./target";

export function caller{idx:02}(): number {{
  return target();
}}
"#
            ),
        )
        .unwrap();
    }
    for idx in 0..5 {
        fs::write(
            root.join(format!("src/__tests__/caller{idx:02}.test.ts")),
            format!(
                r#"import {{ target, smallTarget }} from "../target";

export function aaaTestCaller{idx:02}(): number {{
  return target() + smallTarget();
}}
"#
            ),
        )
        .unwrap();
    }
    for idx in 0..3 {
        fs::write(
            root.join(format!("src/small_caller{idx:02}.ts")),
            format!(
                r#"import {{ smallTarget }} from "./target";

export function smallCaller{idx:02}(): number {{
  return smallTarget();
}}
"#
            ),
        )
        .unwrap();
    }

    let mut aft = AftProcess::spawn();
    configure_project(&mut aft, root);

    let target_path = root.join("src/target.ts");
    let target_path_json = crate::helpers::json_string(&target_path.display());
    let default_response = aft.send(&format!(
        r#"{{"id":"gather-default","command":"gather","symbol":"target","filePath":{} }}"#,
        target_path_json
    ));
    assert_eq!(
        default_response["success"], true,
        "default gather should succeed: {default_response:?}"
    );
    let default_text = default_response["text"]
        .as_str()
        .unwrap_or_else(|| panic!("default gather response missing text: {default_response:?}"));
    assert!(
        !default_text.contains("__tests__"),
        "default gather should hide test callers; response: {default_response:?}"
    );

    let include_response = aft.send(&format!(
        r#"{{"id":"gather-with-tests","command":"gather","symbol":"target","filePath":{},"includeTests":true }}"#,
        crate::helpers::json_string(&target_path.display())
    ));
    assert_eq!(
        include_response["success"], true,
        "includeTests gather should succeed: {include_response:?}"
    );
    let include_text = include_response["text"].as_str().unwrap_or_else(|| {
        panic!("includeTests gather response missing text: {include_response:?}")
    });
    assert!(
        include_text.contains("__tests__"),
        "includeTests gather should show test callers; response: {include_response:?}"
    );

    aft.shutdown();
}
