use super::*;
use std::io::Write;
use std::process::Stdio;

#[test]
fn variable_set_passes_real_pipe_to_native_gh() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config");
    let state = temp.path().join("state");
    let home = temp.path().join("home");
    let project = write_project_repo(temp.path());
    let upstream = temp.path().join("upstream");
    let recorder = temp.path().join("argv.txt");
    let audit = state.join("cortexkit/aft/gh-shim/operator-bypass.jsonl");
    fs::create_dir_all(&upstream).unwrap();
    test_helpers::executable_cache::write_executable(
        &upstream.join("gh"),
        b"#!/bin/sh\n[ -s \"$GH_SHIM_TEST_AUDIT\" ] || exit 70\nprintf '%s\\n' \"$@\" > \"$GH_SHIM_TEST_RECORD\"\nIFS= read -r value || exit 71\nprintf '%s\\n' \"$value\"\n",
    ).unwrap();
    let mut manifest: Value =
        serde_json::from_str(include_str!("../fixtures/gh_shim/v10-manifest.json")).unwrap();
    let additions: Value =
        serde_json::from_str(include_str!("../fixtures/gh_shim/v30-additions.json")).unwrap();
    manifest["tiers"]["admin"]
        .as_array_mut()
        .unwrap()
        .extend(additions["tiers"]["admin"].as_array().unwrap().clone());
    write_manifest_value(&state, unix_seconds(), manifest, 30);
    write_user_config(&config, &write_dead_connection_file(temp.path()), None);
    let mut child = shim_command(
        &[
            "variable",
            "set",
            "REQUIRES_CORE",
            "--env",
            "alpha",
            "-R",
            "test-org/unbound",
        ],
        &project,
        &config,
        &state,
        &home,
        &upstream,
        &recorder,
    )
    .env("GH_SHIM_BYPASS", "operator")
    .env("GH_SHIM_TEST_AUDIT", &audit)
    .env_remove("AFT_GH_SHIM_TICKET")
    .env_remove("AFT_GH_SHIM_WORKER")
    .env_remove("AFT_GH_WORKER_WRITE_POLICY")
    .env_remove("AFT_GH_SPEECH_PROVISIONED")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    writeln!(child.stdin.take().unwrap(), "0.24.8").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"0.24.8\n");
    assert_eq!(
        fs::read_to_string(&recorder).unwrap(),
        "variable\nset\nREQUIRES_CORE\n--repo\ngithub.com/test-org/unbound\n--env\nalpha\n"
    );
    let record: Value = serde_json::from_slice(&fs::read(&audit).unwrap()).unwrap();
    assert_eq!(
        record,
        json!({ "as_of_unix_secs": record["as_of_unix_secs"], "tuple": "variable set", "repository": "test-org/unbound", "environment": "alpha", "variable_name": "REQUIRES_CORE" })
    );
}
