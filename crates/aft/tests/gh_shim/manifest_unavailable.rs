use super::*;
use std::os::unix::fs::PermissionsExt;
use std::process::Output;

const UNAVAILABLE: &str = "gh-shim: gh_shim_manifest_unavailable: GitHub writes are refused because this machine's signed routing manifest is missing or invalid; without it the write would run as the operator's own account. Ask the operator to repair the manifest. Reads still work.\n";
const UNPINNED: &str = "gh-shim: gh_shim_manifest_unavailable: GitHub writes are refused because this shell has no usable pinned policy root. Start a new shell. If the problem persists, ask the operator to check the daemon and run `ck policy-root set` if no root is pinned. Known safe commands still work.\n";

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    state_home: PathBuf,
    state: PathBuf,
    config: PathBuf,
    bin: PathBuf,
    record: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let state_home = root.path().join("state");
        let state = state_home.join("cortexkit/aft/gh-shim");
        let config = root.path().join("config");
        let bin = root.path().join("bin");
        for dir in [&home, &state, &config, &bin] {
            fs::create_dir_all(dir).unwrap();
        }
        let record = root.path().join("upstream-record");
        let gh = bin.join("gh");
        fs::write(
            &gh,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$FAKE_GH_RECORD\"\nprintf '%s\\n' \"$HOME\" \"$XDG_STATE_HOME\" \"$GH_TOKEN\" \"$GITHUB_TOKEN\" \"${GH_SHIM_BYPASS-unset}\" >> \"$FAKE_GH_RECORD\"\nexit 73\n",
        )
        .unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            _root: root,
            home,
            state_home,
            state,
            config,
            bin,
            record,
        }
    }

    fn command(&self, args: &[&str], bypass: bool) -> Command {
        let mut command = Command::new(aft_binary());
        command
            .env_clear()
            .arg("gh-shim")
            .args(args)
            .current_dir(&self.home)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("AFT_GH_SHIM_STATE_DIR", &self.state)
            .env(
                "AFT_GH_SHIM_TRUST_ANCHOR",
                crate::test_helpers::gh_fixture_anchor(),
            )
            .env("AFT_STORAGE_DIR", self._root.path().join("storage"))
            .env("PATH", &self.bin)
            .env("GH_REPO", "cortexkit/aft")
            .env("GH_TOKEN", "synthetic-gh-token")
            .env("GITHUB_TOKEN", "synthetic-github-token")
            .env("FAKE_GH_RECORD", &self.record);
        if bypass {
            command.env("GH_SHIM_BYPASS", "operator");
        }
        command
    }

    fn run(&self, args: &[&str], bypass: bool) -> Output {
        let _ = fs::remove_file(&self.record);
        self.command(args, bypass).output().unwrap()
    }

    fn connect_daemon(&self, daemon: &SlowTestDaemon) {
        let connection = self._root.path().join("connection.json");
        daemon.write_connection_file(&connection);
        write_user_config(&self.config, &connection, Some(true));
    }

    fn disable_shim(&self) {
        let directory = self.config.join("cortexkit");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("aft.jsonc"), r#"{"github":{"shim":false}}"#).unwrap();
    }

    fn assert_passthrough(&self, args: &[&str], bypass: bool) {
        let output = self.run(args, bypass);
        assert_eq!(output.status.code(), Some(73), "{output:?}");
        let mut expected = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        expected.extend([
            self.home.display().to_string(),
            self.state_home.display().to_string(),
            "synthetic-gh-token".into(),
            "synthetic-github-token".into(),
            if bypass { "operator" } else { "unset" }.into(),
        ]);
        assert_eq!(
            fs::read_to_string(&self.record).unwrap(),
            format!("{}\n", expected.join("\n"))
        );
    }

    fn assert_refused(&self, args: &[&str], bypass: bool, text: &str) {
        let output = self.run(args, bypass);
        assert_eq!(output.status.code(), Some(86), "{args:?}: {output:?}");
        assert_eq!(output.stderr, text.as_bytes());
        assert!(
            !self.record.exists(),
            "refused command executed upstream gh"
        );
    }

    fn high_water(&self) {
        fs::write(
            self.state.join("manifest-version-high-water.json"),
            r#"{"newest_accepted_version":12}"#,
        )
        .unwrap();
    }

    fn activate(&self) {
        write_fresh_v12_manifest(&self.state_home, unix_seconds());
        self.assert_passthrough(&["issue", "view", "1"], false);
        assert!(self.state.join("last-valid-manifest.json").is_file());
        assert!(self
            .state
            .join("manifest-version-high-water.json")
            .is_file());
    }

    fn assert_unavailable(&self) {
        for bypass in [false, true] {
            for args in [
                vec!["issue", "view", "1"],
                vec!["pr", "list"],
                vec!["api", "repos/cortexkit/aft/issues"],
                vec!["--version"],
                vec!["help"],
            ] {
                self.assert_passthrough(&args, bypass);
            }
            for repository in ["cortexkit/aft", "outside/unbound"] {
                for args in [
                    vec!["issue", "comment", "1", "--body", "offline"],
                    vec!["pr", "comment", "1", "--body", "offline"],
                    vec!["pr", "review", "1", "--comment", "--body", "offline"],
                    vec!["issue", "close", "1"],
                    vec!["pr", "close", "1"],
                    vec!["pr", "merge", "1", "--squash"],
                    vec!["api", "--method", "POST", "repos/cortexkit/aft/issues"],
                    vec!["unknown-extension", "write"],
                ] {
                    let args = [args, vec!["-R", repository]].concat();
                    self.assert_refused(&args, bypass, UNAVAILABLE);
                }
            }
        }
    }
}

