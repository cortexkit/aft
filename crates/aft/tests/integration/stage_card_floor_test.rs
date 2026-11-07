//! Placement refusal: `scripts/stage-card.sh` refuses a card below the live
//! storage root's reader floor.
//!
//! These tests drive the gate stage-card runs before a card is staged
//! (`scripts/lib/storage-floor-check.sh`) with the real `aft` binary of this
//! build as the card and a temporary storage root, so the comparison runs
//! against the card's own `--formats` report rather than a stub of it.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use aft::persisted_format::PersistedStore;
use serde_json::{json, Value};

fn repo_root() -> PathBuf {
    crate::test_helpers::manifest_dir()
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn card() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_aft"))
}

fn check(candidate: &Path, storage_root: &Path) -> Output {
    Command::new("bash")
        .arg(repo_root().join("scripts/lib/storage-floor-check.sh"))
        .arg(candidate)
        .arg(storage_root)
        .output()
        .expect("run storage-floor-check.sh")
}

fn card_formats() -> Value {
    let output = Command::new(card())
        .arg("--formats")
        .output()
        .expect("run aft --formats");
    assert!(output.status.success(), "aft --formats failed: {output:?}");
    serde_json::from_slice(&output.stdout).expect("--formats prints JSON")
}

fn write_floor(root: &Path, stores: Value) {
    fs::write(
        root.join("reader-floor.json"),
        serde_json::to_vec(&json!({ "floor_schema": 1, "stores": stores })).unwrap(),
    )
    .unwrap();
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn formats_flag_reports_every_store_this_build_reads() {
    let formats = card_formats();
    for store in PersistedStore::ALL {
        assert_eq!(
            formats["stores"][store.name()],
            json!(store.supported()),
            "--formats must report {store}"
        );
    }
    assert_eq!(
        formats["floor_schema"],
        json!(aft::reader_floor::FLOOR_SCHEMA)
    );
}

#[test]
fn a_floor_equal_to_the_card_stages() {
    let root = tempfile::tempdir().unwrap();
    write_floor(root.path(), card_formats()["stores"].clone());
    let output = check(&card(), root.path());
    assert!(
        output.status.success(),
        "equal floor must stage: {}",
        stderr(&output)
    );
}

#[test]
fn a_floor_written_by_this_build_at_startup_stages() {
    let root = tempfile::tempdir().unwrap();
    // The baseline configure writes when it first opens a storage root.
    let baseline = PersistedStore::ALL
        .into_iter()
        .map(|store| (store, store.written()))
        .collect::<Vec<_>>();
    aft::reader_floor::raise(root.path(), &baseline).unwrap();
    let output = check(&card(), root.path());
    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
fn a_floor_above_the_card_refuses_staging_and_names_the_store() {
    let root = tempfile::tempdir().unwrap();
    let mut stores = card_formats()["stores"].clone();
    let above = PersistedStore::SemanticIndex.supported() + 1;
    stores["semantic"] = json!(above);
    write_floor(root.path(), stores);
    let output = check(&card(), root.path());
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let message = stderr(&output);
    assert!(message.contains("semantic needs format"), "{message}");
    assert!(message.contains(&above.to_string()), "{message}");
}

#[test]
fn a_floor_naming_a_store_the_card_does_not_know_refuses_staging() {
    let root = tempfile::tempdir().unwrap();
    let mut stores = card_formats()["stores"].clone();
    stores["store_from_a_newer_build"] = json!(1);
    write_floor(root.path(), stores);
    let output = check(&card(), root.path());
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("store_from_a_newer_build"));
}

#[test]
fn a_newer_floor_schema_refuses_staging() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("reader-floor.json"),
        serde_json::to_vec(&json!({ "floor_schema": 2, "stores": {} })).unwrap(),
    )
    .unwrap();
    let output = check(&card(), root.path());
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("floor_schema"));
}

#[test]
fn a_card_that_cannot_report_its_formats_is_refused() {
    let root = tempfile::tempdir().unwrap();
    write_floor(root.path(), card_formats()["stores"].clone());
    // A build from before the floor scheme has no --formats flag.
    let old_card = root.path().join("old-card");
    crate::test_helpers::executable_cache::link_cached_executable(
        &old_card,
        b"#!/bin/sh\necho 'aft 0.57.2'\nexit 0\n",
    )
    .unwrap();
    let output = check(&old_card, root.path());
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("--formats"), "{}", stderr(&output));
}

#[test]
fn a_root_without_a_floor_stages() {
    let root = tempfile::tempdir().unwrap();
    let output = check(&card(), root.path());
    assert!(output.status.success(), "{}", stderr(&output));
}

/// stage-card must run the gate on the card before it is staged or declared
/// current, and stop when the gate refuses.
#[test]
fn stage_card_runs_the_floor_gate_before_staging_the_card() {
    let script = fs::read_to_string(repo_root().join("scripts/stage-card.sh")).unwrap();
    let gate = script
        .find("if ! ck_aft_check_storage_floor \"$TMP\" \"$STORAGE_ROOT\"; then")
        .expect("stage-card calls the floor gate on the card");
    let staged = script
        .find("mv \"$TMP\" \"$STAGING/$CARD\"")
        .expect("stage-card stages the card");
    let current = script
        .find("> \"$STAGING/ck-aft.current\"")
        .expect("stage-card declares the current card");
    assert!(
        gate < staged && gate < current,
        "the gate must run before staging"
    );
    assert!(
        script[gate..staged].contains("exit 2"),
        "a refused card must stop stage-card"
    );
}

/// Test-launched candidate probes must not borrow the production executable name.
#[test]
fn stage_card_executes_candidates_under_ckdev_names() {
    let root = repo_root();
    let script = fs::read_to_string(root.join("scripts/stage-card.sh")).unwrap();
    let signature = fs::read_to_string(root.join("scripts/lib/ck-aft-signature.sh")).unwrap();

    assert!(
        script.contains(r#"TMP="$(mktemp "$STAGING/ckdev-aft.tmp.XXXXXX")""#),
        "the candidate that receives the format probe must have a developer name"
    );
    assert!(
        script.contains(r#"VERSION_BINARY="$(mktemp "$STAGING/ckdev-aft.version.XXXXXX")""#)
            && script.contains(r#"VERSION="$("$VERSION_BINARY" --version"#),
        "the staged card version probe must execute a ckdev copy"
    );
    assert!(
        signature.contains(r#"cp "$binary" "$work/ckdev-aft""#)
            && signature.contains(r#""$work/ckdev-aft" warmup"#),
        "the ONNX smoke test must execute a ckdev copy"
    );
}
