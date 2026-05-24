use super::helpers::AftProcess;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const VUE_FIXTURE: &str = r#"<template>
  <div class="greeting">
    <h1>{{ message }}</h1>
    <button @click="handleClick">{{ buttonText }}</button>
  </div>
</template>

<script setup lang="ts">
import { ref, computed } from 'vue'

const message = ref('Hello, World!')
const buttonText = computed(() => 'Click me')

function handleClick() {
  console.log('clicked')
}
</script>

<style scoped>
.greeting {
  color: blue;
}
</style>
"#;

fn write_file(root: &Path, relative: &str, content: &str) -> PathBuf {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, content).unwrap();
    path
}

fn send(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

#[test]
fn vue_outline_returns_top_level_sfc_sections() {
    let dir = TempDir::new().unwrap();
    let file = write_file(dir.path(), "src/App.vue", VUE_FIXTURE);

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({"id": "vue-outline", "command": "outline", "file": file}),
    );

    assert_eq!(resp["success"], true, "outline should succeed: {resp:?}");
    let text = resp["text"].as_str().expect("outline text");
    assert!(text.starts_with("App.vue\n"), "unexpected outline: {text}");
    assert!(
        text.contains("- h    <template> 1:6"),
        "missing template section: {text}"
    );
    assert!(
        text.contains("- h    <script setup lang=\"ts\"> 8:17"),
        "missing script section: {text}"
    );
    assert!(
        text.contains("- h    <style scoped> 19:23"),
        "missing style section: {text}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn vue_zoom_targets_template_script_and_style_sections() {
    let dir = TempDir::new().unwrap();
    let file = write_file(dir.path(), "src/App.vue", VUE_FIXTURE);

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    for (symbol, expected) in [
        ("template", "<button @click=\"handleClick\">"),
        ("script", "function handleClick()"),
        ("style", ".greeting"),
    ] {
        let resp = send(
            &mut aft,
            json!({"id": format!("vue-zoom-{symbol}"), "command": "zoom", "file": file, "symbol": symbol}),
        );
        assert_eq!(
            resp["success"], true,
            "zoom {symbol} should succeed: {resp:?}"
        );
        assert_eq!(resp["name"], symbol);
        assert_eq!(resp["kind"], "heading");
        let content = resp["content"].as_str().expect("zoom content");
        assert!(
            content.contains(expected),
            "zoom {symbol} should include {expected:?}: {content}"
        );
    }

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn vue_ast_search_supports_sfc_and_dollar_meta_variables() {
    let dir = TempDir::new().unwrap();
    let file = write_file(dir.path(), "src/App.vue", VUE_FIXTURE);

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let script_search = send(
        &mut aft,
        json!({
            "id": "vue-script-search",
            "command": "ast_search",
            "lang": "vue",
            "pattern": "<script setup lang=\"ts\">\nimport { ref, computed } from 'vue'\n\nconst message = ref('Hello, World!')\nconst buttonText = computed(() => 'Click me')\n\nfunction handleClick() {\n  console.log('clicked')\n}\n</script>",
            "paths": [file],
        }),
    );
    assert_eq!(
        script_search["success"], true,
        "Vue script-block ast_search should succeed: {script_search:?}"
    );
    assert_eq!(script_search["total_matches"], 1);

    let meta_search = send(
        &mut aft,
        json!({
            "id": "vue-meta-search",
            "command": "ast_search",
            "lang": "vue",
            "pattern": "<button @click=\"$NAME\">$$$</button>",
            "paths": [file],
        }),
    );
    assert_eq!(
        meta_search["success"], true,
        "Vue meta-var ast_search should succeed: {meta_search:?}"
    );
    assert_eq!(meta_search["total_matches"], 1);
    assert_eq!(
        meta_search["matches"][0]["meta_variables"]["$NAME"],
        "handleClick"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn vue_ast_search_documents_opaque_script_contents() {
    let dir = TempDir::new().unwrap();
    let file = write_file(dir.path(), "src/App.vue", VUE_FIXTURE);

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "vue-ts-inside-script-search",
            "command": "ast_search",
            "lang": "vue",
            "pattern": "function $NAME() { $$$ }",
            "paths": [file],
        }),
    );

    assert_eq!(
        resp["success"], true,
        "opaque script search should not crash: {resp:?}"
    );
    assert_eq!(
        resp["total_matches"], 0,
        "tree-sitter-vue exposes <script> contents as raw text, not TypeScript AST nodes"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn vue_unsupported_formatter_and_import_organize_return_clean_errors() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("src/App.vue");

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let write = send(
        &mut aft,
        json!({
            "id": "vue-write",
            "command": "write",
            "file": file,
            "content": VUE_FIXTURE,
        }),
    );
    assert_eq!(
        write["success"], true,
        "Vue write should succeed: {write:?}"
    );
    assert_eq!(write["formatted"], false);
    assert_eq!(write["format_skipped_reason"], "unsupported_language");
    assert_eq!(write["syntax_valid"], true);

    let organize = send(
        &mut aft,
        json!({
            "id": "vue-organize-imports",
            "command": "organize_imports",
            "file": file,
        }),
    );
    assert_eq!(
        organize["success"], false,
        "organize_imports should reject Vue: {organize:?}"
    );
    assert_eq!(organize["code"], "invalid_request");
    assert!(
        organize["message"]
            .as_str()
            .expect("error message")
            .contains("Vue"),
        "message should name Vue: {organize:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}