#[test]
fn gh_shim_manifest_unavailable_invalid_without_cache() {
    let fixture = Fixture::new();
    write_fresh_manifest(&fixture.state_home, unix_seconds());
    fixture.high_water();
    fs::write(fixture.state.join("gh-routing-manifest.json"), "{invalid").unwrap();
    assert!(!fixture.state.join("last-valid-manifest.json").exists());
    fixture.assert_unavailable();
}

#[test]
fn gh_shim_manifest_unavailable_initially_dormant_provisioned() {
    let fixture = Fixture::new();
    fixture.high_water();
    fixture.assert_refused(
        &["issue", "comment", "1", "--body", "offline"],
        false,
        UNPINNED,
    );
}

#[test]
fn gh_shim_launch_marker_blocks_writes_before_a_pin_is_available() {
    let fixture = Fixture::new();
    let write = ["issue", "comment", "1", "--body", "not native"];
    let output = fixture
        .command(&write, false)
        .env("AFT_GH_SHIM_ENABLED", "1")
        .env_remove("AFT_GH_SHIM_TRUST_ANCHOR")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(86), "{output:?}");
    assert_eq!(output.stderr, UNPINNED.as_bytes());
    assert!(!fixture.record.exists());
    fixture.assert_passthrough(&write, false);
}

#[test]
fn gh_shim_generation_record_alone_blocks_writes() {
    let fixture = Fixture::new();
    fs::write(
        fixture.state.join("trust-root-generation.json"),
        "untrusted history",
    )
    .unwrap();
    fixture.assert_refused(
        &["issue", "comment", "1", "--body", "not native"],
        false,
        UNPINNED,
    );
    fixture.assert_passthrough(&["issue", "view", "1"], false);
}

#[test]
fn gh_shim_final_policy_check_runs_after_the_native_ticket_lookup() {
    let fixture = Fixture::new();
    fixture.activate();
    let manifest = fixture.state.join("gh-routing-manifest.json");
    let daemon = SlowTestDaemon::spawn_removing_during_session_lookup(manifest.clone());
    fixture.connect_daemon(&daemon);
    let output = fixture
        .command(
            &[
                "issue",
                "comment",
                "1",
                "--body",
                "not native",
                "-R",
                "outside/unbound",
            ],
            true,
        )
        .env("AFT_GH_SHIM_TICKET", "synthetic-head-ticket")
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .output()
        .unwrap();
    assert!(
        !manifest.exists(),
        "the fake daemon did not receive the lookup: {output:?}"
    );
    assert_eq!(output.status.code(), Some(86), "{output:?}");
    assert_eq!(output.stderr, b"gh-shim: gh_shim_manifest_unavailable: GitHub routing policy changed before the command could run. Start the command again.\n");
    assert!(!fixture.record.exists());
}

