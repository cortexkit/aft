#![cfg(unix)]

use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use aft::bash_rewrite::differential::{parse_corpus, AggregateFailures, CorpusRow};
use aft::bash_rewrite::dispatch::{DispatchRecord, DispatchRoute};
use aft::bash_rewrite::observation::{
    observation_from_aft_response, observation_from_process, observation_summary,
    reduce_observation,
};
use serde_json::{json, Value};

use crate::test_helpers::{user_config, AftProcess};

const CORPUS: &str = include_str!("../fixtures/bash_rewrite_diff/corpus.toml");
const CONTROLLED_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:/opt/homebrew/bin";

#[test]
fn schema_and_branch_inventory_are_closed() {
    let corpus = parse_corpus(CORPUS).expect("bash rewrite corpus validates");
    assert_eq!(corpus.schema_version, 1);
    assert!(corpus.rows.iter().any(|row| row.route == "native"));
}

#[test]
fn branch_coverage_mutation_is_rejected() {
    let broken = CORPUS.replace("branch_ids = [\"grep.accept\"]", "branch_ids = []");
    let error = parse_corpus(&broken).expect_err("removing a branch row must fail");
    assert!(error.contains("grep.accept"), "coverage error: {error}");
}

#[test]
fn aggregate_failure_scratch_rows_are_reported_together() {
    let mut failures = AggregateFailures::default();
    failures.push("scratch-one", "first deliberate mismatch");
    failures.push("scratch-two", "second deliberate mismatch");
    let report = failures
        .finish()
        .expect_err("scratch failures must be visible");
    assert!(report.contains("scratch-one") && report.contains("scratch-two"));
}

#[test]
fn iterating_differential_harness_compares_every_available_row() {
    let corpus = parse_corpus(CORPUS).expect("bash rewrite corpus validates");
    let mut failures = AggregateFailures::default();
    let mut executed = 0;
    let mut skipped = 0;

    for row in &corpus.rows {
        if !row.platform.enabled_on_host() {
            skipped += 1;
            continue;
        }
        if !utility_available(row) {
            skipped += 1;
            continue;
        }
        executed += 1;
        if let Err(error) = run_row(row) {
            failures.push(
                &row.id,
                format!(
                    "command={:?}; workdir={:?}; manifest={:?}; basis={}; normalizations={:?}; expectations={:?}; env={{PATH:{:?}, LC_ALL:C, LANG:C, HOME:<side-root>/home}}; {error}",
                    row.command,
                    row.workdir,
                    row.manifest,
                    row.basis,
                    row.normalizations,
                    row.expectations,
                    CONTROLLED_PATH,
                ),
            );
        }
    }

    assert!(executed > 0, "the differential corpus executed no rows");
    failures
        .finish()
        .unwrap_or_else(|report| panic!("{report}\nexecuted={executed} skipped={skipped}"));
}

fn utility_available(row: &CorpusRow) -> bool {
    let Some(utility) = row.command.split_whitespace().next() else {
        return false;
    };
    if matches!(utility, "printf" | "echo") {
        return true;
    }
    Command::new(utility)
        .arg("--version")
        .env("PATH", CONTROLLED_PATH)
        .output()
        .is_ok()
}

fn run_row(row: &CorpusRow) -> Result<(), String> {
    let oracle_root = tempfile::tempdir().map_err(|error| error.to_string())?;
    let aft_root = tempfile::tempdir().map_err(|error| error.to_string())?;
    let oracle_home = oracle_root.path().join("home");
    let aft_home = aft_root.path().join("home");
    fs::create_dir_all(&oracle_home).map_err(|error| error.to_string())?;
    fs::create_dir_all(&aft_home).map_err(|error| error.to_string())?;
    row.materialize(oracle_root.path())?;
    row.materialize(aft_root.path())?;
    let oracle_before = aft::bash_rewrite::differential::manifest_for(oracle_root.path())?;
    let aft_before = aft::bash_rewrite::differential::manifest_for(aft_root.path())?;
    if oracle_before != aft_before {
        return Err("independent fixture manifests differ before execution".to_string());
    }

    let oracle = run_oracle(row, oracle_root.path(), &oracle_home)?;
    let route_file = tempfile::NamedTempFile::new().map_err(|error| error.to_string())?;
    let storage = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut aft = AftProcess::spawn_with_env(&[
        ("PATH", OsStr::new(CONTROLLED_PATH)),
        ("LC_ALL", OsStr::new("C")),
        ("LANG", OsStr::new("C")),
        (
            "AFT_BASH_REWRITE_ROUTE_RECORD",
            route_file.path().as_os_str(),
        ),
    ]);

    let configured = aft.send(
        &json!({
            "id": format!("{}-configure", row.id),
            "command": "configure",
            "harness": "runner",
            "project_root": aft_root.path(),
            "storage_dir": storage.path(),
            "config": user_config(json!({
                "bash": { "rewrite": true },
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": false,
            })),
        })
        .to_string(),
    );
    if configured["success"] != true {
        let _ = aft.shutdown();
        return Err(format!("configure failed: {configured}"));
    }

    let workdir = if row.workdir.is_empty() {
        aft_root.path().to_path_buf()
    } else {
        aft_root.path().join(&row.workdir)
    };
    let aft_response = run_aft(&mut aft, row, &workdir, &aft_home);
    let shutdown = aft.shutdown();
    if !shutdown.success() {
        return Err(format!("AFT shutdown failed: {shutdown:?}"));
    }
    let aft_response = aft_response?;
    let record = read_route_record(route_file.path())
        .map_err(|error| format!("{error}; configure={configured}; AFT response={aft_response}"))?;
    assert_route(row, &record)?;

    let aft_exit_code = if row.route.starts_with("rewritten:") {
        Some(0)
    } else {
        aft_response
            .get("exit_code")
            .and_then(Value::as_i64)
            .map(|code| code as i32)
    };
    let mut aft_observation =
        observation_from_aft_response(&aft_response, aft_root.path(), aft_exit_code);
    if row.mutating && !row.route.starts_with("native") {
        // The internal edit response is a tool presentation, not shell
        // stdout. Mutation rows compare the exact filesystem manifest below.
        aft_observation.raw_stdout.clear();
        aft_observation.structured.stdout.clear();
    }
    let basis = reduce_observation(
        &aft_observation,
        &row.basis,
        &row.normalizations
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    )?;
    let oracle_basis = reduce_observation(
        &oracle,
        &row.basis,
        &row.normalizations
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    )?;
    if basis != oracle_basis {
        return Err(format!(
            "basis {} differs\nAFT={basis}\nORACLE={oracle_basis}\nraw_aft={}\nraw_oracle={}",
            row.basis,
            observation_summary(&aft_observation),
            observation_summary(&oracle),
        ));
    }

    let oracle_after = aft::bash_rewrite::differential::manifest_for(oracle_root.path())?;
    let aft_after = aft::bash_rewrite::differential::manifest_for(aft_root.path())?;
    if row.mutating {
        if oracle_after != aft_after {
            return Err(format!(
                "mutating final manifests differ\nORACLE={oracle_after:?}\nAFT={aft_after:?}"
            ));
        }
    } else if oracle_after != oracle_before || aft_after != aft_before {
        return Err("read-only row changed its filesystem manifest".to_string());
    }
    Ok(())
}

