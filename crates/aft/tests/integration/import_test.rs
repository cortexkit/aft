//! Integration tests for the add_import command through the binary protocol.

use super::helpers::{fixture_path, AftProcess};
use aft::imports::{generate_import_line_with_namespace, parse_file_imports};
use aft::parser::LangId;
use std::fs;

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
        content.contains("import { useState } from 'react';"),
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
    assert_eq!(resp["code"], "invalid_request");

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

// ===========================================================================
// organize_imports tests
// ===========================================================================

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
        polyfill_a_pos < react_pos,
        "side-effects come before value imports within the same group:\n{content}"
    );
    assert!(
        react_pos < zod_pos,
        "value imports alphabetized (react before zod):\n{content}"
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
        content.contains("import 'fs';"),
        "side-effect import must survive alongside namespace import. got:\n{content}"
    );
    assert!(
        content.contains("import * as fs from 'fs';"),
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
        content.contains("import 'fs';"),
        "side-effect import must survive when namespace import appears first. got:\n{content}"
    );
    assert!(
        content.contains("import * as fs from 'fs';"),
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
        content.matches("import * as fs from 'fs';").count(),
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
        content.contains("import * as foo from 'fs';"),
        "namespace alias `foo` must survive. got:\n{content}"
    );
    assert!(
        content.contains("import * as bar from 'fs';"),
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
        content.contains("import { type Bar, type Foo, stdin as input, useState } from 'x';"),
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