#[test]
fn gh_shim_manifest_unavailable_pin_removed_after_acceptance_refuses_writes() {
    let fixture = Fixture::new();
    fixture.activate();
    fs::remove_file(fixture.state.join("trust-root.json")).unwrap();
    for bypass in [false, true] {
        fixture.assert_refused(
            &["issue", "comment", "1", "--body", "offline"],
            bypass,
            UNPINNED,
        );
        fixture.assert_passthrough(&["pr", "view", "1"], bypass);
    }
}

#[test]
fn gh_shim_manifest_unavailable_unreadable() {
    let fixture = Fixture::new();
    fixture.activate();
    let manifest = fixture.state.join("gh-routing-manifest.json");
    fs::remove_file(&manifest).unwrap();
    // A symlink loop makes the read fail even when the test runner is root.
    std::os::unix::fs::symlink("gh-routing-manifest.json", &manifest).unwrap();
    assert!(fs::read(manifest).is_err());
    for bypass in [false, true] {
        fixture.assert_refused(
            &["issue", "comment", "1", "--body", "offline"],
            bypass,
            "gh-shim: gh_shim_manifest_regressed: the manifest artifact fails validation; issue comment is refused until the manifest is repaired\n",
        );
        fixture.assert_passthrough(&["pr", "view", "1"], bypass);
    }
}

#[test]
fn gh_shim_manifest_unavailable_deleted_after_valid() {
    let fixture = Fixture::new();
    fixture.activate();
    fs::remove_file(fixture.state.join("gh-routing-manifest.json")).unwrap();
    fixture.assert_unavailable();
}

#[test]
fn gh_shim_manifest_unavailable_empty_override_cannot_hide_default() {
    let mut fixture = Fixture::new();
    fixture.activate();
    fs::remove_file(fixture.state.join("gh-routing-manifest.json")).unwrap();
    // Either durable record alone must identify this installation as previously
    // provisioned, even when the override directory is empty.
    for record in [
        "last-valid-manifest.json",
        "manifest-version-high-water.json",
    ] {
        let bytes = fs::read(fixture.state.join(record)).unwrap();
        let other = if record == "last-valid-manifest.json" {
            "manifest-version-high-water.json"
        } else {
            "last-valid-manifest.json"
        };
        let other_bytes = fs::read(fixture.state.join(other)).unwrap();
        fs::remove_file(fixture.state.join(other)).unwrap();
        let default = fixture.state.clone();
        fixture.state = fixture._root.path().join("empty-override");
        fs::create_dir_all(&fixture.state).unwrap();
        fixture.assert_unavailable();
        fixture.state = default;
        assert_eq!(fs::read(fixture.state.join(record)).unwrap(), bytes);
        fs::write(fixture.state.join(other), other_bytes).unwrap();
    }
}

#[test]
fn gh_shim_manifest_unavailable_home_default_survives_empty_override() {
    let mut fixture = Fixture::new();
    let default = fixture.home.join(".local/state/cortexkit/aft/gh-shim");
    fs::create_dir_all(&default).unwrap();
    fs::write(
        default.join("manifest-version-high-water.json"),
        r#"{"newest_accepted_version":12}"#,
    )
    .unwrap();
    fixture.state = fixture._root.path().join("empty-override");
    let output = fixture
        .command(&["pr", "close", "1"], true)
        .env_remove("XDG_STATE_HOME")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(86), "{output:?}");
    assert_eq!(output.stderr, UNAVAILABLE.as_bytes());
    assert!(!fixture.record.exists());
}

#[test]
fn gh_shim_manifest_unavailable_uninspectable_provisioning_records() {
    let mut fixture = Fixture::new();
    let default = fixture.state.clone();
    fs::remove_dir(&default).unwrap();
    fs::write(&default, "not a directory").unwrap();
    // A regular-file parent makes metadata lookup fail even as root, unlike
    // permission bits that root can bypass.
    for record in [
        "last-valid-manifest.json",
        "manifest-version-high-water.json",
    ] {
        let error = fs::symlink_metadata(default.join(record)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotADirectory);
    }
    fixture.state = fixture._root.path().join("empty-override");
    fs::create_dir_all(&fixture.state).unwrap();
    fixture.assert_unavailable();
}

