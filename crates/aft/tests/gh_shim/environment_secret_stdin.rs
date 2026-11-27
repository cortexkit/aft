use super::*;
use std::io::Write;
use std::process::Stdio;

#[test]
fn environment_secret_input_is_not_in_argv_diagnostics_or_audit() {
    const VALUE: &str = "synthetic-stdin-sentinel-not-for-output-198830";
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config");
    let state = temp.path().join("state");
    let home = temp.path().join("home");
    let project = write_project_repo(temp.path());
    let upstream = temp.path().join("upstream");
    let recorder = temp.path().join("argv.txt");
    fs::create_dir_all(&upstream).unwrap();
    let audit = state.join("cortexkit/aft/gh-shim/operator-bypass.jsonl");
    test_helpers::executable_cache::write_executable(
        &upstream.join("gh"),
        format!(
            "#!/bin/sh\n[ -s \"$GH_SHIM_TEST_AUDIT\" ] || exit 70\n[ \"$GH_PROMPT_DISABLED\" = 1 ] && [ -z \"${{GH_DEBUG+x}}\" ] || exit 71\nprintf '%s\\n' \"$@\" > \"$GH_SHIM_TEST_RECORD\"\nIFS= read -r value || exit 72\n[ \"$value\" = {VALUE} ] || exit 73\nprintf '%s\\n' \"$value\"\nprintf '%s\\n' \"$value\" >&2\nexit 74\n"
        ).as_bytes(),
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

    let shell = |args: &[&str]| {
        let mut cmd = shim_command(args, &project, &config, &state, &home, &upstream, &recorder);
        cmd.env("GH_SHIM_BYPASS", "operator")
            .env("GH_SHIM_TEST_AUDIT", &audit)
            .env("GH_DEBUG", "api")
            .env_remove("AFT_GH_SHIM_TICKET")
            .env_remove("AFT_GH_SHIM_WORKER")
            .env_remove("AFT_GH_WORKER_WRITE_POLICY")
            .env_remove("AFT_GH_SPEECH_PROVISIONED");
        cmd
    };
    let args = [
        "secret",
        "set",
        "RELEASE_KEY",
        "--env",
        "release",
        "-R",
        "test-org/unbound",
    ];
    let missing = shell(&args).stdin(Stdio::null()).output().unwrap();
    assert_eq!(missing.status.code(), Some(86));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("stdin from a pipe or file"));
    assert!(!audit.exists());
    assert!(!recorder.exists());
    let mut child = shell(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "{VALUE}").unwrap();
    let result = child.wait_with_output().unwrap();
    // 74 proves the fake gh received the real stdin and attempted to echo it.
    assert_eq!(result.status.code(), Some(74));
    assert!(result.stdout.is_empty());
    assert_eq!(result.stderr, b"gh-shim: upstream gh failed to set the environment secret; its output was suppressed to protect the secret value\n");
    assert_eq!(
        fs::read_to_string(&recorder).unwrap(),
        "secret\nset\nRELEASE_KEY\n--env\nrelease\n--repo\ngithub.com/test-org/unbound\n"
    );
    let record: Value = serde_json::from_slice(&fs::read(&audit).unwrap()).unwrap();
    assert_eq!(
        record,
        json!({ "as_of_unix_secs": record["as_of_unix_secs"], "tuple": "secret set", "repository": "test-org/unbound", "environment": "release", "secret_name": "RELEASE_KEY" })
    );

    let status = shell(&["--status", "--audit"]).output().unwrap();
    assert!(status.status.success());
    assert!(!String::from_utf8_lossy(&status.stdout).contains(VALUE));
    assert!(!String::from_utf8_lossy(&status.stderr).contains(VALUE));
    fn assert_no_value(path: &Path) {
        if path.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                assert_no_value(&entry.unwrap().path());
            }
        } else if path.is_file() {
            let bytes = fs::read(path).unwrap();
            assert!(
                !bytes
                    .windows(VALUE.len())
                    .any(|window| window == VALUE.as_bytes()),
                "value found in {}",
                path.display()
            );
        }
    }
    for root in [&state, &home, &config] {
        assert_no_value(root);
    }
}
