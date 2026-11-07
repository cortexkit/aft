use serde_json::json;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

use super::helpers::AftProcess;

fn setup_project(files: &[(&str, &str)]) -> TempDir {
    let temp_dir = crate::test_helpers::tempdir();

    for (relative_path, content) in files {
        let path = temp_dir.path().join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directories");
        }
        fs::write(path, content).expect("write fixture file");
    }

    temp_dir
}

fn configure(aft: &mut AftProcess, root: &Path) {
    let resp = aft.configure(root);
    assert_eq!(resp["success"], true, "configure should succeed: {resp:?}");
}

fn send(aft: &mut AftProcess, request: serde_json::Value) -> serde_json::Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

#[test]
fn test_r_outline_zoom_and_extensions() {
    let project = setup_project(&[
        (
            "analysis.R",
            r#"
# Calculate totals for a data frame.
summarise <- function(data, column) {
  total <- sum(data[[column]])
  total
}

scale_values = function(values) {
  values / max(values)
}

function(x) {
  x + 1
} -> increment

threshold <- 10
label = "ready"
"#,
        ),
        ("lowercase.r", "lowercase_fn <- function(x) { x }\n"),
    ]);

    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let file_path = project.path().join("analysis.R");
    let outline_resp = send(
        &mut aft,
        json!({
            "id": "outline-r",
            "command": "outline",
            "file": file_path,
        }),
    );

    assert_eq!(
        outline_resp["success"], true,
        "outline should succeed: {outline_resp:?}"
    );
    let text = outline_resp["text"].as_str().expect("outline text");
    for expected in [
        "analysis.R",
        "summarise",
        "scale_values",
        "increment",
        "threshold",
        "label",
    ] {
        assert!(
            text.contains(expected),
            "missing {expected} in outline: {text}"
        );
    }
    // Signature lines carry the declaration text itself rather than a kind
    // column, so function coverage is asserted on the R function syntax.
    assert!(
        text.contains("function("),
        "functions should show their function(...) signatures: {text}"
    );
    // Assignments likewise render as their own source text.
    assert!(
        text.contains("threshold <- 10"),
        "assignments should show their assignment signatures: {text}"
    );

    let zoom_resp = send(
        &mut aft,
        json!({
            "id": "zoom-r",
            "command": "zoom",
            "file": project.path().join("analysis.R"),
            "symbol": "summarise",
        }),
    );

    assert_eq!(
        zoom_resp["success"], true,
        "zoom should succeed: {zoom_resp:?}"
    );
    assert_eq!(zoom_resp["name"], "summarise");
    assert_eq!(zoom_resp["kind"], "function");
    let content = zoom_resp["content"].as_str().expect("zoom content");
    assert!(
        content.contains("summarise <- function(data, column)") && content.contains("total <- sum"),
        "zoom content should contain function body: {content}"
    );

    let lowercase_outline = send(
        &mut aft,
        json!({
            "id": "outline-r-lowercase",
            "command": "outline",
            "file": project.path().join("lowercase.r"),
        }),
    );
    assert_eq!(
        lowercase_outline["success"], true,
        "lowercase .r should be detected: {lowercase_outline:?}"
    );
    assert!(
        lowercase_outline["text"]
            .as_str()
            .unwrap_or("")
            .contains("lowercase_fn"),
        "lowercase .r outline should include function: {lowercase_outline:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_r_ast_grep_search_and_replace_with_meta_variables() {
    let project = setup_project(&[(
        "analysis.R",
        r#"
summarise <- function(values) {
  result <- sum(values)
  result
}

other <- function(values) {
  result <- sum(values + 1)
  result
}
"#,
    )]);

    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let search_resp = send(
        &mut aft,
        json!({
            "id": "search-r",
            "command": "ast_search",
            "pattern": "$NAME <- sum($VALUES)",
            "lang": "r",
        }),
    );

    assert_eq!(
        search_resp["success"], true,
        "ast_search should succeed: {search_resp:?}"
    );
    assert_eq!(
        search_resp["total_matches"], 2,
        "R meta-var pattern must not silently match zero: {search_resp:?}"
    );
    let first_name = search_resp["matches"][0]["meta_variables"]["$NAME"]
        .as_str()
        .expect("captured $NAME");
    assert_eq!(first_name, "result");

    let replace_resp = send(
        &mut aft,
        json!({
            "id": "replace-r",
            "command": "ast_replace",
            "pattern": "$NAME <- sum($VALUES)",
            "rewrite": "$NAME <- mean($VALUES)",
            "lang": "r",
            "dry_run": false,
        }),
    );

    assert_eq!(
        replace_resp["success"], true,
        "ast_replace should succeed: {replace_resp:?}"
    );
    assert_eq!(replace_resp["total_replacements"], 2);

    let updated_content = fs::read_to_string(project.path().join("analysis.R")).unwrap();
    assert!(
        updated_content.contains("result <- mean(values)"),
        "content should rewrite simple sum call: {updated_content}"
    );
    assert!(
        updated_content.contains("result <- mean(values + 1)"),
        "content should rewrite captured expression: {updated_content}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}
