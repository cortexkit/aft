//! Integration tests for compound operations: add_derive, wrap_try_catch,
//! add_decorator, add_struct_tags through the binary protocol.

use std::fs;

use super::helpers::{fixture_path, AftProcess};

/// Helper: copy a fixture to a uniquely-named temp file for mutation testing.
fn temp_copy(fixture_name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let src = fixture_path(fixture_name);
    let dir = tempfile::tempdir().unwrap();

    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let (stem, ext) = fixture_name.rsplit_once('.').unwrap_or((fixture_name, ""));
    let unique = if ext.is_empty() {
        format!("{}_{}", stem, n)
    } else {
        format!("{}_{}.{}", stem, n, ext)
    };
    let dest = dir.path().join(unique);
    fs::copy(&src, &dest).unwrap();
    (dir, dest)
}

// ============================================================
// add_derive tests
// ============================================================

fn send_add_derive(
    aft: &mut AftProcess,
    id: &str,
    file: &str,
    target: &str,
    derives: &[&str],
) -> serde_json::Value {
    let params = serde_json::json!({
        "id": id,
        "command": "add_derive",
        "file": file,
        "target": target,
        "derives": derives,
    });
    aft.send(&serde_json::to_string(&params).unwrap())
}

#[test]
fn add_derive_append_to_existing() {
    let (_dir, tmp) = temp_copy("structure_rs.rs");
    let file = tmp.to_str().unwrap();
    let mut aft = AftProcess::spawn();

    let resp = send_add_derive(&mut aft, "d1", file, "User", &["Clone"]);
    assert_eq!(resp["success"], true, "response: {:?}", resp);
    assert_eq!(resp["syntax_valid"], true);

    let content = fs::read_to_string(&tmp).unwrap();
    assert!(
        content.contains("#[derive(Debug, Clone)]"),
        "Expected derive to be appended. Content:\n{}",
        content
    );

    // Verify the struct is still intact
    assert!(content.contains("pub struct User"));
    aft.shutdown();
}

#[test]
fn add_derive_create_new() {
    let (_dir, tmp) = temp_copy("structure_rs.rs");
    let file = tmp.to_str().unwrap();
    let mut aft = AftProcess::spawn();

    // Config has no derive attribute
    let resp = send_add_derive(&mut aft, "d2", file, "Config", &["Debug", "Clone"]);
    assert_eq!(resp["success"], true, "response: {:?}", resp);
    assert_eq!(resp["syntax_valid"], true);

    let content = fs::read_to_string(&tmp).unwrap();
    assert!(
        content.contains("#[derive(Debug, Clone)]\npub struct Config"),
        "Expected new derive attribute. Content:\n{}",
        content
    );
    aft.shutdown();
}

#[test]
fn add_derive_dedup_existing() {
    let (_dir, tmp) = temp_copy("structure_rs.rs");
    let file = tmp.to_str().unwrap();
    let mut aft = AftProcess::spawn();

    // User already has Debug — adding Debug + Clone should not duplicate Debug
    let resp = send_add_derive(&mut aft, "d3", file, "User", &["Debug", "Clone"]);
    assert_eq!(resp["success"], true, "response: {:?}", resp);

    let derives: Vec<String> = resp["derives"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(derives, vec!["Debug", "Clone"]);

    let content = fs::read_to_string(&tmp).unwrap();
    // The derive attribute for User should have Debug exactly once
    assert!(
        content.contains("#[derive(Debug, Clone)]"),
        "Expected merged derive. Content:\n{}",
        content
    );
    // Verify no doubled Debug in the User derive
    assert!(
        !content.contains("#[derive(Debug, Debug"),
        "Debug should not be duplicated in the derive"
    );
    aft.shutdown();
}

#[test]
fn add_derive_enum() {
    let (_dir, tmp) = temp_copy("structure_rs.rs");
    let file = tmp.to_str().unwrap();
    let mut aft = AftProcess::spawn();

    // Status has #[derive(Debug, Serialize)] — add Clone
    let resp = send_add_derive(&mut aft, "d4", file, "Status", &["Clone"]);
    assert_eq!(resp["success"], true, "response: {:?}", resp);
    assert_eq!(resp["syntax_valid"], true);

    let content = fs::read_to_string(&tmp).unwrap();
    assert!(
        content.contains("#[derive(Debug, Serialize, Clone)]"),
        "Expected Clone appended to enum derive. Content:\n{}",
        content
    );
    aft.shutdown();
}

#[test]
fn add_derive_target_not_found() {
    let (_dir, tmp) = temp_copy("structure_rs.rs");
    let file = tmp.to_str().unwrap();
    let mut aft = AftProcess::spawn();

    let resp = send_add_derive(&mut aft, "d5", file, "Nonexistent", &["Debug"]);
    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "target_not_found");
    aft.shutdown();
}