fn run_oracle(
    row: &CorpusRow,
    root: &Path,
    home: &Path,
) -> Result<aft::bash_rewrite::observation::Observation, String> {
    let output = Command::new("/bin/bash")
        .args(["--noprofile", "--norc", "-c", &row.command])
        .current_dir(root.join(&row.workdir))
        .env_clear()
        .env("PATH", CONTROLLED_PATH)
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("HOME", home)
        .output()
        .map_err(|error| format!("oracle spawn failed: {error}"))?;
    Ok(observation_from_process(
        &output.stdout,
        &output.stderr,
        output.status.code(),
        root,
    ))
}

fn run_aft(
    aft: &mut AftProcess,
    row: &CorpusRow,
    workdir: &Path,
    home: &Path,
) -> Result<Value, String> {
    // PATH is deliberately inherited from the AFT child process. The public
    // bash contract rejects PATH in the per-command env map, while the child
    // process itself is launched with the controlled PATH above.
    let env = json!({
        "LC_ALL": "C",
        "LANG": "C",
        "HOME": home,
    });
    let mut response = aft.send(
        &json!({
            "id": row.id,
            "command": "bash",
            "params": {
                "command": row.command,
                "workdir": workdir,
                "env": env,
                "compressed": false,
            },
        })
        .to_string(),
    );
    if response["status"] == "running" {
        let task_id = response["task_id"]
            .as_str()
            .ok_or_else(|| format!("running response has no task_id: {response}"))?
            .to_string();
        let started = Instant::now();
        loop {
            if started.elapsed() > Duration::from_secs(30) {
                return Err(format!("native task timed out: {response}"));
            }
            response = aft.send(
                &json!({
                    "id": format!("{}-status", row.id),
                    "command": "bash_status",
                    "params": { "task_id": task_id },
                })
                .to_string(),
            );
            if response["status"] == "completed" || response["status"] == "failed" {
                if let Some(preview) = response.get("output_preview").and_then(Value::as_str) {
                    response["output"] = Value::String(preview.to_string());
                }
                break;
            }
        }
    }
    Ok(response)
}

fn read_route_record(path: &Path) -> Result<DispatchRecord, String> {
    let content =
        fs::read_to_string(path).map_err(|error| format!("route record missing: {error}"))?;
    let records = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<DispatchRecord>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("invalid route record: {error}"))?;
    if records.len() != 1 {
        return Err(format!(
            "expected exactly one route record, got {}",
            records.len()
        ));
    }
    Ok(records.into_iter().next().expect("one route record"))
}

fn assert_route(row: &CorpusRow, record: &DispatchRecord) -> Result<(), String> {
    if record.request_id != row.id {
        return Err(format!("route request ID is {}", record.request_id));
    }
    match (row.route.strip_prefix("rewritten:"), &record.route) {
        (Some(expected_rule), DispatchRoute::Rewritten { rule_id, .. })
            if expected_rule == rule_id =>
        {
            Ok(())
        }
        (None, DispatchRoute::Native { .. }) if row.route == "native" => Ok(()),
        _ => Err(format!(
            "declared route {} but observed {:?}",
            row.route, record.route
        )),
    }
}

#[test]
fn sandbox_locking_control_never_dispatches_rewrite() {
    use aft::bash_rewrite::dispatch::route_record;
    use aft::bash_rewrite::try_rewrite_for_request;
    use aft::config::{Config, SandboxConfig};
    use aft::context::AppContext;
    use aft::parser::TreeSitterProvider;
    use aft::sandbox_spawn::AuthenticatedPrincipal;

    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("note.txt"), "note\n").unwrap();
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config {
            project_root: Some(root.path().to_path_buf()),
            experimental_bash_rewrite: true,
            sandbox: SandboxConfig {
                enabled: true,
                ..SandboxConfig::default()
            },
            ..Config::default()
        }),
    );
    assert!(try_rewrite_for_request(
        "cat note.txt",
        "sandbox-lock",
        None,
        &ctx,
        &AuthenticatedPrincipal::FirstParty,
    )
    .is_none());
    assert!(matches!(
        route_record("sandbox-lock").unwrap().route,
        DispatchRoute::Native { role, .. } if role == "sandbox"
    ));
}
