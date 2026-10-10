//! Integration tests for the add_import command through the binary protocol.

use super::helpers::{fixture_path, AftProcess};
use aft::imports::{generate_import_line_with_namespace, parse_file_imports};
use aft::parser::LangId;
use std::fs;
use std::path::Path;
use std::process::Output;

/// Execute an ES module with Node so runtime import semantics, not only syntax, are checked.
fn assert_node_succeeds(file: &Path, context: &str) -> Output {
    let output = std::process::Command::new("node")
        .arg(file)
        .output()
        .expect("Node.js is required for JSON module import regression tests");
    assert!(
        output.status.success(),
        "{context}: Node exited {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Node 24 no longer parses legacy `assert` clauses. Execute a temporary `with`
/// spelling of the same clause to adjudicate module semantics without changing
/// the fixture whose legacy spelling the import engine must preserve.
fn assert_node_import_semantics_succeed(file: &Path, context: &str) -> Output {
    let source = fs::read_to_string(file).unwrap();
    if !source.contains(" assert {") {
        return assert_node_succeeds(file, context);
    }

    let runtime_file = file.with_extension("node-with.mjs");
    fs::write(&runtime_file, source.replacen(" assert {", " with {", 1)).unwrap();
    let output = assert_node_succeeds(&runtime_file, context);
    fs::remove_file(runtime_file).unwrap();
    output
}

/// Execute an attributed ES module and require Node to reject its binding shape.
/// Legacy `assert` clauses are rewritten only in the temporary runtime copy so
/// the import engine's preservation of that spelling remains under test.
fn assert_node_import_semantics_fail(file: &Path, context: &str) -> Output {
    let source = fs::read_to_string(file).unwrap();
    let (runtime_file, runtime_source) = if source.contains(" assert {") {
        (
            Some(file.with_extension("node-with.mjs")),
            source.replacen(" assert {", " with {", 1),
        )
    } else {
        (None, source)
    };
    if let Some(runtime_file) = &runtime_file {
        fs::write(runtime_file, runtime_source).unwrap();
    }
    let path = runtime_file.as_deref().unwrap_or(file);
    let output = std::process::Command::new("node")
        .arg(path)
        .output()
        .expect("Node.js is required for JSON module import regression tests");
    if let Some(runtime_file) = runtime_file {
        fs::remove_file(runtime_file).unwrap();
    }
    assert!(
        !output.status.success(),
        "{context}: Node unexpectedly accepted an invalid import shape"
    );
    output
}

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

/// Helper: send an add_import request and return the response.
fn send_add_import(
    aft: &mut AftProcess,
    id: &str,
    file: &str,
    module: &str,
    names: Option<&[&str]>,
    default_import: Option<&str>,
    type_only: bool,
) -> serde_json::Value {
    let mut params = serde_json::json!({
        "id": id,
        "command": "add_import",
        "file": file,
        "module": module,
    });

    if let Some(names) = names {
        params["names"] = serde_json::json!(names);
    }
    if let Some(def) = default_import {
        params["default_import"] = serde_json::json!(def);
    }
    if type_only {
        params["type_only"] = serde_json::json!(true);
    }

    aft.send(&serde_json::to_string(&params).unwrap())
}

fn send_add_namespace_import(
    aft: &mut AftProcess,
    id: &str,
    file: &str,
    module: &str,
    namespace: &str,
) -> serde_json::Value {
    let params = serde_json::json!({
        "id": id,
        "command": "add_import",
        "file": file,
        "module": module,
        "namespace": namespace,
    });
    aft.send(&serde_json::to_string(&params).unwrap())
}

// --- TS tests ---

#[test]
fn add_import_ts_external_group() {
    let mut aft = AftProcess::spawn();
    let (_dir, file) = temp_copy("imports_ts.ts");
    let file_str = file.display().to_string();

    let resp = send_add_import(
        &mut aft,
        "imp-1",
        &file_str,
        "lodash",
        Some(&["debounce"]),
        None,
        false,
    );

    assert_eq!(
        resp["success"], true,
        "add_import should succeed: {:?}",
        resp
    );
    assert_eq!(resp["added"], true);
    assert_eq!(resp["group"], "external");

    // Verify the import was added to the file
    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("import { debounce } from 'lodash';"),
        "should contain the new import. content:\n{}",
        content
    );

    // Verify it's in the external group (before relative imports)
    let lodash_pos = content.find("import { debounce } from 'lodash'").unwrap();
    let relative_pos = content.find("import { helper } from './utils'").unwrap();
    assert!(
        lodash_pos < relative_pos,
        "lodash import should be before relative imports"
    );

    // Syntax should be valid
    assert_eq!(
        resp["syntax_valid"], true,
        "syntax should be valid after add, resp: {:?}",
        resp
    );

    // Cleanup
    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn add_import_ts_relative_group() {
    let mut aft = AftProcess::spawn();
    let (_dir, file) = temp_copy("imports_ts.ts");
    let file_str = file.display().to_string();

    let resp = send_add_import(
        &mut aft,
        "imp-2",
        &file_str,
        "./components",
        Some(&["Button"]),
        None,
        false,
    );

    assert_eq!(
        resp["success"], true,
        "add_import should succeed: {:?}",
        resp
    );
    assert_eq!(resp["added"], true);
    assert_eq!(resp["group"], "internal");

    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("import { Button } from './components';"),
        "should contain the new relative import. content:\n{}",
        content
    );

    // Verify it's in the relative group (after external imports)
    let button_pos = content
        .find("import { Button } from './components'")
        .unwrap();
    let react_pos = content.find("import React from 'react'").unwrap();
    assert!(
        button_pos > react_pos,
        "relative import should be after external imports"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn add_import_ts_allows_parent_relative_module() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("parent_relative.ts");
    fs::write(&file, "export const x = 1;\n").unwrap();

    let resp = send_add_import(
        &mut aft,
        "imp-parent-relative",
        &file.display().to_string(),
        "../config",
        Some(&["Config"]),
        None,
        false,
    );

    assert_eq!(
        resp["success"], true,
        "relative add should succeed: {resp:?}"
    );
    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("import { Config } from \"../config\";"),
        "single-parent ES relative imports must remain allowed:\n{content}"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn add_import_c_allows_parent_relative_include() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("relative_include.c");
    fs::write(&file, "int main(void) { return 0; }\n").unwrap();

    let resp = send_add_import(
        &mut aft,
        "imp-c-parent-relative",
        &file.display().to_string(),
        "\"../foo.h\"",
        None,
        None,
        false,
    );

    assert_eq!(
        resp["success"], true,
        "parent-relative C include should succeed: {resp:?}"
    );
    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("#include \"../foo.h\""),
        "C include should preserve the parent-relative local path:\n{content}"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn add_import_solidity_allows_parent_relative_import() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("RelativeImport.sol");
    fs::write(
        &file,
        "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.0;\n\ncontract C {}\n",
    )
    .unwrap();

    let resp = send_add_import(
        &mut aft,
        "imp-sol-parent-relative",
        &file.display().to_string(),
        "../lib/X.sol",
        None,
        None,
        false,
    );

    assert_eq!(
        resp["success"], true,
        "parent-relative Solidity import should succeed: {resp:?}"
    );
    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("import \"../lib/X.sol\";"),
        "Solidity import should preserve the parent-relative path:\n{content}"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn add_import_solidity_dedupes_single_quoted_path() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("SingleQuotedImport.sol");
    let original = "pragma solidity ^0.8.0;\nimport './X.sol';\ncontract C {}\n";
    fs::write(&file, original).unwrap();

    let resp = send_add_import(
        &mut aft,
        "imp-sol-single-quote-dedup",
        &file.display().to_string(),
        "./X.sol",
        None,
        None,
        false,
    );

    assert_eq!(resp["success"], true, "dedup should succeed: {resp:?}");
    assert_eq!(
        resp["added"], false,
        "the existing single-quoted import should satisfy the request: {resp:?}"
    );
    assert_eq!(resp["already_present"], true, "expected a dedup no-op");
    assert_eq!(fs::read_to_string(&file).unwrap(), original);

    aft.shutdown();
}

#[test]
fn add_import_lua_dedup_only_recognizes_pure_require_declarations() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();

    let pure_file = dir.path().join("pure.lua");
    let pure_original = "local pure = require(\"module\")\n";
    fs::write(&pure_file, pure_original).unwrap();
    let pure = send_add_import(
        &mut aft,
        "add-lua-pure-dedup",
        &pure_file.display().to_string(),
        "module",
        None,
        Some("pure"),
        false,
    );
    assert_eq!(pure["success"], true, "pure add should succeed: {pure:?}");
    assert_eq!(pure["added"], false, "pure require should dedup: {pure:?}");
    assert_eq!(pure["already_present"], true);
    assert_eq!(fs::read_to_string(&pure_file).unwrap(), pure_original);

    let mixed_file = dir.path().join("mixed.lua");
    let mixed_original = "local mixed = require(\"module\"), keep()\n";
    fs::write(&mixed_file, mixed_original).unwrap();
    let mixed = send_add_import(
        &mut aft,
        "add-lua-mixed-not-dedup",
        &mixed_file.display().to_string(),
        "module",
        None,
        Some("mixed"),
        false,
    );
    assert_eq!(
        mixed["success"], true,
        "mixed-RHS add should succeed: {mixed:?}"
    );
    assert_eq!(
        mixed["added"], true,
        "mixed-RHS declaration must not satisfy dedup: {mixed:?}"
    );
    assert_eq!(
        fs::read_to_string(&mixed_file).unwrap(),
        format!("local mixed = require(\"module\")\n\n{mixed_original}")
    );

    aft.shutdown();
}

#[test]
fn add_import_c_rejects_absolute_modules() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("unsafe_include.c");
    let original = "int main(void) { return 0; }\n";
    fs::write(&file, original).unwrap();
    let file_str = file.display().to_string();

    for (id, module) in [
        ("imp-c-posix-absolute", "/etc/passwd"),
        ("imp-c-drive-absolute", "C:\\evil"),
        ("imp-c-unc-absolute", "\\\\srv\\x"),
    ] {
        let resp = send_add_import(&mut aft, id, &file_str, module, None, None, false);
        assert_eq!(
            resp["success"], false,
            "absolute module {module:?} should be rejected: {resp:?}"
        );
        assert_eq!(resp["code"], "invalid_request");
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            original,
            "rejected module {module:?} must not mutate the file"
        );
    }

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn add_import_java_and_php_reject_filesystem_modules() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();

    let java_file = dir.path().join("Unsafe.java");
    let java_original = "package demo;\n\nclass Unsafe {}\n";
    fs::write(&java_file, java_original).unwrap();
    let java_file_str = java_file.display().to_string();

    for (id, module) in [
        ("imp-java-parent", "../evil"),
        ("imp-java-slash", "/evil"),
        ("imp-java-drive", "C:\\evil"),
    ] {
        let resp = send_add_import(&mut aft, id, &java_file_str, module, None, None, false);
        assert_eq!(
            resp["success"], false,
            "Java filesystem module {module:?} should be rejected: {resp:?}"
        );
        assert_eq!(resp["code"], "invalid_request");
        assert_eq!(fs::read_to_string(&java_file).unwrap(), java_original);
    }

    let php_file = dir.path().join("unsafe.php");
    let php_original = "<?php\n\nnamespace Demo;\n\nclass C {}\n";
    fs::write(&php_file, php_original).unwrap();
    let php_file_str = php_file.display().to_string();

    for (id, module) in [
        ("imp-php-parent", "..\\Evil"),
        ("imp-php-slash", "/tmp/Evil"),
        ("imp-php-drive", "C:\\evil"),
    ] {
        let resp = send_add_import(&mut aft, id, &php_file_str, module, None, None, false);
        assert_eq!(
            resp["success"], false,
            "PHP filesystem module {module:?} should be rejected: {resp:?}"
        );
        assert_eq!(resp["code"], "invalid_request");
        assert_eq!(fs::read_to_string(&php_file).unwrap(), php_original);
    }

    fs::remove_file(&java_file).ok();
    fs::remove_file(&php_file).ok();
    aft.shutdown();
}

