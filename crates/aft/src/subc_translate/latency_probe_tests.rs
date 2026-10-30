//! Opt-in probes for the work a tool call does before its handler runs: the
//! argument translation and the undo snapshot an edit takes first.
//!
//! Both are `#[ignore]`d because the numbers depend on the machine. Run them
//! with:
//!
//! ```text
//! cargo test -p agent-file-tools --lib -- --ignored --nocapture latency_probe_tests
//! ```
//!
//! The recorded results live in
//! `.cortexkit/internal-docs/investigations/edit-translate-latency.md`.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SAMPLES: usize = 50;

fn root(name: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("edit-translate-probe")
        .join(name);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create probe root");
    root
}

fn fixture_file(root: &Path, name: &str, lines: usize, token: &str) -> PathBuf {
    let path = root.join(name);
    let mut body = String::new();
    for index in 0..lines {
        if index % 40 == 7 {
            body.push_str(&format!("const VALUE_{index} = \"{token}\";\n"));
        } else {
            body.push_str(&format!(
                "export function fn_{index}() {{\n  return {index};\n}}\n"
            ));
        }
    }
    std::fs::write(&path, body).expect("write fixture");
    path
}

fn ten_hunk_find_replace(padding: usize) -> Value {
    let filler = "x".repeat(padding);
    let hunks: Vec<Value> = (0..10)
        .map(|index| {
            let line = 40 * index + 7;
            json!({
                "oldString": format!("const VALUE_{line} = \"TOKEN_A\"; {filler}"),
                "newString": format!("const VALUE_{line} = \"TOKEN_B\"; {filler}"),
            })
        })
        .collect();
    json!({ "path": "two.ts", "edits": hunks })
}

fn ten_hunk_lines() -> Value {
    let items: Vec<Value> = (0..10)
        .map(|index| {
            let line = index * 40 + 1;
            json!({ "startLine": line, "endLine": line + 1, "content": format!("// block {index}\n") })
        })
        .collect();
    json!({ "path": "two.ts", "edits": items })
}

fn patch(hunks: usize) -> String {
    let mut text = String::from("*** Begin Patch\n");
    text.push_str("*** Update File: one.ts\n");
    for index in 0..hunks {
        let line = 40 * index + 7;
        text.push_str(&format!("@@ const VALUE_{line} @@\n"));
        text.push_str(&format!("-const VALUE_{line} = \"TOKEN_A\";\n"));
        text.push_str(&format!("+const VALUE_{line} = \"TOKEN_B\";\n"));
    }
    text.push_str("*** End Patch\n");
    text
}

fn translate_cases() -> Vec<(&'static str, &'static str, Value)> {
    let body = "export const x = 1;\n".repeat(300);
    vec![
        (
            "edit",
            "find/replace 1 hunk",
            json!({
                "path": "one.ts",
                "edits": [{ "oldString": "const VALUE_47 = \"TOKEN_A\";", "newString": "const VALUE_47 = \"TOKEN_B\";" }],
            }),
        ),
        ("edit", "find/replace 10 hunks", ten_hunk_find_replace(0)),
        (
            "edit",
            "find/replace 10 hunks, 4 KB",
            ten_hunk_find_replace(200),
        ),
        ("edit", "line-range 10 hunks", ten_hunk_lines()),
        (
            "edit",
            "oldString/newString folded",
            json!({
                "path": "one.ts",
                "oldString": "const VALUE_7 = \"TOKEN_A\";",
                "newString": "const VALUE_7 = \"TOKEN_B\";",
            }),
        ),
        (
            "write",
            "6 KB body",
            json!({ "path": "three.ts", "content": body }),
        ),
        ("apply_patch", "1 hunk", json!({ "patchText": patch(1) })),
        ("apply_patch", "10 hunks", json!({ "patchText": patch(10) })),
    ]
}

fn translate(tool: &str, args: &Value, project_root: &Path, app_ctx: &crate::context::AppContext) {
    // The probe `run_tool_call` installs for a glob-spelled target. It is
    // consulted only for arguments that reach the glob branch, so a call it
    // receives names a filesystem validation the prepare performed.
    let probe = |resolved: &str| {
        crate::commands::edit_match::classify_edit_target(resolved, app_ctx, "probe")
            != crate::commands::edit_match::EditTargetKind::Literal
    };
    let typed = args.as_object().expect("probe arguments are an object");
    if let Err(error) = crate::subc_translate::subc_translate_owned_with_edit_target_probe(
        tool,
        Value::Object(typed.clone()),
        project_root,
        crate::subc_translate::TranslateContext::default(),
        &probe,
    ) {
        panic!("probe arguments must translate: {:?}", error.message);
    }
}