// ============================================================
// wrap_try_catch tests
// ============================================================

fn send_wrap_try_catch(
    aft: &mut AftProcess,
    id: &str,
    file: &str,
    target: &str,
    catch_body: Option<&str>,
) -> serde_json::Value {
    let mut params = serde_json::json!({
        "id": id,
        "command": "wrap_try_catch",
        "file": file,
        "target": target,
    });
    if let Some(cb) = catch_body {
        params["catch_body"] = serde_json::json!(cb);
    }
    aft.send(&serde_json::to_string(&params).unwrap())
}

#[test]
fn wrap_try_catch_simple_function() {
    let (_dir, tmp) = temp_copy("structure_ts.ts");
    let file = tmp.to_str().unwrap();
    let mut aft = AftProcess::spawn();

    let resp = send_wrap_try_catch(&mut aft, "w1", file, "processData", None);
    assert_eq!(resp["success"], true, "response: {:?}", resp);
    assert_eq!(resp["syntax_valid"], true);

    let content = fs::read_to_string(&tmp).unwrap();
    assert!(
        content.contains("try {"),
        "Expected try block in content:\n{}",
        content
    );
    assert!(content.contains("catch (error)"), "Expected catch block");
    assert!(
        content.contains("throw error;"),
        "Expected default catch body"
    );
    // Original body should still be present (re-indented)
    assert!(
        content.contains("results.push(item.toUpperCase())"),
        "Body should be preserved"
    );
    aft.shutdown();
}

#[test]
fn wrap_try_catch_method_in_class() {
    let (_dir, tmp) = temp_copy("structure_ts.ts");
    let file = tmp.to_str().unwrap();
    let mut aft = AftProcess::spawn();

    let resp = send_wrap_try_catch(
        &mut aft,
        "w2",
        file,
        "fetch",
        Some("console.error(error);\nthrow error;"),
    );
    assert_eq!(resp["success"], true, "response: {:?}", resp);
    assert_eq!(resp["syntax_valid"], true);

    let content = fs::read_to_string(&tmp).unwrap();
    assert!(content.contains("try {"), "Expected try block");
    assert!(
        content.contains("console.error(error);"),
        "Expected custom catch body"
    );
    aft.shutdown();
}

#[test]
fn wrap_try_catch_target_not_found() {
    let (_dir, tmp) = temp_copy("structure_ts.ts");
    let file = tmp.to_str().unwrap();
    let mut aft = AftProcess::spawn();

    let resp = send_wrap_try_catch(&mut aft, "w3", file, "nonexistent", None);
    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "target_not_found");
    aft.shutdown();
}

#[test]
fn wrap_try_catch_custom_catch_body() {
    let (_dir, tmp) = temp_copy("structure_ts.ts");
    let file = tmp.to_str().unwrap();
    let mut aft = AftProcess::spawn();

    let resp = send_wrap_try_catch(&mut aft, "w4", file, "processData", Some("return [];"));
    assert_eq!(resp["success"], true, "response: {:?}", resp);

    let content = fs::read_to_string(&tmp).unwrap();
    assert!(content.contains("return [];"), "Expected custom catch body");
    aft.shutdown();
}

// ============================================================
// add_decorator tests
// ============================================================

