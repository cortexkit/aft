//! Integration tests for the add_member command through the binary protocol.

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

/// Helper: send an add_member request and return the response.
fn send_add_member(
    aft: &mut AftProcess,
    id: &str,
    file: &str,
    scope: &str,
    code: &str,
    position: Option<&str>,
) -> serde_json::Value {
    let mut params = serde_json::json!({
        "id": id,
        "command": "add_member",
        "file": file,
        "scope": scope,
        "code": code,
    });

    if let Some(pos) = position {
        params["position"] = serde_json::json!(pos);
    }

    aft.send(&serde_json::to_string(&params).unwrap())
}

// --- TS tests ---

#[test]
fn add_member_ts_class_last() {
    let (_dir, tmp) = temp_copy("member_ts.ts");
    let mut aft = AftProcess::spawn();

    let resp = send_add_member(
        &mut aft,
        "1",
        tmp.to_str().unwrap(),
        "UserService",
        "farewell(): string {\n  return `Goodbye, ${this.name}`;\n}",
        None, // default "last"
    );

    assert_eq!(resp["success"], true, "response: {:?}", resp);
    assert_eq!(resp["scope"], "UserService");
    assert_eq!(resp["syntax_valid"], true);

    let content = fs::read_to_string(&tmp).unwrap();
    assert!(
        content.contains("farewell()"),
        "should contain farewell method: {}",
        content
    );
    // Method should be indented (2 spaces for TS)
    assert!(
        content.contains("  farewell()"),
        "farewell should be indented with 2 spaces: {}",
        content
    );
}

#[test]
fn add_member_ts_class_first() {
    let (_dir, tmp) = temp_copy("member_ts.ts");
    let mut aft = AftProcess::spawn();

    let resp = send_add_member(
        &mut aft,
        "1",
        tmp.to_str().unwrap(),
        "UserService",
        "id: number;",
        Some("first"),
    );

    assert_eq!(resp["success"], true, "response: {:?}", resp);
    assert_eq!(resp["syntax_valid"], true);

    let content = fs::read_to_string(&tmp).unwrap();
    assert!(
        content.contains("id: number;"),
        "should contain id field: {}",
        content
    );
    // id should appear before name
    let id_pos = content.find("id: number;").unwrap();
    let name_pos = content.find("name: string;").unwrap();
    assert!(
        id_pos < name_pos,
        "id should appear before name: id@{}, name@{}",
        id_pos,
        name_pos
    );
}

#[test]
fn add_member_ts_after_name() {
    let (_dir, tmp) = temp_copy("member_ts.ts");
    let mut aft = AftProcess::spawn();

    let resp = send_add_member(
        &mut aft,
        "1",
        tmp.to_str().unwrap(),
        "UserService",
        "getId(): number {\n  return 42;\n}",
        Some("after:constructor"),
    );

    assert_eq!(resp["success"], true, "response: {:?}", resp);
    assert_eq!(resp["syntax_valid"], true);

    let content = fs::read_to_string(&tmp).unwrap();
    assert!(
        content.contains("getId()"),
        "should contain getId method: {}",
        content
    );
    // getId should appear after constructor but before greet
    let get_id_pos = content.find("getId()").unwrap();
    let constructor_pos = content.find("constructor(").unwrap();
    let greet_pos = content.find("greet()").unwrap();
    assert!(
        get_id_pos > constructor_pos,
        "getId should be after constructor"
    );
    assert!(get_id_pos < greet_pos, "getId should be before greet");
}

#[test]
fn add_member_ts_empty_class() {
    let (_dir, tmp) = temp_copy("member_ts.ts");
    let mut aft = AftProcess::spawn();

    let resp = send_add_member(
        &mut aft,
        "1",
        tmp.to_str().unwrap(),
        "EmptyClass",
        "doSomething(): void {}",
        None,
    );

    assert_eq!(resp["success"], true, "response: {:?}", resp);
    assert_eq!(resp["syntax_valid"], true);

    let content = fs::read_to_string(&tmp).unwrap();
    assert!(
        content.contains("doSomething()"),
        "should contain doSomething: {}",
        content
    );
}

// --- Python tests ---

#[test]
fn add_member_py_class_last() {
    let (_dir, tmp) = temp_copy("member_py.py");
    let mut aft = AftProcess::spawn();

    let resp = send_add_member(
        &mut aft,
        "1",
        tmp.to_str().unwrap(),
        "UserService",
        "def farewell(self) -> str:\n    return f\"Goodbye, {self.name}\"",
        None,
    );

    assert_eq!(resp["success"], true, "response: {:?}", resp);
    assert_eq!(resp["syntax_valid"], true);

    let content = fs::read_to_string(&tmp).unwrap();
    assert!(
        content.contains("def farewell"),
        "should contain farewell method: {}",
        content
    );
    // Check indentation: method should be at 4-space indent
    assert!(
        content.contains("    def farewell"),
        "farewell should be indented with 4 spaces: {}",
        content
    );
}