#[test]
fn gh_shim_manifest_unavailable_standalone_passes_writes_only_without_artifacts() {
    let fixture = Fixture::new();
    assert!(!fixture.state.join("trust-root.json").exists());
    let write = ["issue", "comment", "1", "--body", "offline"];
    for bypass in [false, true] {
        fixture.assert_passthrough(&write, bypass);
    }
    for artifact in ["gh-routing-manifest.json", "trust-root.json"] {
        let artifact = fixture.state.join(artifact);
        fs::write(&artifact, "{invalid").unwrap();
        for bypass in [false, true] {
            // A malformed pin is invalid rather than missing.
            let refusal = if artifact.ends_with("trust-root.json") {
                UNAVAILABLE
            } else {
                UNPINNED
            };
            fixture.assert_refused(&write, bypass, refusal);
            fixture.assert_passthrough(&["pr", "view", "1"], bypass);
        }
        fs::remove_file(artifact).unwrap();
    }
}

#[test]
fn gh_shim_manifest_unavailable_write_relay_does_not_replace_manifest_history() {
    let fixture = Fixture::new();
    let daemon = SlowTestDaemon::spawn(SlowDaemonConfig::default());
    fixture.connect_daemon(&daemon);

    let write = ["issue", "comment", "1", "--body", "offline"];
    for bypass in [false, true] {
        fixture.assert_passthrough(&write, bypass);
    }

    fixture.high_water();
    for bypass in [false, true] {
        fixture.assert_refused(&write, bypass, UNAVAILABLE);
        fixture.assert_passthrough(&["issue", "view", "1"], bypass);
    }
    assert_eq!(daemon.catalog_requests(), 0);
}

#[test]
fn gh_shim_manifest_unavailable_fresh_with_read_only_relay_passes_writes() {
    let fixture = Fixture::new();
    let daemon = SlowTestDaemon::spawn_with_catalog(SlowDaemonConfig::default(), None, false);
    fixture.connect_daemon(&daemon);

    for bypass in [false, true] {
        fixture.assert_passthrough(&["issue", "comment", "1", "--body", "offline"], bypass);
    }
    assert_eq!(daemon.catalog_requests(), 0);
}

#[test]
fn gh_shim_manifest_unavailable_without_history_does_not_query_slow_catalog() {
    let fixture = Fixture::new();
    let daemon = SlowTestDaemon::spawn(SlowDaemonConfig {
        catalog_delay: Duration::from_secs(30),
        ..SlowDaemonConfig::default()
    });
    fixture.connect_daemon(&daemon);

    fixture.assert_passthrough(&["issue", "comment", "1", "--body", "offline"], true);
    assert_eq!(daemon.catalog_requests(), 0);
}

#[test]
fn gh_shim_live_off_delegates_when_a_provisioned_manifest_is_missing_or_invalid() {
    let fixture = Fixture::new();
    fixture.high_water();
    let write = ["issue", "comment", "1", "--body", "offline"];
    fixture.assert_refused(&write, false, UNAVAILABLE);

    fixture.disable_shim();
    for invalid in [false, true] {
        if invalid {
            fs::write(fixture.state.join("gh-routing-manifest.json"), "{invalid").unwrap();
        }
        for bypass in [false, true] {
            fixture.assert_passthrough(&write, bypass);
            fixture.assert_passthrough(&["auth", "status"], bypass);
        }
    }
}

#[test]
fn gh_shim_live_off_delegates_when_the_manifest_has_regressed() {
    let fixture = Fixture::new();
    fixture.activate();
    fs::write(fixture.state.join("gh-routing-manifest.json"), "{invalid").unwrap();
    fixture.disable_shim();
    for bypass in [false, true] {
        fixture.assert_passthrough(&["issue", "comment", "1", "--body", "offline"], bypass);
        fixture.assert_passthrough(&["pr", "merge", "1"], bypass);
    }
}

#[test]
fn gh_shim_live_off_skips_captured_and_worker_shim_policies() {
    let fixture = Fixture::new();
    fixture.high_water();
    fixture.disable_shim();
    let write = ["issue", "comment", "1", "--body", "offline"];
    for policy in ["AFT_GH_SPEECH_PROVISIONED", "AFT_GH_WORKER_WRITE_POLICY"] {
        let output = fixture
            .command(&write, false)
            .env(policy, "1")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(73), "{policy}: {output:?}");
        assert!(output.stderr.is_empty(), "{policy}: {output:?}");
    }
}