fn send_add_decorator(
    aft: &mut AftProcess,
    id: &str,
    file: &str,
    target: &str,
    decorator: &str,
    position: Option<&str>,
) -> serde_json::Value {
    let mut params = serde_json::json!({
        "id": id,
        "command": "add_decorator",
        "file": file,
        "target": target,
        "decorator": decorator,
    });
    if let Some(pos) = position {
        params["position"] = serde_json::json!(pos);
    }
    aft.send(&serde_json::to_string(&params).unwrap())
}

#[test]
fn add_decorator_to_plain_function() {
    let (_dir, tmp) = temp_copy("structure_py.py");
    let file = tmp.to_str().unwrap();
    let mut aft = AftProcess::spawn();

    let resp = send_add_decorator(&mut aft, "dec1", file, "plain_function", "cache", None);
    assert_eq!(resp["success"], true, "response: {:?}", resp);
    assert_eq!(resp["syntax_valid"], true);

    let content = fs::read_to_string(&tmp).unwrap();
    assert!(
        content.contains("@cache\ndef plain_function"),
        "Expected decorator before function. Content:\n{}",
        content
    );
    aft.shutdown();
}

#[test]
fn add_decorator_to_decorated_function_first() {
    let (_dir, tmp) = temp_copy("structure_py.py");
    let file = tmp.to_str().unwrap();
    let mut aft = AftProcess::spawn();

    // get_users already has @app.route("/users") — add @login_required as first
    let resp = send_add_decorator(
        &mut aft,
        "dec2",
        file,
        "get_users",
        "login_required",
        Some("first"),
    );
    assert_eq!(resp["success"], true, "response: {:?}", resp);
    assert_eq!(resp["syntax_valid"], true);

    let content = fs::read_to_string(&tmp).unwrap();
    // @login_required should appear before @app.route("/users")
    let login_pos = content.find("@login_required\n@app.route(\"/users\")");
    assert!(
        login_pos.is_some(),
        "Expected @login_required before @app.route. Content:\n{}",
        content
    );
    aft.shutdown();
}

#[test]
fn add_decorator_to_decorated_function_last() {
    let (_dir, tmp) = temp_copy("structure_py.py");
    let file = tmp.to_str().unwrap();
    let mut aft = AftProcess::spawn();

    // get_users already has @app.route("/users") — add @cache as last (just before def)
    let resp = send_add_decorator(&mut aft, "dec3", file, "get_users", "cache", Some("last"));
    assert_eq!(resp["success"], true, "response: {:?}", resp);
    assert_eq!(resp["syntax_valid"], true);

    let content = fs::read_to_string(&tmp).unwrap();
    // @cache should appear after @app.route and before def
    assert!(
        content.contains("@app.route(\"/users\")\n@cache\ndef get_users"),
        "Expected @cache between existing decorator and def. Content:\n{}",
        content
    );
    aft.shutdown();
}

#[test]
fn add_decorator_preserves_indentation() {
    let (_dir, tmp) = temp_copy("structure_py.py");
    let file = tmp.to_str().unwrap();
    let mut aft = AftProcess::spawn();

    // helper is inside MyService class and already has @staticmethod
    let resp = send_add_decorator(&mut aft, "dec4", file, "helper", "cache", Some("first"));
    assert_eq!(resp["success"], true, "response: {:?}", resp);
    assert_eq!(resp["syntax_valid"], true);

    let content = fs::read_to_string(&tmp).unwrap();
    // Should have 4-space indent for the decorator inside a class
    assert!(
        content.contains("    @cache\n    @staticmethod"),
        "Expected indented decorator. Content:\n{}",
        content
    );
    aft.shutdown();
}

#[test]
fn add_decorator_target_not_found() {
    let (_dir, tmp) = temp_copy("structure_py.py");
    let file = tmp.to_str().unwrap();
    let mut aft = AftProcess::spawn();

    let resp = send_add_decorator(&mut aft, "dec5", file, "nonexistent", "cache", None);
    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "target_not_found");
    aft.shutdown();
}

// ============================================================
// add_struct_tags tests
// ============================================================