#[test]
fn add_import_ts_dedup() {
    let mut aft = AftProcess::spawn();
    let (_dir, file) = temp_copy("imports_ts.ts");
    let file_str = file.display().to_string();

    // Try to add useState which already exists in the fixture
    let resp = send_add_import(
        &mut aft,
        "imp-3",
        &file_str,
        "react",
        Some(&["useState"]),
        None,
        false,
    );

    assert_eq!(resp["success"], true);
    assert_eq!(resp["added"], false, "should not add duplicate");
    assert_eq!(resp["already_present"], true);

    // File should not have been modified
    let original = fs::read_to_string(fixture_path("imports_ts.ts")).unwrap();
    let current = fs::read_to_string(&file).unwrap();
    assert_eq!(
        original, current,
        "file should not have been modified for duplicate"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn add_import_ts_alphabetizes() {
    let mut aft = AftProcess::spawn();
    let (_dir, file) = temp_copy("imports_ts.ts");
    let file_str = file.display().to_string();

    // Add 'axios' which should sort before 'react' and after nothing (first external)
    let resp = send_add_import(
        &mut aft,
        "imp-4",
        &file_str,
        "axios",
        None,
        Some("axios"),
        false,
    );

    assert_eq!(resp["success"], true);
    assert_eq!(resp["added"], true);

    let content = fs::read_to_string(&file).unwrap();
    let axios_pos = content.find("import axios from 'axios'").unwrap();
    let react_pos = content.find("import React from 'react'").unwrap();
    assert!(
        axios_pos < react_pos,
        "axios should sort before react alphabetically. content:\n{}",
        content
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

// --- JS tests ---

#[test]
fn add_import_js_works() {
    let mut aft = AftProcess::spawn();
    let (_dir, file) = temp_copy("imports_js.js");
    let file_str = file.display().to_string();

    let resp = send_add_import(
        &mut aft,
        "imp-5",
        &file_str,
        "cors",
        None,
        Some("cors"),
        false,
    );

    assert_eq!(
        resp["success"], true,
        "add_import on JS should succeed: {:?}",
        resp
    );
    assert_eq!(resp["added"], true);
    assert_eq!(resp["group"], "external");

    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("import cors from 'cors';"),
        "should contain the new JS import. content:\n{}",
        content
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

// --- Edge cases ---

#[test]
fn add_import_empty_file() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static EMPTY_COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let n = EMPTY_COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = dir.path().join(format!("empty_{}.ts", n));
    fs::write(&file, "").unwrap();
    let file_str = file.display().to_string();

    let resp = send_add_import(
        &mut aft,
        "imp-6",
        &file_str,
        "react",
        Some(&["useState"]),
        None,
        false,
    );

    assert_eq!(
        resp["success"], true,
        "add_import on empty file should succeed: {:?}",
        resp
    );
    assert_eq!(resp["added"], true);

    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("import { useState } from \"react\";"),
        "should contain the import at top. content:\n{}",
        content
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn add_import_missing_file_returns_error() {
    let mut aft = AftProcess::spawn();

    let resp = send_add_import(
        &mut aft,
        "imp-7",
        "/tmp/nonexistent_aft_test.ts",
        "react",
        Some(&["useState"]),
        None,
        false,
    );

    assert_eq!(resp["success"], false, "should fail for missing file");
    assert_eq!(resp["code"], "file_not_found");

    aft.shutdown();
}

#[test]
fn add_import_unsupported_language_returns_error() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static UNSUP_COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let n = UNSUP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = dir.path().join(format!("test_{}.txt", n));
    fs::write(&file, "hello world").unwrap();
    let file_str = file.display().to_string();

    let resp = send_add_import(
        &mut aft,
        "imp-8",
        &file_str,
        "react",
        Some(&["useState"]),
        None,
        false,
    );

    assert_eq!(
        resp["success"], false,
        "should fail for unsupported language"
    );
    assert_eq!(
        resp["code"], "unsupported_language",
        "unsupported file type uses the actionable standardized code, not invalid_request"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn add_import_missing_params_returns_error() {
    let mut aft = AftProcess::spawn();

    // Missing 'module' param
    let resp = aft.send(r#"{"id":"imp-9","command":"add_import","file":"/tmp/test.ts"}"#);

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "invalid_request");
    assert!(
        resp["message"].as_str().unwrap().contains("module"),
        "error should mention missing 'module' param"
    );

    aft.shutdown();
}

// --- Python tests ---

#[test]
fn add_import_py_stdlib_group() {
    let mut aft = AftProcess::spawn();
    let (_dir, file) = temp_copy("imports_py.py");
    let file_str = file.display().to_string();

    let resp = send_add_import(
        &mut aft,
        "py-1",
        &file_str,
        "pathlib",
        Some(&["Path"]),
        None,
        false,
    );

    assert_eq!(
        resp["success"], true,
        "add_import py stdlib should succeed: {:?}",
        resp
    );
    assert_eq!(resp["added"], true);
    assert_eq!(resp["group"], "stdlib");

    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("from pathlib import Path"),
        "should contain the new stdlib import. content:\n{}",
        content
    );

    // Verify it's in the stdlib group (before third-party imports)
    let pathlib_pos = content.find("from pathlib import Path").unwrap();
    let requests_pos = content.find("import requests").unwrap();
    assert!(
        pathlib_pos < requests_pos,
        "stdlib import should be before third-party imports"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn add_import_py_third_party_group() {
    let mut aft = AftProcess::spawn();
    let (_dir, file) = temp_copy("imports_py.py");
    let file_str = file.display().to_string();

    let resp = send_add_import(&mut aft, "py-2", &file_str, "click", None, None, false);

    assert_eq!(
        resp["success"], true,
        "add_import py third-party should succeed: {:?}",
        resp
    );
    assert_eq!(resp["added"], true);
    assert_eq!(resp["group"], "external");

    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("import click"),
        "should contain the new third-party import. content:\n{}",
        content
    );

    // Verify it's in the external group (after stdlib, before local)
    let click_pos = content.find("import click").unwrap();
    let os_pos = content.find("import os").unwrap();
    let utils_pos = content.find("from . import utils").unwrap();
    assert!(
        click_pos > os_pos,
        "third-party import should be after stdlib"
    );
    assert!(
        click_pos < utils_pos,
        "third-party import should be before local"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn add_import_py_local_group() {
    let mut aft = AftProcess::spawn();
    let (_dir, file) = temp_copy("imports_py.py");
    let file_str = file.display().to_string();

    let resp = send_add_import(
        &mut aft,
        "py-3",
        &file_str,
        ".models",
        Some(&["User"]),
        None,
        false,
    );

    assert_eq!(
        resp["success"], true,
        "add_import py local should succeed: {:?}",
        resp
    );
    assert_eq!(resp["added"], true);
    assert_eq!(resp["group"], "internal");

    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("from .models import User"),
        "should contain the new local import. content:\n{}",
        content
    );

    // Verify it's in the internal group (after third-party)
    let models_pos = content.find("from .models import User").unwrap();
    let requests_pos = content.find("import requests").unwrap();
    assert!(
        models_pos > requests_pos,
        "local import should be after third-party"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn add_import_py_dedup() {
    let mut aft = AftProcess::spawn();
    let (_dir, file) = temp_copy("imports_py.py");
    let file_str = file.display().to_string();

    // Try to add 'os' which already exists
    let resp = send_add_import(&mut aft, "py-4", &file_str, "os", None, None, false);

    assert_eq!(resp["success"], true);
    assert_eq!(resp["added"], false, "should not add duplicate");
    assert_eq!(resp["already_present"], true);

    fs::remove_file(&file).ok();
    aft.shutdown();
}

// --- Rust tests ---

#[test]
fn add_import_rs_std_group() {
    let mut aft = AftProcess::spawn();
    let (_dir, file) = temp_copy("imports_rs.rs");
    let file_str = file.display().to_string();

    let resp = send_add_import(
        &mut aft,
        "rs-1",
        &file_str,
        "std::fmt::Display",
        None,
        None,
        false,
    );

    assert_eq!(
        resp["success"], true,
        "add_import rs std should succeed: {:?}",
        resp
    );
    assert_eq!(resp["added"], true);
    assert_eq!(resp["group"], "stdlib");

    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("use std::fmt::Display;"),
        "should contain the new std import. content:\n{}",
        content
    );

    // Verify it's in the stdlib group (before external imports)
    let fmt_pos = content.find("use std::fmt::Display;").unwrap();
    let serde_pos = content.find("use serde").unwrap();
    assert!(
        fmt_pos < serde_pos,
        "std import should be before external imports"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn add_import_rs_external_group() {
    let mut aft = AftProcess::spawn();
    let (_dir, file) = temp_copy("imports_rs.rs");
    let file_str = file.display().to_string();

    let resp = send_add_import(
        &mut aft,
        "rs-2",
        &file_str,
        "anyhow::Result",
        None,
        None,
        false,
    );

    assert_eq!(
        resp["success"], true,
        "add_import rs external should succeed: {:?}",
        resp
    );
    assert_eq!(resp["added"], true);
    assert_eq!(resp["group"], "external");

    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("use anyhow::Result;"),
        "should contain the new external import. content:\n{}",
        content
    );

    // Should be in external group (after std, before crate)
    let anyhow_pos = content.find("use anyhow::Result;").unwrap();
    let std_pos = content.find("use std::").unwrap();
    let crate_pos = content.find("use crate::").unwrap();
    assert!(anyhow_pos > std_pos, "external import should be after std");
    assert!(
        anyhow_pos < crate_pos,
        "external import should be before crate"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn add_import_rs_dedup() {
    let mut aft = AftProcess::spawn();
    let (_dir, file) = temp_copy("imports_rs.rs");
    let file_str = file.display().to_string();

    // Try to add std::collections::HashMap which already exists
    let resp = send_add_import(
        &mut aft,
        "rs-3",
        &file_str,
        "std::collections::HashMap",
        None,
        None,
        false,
    );

    assert_eq!(resp["success"], true);
    assert_eq!(resp["added"], false, "should not add duplicate");
    assert_eq!(resp["already_present"], true);

    fs::remove_file(&file).ok();
    aft.shutdown();
}

// --- Go tests ---

#[test]
fn add_import_go_stdlib_group() {
    let mut aft = AftProcess::spawn();
    let (_dir, file) = temp_copy("imports_go.go");
    let file_str = file.display().to_string();

    let resp = send_add_import(&mut aft, "go-1", &file_str, "net/http", None, None, false);

    assert_eq!(
        resp["success"], true,
        "add_import go stdlib should succeed: {:?}",
        resp
    );
    assert_eq!(resp["added"], true);
    assert_eq!(resp["group"], "stdlib");

    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("\"net/http\""),
        "should contain the new stdlib import. content:\n{}",
        content
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn add_import_go_external_group() {
    let mut aft = AftProcess::spawn();
    let (_dir, file) = temp_copy("imports_go.go");
    let file_str = file.display().to_string();

    let resp = send_add_import(
        &mut aft,
        "go-2",
        &file_str,
        "golang.org/x/tools",
        None,
        None,
        false,
    );

    assert_eq!(
        resp["success"], true,
        "add_import go external should succeed: {:?}",
        resp
    );
    assert_eq!(resp["added"], true);
    assert_eq!(resp["group"], "external");

    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("\"golang.org/x/tools\""),
        "should contain the new external import. content:\n{}",
        content
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn add_import_go_dedup() {
    let mut aft = AftProcess::spawn();
    let (_dir, file) = temp_copy("imports_go.go");
    let file_str = file.display().to_string();

    // Try to add "fmt" which already exists
    let resp = send_add_import(&mut aft, "go-3", &file_str, "fmt", None, None, false);

    assert_eq!(resp["success"], true);
    assert_eq!(resp["added"], false, "should not add duplicate");
    assert_eq!(resp["already_present"], true);

    fs::remove_file(&file).ok();
    aft.shutdown();
}

// ===========================================================================
// remove_import tests
// ===========================================================================

/// Helper: send a remove_import request and return the response.
fn send_remove_import(
    aft: &mut AftProcess,
    id: &str,
    file: &str,
    module: &str,
    name: Option<&str>,
) -> serde_json::Value {
    let mut params = serde_json::json!({
        "id": id,
        "command": "remove_import",
        "file": file,
        "module": module,
    });

    if let Some(n) = name {
        params["name"] = serde_json::json!(n);
    }

    aft.send(&serde_json::to_string(&params).unwrap())
}

/// Helper: send an organize_imports request and return the response.
fn send_organize_imports(aft: &mut AftProcess, id: &str, file: &str) -> serde_json::Value {
    let params = serde_json::json!({
        "id": id,
        "command": "organize_imports",
        "file": file,
    });

    aft.send(&serde_json::to_string(&params).unwrap())
}

#[test]
fn remove_import_entire_statement_ts() {
    let mut aft = AftProcess::spawn();
    let (_dir, file) = temp_copy("imports_ts.ts");
    let file_str = file.display().to_string();

    // Remove the 'zod' import entirely
    let resp = send_remove_import(&mut aft, "rm-1", &file_str, "zod", None);

    assert_eq!(
        resp["success"], true,
        "remove_import should succeed: {:?}",
        resp
    );
    assert_eq!(resp["removed"], true);
    assert_eq!(resp["module"], "zod");

    let content = fs::read_to_string(&file).unwrap();
    assert!(
        !content.contains("from 'zod'"),
        "zod import should be removed. content:\n{}",
        content
    );
    // Other imports should remain
    assert!(
        content.contains("from 'react'"),
        "react imports should remain"
    );

    assert_eq!(resp["syntax_valid"], true);

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn remove_import_specific_name_from_multi_ts() {
    let mut aft = AftProcess::spawn();
    let (_dir, file) = temp_copy("imports_ts.ts");
    let file_str = file.display().to_string();

    // Remove 'useState' from `import { useState, useEffect } from 'react';`
    let resp = send_remove_import(&mut aft, "rm-2", &file_str, "react", Some("useState"));

    assert_eq!(
        resp["success"], true,
        "remove_import should succeed: {:?}",
        resp
    );
    assert_eq!(resp["removed"], true);
    assert_eq!(resp["name"], "useState");

    let content = fs::read_to_string(&file).unwrap();
    // Should still have useEffect from react
    assert!(
        content.contains("useEffect") && content.contains("react"),
        "useEffect import from react should remain. content:\n{}",
        content
    );
    // useState should not appear in that specific import anymore
    assert!(
        !content.contains("import { useState, useEffect }"),
        "the original multi-name import should be modified. content:\n{}",
        content
    );

    assert_eq!(resp["syntax_valid"], true);

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn remove_import_missing_module_reports_not_removed() {
    let mut aft = AftProcess::spawn();
    let (_dir, file) = temp_copy("imports_ts.ts");
    let file_str = file.display().to_string();

    let resp = send_remove_import(&mut aft, "rm-3", &file_str, "nonexistent-module", None);

    assert_eq!(resp["success"], true, "request should complete: {resp:?}");
    assert_eq!(resp["removed"], false, "nothing should be removed");
    assert_eq!(resp["reason"], "module_not_found");
    assert_eq!(resp["no_op"], true, "no-match removes must report no_op");

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn remove_import_missing_name_reports_no_op() {
    let mut aft = AftProcess::spawn();
    let (_dir, file) = temp_copy("imports_ts.ts");
    let file_str = file.display().to_string();

    let resp = send_remove_import(
        &mut aft,
        "rm-missing-name",
        &file_str,
        "react",
        Some("useMemo"),
    );

    assert_eq!(resp["success"], true, "request should complete: {resp:?}");
    assert_eq!(resp["removed"], false, "nothing should be removed");
    assert_eq!(resp["reason"], "name_not_found");
    assert_eq!(resp["name"], "useMemo");
    assert_eq!(resp["no_op"], true, "name misses must report no_op");

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn remove_import_preserves_default_when_named_removed() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(format!(
        "remove_default_named_{}.ts",
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    fs::write(
        &file,
        "import React, { useState } from 'react';\n\nexport const App = React.Fragment;\n",
    )
    .unwrap();

    let resp = send_remove_import(
        &mut aft,
        "rm-preserve-default",
        &file.display().to_string(),
        "react",
        Some("useState"),
    );
    assert_eq!(resp["success"], true, "remove should succeed: {resp:?}");
    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("import React from 'react';"),
        "default import should remain:\n{content}"
    );
    assert!(
        !content.contains("useState"),
        "named import should be removed"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn remove_import_solidity_matches_single_quoted_path() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("RemoveSingleQuotedImport.sol");
    fs::write(
        &file,
        "pragma solidity ^0.8.0;\nimport './X.sol';\ncontract C {}\n",
    )
    .unwrap();

    let remove = send_remove_import(
        &mut aft,
        "remove-sol-single-quote",
        &file.display().to_string(),
        "./X.sol",
        None,
    );

    assert_eq!(remove["success"], true, "remove should succeed: {remove:?}");
    assert_eq!(
        remove["removed"], true,
        "the single-quoted import should be removed: {remove:?}"
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "pragma solidity ^0.8.0;\ncontract C {}\n"
    );

    aft.shutdown();
}

#[test]
fn remove_import_lua_ignores_mixed_rhs_but_removes_pure_require() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();

    let mixed_file = dir.path().join("mixed.lua");
    let mixed_original = "local victim = require(\"victim\"), keep()\nprint(\"after\")\n";
    fs::write(&mixed_file, mixed_original).unwrap();
    let mixed = send_remove_import(
        &mut aft,
        "remove-lua-mixed-rhs",
        &mixed_file.display().to_string(),
        "victim",
        None,
    );
    assert_eq!(
        mixed["success"], true,
        "mixed-RHS remove should return a no-op: {mixed:?}"
    );
    assert_eq!(mixed["removed"], false);
    assert_eq!(mixed["reason"], "module_not_found");
    assert_eq!(mixed["no_op"], true);
    assert_eq!(fs::read_to_string(&mixed_file).unwrap(), mixed_original);

    let pure_file = dir.path().join("pure.lua");
    let pure_original = "local pure = require(\"pure\")\nprint(\"after\")\n";
    fs::write(&pure_file, pure_original).unwrap();
    let pure = send_remove_import(
        &mut aft,
        "remove-lua-pure",
        &pure_file.display().to_string(),
        "pure",
        None,
    );
    assert_eq!(
        pure["success"], true,
        "pure remove should succeed: {pure:?}"
    );
    assert_eq!(
        pure["removed"], true,
        "pure require should be removed: {pure:?}"
    );
    assert_eq!(
        fs::read_to_string(&pure_file).unwrap(),
        "print(\"after\")\n"
    );

    aft.shutdown();
}

#[test]
fn java_static_member_add_then_remove_round_trip() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("StaticRoundTrip.java");
    let original = "package com.example;\n\nimport java.util.List;\n\nclass C {}\n";
    fs::write(&file, original).unwrap();
    let file_str = file.display().to_string();

    let add = aft.send(
        &serde_json::json!({
            "id": "java-static-add",
            "command": "add_import",
            "file": file_str,
            "module": "java.util.Collections",
            "names": ["emptyList"],
            "modifiers": ["static"],
        })
        .to_string(),
    );
    assert_eq!(add["success"], true, "add should succeed: {add:?}");
    assert_eq!(add["added"], true, "member import should be added: {add:?}");
    assert!(
        fs::read_to_string(&file)
            .unwrap()
            .contains("import static java.util.Collections.emptyList;"),
        "add should generate the requested static member import"
    );

    let remove = send_remove_import(
        &mut aft,
        "java-static-remove",
        &file_str,
        "java.util.Collections",
        Some("emptyList"),
    );
    assert_eq!(remove["success"], true, "remove should succeed: {remove:?}");
    assert_eq!(
        remove["removed"], true,
        "member import should be removed: {remove:?}"
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), original);

    aft.shutdown();
}

#[test]
fn java_remove_module_removes_static_members_and_exact_import() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("RemoveStaticType.java");
    fs::write(
        &file,
        "package com.example;\n\nimport static java.util.Collections.emptyList;\nimport static java.util.Collections.singletonList;\nimport java.util.Collections;\nimport java.util.List;\n\nclass C {}\n",
    )
    .unwrap();
    let file_str = file.display().to_string();

    let remove = send_remove_import(
        &mut aft,
        "java-static-remove-type",
        &file_str,
        "java.util.Collections",
        None,
    );
    assert_eq!(remove["success"], true, "remove should succeed: {remove:?}");
    assert_eq!(
        remove["removed"], true,
        "imports should be removed: {remove:?}"
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "package com.example;\n\nimport java.util.List;\n\nclass C {}\n"
    );

    aft.shutdown();
}

#[test]
fn java_remove_static_member_keeps_sibling_and_wildcard() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("RemoveOneStaticMember.java");
    fs::write(
        &file,
        "package com.example;\n\nimport static java.util.Collections.*;\nimport static java.util.Collections.emptyList;\nimport static java.util.Collections.singletonList;\n\nclass C {}\n",
    )
    .unwrap();
    let file_str = file.display().to_string();

    let remove = send_remove_import(
        &mut aft,
        "java-static-remove-one",
        &file_str,
        "java.util.Collections",
        Some("emptyList"),
    );
    assert_eq!(remove["success"], true, "remove should succeed: {remove:?}");
    assert_eq!(
        remove["removed"], true,
        "member should be removed: {remove:?}"
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "package com.example;\n\nimport static java.util.Collections.*;\nimport static java.util.Collections.singletonList;\n\nclass C {}\n"
    );

    aft.shutdown();
}

// ===========================================================================
// organize_imports tests
// ===========================================================================

#[test]
fn organize_imports_without_imports_reports_no_op() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("no_imports.ts");
    let original = "export const x = 1;\n";
    fs::write(&file, original).unwrap();

    let resp = send_organize_imports(&mut aft, "org-no-imports", &file.display().to_string());

    assert_eq!(resp["success"], true, "organize should succeed: {resp:?}");
    assert_eq!(resp["groups"].as_array().unwrap().len(), 0);
    assert_eq!(resp["removed_duplicates"], 0);
    assert_eq!(resp["no_op"], true, "no-import organize must report no_op");
    assert_eq!(fs::read_to_string(&file).unwrap(), original);

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn organize_imports_rejects_multi_namespace_php_without_mutating() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("multi_namespace.php");
    let original = r#"<?php

namespace Foo {
use Zed\Last;

class X {}
}

namespace Bar {
use App\Alpha;

class Y {}
}
"#;
    fs::write(&file, original).unwrap();

    let resp = send_organize_imports(&mut aft, "org-multi-php", &file.display().to_string());

    assert_eq!(
        resp["success"], false,
        "multi-namespace PHP organize should be refused: {resp:?}"
    );
    assert_eq!(resp["code"], "multi_region_imports");
    assert!(
        resp["message"]
            .as_str()
            .unwrap_or_default()
            .contains("span multiple code regions"),
        "error should explain the multi-region refusal: {resp:?}"
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        original,
        "refused organize must leave the PHP file byte-for-byte unchanged"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn organize_imports_ts_regroups_and_sorts() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static ORG_COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let n = ORG_COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = dir.path().join(format!("organize_ts_{}.ts", n));

    // Write a scrambled import file
    fs::write(
        &file,
        "\
import { helper } from './utils';
import { z } from 'zod';
import React from 'react';
import { Config } from '../config';
import { useState } from 'react';

export function App() {}
",
    )
    .unwrap();

    let file_str = file.display().to_string();
    let resp = send_organize_imports(&mut aft, "org-1", &file_str);

    assert_eq!(
        resp["success"], true,
        "organize_imports should succeed: {:?}",
        resp
    );

    let content = fs::read_to_string(&file).unwrap();

    // External imports should come before internal
    let react_pos = content.find("react").unwrap();
    let utils_pos = content.find("./utils").unwrap();
    assert!(
        react_pos < utils_pos,
        "external imports should come before internal. content:\n{}",
        content
    );

    // Within external group, should be alphabetical: react before zod
    let zod_pos = content.find("zod").unwrap();
    assert!(
        react_pos < zod_pos,
        "react should come before zod (alphabetical). content:\n{}",
        content
    );

    assert_eq!(resp["syntax_valid"], true);

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn organize_imports_preserves_side_effect_order() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(format!(
        "organize_side_effects_{}.ts",
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    // All imports below are in the EXTERNAL group, so within-group ordering applies:
    //   side-effects (preserve relative source order) before value/type (alphabetical).
    fs::write(
        &file,
        "import 'polyfill-b';\nimport z from 'zod';\nimport 'polyfill-a';\nimport React from 'react';\n\nexport const x = 1;\n",
    )
    .unwrap();

    let resp = send_organize_imports(&mut aft, "org-side-effects", &file.display().to_string());
    assert_eq!(resp["success"], true, "organize should succeed: {resp:?}");
    let content = fs::read_to_string(&file).unwrap();
    let polyfill_b_pos = content.find("import 'polyfill-b';").unwrap();
    let polyfill_a_pos = content.find("import 'polyfill-a';").unwrap();
    let react_pos = content.find("import React from 'react';").unwrap();
    let zod_pos = content.find("import z from 'zod';").unwrap();
    assert!(
        polyfill_b_pos < polyfill_a_pos,
        "side-effect imports keep original relative order (b before a):\n{content}"
    );
    assert!(
        zod_pos < polyfill_a_pos,
        "value imports before a side-effect barrier must not cross it:\n{content}"
    );
    assert!(
        polyfill_a_pos < react_pos,
        "side-effects keep their source position relative to following value imports:\n{content}"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn organize_imports_preserves_c_and_cpp_textual_include_order() {
    let mut aft = AftProcess::spawn();

    for (extension, request_id) in [("c", "org-c-order"), ("cpp", "org-cpp-order")] {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("setup.h"), "#define FEATURE_READY 1\n").unwrap();
        fs::write(
            dir.path().join("reader.h"),
            "#ifdef FEATURE_READY\n#define RESULT 2\n#else\n#define RESULT 1\n#endif\n",
        )
        .unwrap();

        let file = dir.path().join(format!("main.{extension}"));
        let original = "#include \"setup.h\"\n\n#include <reader.h>\n#include <stdio.h>\n\nint observed = RESULT;\n";
        fs::write(&file, original).unwrap();

        let resp = send_organize_imports(&mut aft, request_id, &file.display().to_string());
        assert_eq!(resp["success"], true, "organize should succeed: {resp:?}");
        assert_eq!(resp["removed_duplicates"], 0);
        assert_eq!(resp["syntax_valid"], true);
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            original,
            "textual includes must retain source order for {extension}"
        );
    }

    aft.shutdown();
}

#[test]
fn organize_imports_python_still_sorts_and_deduplicates() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("sorting_control.py");
    fs::write(
        &file,
        "import requests\nimport os\nimport os\n\ndef main():\n    pass\n",
    )
    .unwrap();

    let resp = send_organize_imports(&mut aft, "org-python-control", &file.display().to_string());
    assert_eq!(resp["success"], true, "organize should succeed: {resp:?}");
    assert_eq!(resp["removed_duplicates"], 1);

    let content = fs::read_to_string(&file).unwrap();
    assert_eq!(content.matches("import os").count(), 1);
    assert!(
        content.find("import os").unwrap() < content.find("import requests").unwrap(),
        "Python must remain in the sorting path:\n{content}"
    );

    aft.shutdown();
}

#[test]
fn organize_imports_ruby_preserves_reexecuting_loads_but_deduplicates_requires() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();

    let load_file = dir.path().join("repeated_load.rb");
    fs::write(
        &load_file,
        "load 'worker.rb'\nload 'worker.rb'\n\nputs 'done'\n",
    )
    .unwrap();
    let load_resp =
        send_organize_imports(&mut aft, "org-ruby-load", &load_file.display().to_string());
    assert_eq!(
        load_resp["success"], true,
        "organize should succeed: {load_resp:?}"
    );
    let load_content = fs::read_to_string(&load_file).unwrap();
    assert_eq!(
        load_content.matches("load 'worker.rb'").count(),
        2,
        "each Ruby load must survive organization:\n{load_content}"
    );
    assert_eq!(load_resp["removed_duplicates"], 0);

    let require_file = dir.path().join("repeated_require.rb");
    fs::write(
        &require_file,
        "require 'worker'\nrequire 'worker'\n\nputs 'done'\n",
    )
    .unwrap();
    let require_resp = send_organize_imports(
        &mut aft,
        "org-ruby-require",
        &require_file.display().to_string(),
    );
    assert_eq!(
        require_resp["success"], true,
        "organize should succeed: {require_resp:?}"
    );
    let require_content = fs::read_to_string(&require_file).unwrap();
    assert_eq!(require_content.matches("require 'worker'").count(), 1);
    assert_eq!(require_resp["removed_duplicates"], 1);

    aft.shutdown();
}

#[test]
fn organize_imports_perl_preserves_reexecuting_use_and_no_but_deduplicates_require() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();

    let use_file = dir.path().join("repeated_use.pl");
    fs::write(&use_file, "use Probe;\nuse Probe;\n\nprint 'done';\n").unwrap();
    let use_resp = send_organize_imports(&mut aft, "org-perl-use", &use_file.display().to_string());
    assert_eq!(
        use_resp["success"], true,
        "organize should succeed: {use_resp:?}"
    );
    let use_content = fs::read_to_string(&use_file).unwrap();
    assert_eq!(
        use_content.matches("use Probe;").count(),
        2,
        "each Perl use must survive organization:\n{use_content}"
    );
    assert_eq!(use_resp["removed_duplicates"], 0);

    let no_file = dir.path().join("repeated_no.pl");
    fs::write(&no_file, "no Unimp;\nno Unimp;\n\nprint 'done';\n").unwrap();
    let no_resp = send_organize_imports(&mut aft, "org-perl-no", &no_file.display().to_string());
    assert_eq!(
        no_resp["success"], true,
        "organize should succeed: {no_resp:?}"
    );
    let no_content = fs::read_to_string(&no_file).unwrap();
    assert_eq!(
        no_content.matches("no Unimp;").count(),
        2,
        "each Perl no must survive organization:\n{no_content}"
    );
    assert_eq!(no_resp["removed_duplicates"], 0);

    let require_file = dir.path().join("repeated_require.pl");
    fs::write(
        &require_file,
        "require Probe;\nrequire Probe;\n\nprint 'done';\n",
    )
    .unwrap();
    let require_resp = send_organize_imports(
        &mut aft,
        "org-perl-require",
        &require_file.display().to_string(),
    );
    assert_eq!(
        require_resp["success"], true,
        "organize should succeed: {require_resp:?}"
    );
    let require_content = fs::read_to_string(&require_file).unwrap();
    assert_eq!(require_content.matches("require Probe;").count(), 1);
    assert_eq!(require_resp["removed_duplicates"], 1);

    aft.shutdown();
}

#[test]
fn organize_imports_preserves_inter_import_comments() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(format!(
        "organize_comment_gap_{}.ts",
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let original =
        "import A from 'a';\n// keep me\nimport B from 'b';\n\nexport const x = A || B;\n";
    fs::write(&file, original).unwrap();

    let resp = send_organize_imports(&mut aft, "org-comment-gap", &file.display().to_string());
    assert_eq!(resp["success"], true, "organize should succeed: {resp:?}");
    let content = fs::read_to_string(&file).unwrap();
    assert_eq!(content, original, "comment gap must be preserved");

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn add_remove_import_refuse_csharp_and_php_multi_region_imports() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);

    let cs_file = dir.path().join(format!("multi_region_{n}.cs"));
    fs::write(
        &cs_file,
        "namespace A {\nusing Common;\nclass A {}\n}\nnamespace B {\nusing Common;\nclass B {}\n}\n",
    )
    .unwrap();
    let cs_file_str = cs_file.display().to_string();
    let add_cs = send_add_import(
        &mut aft,
        "cs-multi-add",
        &cs_file_str,
        "System",
        None,
        None,
        false,
    );
    assert_eq!(add_cs["success"], false, "C# add should refuse: {add_cs:?}");
    assert_eq!(add_cs["code"], "multi_region_imports");
    let remove_cs = send_remove_import(&mut aft, "cs-multi-remove", &cs_file_str, "Common", None);
    assert_eq!(
        remove_cs["success"], false,
        "C# remove should refuse: {remove_cs:?}"
    );
    assert_eq!(remove_cs["code"], "multi_region_imports");

    let php_file = dir.path().join(format!("multi_region_{n}.php"));
    fs::write(
        &php_file,
        "<?php\nnamespace A {\nuse Common\\Thing;\nclass A {}\n}\nnamespace B {\nuse Common\\Thing;\nclass B {}\n}\n",
    )
    .unwrap();
    let php_file_str = php_file.display().to_string();
    let add_php = send_add_import(
        &mut aft,
        "php-multi-add",
        &php_file_str,
        "Other\\Thing",
        None,
        None,
        false,
    );
    assert_eq!(
        add_php["success"], false,
        "PHP add should refuse: {add_php:?}"
    );
    assert_eq!(add_php["code"], "multi_region_imports");
    let remove_php = send_remove_import(
        &mut aft,
        "php-multi-remove",
        &php_file_str,
        "Common\\Thing",
        None,
    );
    assert_eq!(
        remove_php["success"], false,
        "PHP remove should refuse: {remove_php:?}"
    );
    assert_eq!(remove_php["code"], "multi_region_imports");

    fs::remove_file(&cs_file).ok();
    fs::remove_file(&php_file).ok();
    aft.shutdown();
}

#[test]
fn php_grouped_use_refuses_memberwise_add_and_remove() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(format!(
        "php_grouped_use_{}.php",
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let original = "<?php\nuse App\\{Foo, Bar as Baz};\n\nclass C {}\n";
    fs::write(&file, original).unwrap();
    let file_str = file.display().to_string();

    let add_resp = send_add_import(
        &mut aft,
        "php-group-add",
        &file_str,
        "App\\Foo",
        None,
        None,
        false,
    );
    assert_eq!(
        add_resp["success"], false,
        "add should refuse: {add_resp:?}"
    );
    assert_eq!(add_resp["code"], "unsupported_grouped_import");

    let remove_resp = send_remove_import(&mut aft, "php-group-remove", &file_str, "App\\Foo", None);
    assert_eq!(
        remove_resp["success"], false,
        "remove should refuse: {remove_resp:?}"
    );
    assert_eq!(remove_resp["code"], "unsupported_grouped_import");
    assert_eq!(fs::read_to_string(&file).unwrap(), original);

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn php_remove_comma_separated_use_rewrites_physical_declaration() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        (
            "first",
            "<?php\nuse App\\Unused, App\\Keep;\n\nclass C { public Keep $keep; }\n",
            "App\\Unused",
            "<?php\nuse App\\Keep;\n\nclass C { public Keep $keep; }\n",
        ),
        (
            "middle",
            "<?php\nuse A, B, C;\n\nclass Example {}\n",
            "B",
            "<?php\nuse A, C;\n\nclass Example {}\n",
        ),
        (
            "only",
            "<?php\nuse A;\n\nclass Example {}\n",
            "A",
            "<?php\n\nclass Example {}\n",
        ),
        (
            "aliased-sibling",
            "<?php\nuse A as X, B;\n\nclass Example { public X $value; }\n",
            "B",
            "<?php\nuse A as X;\n\nclass Example { public X $value; }\n",
        ),
        (
            "function-kind",
            "<?php\nuse function App\\unused, App\\keep;\n\nkeep();\n",
            "App\\unused",
            "<?php\nuse function App\\keep;\n\nkeep();\n",
        ),
    ];

    for (case, input, module, expected) in cases {
        let file = dir.path().join(format!("php_multi_{case}.php"));
        fs::write(&file, input).unwrap();
        let file_str = file.display().to_string();
        let response = send_remove_import(
            &mut aft,
            &format!("php-multi-remove-{case}"),
            &file_str,
            module,
            None,
        );
        assert_eq!(response["success"], true, "remove failed: {response:?}");
        assert_eq!(
            response["removed"], true,
            "remove was a no-op: {response:?}"
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), expected, "case {case}");
    }

    aft.shutdown();
}

#[test]
fn php_add_deduplicates_comma_separated_sibling_and_organize_preserves_it() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("php_multi_dedup.php");
    let original = "<?php\nuse App\\First, App\\Keep;\n\nclass C { public Keep $keep; }\n";
    fs::write(&file, original).unwrap();
    let file_str = file.display().to_string();

    let add = send_add_import(
        &mut aft,
        "php-multi-dedup-add",
        &file_str,
        "App\\Keep",
        None,
        None,
        false,
    );
    assert_eq!(add["success"], true, "add failed: {add:?}");
    assert_eq!(add["added"], false, "duplicate sibling was added: {add:?}");
    assert_eq!(
        add["already_present"], true,
        "sibling was not visible: {add:?}"
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), original);

    let organize = send_organize_imports(&mut aft, "php-multi-dedup-organize", &file_str);
    assert_eq!(organize["success"], true, "organize failed: {organize:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), original);

    aft.shutdown();
}

#[test]
fn organize_imports_go_grouped_block_refuses_internal_comments() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(format!(
        "organize_go_grouped_comments_{}.go",
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let original = "package main\n\nimport (\n\t\"fmt\"\n\t// keep me with this block\n\t\"os\"\n)\n\nfunc main() {}\n";
    fs::write(&file, original).unwrap();

    let resp = send_organize_imports(
        &mut aft,
        "org-go-grouped-comments",
        &file.display().to_string(),
    );
    assert_eq!(
        resp["success"], false,
        "commented Go grouped imports should be refused: {resp:?}"
    );
    assert_eq!(resp["code"], "unsupported_import_comments");
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        original,
        "refused organize must leave the Go file byte-for-byte unchanged"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn organize_imports_go_grouped_block_parses() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(format!(
        "organize_go_grouped_{}.go",
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    fs::write(
        &file,
        "package main\n\nimport (\n\t\"os\"\n\t\"fmt\"\n)\n\nfunc main() {}\n",
    )
    .unwrap();

    let resp = send_organize_imports(&mut aft, "org-go-grouped", &file.display().to_string());
    assert_eq!(resp["success"], true, "organize should succeed: {resp:?}");
    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("import (\n\t\"fmt\"\n\t\"os\"\n)"),
        "grouped block should be regenerated and sorted:\n{content}"
    );
    let mut parser = tree_sitter::Parser::new();
    let language: tree_sitter::Language = tree_sitter_go::LANGUAGE.into();
    parser.set_language(&language).expect("set go grammar");
    let tree = parser.parse(&content, None).expect("parse go");
    assert!(
        !tree.root_node().has_error(),
        "Go output should parse:\n{content}"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn organize_imports_ts_deduplicates() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static DEDUP_COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let n = DEDUP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = dir.path().join(format!("organize_dedup_{}.ts", n));

    // Write a file with duplicate imports
    fs::write(
        &file,
        "\
import { z } from 'zod';
import { z } from 'zod';
import React from 'react';
import React from 'react';

export function App() {}
",
    )
    .unwrap();

    let file_str = file.display().to_string();
    let resp = send_organize_imports(&mut aft, "org-2", &file_str);

    assert_eq!(
        resp["success"], true,
        "organize_imports should succeed: {:?}",
        resp
    );
    assert!(
        resp["removed_duplicates"].as_u64().unwrap() >= 2,
        "should remove at least 2 duplicates: {:?}",
        resp
    );

    let content = fs::read_to_string(&file).unwrap();
    // Count occurrences of 'zod' — should appear exactly once
    let zod_count = content.matches("'zod'").count();
    assert_eq!(
        zod_count, 1,
        "should have exactly one zod import. content:\n{}",
        content
    );

    let react_count = content.matches("from 'react'").count();
    assert_eq!(
        react_count, 1,
        "should have exactly one react import. content:\n{}",
        content
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn organize_imports_py_isort_grouping() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static PY_ORG_COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let n = PY_ORG_COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = dir.path().join(format!("organize_py_{}.py", n));

    // Write a scrambled Python import file (wrong order: local, external, stdlib)
    fs::write(
        &file,
        "\
from . import utils
import requests
import os
import sys
from ..config import Settings

def main():
    pass
",
    )
    .unwrap();

    let file_str = file.display().to_string();
    let resp = send_organize_imports(&mut aft, "org-3", &file_str);

    assert_eq!(
        resp["success"], true,
        "organize_imports should succeed: {:?}",
        resp
    );

    // Check groups: should be stdlib, external, internal
    let groups = resp["groups"].as_array().unwrap();
    assert!(
        groups.len() >= 2,
        "should have at least 2 groups: {:?}",
        groups
    );
    assert_eq!(groups[0]["name"], "stdlib", "first group should be stdlib");

    let content = fs::read_to_string(&file).unwrap();

    // Stdlib (os, sys) should come before external (requests)
    let os_pos = content.find("import os").unwrap();
    let requests_pos = content.find("import requests").unwrap();
    assert!(
        os_pos < requests_pos,
        "stdlib should come before external. content:\n{}",
        content
    );

    // External should come before internal
    let utils_pos = content.find("utils").unwrap();
    assert!(
        requests_pos < utils_pos,
        "external should come before internal. content:\n{}",
        content
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn organize_imports_rs_merges_common_prefix() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static RS_ORG_COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let n = RS_ORG_COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = dir.path().join(format!("organize_rs_{}.rs", n));

    // Write Rust file with separate use declarations that share a common prefix
    fs::write(
        &file,
        "\
use std::path::PathBuf;
use std::path::Path;
use std::collections::HashMap;
use serde::Deserialize;
use serde::Serialize;
use crate::config::Settings;

fn main() {}
",
    )
    .unwrap();

    let file_str = file.display().to_string();
    let resp = send_organize_imports(&mut aft, "org-4", &file_str);

    assert_eq!(
        resp["success"], true,
        "organize_imports should succeed: {:?}",
        resp
    );

    let content = fs::read_to_string(&file).unwrap();

    // std::path::Path and std::path::PathBuf should be merged
    assert!(
        content.contains("use std::path::{Path, PathBuf};"),
        "should merge std::path imports into a use tree. content:\n{}",
        content
    );

    // serde::Deserialize and serde::Serialize should be merged
    assert!(
        content.contains("use serde::{Deserialize, Serialize};"),
        "should merge serde imports into a use tree. content:\n{}",
        content
    );

    // Groups should be in order: stdlib, external, internal
    let std_pos = content.find("use std::").unwrap();
    let serde_pos = content.find("use serde::").unwrap();
    let crate_pos = content.find("use crate::").unwrap();
    assert!(std_pos < serde_pos, "stdlib before external");
    assert!(serde_pos < crate_pos, "external before internal");

    assert_eq!(resp["syntax_valid"], true);

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn organize_imports_rs_preserves_nested_use_tree() {
    // Regression: a nested use tree like
    //   use std::collections::{hash_map::{Entry, HashMap}, BTreeMap};
    // was split on raw commas, corrupting the nested subtree into
    //   `hash_map::{Entry` / `HashMap}` / `BTreeMap`, which regrouped into
    //   `use std::collections::{BTreeMap, HashMap}, hash_map::{Entry};`
    // — invalid Rust. Brace-aware splitting must keep the subtree intact.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = dir.path().join(format!("organize_rs_nested_{}.rs", n));

    fs::write(
        &file,
        "\
use std::collections::{hash_map::{Entry, HashMap}, BTreeMap};
use std::fmt;

fn main() {}
",
    )
    .unwrap();

    let file_str = file.display().to_string();
    let resp = send_organize_imports(&mut aft, "org-rs-nested", &file_str);

    assert_eq!(
        resp["success"], true,
        "organize_imports should succeed: {resp:?}"
    );
    assert_eq!(
        resp["syntax_valid"], true,
        "organized Rust must stay syntactically valid: {resp:?}"
    );

    let content = fs::read_to_string(&file).unwrap();

    // The nested subtree must survive as one item, not be exploded into
    // sibling top-level entries.
    assert!(
        content.contains("hash_map::{Entry, HashMap}"),
        "nested subtree must be preserved intact. content:\n{content}"
    );
    // The corruption signature must NOT appear: a brace tree followed by a
    // comma and another path at the same level inside one use statement.
    assert!(
        !content.contains("}, hash_map::{Entry};")
            && !content.contains("{BTreeMap, HashMap}, hash_map"),
        "must not produce invalid comma-joined brace trees. content:\n{content}"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn organize_imports_rs_refuses_to_detach_outer_attribute() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("organize_rs_attribute.rs");
    let original = "\
#[cfg(unix)]
use platform::unix::Thing;
use alpha::Common;

fn main() {}
";
    fs::write(&file, original).unwrap();

    let response = send_organize_imports(&mut aft, "org-rs-attribute", &file.display().to_string());

    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        original,
        "organize must not move #[cfg] onto another import; response: {response:?}"
    );
    assert_eq!(response["success"], false, "response: {response:?}");
    assert_eq!(
        response["code"], "unsupported_import_attributes",
        "response: {response:?}"
    );

    aft.shutdown();
}

#[test]
fn organize_imports_rs_preserves_pub_use_and_private_use_pair() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = dir.path().join(format!("organize_rs_pub_private_{}.rs", n));

    fs::write(
        &file,
        "\
pub use serde;
use serde;

fn main() {}
",
    )
    .unwrap();

    let file_str = file.display().to_string();
    let resp = send_organize_imports(&mut aft, "org-rs-pub-private", &file_str);

    assert_eq!(
        resp["success"], true,
        "organize_imports should succeed: {:?}",
        resp
    );

    let content = fs::read_to_string(&file).unwrap();
    assert!(content.contains("pub use serde;"), "content:\n{content}");
    assert!(content.contains("use serde;"), "content:\n{content}");

    fs::remove_file(&file).ok();
    aft.shutdown();
}

// ---------------------------------------------------------------------------
// Regression: TS/JS named-import aliases and per-name type modifiers must
// survive `organize_imports` round-trips. Reported in dogfooding session
// `ses_23180bd14ffeTODg3ZRGsHKA55`: organize silently rewrote
// `import { stdin as input, stdout as output } from 'node:process'` to
// `import { stdin, stdout }`, breaking every reference to `input`/`output`
// in the file. Returned `success: true, syntax_valid: true` — silent
// semantic corruption. Fixed by storing TS/JS specifiers verbatim
// (alias and per-name `type` prefix included) so the regenerator can
// emit them unchanged.
// ---------------------------------------------------------------------------

#[test]
fn organize_imports_ts_preserves_named_aliases() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = dir.path().join(format!("alias_ts_{}.ts", n));

    fs::write(
        &file,
        "import { stdin as input, stdout as output } from 'node:process'\n\
         \n\
         const rl = createInterface({ input, output })\n",
    )
    .unwrap();

    let file_str = file.display().to_string();
    let resp = send_organize_imports(&mut aft, "alias-ts", &file_str);
    assert_eq!(resp["success"], true, "organize succeeded: {:?}", resp);

    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("stdin as input"),
        "alias `stdin as input` must survive organize. got:\n{content}"
    );
    assert!(
        content.contains("stdout as output"),
        "alias `stdout as output` must survive organize. got:\n{content}"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn organize_imports_ts_preserves_per_name_type_prefix() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = dir.path().join(format!("typeprefix_ts_{}.ts", n));

    fs::write(
        &file,
        "import { type Foo, Bar, baz as qux } from './a'\n\
         \n\
         export type X = Foo\n\
         export const y: typeof Bar = baz\n\
         qux()\n",
    )
    .unwrap();

    let file_str = file.display().to_string();
    let resp = send_organize_imports(&mut aft, "typeprefix-ts", &file_str);
    assert_eq!(resp["success"], true, "organize succeeded: {:?}", resp);

    let content = fs::read_to_string(&file).unwrap();
    // Per-specifier `type` prefix is preserved (not promoted to `import type`
    // and not silently dropped).
    assert!(
        content.contains("type Foo"),
        "per-name `type Foo` modifier must survive. got:\n{content}"
    );
    assert!(
        content.contains("Bar"),
        "non-type `Bar` import must survive. got:\n{content}"
    );
    assert!(
        content.contains("baz as qux"),
        "alias `baz as qux` must survive alongside type modifiers. got:\n{content}"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn organize_imports_ts_aliased_and_bare_are_not_duplicates() {
    // `{ Foo }` and `{ Foo as Bar }` introduce different local bindings, so
    // dedup must not collapse them into one. This guards against an aliased
    // import being silently dropped during organize.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = dir.path().join(format!("alias_dedup_ts_{}.ts", n));

    fs::write(
        &file,
        "import { Foo } from './a'\n\
         import { Foo as Bar } from './a'\n\
         \n\
         export const x = Foo\n\
         export const y = Bar\n",
    )
    .unwrap();

    let file_str = file.display().to_string();
    let resp = send_organize_imports(&mut aft, "alias-dedup-ts", &file_str);
    assert_eq!(resp["success"], true, "organize succeeded: {:?}", resp);

    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("Foo as Bar"),
        "aliased import `Foo as Bar` must not be dedup'd away by bare `Foo`. got:\n{content}"
    );
    // Bare `Foo` must also still be reachable (could be merged into the
    // same import statement or kept separate — both are correct).
    assert!(
        content.matches("Foo").count() >= 2,
        "bare `Foo` and `Foo as Bar` must both survive. got:\n{content}"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn organize_imports_ts_preserves_namespace_and_side_effect_imports() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = dir
        .path()
        .join(format!("namespace_side_effect_ts_{}.ts", n));

    fs::write(
        &file,
        "import 'fs'\n\
         import * as fs from 'fs'\n\
         \n\
         export const exists = fs.existsSync\n",
    )
    .unwrap();

    let file_str = file.display().to_string();
    let resp = send_organize_imports(&mut aft, "namespace-side-effect-ts", &file_str);
    assert_eq!(resp["success"], true, "organize succeeded: {:?}", resp);

    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("import 'fs'\n"),
        "side-effect import must survive alongside namespace import. got:\n{content}"
    );
    assert!(
        content.contains("import * as fs from 'fs'\n"),
        "namespace import must not be dedup'd as a side-effect import. got:\n{content}"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn organize_imports_ts_preserves_side_effect_and_namespace_imports_reverse_order() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = dir
        .path()
        .join(format!("side_effect_namespace_ts_{}.ts", n));

    fs::write(
        &file,
        "import * as fs from 'fs'\n\
         import 'fs'\n\
         \n\
         export const exists = fs.existsSync\n",
    )
    .unwrap();

    let file_str = file.display().to_string();
    let resp = send_organize_imports(&mut aft, "side-effect-namespace-ts", &file_str);
    assert_eq!(resp["success"], true, "organize succeeded: {:?}", resp);

    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("import 'fs'\n"),
        "side-effect import must survive when namespace import appears first. got:\n{content}"
    );
    assert!(
        content.contains("import * as fs from 'fs'\n"),
        "namespace import must survive when side-effect import appears second. got:\n{content}"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn organize_imports_ts_dedupes_identical_namespace_imports() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = dir.path().join(format!("namespace_dedup_ts_{}.ts", n));

    fs::write(
        &file,
        "import * as fs from 'fs'\n\
         import * as fs from 'fs'\n\
         \n\
         export const exists = fs.existsSync\n",
    )
    .unwrap();

    let file_str = file.display().to_string();
    let resp = send_organize_imports(&mut aft, "namespace-dedup-ts", &file_str);
    assert_eq!(resp["success"], true, "organize succeeded: {:?}", resp);

    let content = fs::read_to_string(&file).unwrap();
    assert_eq!(
        content.matches("import * as fs from 'fs'").count(),
        1,
        "identical namespace imports should dedupe. got:\n{content}"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn organize_imports_ts_keeps_distinct_namespace_aliases() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = dir.path().join(format!("namespace_aliases_ts_{}.ts", n));

    fs::write(
        &file,
        "import * as foo from 'fs'\n\
         import * as bar from 'fs'\n\
         \n\
         export const a = foo.existsSync\n\
         export const b = bar.readFileSync\n",
    )
    .unwrap();

    let file_str = file.display().to_string();
    let resp = send_organize_imports(&mut aft, "namespace-aliases-ts", &file_str);
    assert_eq!(resp["success"], true, "organize succeeded: {:?}", resp);

    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("import * as foo from 'fs'\n"),
        "namespace alias `foo` must survive. got:\n{content}"
    );
    assert!(
        content.contains("import * as bar from 'fs'\n"),
        "namespace alias `bar` must survive. got:\n{content}"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn organize_imports_ts_sorts_named_specifiers_by_imported_name() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let file = dir.path().join(format!("specifier_sort_ts_{}.ts", n));

    fs::write(
        &file,
        "import { useState, type Foo, stdin as input, type Bar } from 'x'\n\
         \n\
         export const value = [input, useState]\n",
    )
    .unwrap();

    let file_str = file.display().to_string();
    let resp = send_organize_imports(&mut aft, "specifier-sort-ts", &file_str);
    assert_eq!(resp["success"], true, "organize succeeded: {:?}", resp);

    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("import { type Bar, type Foo, stdin as input, useState } from 'x'\n"),
        "named specifiers should sort by imported name, ignoring `type` and aliases. got:\n{content}"
    );

    fs::remove_file(&file).ok();
    aft.shutdown();
}

#[test]
fn generate_ts_namespace_import_line_round_trips_namespace_only() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file = tmp.path().join("namespace.ts");
    fs::write(&file, "import * as ns from './mod';\n").expect("write import file");
    let (_, _, block) = parse_file_imports(&file, LangId::TypeScript).expect("parse imports");
    let import = block.imports.first().expect("parsed import");

    let line = generate_import_line_with_namespace(
        LangId::TypeScript,
        &import.module_path,
        &import.names,
        import.default_import.as_deref(),
        import.namespace_import.as_deref(),
        false,
    );

    assert_eq!(line, "import * as ns from './mod';");
}

#[test]
fn generate_ts_namespace_import_line_round_trips_default_and_namespace() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file = tmp.path().join("default_namespace.ts");
    fs::write(&file, "import Foo, * as ns from './mod';\n").expect("write import file");
    let (_, _, block) = parse_file_imports(&file, LangId::TypeScript).expect("parse imports");
    let import = block.imports.first().expect("parsed import");

    let line = generate_import_line_with_namespace(
        LangId::TypeScript,
        &import.module_path,
        &import.names,
        import.default_import.as_deref(),
        import.namespace_import.as_deref(),
        false,
    );

    assert_eq!(line, "import Foo, * as ns from './mod';");
}

// --- Merge into existing same-module import ---

#[test]
fn add_import_merges_into_existing_same_module_named_import() {
    let mut aft = AftProcess::spawn();

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("lib.mjs");
    let file = dir.path().join("merge.mjs");
    fs::write(
        &module,
        "export const foo = () => 'foo';\nexport const baz = () => 'baz';\n",
    )
    .unwrap();
    fs::write(
        &file,
        "import { foo } from './lib.mjs';\n\nconsole.log(foo());\n",
    )
    .unwrap();
    assert_eq!(
        assert_node_succeeds(&file, "ordinary named import before merge")
            .status
            .code(),
        Some(0)
    );

    aft.send(&format!(
        r#"{{"id":"cfg","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&dir.path().display())
    ));

    let resp = send_add_import(
        &mut aft,
        "merge1",
        file.to_str().unwrap(),
        "./lib.mjs",
        Some(&["baz"]),
        None,
        false,
    );

    assert_eq!(
        resp["success"], true,
        "merge add should succeed: {:?}",
        resp
    );
    assert_eq!(resp["added"], true, "should report added=true: {:?}", resp);

    let content = fs::read_to_string(&file).unwrap();
    assert_eq!(
        content.matches("./lib.mjs").count(),
        1,
        "should have exactly one statement importing from './lib.mjs':\n{content}"
    );
    assert!(
        content.contains("baz") && content.contains("foo"),
        "merged statement should contain both names:\n{content}"
    );
    assert_eq!(
        assert_node_succeeds(&file, "ordinary named import after merge")
            .status
            .code(),
        Some(0)
    );

    aft.shutdown();
}

fn send_scala_add_import(
    aft: &mut AftProcess,
    id: &str,
    file: &std::path::Path,
    module: &str,
    names: &[&str],
    modifiers: &[&str],
) -> serde_json::Value {
    let mut params = serde_json::json!({
        "id": id,
        "command": "add_import",
        "file": file,
        "module": module,
    });
    if !names.is_empty() {
        params["names"] = serde_json::json!(names);
    }
    if !modifiers.is_empty() {
        params["modifiers"] = serde_json::json!(modifiers);
    }
    aft.send(&serde_json::to_string(&params).unwrap())
}

fn assert_scala2_add_is_noop(input: &str, module: &str, names: &[&str], modifiers: &[&str]) {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("dedup.scala");
    fs::write(&file, input).unwrap();

    let response = send_scala_add_import(&mut aft, "scala2-dedup", &file, module, names, modifiers);

    assert_eq!(response["success"], true, "add failed: {response:?}");
    assert_eq!(
        response["added"], false,
        "add was not deduped: {response:?}"
    );
    assert_eq!(
        response["already_present"], true,
        "missing dedup result: {response:?}"
    );
    assert_eq!(fs::read(&file).unwrap(), input.as_bytes());
    aft.shutdown();
}

#[test]
fn add_import_scala2_dedupes_ordinary_import() {
    assert_scala2_add_is_noop(
        "import a.b._\nimport c.d.C\n\nobject Main {}\n",
        "c.d.C",
        &[],
        &[],
    );
}

#[test]
fn add_import_scala2_dedupes_wildcard_import() {
    assert_scala2_add_is_noop(
        "import a.b._\nimport cats.syntax.all._\n\nobject Main {}\n",
        "cats.syntax.all",
        &[],
        &["wildcard"],
    );
}

#[test]
fn add_import_scala2_dedupes_renamed_import() {
    assert_scala2_add_is_noop(
        "import a.b._\nimport scala.concurrent.{ExecutionContext => EC}\n\nobject Main {}\n",
        "scala.concurrent",
        &["ExecutionContext as EC"],
        &[],
    );
}

#[test]
fn add_import_scala2_new_wildcard_uses_existing_dialect() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("new.scala");
    let input = "import a.b._\n\nobject Main {}\n";
    fs::write(&file, input).unwrap();

    let response = send_scala_add_import(
        &mut aft,
        "scala2-new",
        &file,
        "cats.syntax.all",
        &[],
        &["wildcard"],
    );

    assert_eq!(response["success"], true, "add failed: {response:?}");
    assert_eq!(
        response["added"], true,
        "new import was not added: {response:?}"
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "import a.b._\nimport cats.syntax.all._\n\nobject Main {}\n"
    );
    aft.shutdown();
}

#[test]
fn organize_imports_preserves_es_attributes_and_ordinary_organization() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("attributes.mjs");
    fs::write(
        &file,
        "import same from './config.json' with { type: 'json' };\n\
import same from './config.json';\n\
import legacy from './legacy.json' assert { type: 'json' };\n\
import { zebra, alpha } from 'ordinary';\n\
import { alpha, zebra } from 'ordinary';\n",
    )
    .unwrap();
    aft.send(&format!(
        r#"{{"id":"cfg-attrs","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&dir.path().display())
    ));

    let resp = send_organize_imports(&mut aft, "organize-attrs", file.to_str().unwrap());
    assert_eq!(resp["success"], true, "organize should succeed: {resp:?}");
    assert_eq!(resp["removed_duplicates"], 1);

    let content = fs::read_to_string(&file).unwrap();
    assert!(content.contains("with { type: 'json' }"), "{content}");
    assert!(content.contains("assert { type: 'json' }"), "{content}");
    assert_eq!(
        content.matches("from './config.json'").count(),
        2,
        "{content}"
    );
    assert_eq!(content.matches("from 'ordinary'").count(), 1, "{content}");
    assert!(
        content.contains("import { alpha, zebra } from 'ordinary';"),
        "ordinary ES imports must still sort and deduplicate:\n{content}"
    );

    aft.shutdown();
}

#[test]
fn add_and_partial_remove_preserve_es_import_attributes() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("config.json");
    fs::write(&module, "{\"ok\":true}\n").unwrap();
    aft.send(&format!(
        r#"{{"id":"cfg-attrs-mutate","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&dir.path().display())
    ));

    for keyword in ["with", "assert"] {
        let file = dir.path().join(format!("attributes-{keyword}.mjs"));
        let clause = format!(r#"{keyword} {{ type: "json" }}"#);
        fs::write(
            &file,
            format!("import config from './config.json' {clause};\nconsole.log(config.ok);\n"),
        )
        .unwrap();
        assert_eq!(
            assert_node_import_semantics_succeed(
                &file,
                &format!("JSON default import before {keyword} alias merge")
            )
            .status
            .code(),
            Some(0)
        );

        let add = send_add_import(
            &mut aft,
            &format!("add-attrs-{keyword}"),
            file.to_str().unwrap(),
            "./config.json",
            Some(&["default as configAlias"]),
            None,
            false,
        );
        assert_eq!(add["success"], true, "add should succeed: {add:?}");
        let after_add = fs::read_to_string(&file).unwrap();
        assert_eq!(after_add.matches("./config.json").count(), 1, "{after_add}");
        assert!(
            after_add.contains(&format!(
                "import config, {{ default as configAlias }} from './config.json' {clause};"
            )) || after_add.contains(&format!(
                "import config, {{ default as configAlias }} from \"./config.json\" {clause};"
            )),
            "a legal alias of the JSON default export should merge without dropping its {keyword} attribute:\n{after_add}"
        );
        assert_eq!(
            assert_node_import_semantics_succeed(
                &file,
                &format!("JSON default alias after {keyword} merge")
            )
            .status
            .code(),
            Some(0)
        );

        let remove = send_remove_import(
            &mut aft,
            &format!("remove-attrs-{keyword}"),
            file.to_str().unwrap(),
            "./config.json",
            Some("configAlias"),
        );
        assert_eq!(remove["success"], true, "remove should succeed: {remove:?}");
        let after_remove = fs::read_to_string(&file).unwrap();
        assert!(after_remove.contains(&clause), "{after_remove}");
        assert!(
            after_remove.contains("import config from"),
            "{after_remove}"
        );
        assert!(!after_remove.contains("configAlias"), "{after_remove}");
        assert_eq!(
            assert_node_import_semantics_succeed(
                &file,
                &format!("JSON default import after {keyword} partial removal")
            )
            .status
            .code(),
            Some(0)
        );
    }

    aft.shutdown();
}

#[test]
fn add_import_rejects_non_default_json_named_export_without_mutation_or_backup() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("config.json");
    fs::write(&module, "{\"ok\":true}\n").unwrap();
    aft.send(&format!(
        r#"{{"id":"cfg-json-reject","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&dir.path().display())
    ));

    for (keyword, quote, quote_name) in [
        ("with", "\"", "double"),
        ("with", "'", "single"),
        ("assert", "\"", "double"),
        ("assert", "'", "single"),
    ] {
        let case = format!("{keyword}-{quote_name}");
        let clause = format!("{keyword} {{ type: {quote}json{quote} }}");
        let file = dir.path().join(format!("reject-json-name-{case}.mjs"));
        let original =
            format!("import config from './config.json' {clause};\nconsole.log(config.ok);\n");
        fs::write(&file, &original).unwrap();

        assert_eq!(
            assert_node_import_semantics_succeed(
                &file,
                &format!("JSON default import before rejected {case} add")
            )
            .status
            .code(),
            Some(0)
        );
        let add = send_add_import(
            &mut aft,
            &format!("add-invalid-json-name-{case}"),
            file.to_str().unwrap(),
            "./config.json",
            Some(&["extra"]),
            None,
            false,
        );

        assert_eq!(add["success"], false, "add should be refused: {add:?}");
        assert_eq!(add["code"], "unsupported_json_named_export", "{add:?}");
        assert!(
            add["message"].as_str().is_some_and(|message| message
                .contains("JSON modules expose no named exports other than 'default'")),
            "the refusal should explain the module constraint: {add:?}"
        );
        assert!(
            add.get("backup_id").is_none(),
            "a pre-edit refusal must not report a backup: {add:?}"
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), original);

        let history = aft.send(&format!(
            r#"{{"id":"history-json-reject-{case}","command":"edit_history","file":{}}}"#,
            crate::helpers::json_string(&file.display())
        ));
        assert_eq!(
            history["success"], true,
            "history should succeed: {history:?}"
        );
        assert_eq!(
            history["entries"],
            serde_json::json!([]),
            "a rejected add must not take a backup: {history:?}"
        );
        assert_eq!(
            assert_node_import_semantics_succeed(
                &file,
                &format!("JSON default import after rejected {case} add")
            )
            .status
            .code(),
            Some(0)
        );
    }

    aft.shutdown();
}

#[test]
fn add_import_rejects_fresh_json_named_import_without_mutation_or_backup() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("config.json");
    fs::write(&module, "{\"ok\":true}\n").unwrap();
    aft.send(&format!(
        r#"{{"id":"cfg-json-fresh","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&dir.path().display())
    ));

    for (keyword, quote, quote_name) in [
        ("with", "\"", "double"),
        ("with", "'", "single"),
        ("assert", "\"", "double"),
        ("assert", "'", "single"),
    ] {
        let case = format!("{keyword}-{quote_name}");
        let clause = format!("{keyword} {{ type: {quote}json{quote} }}");
        let file = dir.path().join(format!("fresh-json-name-{case}.mjs"));
        let original = format!(
            "import * as config from './config.json' {clause};\nconsole.log(config.default.ok);\n"
        );
        fs::write(&file, &original).unwrap();
        assert_eq!(
            assert_node_import_semantics_succeed(
                &file,
                &format!("JSON namespace import before rejected fresh {case} add")
            )
            .status
            .code(),
            Some(0)
        );

        let invalid = file.with_extension("invalid.mjs");
        fs::write(
            &invalid,
            format!("{original}import {{ extra }} from './config.json' {clause};\n"),
        )
        .unwrap();
        assert_node_import_semantics_fail(&invalid, &format!("JSON named import sibling {case}"));
        fs::remove_file(&invalid).unwrap();

        let add = send_add_import(
            &mut aft,
            &format!("add-invalid-json-fresh-{case}"),
            file.to_str().unwrap(),
            "./config.json",
            Some(&["extra"]),
            None,
            false,
        );
        assert_eq!(add["success"], false, "add should be refused: {add:?}");
        assert_eq!(add["code"], "unsupported_json_named_export", "{add:?}");
        assert!(add.get("backup_id").is_none(), "{add:?}");
        assert_eq!(fs::read_to_string(&file).unwrap(), original);
    }

    aft.shutdown();
}

#[test]
fn add_import_rejects_json_namespace_binding_without_mutation_or_backup() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("config.json");
    let file = dir.path().join("namespace-json.mjs");
    fs::write(&module, "{\"ok\":true}\n").unwrap();
    let original = "import config from './config.json' with { type: \"json\" };\n";
    fs::write(&file, original).unwrap();
    aft.send(&format!(
        r#"{{"id":"cfg-json-namespace","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&dir.path().display())
    ));

    let add = send_add_namespace_import(
        &mut aft,
        "add-invalid-json-namespace",
        file.to_str().unwrap(),
        "./config.json",
        "configNamespace",
    );
    assert_eq!(
        add["success"], false,
        "namespace add should be refused: {add:?}"
    );
    assert_eq!(add["code"], "unsupported_json_named_export", "{add:?}");
    assert!(add.get("backup_id").is_none(), "{add:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), original);

    aft.shutdown();
}

#[test]
fn add_import_allows_json_default_import_add_and_dedup() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("config.json");
    let file = dir.path().join("default-json.mjs");
    fs::write(&module, "{\"ok\":true}\n").unwrap();
    let original =
        "import config from './config.json' with { type: \"json\" };\nconsole.log(config.ok);\n";
    fs::write(&file, original).unwrap();
    aft.send(&format!(
        r#"{{"id":"cfg-json-default","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&dir.path().display())
    ));

    let add = send_add_import(
        &mut aft,
        "add-json-default",
        file.to_str().unwrap(),
        "./config.json",
        None,
        Some("settings"),
        false,
    );
    assert_eq!(add["success"], true, "default add should succeed: {add:?}");
    assert_eq!(add["added"], true, "default add should mutate: {add:?}");
    let after_add = fs::read_to_string(&file).unwrap();
    assert!(
        after_add.contains("import settings from './config.json'"),
        "{after_add}"
    );
    assert!(
        after_add.matches("type: \"json\"").count() >= 2,
        "{after_add}"
    );
    assert_eq!(
        assert_node_import_semantics_succeed(&file, "JSON default import after default add")
            .status
            .code(),
        Some(0)
    );

    let duplicate = send_add_import(
        &mut aft,
        "dedup-json-default",
        file.to_str().unwrap(),
        "./config.json",
        None,
        Some("settings"),
        false,
    );
    assert_eq!(
        duplicate["success"], true,
        "default dedup should succeed: {duplicate:?}"
    );
    assert_eq!(
        duplicate["added"], false,
        "default dedup should be a no-op: {duplicate:?}"
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), after_add);

    aft.shutdown();
}

#[test]
fn add_import_allows_named_export_for_non_json_attributed_module() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("webassembly-attribute.mjs");
    fs::write(
        &file,
        "import { foo } from './module.wasm' with { type: \"webassembly\" };\nconsole.log(foo);\n",
    )
    .unwrap();
    aft.send(&format!(
        r#"{{"id":"cfg-wasm-attribute","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&dir.path().display())
    ));

    let add = send_add_import(
        &mut aft,
        "add-wasm-name",
        file.to_str().unwrap(),
        "./module.wasm",
        Some(&["bar"]),
        None,
        false,
    );
    assert_eq!(
        add["success"], true,
        "non-JSON module attributes must not trigger the JSON export guard: {add:?}"
    );
    let content = fs::read_to_string(file).unwrap();
    assert!(
        content.contains("{ bar, foo }") && content.contains("type: \"webassembly\""),
        "ordinary named exports and the non-JSON attribute should both survive:\n{content}"
    );

    aft.shutdown();
}

fn assert_rust_library_compiles(file: &Path) {
    let output = std::process::Command::new("rustc")
        .args(["--edition", "2021", "--crate-type", "lib"])
        .arg(file)
        .arg("-o")
        .arg(file.with_extension("rlib"))
        .output()
        .expect("rustc is required for Rust import regressions");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn rust_use_list_add_remove_organize_compiles() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("lib.rs");
    let source = "use a::{x, y};\n\nmod a { pub fn x() {} pub fn y() {} pub fn z() {} }\npub fn f() { x(); y(); }\n";
    fs::write(&file, source).unwrap();
    assert_rust_library_compiles(&file);
    let mut aft = AftProcess::spawn();
    let response = send_add_import(
        &mut aft,
        "rust-list-duplicate",
        file.to_str().unwrap(),
        "a",
        Some(&["x"]),
        None,
        false,
    );
    assert_eq!(response["already_present"], true, "{response}");
    assert_eq!(fs::read_to_string(&file).unwrap(), source);
    assert_rust_library_compiles(&file);
    let response = send_add_import(
        &mut aft,
        "rust-list-merge",
        file.to_str().unwrap(),
        "a",
        Some(&["z"]),
        None,
        false,
    );
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        source.replace("{x, y}", "{x, y, z}")
    );
    assert_rust_library_compiles(&file);
    let before = fs::read_to_string(&file).unwrap();
    let response = aft.send(
        &serde_json::json!({"id":"rust-list-organize", "command":"organize_imports", "file":file})
            .to_string(),
    );
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(fs::read_to_string(&file).unwrap(), before);
    assert_rust_library_compiles(&file);
    // Remove the call to x too, so removing its import leaves no unresolved reference.
    fs::write(&file, before.replace("x(); ", "")).unwrap();
    let response = aft.send(&serde_json::json!({"id":"rust-list-remove", "command":"remove_import", "file":file, "module":"a", "name":"x"}).to_string());
    assert_eq!(response["success"], true, "{response}");
    assert!(
        fs::read_to_string(&file)
            .unwrap()
            .contains("use a::{y, z};"),
        "{response}"
    );
    assert_rust_library_compiles(&file);
    aft.shutdown();
}

#[test]
fn rust_use_list_complex_entries_roundtrip() {
    for (declaration, module, names) in [
        ("use a::{b::{c, d}, e};", "a", vec!["b::{c, d}", "e"]),
        ("use a::{self, x};", "a", vec!["self", "x"]),
        ("pub use a::{x as y, z};", "a", vec!["x as y", "z"]),
        ("use {a, b};", "", vec!["a", "b"]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        fs::write(&file, declaration).unwrap();
        let (_, _, block) = parse_file_imports(&file, LangId::Rust).unwrap();
        assert_eq!(block.imports.len(), 1);
        let imp = &block.imports[0];
        assert_eq!(imp.module_path, module);
        assert_eq!(imp.names, names);
        assert_eq!(
            generate_import_line_with_namespace(
                LangId::Rust,
                &imp.module_path,
                &imp.names,
                imp.default_import.as_deref(),
                None,
                false
            ),
            declaration
        );
    }
}

#[test]
fn rust_complex_use_lists_survive_edits_and_compile() {
    for (declaration, module, added, removed, expected) in [
        (
            "use a::{b::{c, d}, e};",
            "a",
            "z",
            "e",
            "use a::{b::{c, d}, z};",
        ),
        ("pub use a::{x as y, z};", "a", "z", "y", "pub use a::{z};"),
        ("use {a::x, a::z};", "", "a::e", "a::x", "use {a::e, a::z};"),
        (
            "use std::fmt::{self, Debug};",
            "std::fmt",
            "Display",
            "Debug",
            "use std::fmt::{self, Display};",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        let definitions = "pub mod a { pub fn x() {} pub fn e() {} pub fn z() {} pub mod b { pub fn c() {} pub fn d() {} } }\n";
        let source = format!("{declaration}\n\n{definitions}");
        fs::write(&file, source).unwrap();
        assert_rust_library_compiles(&file);
        let mut aft = AftProcess::spawn();
        let response = send_add_import(
            &mut aft,
            "complex-add",
            file.to_str().unwrap(),
            module,
            Some(&[added]),
            None,
            false,
        );
        assert_eq!(response["success"], true, "{response}");
        assert_rust_library_compiles(&file);
        let response = aft.send(&serde_json::json!({"id":"complex-remove", "command":"remove_import", "file":file, "module":module, "name":removed}).to_string());
        assert_eq!(response["success"], true, "{response}");
        let output = fs::read_to_string(&file).unwrap();
        assert!(output.contains(expected), "{output}");
        assert_rust_library_compiles(&file);
        let response = aft.send(&serde_json::json!({"id":"complex-organize", "command":"organize_imports", "file":file}).to_string());
        assert_eq!(response["success"], true, "{response}");
        assert_rust_library_compiles(&file);
        aft.shutdown();
    }
}

#[test]
fn es_import_commands_preserve_quotes() {
    for (extension, source) in [
        ("ts", "import { z, a } from \"z\";\nimport { b } from 'b';\n"),
        ("tsx", "import { z, a } from \"z\";\nimport { b } from 'b';\n"),
        ("js", "import { z, a } from \"z\";\nimport { b } from 'b';\n"),
        ("vue", "<script setup lang=\"ts\">\nimport { z, a } from \"z\";\nimport { b } from 'b';\n</script>\n"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(format!("quotes.{extension}"));
        fs::write(&file, source).unwrap();
        let mut aft = AftProcess::spawn();
        let response = send_add_import(&mut aft, "quote-add", file.to_str().unwrap(), "c", Some(&["c"]), None, false);
        assert_eq!(response["success"], true, "{response}");
        assert!(fs::read_to_string(&file).unwrap().contains("import { c } from \"c\";"));
        let response = aft.send(&serde_json::json!({
            "id": "quote-remove", "command": "remove_import", "file": file,
            "module": "z", "name": "z"
        }).to_string());
        assert_eq!(response["success"], true, "{response}");
        assert!(fs::read_to_string(&file).unwrap().contains("import { a } from \"z\";"));
        let response = aft.send(&serde_json::json!({
            "id": "quote-organize", "command": "organize_imports", "file": file
        }).to_string());
        assert_eq!(response["success"], true, "{response}");
        let organized = fs::read_to_string(&file).unwrap();
        assert!(organized.contains("import { a } from \"z\";"));
        assert!(organized.contains("import { b } from 'b';"));
    }
}

/// Semicolon-free import lines must come back semicolon-free: a project using
/// Biome `semicolons: "asNeeded"` or Prettier `semi: false` fails its format
/// check when a rewrite adds a terminator the original statement did not have.
#[test]
fn es_import_commands_preserve_missing_semicolons() {
    for (extension, source) in [
        ("ts", "import type { A, B } from './types.ts'\nimport { z } from 'z'\n\nexport const x = 1\n"),
        ("tsx", "import type { A, B } from './types.ts'\nimport { z } from 'z'\n\nexport const x = 1\n"),
        ("js", "import { A, B } from './types.js'\nimport { z } from 'z'\n\nexport const x = 1\n"),
        ("vue", "<script setup lang=\"ts\">\nimport type { A, B } from './types.ts'\nimport { z } from 'z'\n</script>\n"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(format!("nosemi.{extension}"));
        fs::write(&file, source).unwrap();
        let types = if extension == "js" { "./types.js" } else { "./types.ts" };
        let mut aft = AftProcess::spawn();

        let response = aft.send(&serde_json::json!({
            "id": "nosemi-remove", "command": "remove_import", "file": file,
            "module": types, "name": "A"
        }).to_string());
        assert_eq!(response["success"], true, "{response}");
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.contains(&format!("{{ B }} from '{types}'\n")), "{extension} remove:\n{text}");

        let response = send_add_import(&mut aft, "nosemi-merge", file.to_str().unwrap(), "z", Some(&["y"]), None, false);
        assert_eq!(response["success"], true, "{response}");
        let text = fs::read_to_string(&file).unwrap();
        let merged = "import { y, z } from 'z'\n";
        assert!(text.contains(merged), "{extension} add to existing:\n{text}");

        let response = send_add_import(&mut aft, "nosemi-new", file.to_str().unwrap(), "c", Some(&["c"]), None, false);
        assert_eq!(response["success"], true, "{response}");
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.contains("import { c } from 'c'\n"), "{extension} add new:\n{text}");

        let response = aft.send(&serde_json::json!({
            "id": "nosemi-organize", "command": "organize_imports", "file": file
        }).to_string());
        assert_eq!(response["success"], true, "{response}");
        let text = fs::read_to_string(&file).unwrap();
        assert!(!text.contains(';'), "{extension} organize added a semicolon:\n{text}");
        assert!(text.contains("import { c } from 'c'\n"), "{extension} organize:\n{text}");
        aft.shutdown();
    }
}

/// The same commands on semicolon-terminated imports keep the semicolons.
#[test]
fn es_import_commands_keep_semicolons() {
    for (extension, source) in [
        ("ts", "import type { A, B } from './types.ts';\nimport { z } from 'z';\n\nexport const x = 1;\n"),
        ("js", "import { A, B } from './types.ts';\nimport { z } from 'z';\n\nexport const x = 1;\n"),
        ("vue", "<script setup lang=\"ts\">\nimport type { A, B } from './types.ts';\nimport { z } from 'z';\n</script>\n"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(format!("semi.{extension}"));
        fs::write(&file, source).unwrap();
        let mut aft = AftProcess::spawn();

        let response = aft.send(&serde_json::json!({
            "id": "semi-remove", "command": "remove_import", "file": file,
            "module": "./types.ts", "name": "A"
        }).to_string());
        assert_eq!(response["success"], true, "{response}");
        let response = send_add_import(&mut aft, "semi-merge", file.to_str().unwrap(), "z", Some(&["y"]), None, false);
        assert_eq!(response["success"], true, "{response}");
        let response = send_add_import(&mut aft, "semi-new", file.to_str().unwrap(), "c", Some(&["c"]), None, false);
        assert_eq!(response["success"], true, "{response}");
        let response = aft.send(&serde_json::json!({
            "id": "semi-organize", "command": "organize_imports", "file": file
        }).to_string());
        assert_eq!(response["success"], true, "{response}");

        let text = fs::read_to_string(&file).unwrap();
        let merged = "import { y, z } from 'z';\n";
        for line in ["{ B } from './types.ts';\n", merged, "import { c } from 'c';\n"] {
            assert!(text.contains(line), "{extension} missing {line:?}:\n{text}");
        }
    }
}

/// A rewrite keeps the rewritten statement's own terminator even when the
/// rest of the file disagrees, while a brand-new statement follows the
/// file's majority.
#[test]
fn es_import_rewrites_keep_each_statements_terminator() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("mixed.ts");
    fs::write(
        &file,
        "import { a, b } from 'a';\nimport { d } from 'd';\nimport { e, f } from 'e'\n",
    )
    .unwrap();
    let mut aft = AftProcess::spawn();

    let response = aft.send(
        &serde_json::json!({
            "id": "mixed-remove", "command": "remove_import", "file": file,
            "module": "e", "name": "e"
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "{response}");
    let response = send_add_import(
        &mut aft,
        "mixed-merge",
        file.to_str().unwrap(),
        "a",
        Some(&["c"]),
        None,
        false,
    );
    assert_eq!(response["success"], true, "{response}");
    let response = send_add_import(
        &mut aft,
        "mixed-new",
        file.to_str().unwrap(),
        "g",
        Some(&["g"]),
        None,
        false,
    );
    assert_eq!(response["success"], true, "{response}");
    let text = fs::read_to_string(&file).unwrap();
    for line in [
        "import { a, b, c } from 'a';\n",
        "import { f } from 'e'\n",
        "import { g } from 'g';\n",
    ] {
        assert!(text.contains(line), "missing {line:?}:\n{text}");
    }

    let response = aft.send(
        &serde_json::json!({
            "id": "mixed-organize", "command": "organize_imports", "file": file
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "{response}");
    let text = fs::read_to_string(&file).unwrap();
    assert_eq!(
        text,
        "import { a, b, c } from 'a';\nimport { d } from 'd';\nimport { f } from 'e'\nimport { g } from 'g';\n"
    );
}

/// With no import to copy, a new statement follows the project's Biome
/// `javascript.formatter.semicolons` setting.
#[test]
fn es_add_import_follows_biome_semicolons_without_existing_imports() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::write(
        root.join("biome.json"),
        r#"{"javascript":{"formatter":{"semicolons":"asNeeded","quoteStyle":"single"}}}"#,
    )
    .unwrap();
    let file = root.join("empty.ts");
    fs::write(&file, "export const x = 1\n").unwrap();
    let mut aft = AftProcess::spawn();
    let configure = aft.send(&format!(
        r#"{{"id":"cfg","command":"configure","harness":"opencode","project_root":{}}}"#,
        serde_json::to_string(root.to_str().unwrap()).unwrap()
    ));
    assert_eq!(configure["success"], true, "{configure}");

    let response = send_add_import(
        &mut aft,
        "biome-new",
        file.to_str().unwrap(),
        "c",
        Some(&["c"]),
        None,
        false,
    );
    assert_eq!(response["success"], true, "{response}");
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.starts_with("import { c } from 'c'\n"), "{text}");
}

#[test]
fn remove_named_specifier_preserves_existing_list_layout() {
    let mut aft = AftProcess::spawn();
    for (extension, source, module, name, expected) in [
        (
            "ts",
            "import {\n  readdirSync,\n  readFileSync, // retained comment\n  linkSync,\n} from \"node:fs\";\n",
            "node:fs",
            "linkSync",
            "import {\n  readdirSync,\n  readFileSync, // retained comment\n} from \"node:fs\";\n",
        ),
        (
            "tsx",
            "import {\n  readdirSync,\n  readFileSync,\n  linkSync,\n} from \"node:fs\";\n",
            "node:fs",
            "linkSync",
            "import {\n  readdirSync,\n  readFileSync,\n} from \"node:fs\";\n",
        ),
        (
            "js",
            "import { readdirSync, readFileSync, linkSync } from \"node:fs\";\n",
            "node:fs",
            "linkSync",
            "import { readdirSync, readFileSync } from \"node:fs\";\n",
        ),
        (
            "py",
            "from pathlib import (\n    Path,\n    PurePath,\n    PurePosixPath,\n)\n",
            "pathlib",
            "PurePath",
            "from pathlib import (\n    Path,\n    PurePosixPath,\n)\n",
        ),
        (
            "rs",
            "use std::fs::{\n    read_dir,\n    read_to_string,\n    remove_file,\n};\n",
            "std::fs",
            "read_to_string",
            "use std::fs::{\n    read_dir,\n    remove_file,\n};\n",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(format!("layout.{extension}"));
        fs::write(&file, source).unwrap();
        let response = aft.send(
            &serde_json::json!({
                "id": format!("remove-layout-{extension}"),
                "command": "remove_import",
                "file": file,
                "module": module,
                "name": name,
            })
            .to_string(),
        );
        assert_eq!(response["success"], true, "{extension}: {response}");
        let actual = fs::read_to_string(&file).unwrap();
        assert_eq!(actual, expected, "{extension} import layout changed");
    }

    for (extension, source, module) in [
        ("ts", "import { only } from \"pkg\";\n", "pkg"),
        ("tsx", "import { only } from \"pkg\";\n", "pkg"),
        ("js", "import { only } from \"pkg\";\n", "pkg"),
        ("py", "from pkg import (only)\n", "pkg"),
        ("rs", "pub use pkg::{only};\n", "pkg"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(format!("only-one.{extension}"));
        fs::write(&file, source).unwrap();
        let response = aft.send(
            &serde_json::json!({
                "id": format!("remove-only-name-{extension}"),
                "command": "remove_import",
                "file": file,
                "module": module,
                "name": "only",
            })
            .to_string(),
        );
        assert_eq!(response["success"], true, "{extension}: {response}");
        assert_eq!(response["only_name"], true, "{extension}: {response}");
        assert!(fs::read_to_string(&file).unwrap().is_empty(), "{extension}");
    }
    aft.shutdown();
}

#[test]
fn add_named_specifier_preserves_existing_multiline_layout() {
    let mut aft = AftProcess::spawn();
    for (extension, source, module, expected) in [
        (
            "ts",
            "import {\n  alpha,\n  gamma,\n} from \"pkg\";\n",
            "pkg",
            "import {\n  alpha,\n  beta,\n  gamma,\n} from \"pkg\";\n",
        ),
        (
            "tsx",
            "import {\n  gamma,\n  alpha,\n} from \"pkg\";\n",
            "pkg",
            "import {\n  gamma,\n  alpha,\n  beta,\n} from \"pkg\";\n",
        ),
        (
            "js",
            "import { alpha, gamma } from \"pkg\";\n",
            "pkg",
            "import { alpha, beta, gamma } from \"pkg\";\n",
        ),
        (
            "py",
            "from pkg import (\n    alpha,\n    gamma,\n)\n",
            "pkg",
            "from pkg import (\n    alpha,\n    beta,\n    gamma,\n)\n",
        ),
        (
            "rs",
            "use pkg::{\n    alpha,\n    gamma,\n};\n",
            "pkg",
            "use pkg::{\n    alpha,\n    beta,\n    gamma,\n};\n",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(format!("add-layout.{extension}"));
        fs::write(&file, source).unwrap();
        let response = send_add_import(
            &mut aft,
            &format!("add-layout-{extension}"),
            file.to_str().unwrap(),
            module,
            Some(&["beta"]),
            None,
            false,
        );
        assert_eq!(response["success"], true, "{extension}: {response}");
        let actual = fs::read_to_string(&file).unwrap();
        assert_eq!(actual, expected, "{extension} import layout changed");
    }
    aft.shutdown();
}

fn send_add_es_names(
    aft: &mut AftProcess,
    file: &Path,
    module: &str,
    names: &[&str],
    type_only: bool,
) -> serde_json::Value {
    aft.send(
        &serde_json::json!({
            "id": "add-es-names", "command": "tool_call", "name": "import",
            "arguments": {
                "op": "add", "filePath": file, "module": module,
                "names": names, "typeOnly": type_only
            }
        })
        .to_string(),
    )
}

fn es_script(extension: &str, script: &str) -> String {
    if extension == "vue" {
        format!("<template><div /></template>\n<script setup lang=\"ts\">\n{script}</script>\n")
    } else {
        script.to_string()
    }
}

#[test]
fn add_import_es_inline_type_dedup_reports_partial_add() {
    for extension in ["ts", "tsx", "vue"] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(format!("inline.{extension}"));
        let module = "@cortexkit/anthropic-auth-core";
        let source = es_script(extension, "import {\n  Zap, // keep authored order\n  Api,\n  type LogTestRecord,\n} from \"@cortexkit/anthropic-auth-core\";\n");
        let expected = es_script(extension, "import {\n  Zap, // keep authored order\n  Api,\n  type LogTestRecord,\n  type PrimeManager,\n} from \"@cortexkit/anthropic-auth-core\";\n");
        fs::write(&file, &source).unwrap();
        let mut aft = AftProcess::spawn();
        let response = send_add_es_names(
            &mut aft,
            &file,
            module,
            &["PrimeManager", "LogTestRecord"],
            true,
        );
        assert_eq!(response["success"], true, "{extension}: {response}");
        assert_eq!(
            response["text"].as_str().unwrap().lines().next().unwrap(),
            "added PrimeManager; LogTestRecord already imported",
            "{extension}: {response}"
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), expected, "{extension}");
        assert_eq!(response["added_names"], serde_json::json!(["PrimeManager"]));
        assert_eq!(
            response["already_imported_names"],
            serde_json::json!(["LogTestRecord"])
        );
        aft.shutdown();
    }
}

#[test]
fn add_import_es_all_names_present_is_byte_identical() {
    for extension in ["ts", "tsx", "js", "vue"] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(format!("present.{extension}"));
        let source = es_script(
            extension,
            "// keep bytes\r\nimport { B as LocalB, A } from 'pkg'\r\n",
        );
        fs::write(&file, &source).unwrap();
        let mut aft = AftProcess::spawn();
        let response = send_add_es_names(&mut aft, &file, "pkg", &["A", "B as LocalB"], true);
        assert_eq!(response["success"], true, "{extension}: {response}");
        assert_eq!(response["added"], false, "{extension}: {response}");
        assert_eq!(response["already_present"], true, "{extension}: {response}");
        assert_eq!(
            response["text"].as_str().unwrap().lines().next().unwrap(),
            "A, B as LocalB already imported"
        );
        assert_eq!(fs::read(&file).unwrap(), source.as_bytes(), "{extension}");
        assert!(response.get("backup_id").is_none(), "{response}");
        aft.shutdown();
    }
}

#[test]
fn add_import_es_alias_dedup_uses_local_binding() {
    for extension in ["ts", "tsx", "js", "vue"] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(format!("aliases.{extension}"));
        let source = es_script(extension, "import { X as Y, Z } from 'pkg';\n");
        fs::write(&file, &source).unwrap();
        let mut aft = AftProcess::spawn();
        let response = send_add_es_names(
            &mut aft,
            &file,
            "pkg",
            &["X as Y", "Y", "X", "X as Other"],
            false,
        );
        assert_eq!(response["success"], true, "{extension}: {response}");
        assert_eq!(
            response["text"].as_str().unwrap().lines().next().unwrap(),
            "added X, X as Other; X as Y, Y already imported"
        );
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            es_script(
                extension,
                "import { X as Other, X, X as Y, Z } from 'pkg';\n"
            ),
            "{extension}"
        );
        aft.shutdown();
    }
}

#[test]
fn add_import_es_type_statement_dedup_and_in_place_extension() {
    for extension in ["ts", "tsx", "vue"] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(format!("types.{extension}"));
        let source = es_script(extension, "import { Value } from 'pkg'\nimport type {\n  Zed, // preserve order and comment\n  A as LocalA,\n} from 'pkg'\n");
        fs::write(&file, &source).unwrap();
        let mut aft = AftProcess::spawn();
        let response = send_add_es_names(
            &mut aft,
            &file,
            "pkg",
            &["Value", "A as LocalA", "NewType"],
            true,
        );
        assert_eq!(response["success"], true, "{extension}: {response}");
        assert_eq!(
            response["text"].as_str().unwrap().lines().next().unwrap(),
            "added NewType; Value, A as LocalA already imported"
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), es_script(extension, "import { Value } from 'pkg'\nimport type {\n  Zed, // preserve order and comment\n  A as LocalA,\n  NewType,\n} from 'pkg'\n"), "{extension}");
        let before = fs::read(&file).unwrap();
        let response =
            send_add_es_names(&mut aft, &file, "pkg", &["NewType", "A as LocalA"], false);
        assert_eq!(response["added"], false, "{extension}: {response}");
        assert_eq!(
            response["text"].as_str().unwrap().lines().next().unwrap(),
            "NewType, A as LocalA already imported"
        );
        assert_eq!(fs::read(&file).unwrap(), before);
        aft.shutdown();
    }
}

#[test]
fn add_import_es_prefers_type_statement_over_inline_type() {
    for extension in ["ts", "tsx", "vue"] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(format!("prefer.{extension}"));
        let source = es_script(
            extension,
            "import { Value, type Inline } from 'pkg';\nimport type { Existing } from 'pkg';\n",
        );
        fs::write(&file, &source).unwrap();
        let mut aft = AftProcess::spawn();
        let response = send_add_es_names(&mut aft, &file, "pkg", &["NewType"], true);
        assert_eq!(response["success"], true, "{extension}: {response}");
        assert_eq!(
            response["text"].as_str().unwrap().lines().next().unwrap(),
            "added NewType"
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), es_script(extension, "import { Value, type Inline } from 'pkg';\nimport type { Existing, NewType } from 'pkg';\n"), "{extension}");
        aft.shutdown();
    }
}

#[test]
fn add_import_es_inline_type_present_and_plain_value_fallback() {
    for extension in ["ts", "tsx", "vue"] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(format!("fallback.{extension}"));
        let mut aft = AftProcess::spawn();
        let source = es_script(
            extension,
            "import { Value, type X as LocalX } from 'pkg';\n",
        );
        fs::write(&file, &source).unwrap();
        let response = send_add_es_names(&mut aft, &file, "pkg", &["X as LocalX"], true);
        assert_eq!(response["success"], true, "{extension}: {response}");
        assert_eq!(response["added"], false, "{extension}: {response}");
        assert_eq!(
            response["text"].as_str().unwrap().lines().next().unwrap(),
            "X as LocalX already imported"
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), source);
        let source = es_script(extension, "import { Value } from 'pkg';\n");
        fs::write(&file, &source).unwrap();
        let response = send_add_es_names(&mut aft, &file, "pkg", &["Value", "NewType"], true);
        assert_eq!(response["success"], true, "{extension}: {response}");
        let text = fs::read_to_string(&file).unwrap();
        assert!(
            text.contains("import { Value } from 'pkg';\n"),
            "{extension}: {text}"
        );
        assert!(
            text.contains("import type { NewType } from 'pkg';\n"),
            "{extension}: {text}"
        );
        assert_eq!(
            response["text"].as_str().unwrap().lines().next().unwrap(),
            "added NewType; Value already imported"
        );
        aft.shutdown();
    }
}