#[test]
fn add_member_py_indentation_matches() {
    let (_dir, tmp) = temp_copy("member_py.py");
    let mut aft = AftProcess::spawn();

    let resp = send_add_member(
        &mut aft,
        "1",
        tmp.to_str().unwrap(),
        "UserService",
        "def farewell(self) -> str:\n    return f\"Goodbye, {self.name}\"",
        None,
    );

    assert_eq!(resp["success"], true, "response: {:?}", resp);

    let content = fs::read_to_string(&tmp).unwrap();

    // The existing __init__ is at 4-space indent, farewell should match
    for line in content.lines() {
        if line.contains("def farewell") {
            let leading: String = line.chars().take_while(|c| *c == ' ').collect();
            assert_eq!(
                leading.len(),
                4,
                "farewell should have 4-space indent, got {}: '{}'",
                leading.len(),
                line
            );
        }
    }
}

// --- Rust tests ---

#[test]
fn add_member_rs_struct_field() {
    let (_dir, tmp) = temp_copy("member_rs.rs");
    let mut aft = AftProcess::spawn();

    // Use EmptyStruct which has no impl block — struct is the only match
    let resp = send_add_member(
        &mut aft,
        "1",
        tmp.to_str().unwrap(),
        "EmptyStruct",
        "pub enabled: bool,",
        None, // last
    );

    assert_eq!(resp["success"], true, "response: {:?}", resp);
    assert_eq!(resp["syntax_valid"], true);

    let content = fs::read_to_string(&tmp).unwrap();
    assert!(
        content.contains("pub enabled: bool"),
        "should contain enabled field: {}",
        content
    );
    // Should be indented at 4 spaces
    assert!(
        content.contains("    pub enabled: bool"),
        "enabled should have 4-space indent: {}",
        content
    );
}

#[test]
fn add_member_rs_impl_method() {
    let (_dir, tmp) = temp_copy("member_rs.rs");
    let mut aft = AftProcess::spawn();

    // Config has both struct and impl — impl is preferred
    let resp = send_add_member(
        &mut aft,
        "1",
        tmp.to_str().unwrap(),
        "Config",
        "pub fn is_valid(&self) -> bool {\n    !self.name.is_empty()\n}",
        None,
    );

    assert_eq!(resp["success"], true, "response: {:?}", resp);
    assert_eq!(resp["syntax_valid"], true);

    let content = fs::read_to_string(&tmp).unwrap();
    assert!(
        content.contains("is_valid"),
        "should contain is_valid method: {}",
        content
    );
    // Should be inside the impl block
    assert!(
        content.contains("    pub fn is_valid"),
        "is_valid should have 4-space indent: {}",
        content
    );
}

// --- Go tests ---

#[test]
fn add_member_go_struct_field() {
    let (_dir, tmp) = temp_copy("member_go.go");
    let mut aft = AftProcess::spawn();

    let resp = send_add_member(
        &mut aft,
        "1",
        tmp.to_str().unwrap(),
        "Config",
        "Enabled bool",
        None, // last
    );

    assert_eq!(resp["success"], true, "response: {:?}", resp);
    assert_eq!(resp["syntax_valid"], true);

    let content = fs::read_to_string(&tmp).unwrap();
    assert!(
        content.contains("Enabled bool"),
        "should contain Enabled field: {}",
        content
    );
}

#[test]
fn add_member_go_empty_struct() {
    let (_dir, tmp) = temp_copy("member_go.go");
    let mut aft = AftProcess::spawn();

    let resp = send_add_member(
        &mut aft,
        "1",
        tmp.to_str().unwrap(),
        "EmptyStruct",
        "Name string",
        None,
    );

    assert_eq!(resp["success"], true, "response: {:?}", resp);
    assert_eq!(resp["syntax_valid"], true);

    let content = fs::read_to_string(&tmp).unwrap();
    assert!(
        content.contains("Name string"),
        "should contain Name field: {}",
        content
    );
}

// --- Error cases ---

#[test]
fn add_member_scope_not_found() {
    let (_dir, tmp) = temp_copy("member_ts.ts");
    let mut aft = AftProcess::spawn();

    let resp = send_add_member(
        &mut aft,
        "1",
        tmp.to_str().unwrap(),
        "NonExistent",
        "foo(): void {}",
        None,
    );

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "scope_not_found");
    // Error message should include available scopes
    let msg = resp["message"].as_str().unwrap();
    assert!(
        msg.contains("UserService") || msg.contains("EmptyClass"),
        "error should list available scopes: {}",
        msg
    );
}

#[test]
fn add_member_member_not_found() {
    let (_dir, tmp) = temp_copy("member_ts.ts");
    let mut aft = AftProcess::spawn();

    let resp = send_add_member(
        &mut aft,
        "1",
        tmp.to_str().unwrap(),
        "UserService",
        "foo(): void {}",
        Some("after:nonExistentMethod"),
    );

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "member_not_found");
}

#[test]
fn add_member_file_not_found() {
    let mut aft = AftProcess::spawn();

    let resp = send_add_member(
        &mut aft,
        "1",
        "/tmp/definitely_not_a_real_file_abc123.ts",
        "Foo",
        "bar(): void {}",
        None,
    );

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "file_not_found");
}

#[test]
fn add_member_missing_params() {
    let mut aft = AftProcess::spawn();

    // Missing scope
    let resp = aft.send(r#"{"id":"1","command":"add_member","file":"/tmp/x.ts","code":"x"}"#);
    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "invalid_request");

    // Missing code
    let resp = aft.send(r#"{"id":"2","command":"add_member","file":"/tmp/x.ts","scope":"Foo"}"#);
    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "invalid_request");
}