fn send_add_struct_tags(
    aft: &mut AftProcess,
    id: &str,
    file: &str,
    target: &str,
    field: &str,
    tag: &str,
    value: &str,
) -> serde_json::Value {
    let params = serde_json::json!({
        "id": id,
        "command": "add_struct_tags",
        "file": file,
        "target": target,
        "field": field,
        "tag": tag,
        "value": value,
    });
    aft.send(&serde_json::to_string(&params).unwrap())
}

#[test]
fn add_struct_tags_no_existing_tag() {
    let (_dir, tmp) = temp_copy("structure_go.go");
    let file = tmp.to_str().unwrap();
    let mut aft = AftProcess::spawn();

    let resp = send_add_struct_tags(&mut aft, "t1", file, "User", "Name", "json", "name");
    assert_eq!(resp["success"], true, "response: {:?}", resp);
    assert_eq!(resp["syntax_valid"], true);

    let content = fs::read_to_string(&tmp).unwrap();
    assert!(
        content.contains("Name    string `json:\"name\"`"),
        "Expected tag added to field. Content:\n{}",
        content
    );
    aft.shutdown();
}

#[test]
fn add_struct_tags_with_existing_tags() {
    let (_dir, tmp) = temp_copy("structure_go.go");
    let file = tmp.to_str().unwrap();
    let mut aft = AftProcess::spawn();

    // Email already has `json:"email"` — add xml tag
    let resp = send_add_struct_tags(&mut aft, "t2", file, "User", "Email", "xml", "email");
    assert_eq!(resp["success"], true, "response: {:?}", resp);
    assert_eq!(resp["syntax_valid"], true);

    let content = fs::read_to_string(&tmp).unwrap();
    assert!(
        content.contains(r#"`json:"email" xml:"email"`"#),
        "Expected both tags. Content:\n{}",
        content
    );
    aft.shutdown();
}

#[test]
fn add_struct_tags_update_existing_value() {
    let (_dir, tmp) = temp_copy("structure_go.go");
    let file = tmp.to_str().unwrap();
    let mut aft = AftProcess::spawn();

    // Email has `json:"email"` — update json value
    let resp = send_add_struct_tags(
        &mut aft,
        "t3",
        file,
        "User",
        "Email",
        "json",
        "email_addr,omitempty",
    );
    assert_eq!(resp["success"], true, "response: {:?}", resp);

    let content = fs::read_to_string(&tmp).unwrap();
    assert!(
        content.contains(r#"`json:"email_addr,omitempty"`"#),
        "Expected updated tag value. Content:\n{}",
        content
    );
    // Should NOT still have the old value
    assert!(
        !content.contains(r#"`json:"email"`"#),
        "Old tag value should be replaced"
    );
    aft.shutdown();
}

#[test]
fn add_struct_tags_preserves_other_keys() {
    let (_dir, tmp) = temp_copy("structure_go.go");
    let file = tmp.to_str().unwrap();
    let mut aft = AftProcess::spawn();

    // Address has `json:"address" xml:"address"` — add yaml tag
    let resp = send_add_struct_tags(&mut aft, "t4", file, "User", "Address", "yaml", "address");
    assert_eq!(resp["success"], true, "response: {:?}", resp);

    let content = fs::read_to_string(&tmp).unwrap();
    // All three tags should be present
    assert!(content.contains(r#"json:"address""#), "json tag preserved");
    assert!(content.contains(r#"xml:"address""#), "xml tag preserved");
    assert!(content.contains(r#"yaml:"address""#), "yaml tag added");
    aft.shutdown();
}

#[test]
fn add_struct_tags_struct_not_found() {
    let (_dir, tmp) = temp_copy("structure_go.go");
    let file = tmp.to_str().unwrap();
    let mut aft = AftProcess::spawn();

    let resp = send_add_struct_tags(&mut aft, "t5", file, "Nonexistent", "Name", "json", "name");
    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "target_not_found");
    aft.shutdown();
}

#[test]
fn add_struct_tags_field_not_found() {
    let (_dir, tmp) = temp_copy("structure_go.go");
    let file = tmp.to_str().unwrap();
    let mut aft = AftProcess::spawn();

    let resp = send_add_struct_tags(&mut aft, "t6", file, "User", "Missing", "json", "x");
    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "field_not_found");
    aft.shutdown();
}