#[test]
fn gh_shim_live_off_does_not_bypass_native_worker_guard() {
    let fixture = Fixture::new();
    fixture.high_water();
    fixture.disable_shim();
    let output = fixture
        .command(&["issue", "comment", "1", "--body", "offline"], false)
        .env("AFT_GH_SHIM_WORKER", "1")
        .env("AFT_GH_WORKER_WRITE_POLICY", "1")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(86), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("gh_shim_worker_native_gh"));
    assert!(!fixture.record.exists());
}

#[test]
fn gh_shim_live_off_affects_new_children_but_not_an_already_launched_child() {
    use std::io::Write;
    use std::process::{Child, Stdio};

    let fixture = Fixture::new();
    fixture.high_water();
    let storage = fixture._root.path().join("storage");
    let mut enabled = Config::default();
    enabled.github.shim = true;
    enabled.git.co_author = "auto".into();
    enabled.gh_shim.binary_path = Some(aft_binary());
    let spawn = |config: &Config| {
        let mut env = fixture
            .command(&[], false)
            .get_envs()
            .map(|(key, val)| {
                (
                    key.to_str().unwrap().to_owned(),
                    val.unwrap().to_str().unwrap().to_owned(),
                )
            })
            .collect();
        aft::agent_child_env::inject(config, &storage, &mut env, None).unwrap();
        Command::new("/bin/sh")
            .env_clear()
            .envs(env)
            .env("AFT_STORAGE_DIR", &storage)
            .env("AFT_GH_SHIM_STATE_DIR", &fixture.state)
            .arg("-c")
            .arg("read -r go || exit; printf '%s\\n' \"${AFT_GH_SHIM_ENABLED-unset}\"; exec gh issue comment 1 --body offline")
            .current_dir(&fixture.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let finish = |mut child: Child| {
        child.stdin.take().unwrap().write_all(b"go\n").unwrap();
        child.wait_with_output().unwrap()
    };

    let old_child = spawn(&enabled);
    fixture.disable_shim();
    let mut candidate = enabled.clone();
    candidate.github.shim = false;
    let live = aft::config_live::apply_live_config(&enabled, &candidate, &enabled);
    let new = finish(spawn(&live.next));
    assert_eq!(new.status.code(), Some(73), "{new:?}");
    assert_eq!(new.stdout, b"unset\n");
    assert!(new.stderr.is_empty(), "{new:?}");
    fs::remove_file(&fixture.record).unwrap();

    let old = finish(old_child);
    assert_eq!(old.status.code(), Some(86), "{old:?}");
    assert_eq!(old.stdout, b"1\n");
    assert_eq!(old.stderr, UNAVAILABLE.as_bytes());
    assert!(!fixture.record.exists());
}

#[test]
fn gh_shim_manifest_unavailable_corrupt_after_valid_still_regressed() {
    let fixture = Fixture::new();
    fixture.activate();
    fs::write(fixture.state.join("gh-routing-manifest.json"), "{invalid").unwrap();
    for bypass in [false, true] {
        for args in [
            vec!["issue", "comment", "1", "--body", "offline"],
            vec!["pr", "merge", "1"],
        ] {
            let text = format!("gh-shim: gh_shim_manifest_regressed: the manifest artifact fails validation; {} {} is refused until the manifest is repaired\n", args[0], args[1]);
            fixture.assert_refused(&args, bypass, &text);
        }
        fixture.assert_passthrough(&["issue", "view", "1"], bypass);
    }
}

#[test]
fn gh_shim_manifest_unavailable_safe_reads_keep_native_worker_guard() {
    let fixture = Fixture::new();
    fixture.high_water();
    let output = fixture
        .command(&["issue", "view", "1"], false)
        .env("AFT_GH_SHIM_WORKER", "1")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(86), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("gh_shim_worker_native_gh"));
    assert!(!fixture.record.exists());
}

#[test]
fn gh_shim_manifest_unavailable_captured_authority_and_operator_bypass() {
    use sha2::Digest;

    let fixture = Fixture::new();
    fixture.activate();
    let manifest = fixture.state.join("gh-routing-manifest.json");
    let envelope = fs::read(&manifest).unwrap();
    let ticket = (0..TEST_GH_SHIM_TICKET.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&TEST_GH_SHIM_TICKET[index..index + 2], 16).unwrap())
        .collect::<Vec<_>>();
    let view = fixture._root.path().join("speech-start.json");
    fs::write(
        &view,
        serde_json::to_vec(&json!({
            "ticket_digest": blake3::derive_key("aft-gh-speech-ticket-v1", &ticket).to_vec(),
            "policy": {
                "artifact_id": "gh-routing-manifest",
                "version": 12,
                "digest": sha2::Sha256::digest(&envelope).to_vec(),
                "signed_envelope": envelope,
            },
        }))
        .unwrap(),
    )
    .unwrap();
    fs::remove_file(manifest).unwrap();
    fs::remove_file(&fixture.record).unwrap();

    for bypass in [false, true] {
        for args in [
            vec!["issue", "comment", "1", "--body", "offline"],
            vec!["pr", "merge", "1", "--squash"],
        ] {
            let output = fixture
                .command(&args, bypass)
                .env("AFT_GH_SPEECH_PROVISIONED", "1")
                .env("AFT_GH_SPEECH_START_VIEW", &view)
                .env("AFT_GH_SHIM_TICKET", TEST_GH_SHIM_TICKET)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(86), "{output:?}");
            assert_eq!(output.stderr, UNAVAILABLE.as_bytes());
            assert!(!fixture.record.exists());
            assert!(!fixture.state.join("operator-bypass.jsonl").exists());
        }
    }
}