fn pair(mut samples: Vec<Duration>) -> (f64, f64) {
    samples.sort_unstable();
    let p50 = samples[samples.len() / 2].as_secs_f64() * 1000.0;
    let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)].as_secs_f64() * 1000.0;
    (p50, p95)
}

#[test]
#[ignore = "timing and allocation probe; run with --ignored --nocapture"]
fn edit_translate_timing_probe() {
    let project_root = root("translate");
    fixture_file(&project_root, "one.ts", 400, "TOKEN_A");
    fixture_file(&project_root, "two.ts", 400, "TOKEN_A");
    fixture_file(&project_root, "three.ts", 400, "TOKEN_C");
    let app_ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config::default(),
    );

    println!(
        "{:12} {:28} {:>10} {:>10} {:>12}",
        "tool", "shape", "p50 ms", "p95 ms", "allocations"
    );
    for (tool, label, args) in translate_cases() {
        translate(tool, &args, &project_root, &app_ctx);

        let mut samples = Vec::new();
        for _ in 0..SAMPLES {
            let started = Instant::now();
            translate(tool, &args, &project_root, &app_ctx);
            samples.push(started.elapsed());
        }
        let (p50, p95) = pair(samples);

        // The crate's test allocator counts this thread's allocations, so the
        // number is the translate call's own.
        let (_, used) = crate::test_allocations::count(|| {
            translate(tool, &args, &project_root, &app_ctx);
        });
        println!("{tool:12} {label:28} {p50:10.3} {p95:10.3} {used:12}");
    }
}

/// A store rooted at `root/backups`, the layout the daemon uses. Each timing
/// sample gets its own store so an earlier sample's history cannot carry into
/// the depth a later one starts from.
fn backup_store(root: &Path) -> crate::backup::BackupStore {
    std::fs::create_dir_all(root).expect("create store root");
    let mut store = crate::backup::BackupStore::new();
    // The production layout: one storage directory holding per-session stacks.
    store.set_storage_dir(root.join("backups"), 24);
    store.set_db_project_key("edit-translate-probe".to_string());
    store
}

/// Time `SAMPLES` snapshots of one file, each starting from the same stack
/// depth. Every sample gets a fresh store and session prepared to `depth`
/// entries outside the timed section, because `BackupStore` appends to the
/// per-file stack: reusing one store would carry the previous samples' history
/// forward and the 50 calls alone would reach the undo depth cap.
fn snapshot_samples(root: &Path, fixture: &Path, label: &str, depth: usize) -> f64 {
    let mut samples = Vec::new();
    for _ in 0..SAMPLES {
        let store_root = root.join(format!("store-{label}-{}", samples.len()));
        let mut store = backup_store(&store_root);
        let session = "probe-session";
        for index in 0..depth {
            store
                .snapshot_with_op(session, fixture, &format!("warm {index}"), None)
                .expect("warm snapshot");
        }
        let started = Instant::now();
        store
            .snapshot_with_op(session, fixture, "probe", None)
            .expect("snapshot");
        samples.push(started.elapsed());
        // Only the undo history is thrown away; the file under test lives in
        // the parent fixture directory.
        let _ = std::fs::remove_dir_all(&store_root);
    }
    pair(samples).0
}

#[test]
#[ignore = "timing probe; run with --ignored --nocapture"]
fn backup_snapshot_cost_probe() {
    let root = root("backup");
    let file = fixture_file(&root, "big.ts", 2000, "TOKEN_A");
    let five_files: Vec<PathBuf> = (0..5)
        .map(|index| fixture_file(&root, &format!("five_{index}.ts"), 2000, "TOKEN_A"))
        .collect();
    let empty = snapshot_samples(&root, &file, "empty", 0);
    println!("first snapshot of a 2,000-line file: p50 {empty:.3} ms");
    let warm = snapshot_samples(&root, &file, "three", 3);
    println!("snapshot on a stack of 3 entries:     p50 {warm:.3} ms");
    let deep = snapshot_samples(&root, &file, "ten", 10);
    println!("snapshot on a stack of 10 entries:    p50 {deep:.3} ms");

    let mut batch = Vec::new();
    for sample in 0..SAMPLES {
        let store_root = root.join(format!("store-batch-{sample}"));
        let mut store = backup_store(&store_root);
        let started = Instant::now();
        for path in &five_files {
            store
                .snapshot_with_op("probe-session", path, "batch", Some("probe-op"))
                .expect("batch snapshot");
        }
        batch.push(started.elapsed());
        let _ = std::fs::remove_dir_all(&store_root);
    }
    let batch_p50 = pair(batch).0;
    println!(
        "five files in one operation:          p50 {batch_p50:.3} ms (per file {:.3} ms)",
        batch_p50 / 5.0
    );
}