#[test]
fn gh_shim_manifest_unavailable_worker_refusal_keeps_priority_with_bypass() {
    let fixture = Fixture::new();
    fixture.high_water();
    let capability = fixture._root.path().join("capability");
    fs::write(&capability, "0123456789abcdef0123456789abcdef").unwrap();
    for policy_marker in [false, true] {
        for bypass in [false, true] {
            for args in [
                vec!["issue", "comment", "1", "--body", "offline"],
                vec!["pr", "merge", "1", "--squash"],
            ] {
                let mut command = fixture.command(&args, bypass);
                command
                    .env("AFT_GH_SHIM_WORKER", "1")
                    .env("AFT_GH_SPEECH_PROVISIONED", "1");
                if policy_marker {
                    command.env("AFT_GH_WORKER_WRITE_POLICY", "1");
                } else {
                    command
                        .env(
                            "AFT_GH_READ_SOCKET",
                            fixture._root.path().join("broker.sock"),
                        )
                        .env("AFT_GH_READ_CAPABILITY_FILE", &capability);
                }
                let output = command.output().unwrap();
                assert_eq!(output.status.code(), Some(86), "{output:?}");
                assert_eq!(
                    output.stderr,
                    b"gh-shim: gh_shim_worker_write_refused: workers cannot write to GitHub; report what should be posted to your parent instead\n"
                );
                assert!(!fixture.record.exists());
                assert!(!fixture.state.join("operator-bypass.jsonl").exists());
            }
        }
    }
}

#[test]
fn gh_shim_manifest_unavailable_disappears_at_late_r3_recheck() {
    let fixture = Fixture::new();
    write_fresh_v12_manifest(&fixture.state_home, unix_seconds());
    let manifest = fixture.state.join("gh-routing-manifest.json");
    let daemon = SlowTestDaemon::spawn_with_manifest_removal(
        SlowDaemonConfig {
            handshake_delay: Duration::ZERO,
            catalog_delay: Duration::ZERO,
            open_route_delay: Duration::ZERO,
            request_delay: Duration::ZERO,
            first_connection_only: false,
        },
        Some(manifest.clone()),
    );
    let connection = fixture._root.path().join("connection.json");
    daemon.write_connection_file(&connection);
    write_user_config(&fixture.config, &connection, Some(true));
    let output = fixture
        .command(&["issue", "comment", "1", "--body", "offline"], false)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env("AFT_GH_SHIM_TICKET", TEST_GH_SHIM_TICKET)
        .output()
        .unwrap();
    assert!(!manifest.exists(), "discovery must remove the manifest");
    let rung: Value =
        serde_json::from_slice(&fs::read(fixture.state.join("rung-cache.json")).unwrap()).unwrap();
    assert_eq!(rung["rung"], "R3", "{rung}");
    assert_eq!(output.status.code(), Some(86), "{output:?}");
    assert_eq!(output.stderr, UNAVAILABLE.as_bytes());
    assert!(
        !fixture.record.exists(),
        "late refusal executed upstream gh"
    );
}
