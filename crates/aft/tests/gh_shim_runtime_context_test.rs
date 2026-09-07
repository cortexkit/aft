#![cfg(unix)]

#[path = "helpers/mod.rs"]
mod test_helpers;

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use aft::config::Config;
use base64::Engine;
use ring::signature::Ed25519KeyPair;
use serde_json::{json, Value};
use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};
use subc_transport::{DAEMON_ID_LEN, KEY_LEN};

const DEV_MANIFEST_SEED: [u8; 32] = [
    0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c, 0xc4,
    0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae, 0x7f, 0x60,
];

fn aft_binary() -> PathBuf {
    std::env::var_os("AFT_TEST_AFT_BINARY")
        .or_else(|| std::env::var_os("NEXTEST_BIN_EXE_aft"))
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_aft"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_aft")))
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after Unix epoch")
        .as_secs()
}

fn write_manifest_value(state_home: &Path, now: u64, mut manifest: Value, manifest_version: u64) {
    manifest["manifest_version"] = json!(manifest_version);
    manifest["issued_at_unix_secs"] = json!(now);
    let manifest_bytes = serde_json::to_vec(&manifest).expect("serialize fresh manifest");
    let key = Ed25519KeyPair::from_seed_unchecked(&DEV_MANIFEST_SEED).expect("build test key");
    let envelope = json!({
        "artifact_id": "gh-routing-manifest",
        "envelope_version": 2,
        "key_id": "gh-routing-dev-test-key-v1",
        "fetched_at_unix_secs": now,
        "signature": base64::engine::general_purpose::STANDARD.encode(key.sign(&manifest_bytes).as_ref()),
        "manifest_bytes": String::from_utf8(manifest_bytes).expect("manifest fixture is UTF-8"),
    });
    let manifest_path = state_home.join("cortexkit/aft/gh-shim/gh-routing-manifest.json");
    fs::create_dir_all(manifest_path.parent().expect("manifest parent"))
        .expect("create shim state directory");
    fs::write(
        manifest_path,
        serde_json::to_vec(&envelope).expect("serialize manifest envelope"),
    )
    .expect("write manifest envelope");
}

fn write_fresh_manifest_from_fixture(
    state_home: &Path,
    now: u64,
    fixture: &str,
    manifest_version: u64,
) {
    let manifest: Value = serde_json::from_str(fixture).expect("parse manifest fixture");
    write_manifest_value(state_home, now, manifest, manifest_version);
}

fn write_fresh_v10_admin_manifest(state_home: &Path, now: u64) {
    let mut manifest: Value =
        serde_json::from_str(include_str!("fixtures/gh_shim/v10-manifest.json"))
            .expect("parse v10 manifest fixture");
    let admin = manifest["tiers"]["admin"]
        .as_array_mut()
        .expect("v10 admin tier");
    for tuple in ["run rerun", "run cancel"] {
        admin.push(json!({
            "tuple": tuple,
            "platform": ["macos", "linux"]
        }));
    }
    write_manifest_value(state_home, now, manifest, 10);
}

fn write_fresh_manifest(state_home: &Path, now: u64) {
    write_fresh_manifest_from_fixture(
        state_home,
        now,
        include_str!("fixtures/gh_shim/initial-manifest-v1.json"),
        1,
    );
}

fn write_fresh_v9_manifest(state_home: &Path, now: u64) {
    write_fresh_manifest_from_fixture(
        state_home,
        now,
        include_str!("fixtures/gh_shim/v9-manifest.json"),
        9,
    );
}

fn write_fresh_v10_manifest(state_home: &Path, now: u64, manifest_version: u64) {
    write_fresh_manifest_from_fixture(
        state_home,
        now,
        include_str!("fixtures/gh_shim/v10-manifest.json"),
        manifest_version,
    );
}

fn write_fresh_v11_manifest(state_home: &Path, now: u64) {
    write_fresh_manifest_from_fixture(
        state_home,
        now,
        include_str!("fixtures/gh_shim/v11-manifest.json"),
        11,
    );
}

fn write_fresh_v12_manifest(state_home: &Path, now: u64) {
    write_fresh_manifest_from_fixture(
        state_home,
        now,
        include_str!("fixtures/gh_shim/v12-manifest.json"),
        12,
    );
}

fn write_fresh_s2_manifest(state_home: &Path, now: u64) {
    write_fresh_manifest_from_fixture(
        state_home,
        now,
        include_str!("fixtures/gh_shim/s2-manifest-v1.json"),
        1,
    );
}

fn write_fresh_r3_cache(state_home: &Path, now: u64) {
    write_fresh_r3_cache_for_manifest(state_home, now, 1);
}

fn write_fresh_r3_cache_for_manifest(state_home: &Path, now: u64, manifest_version: u64) {
    let rung_path = state_home.join("cortexkit/aft/gh-shim/rung-cache.json");
    fs::create_dir_all(rung_path.parent().expect("rung cache parent"))
        .expect("create shim state directory");
    fs::write(
        rung_path,
        serde_json::to_vec(&json!({
            "rung": "R3",
            "as_of_unix_secs": now,
            "inputs": {
                "connection_file": "ready",
                "catalog_gh_route": "ready",
                "agent_binding": "ready",
                "manifest": "ready",
                "agent_credentials_present": "absent"
            },
            "manifest_version": manifest_version
        }))
        .expect("serialize R3 rung cache"),
    )
    .expect("write R3 rung cache");
}

fn write_fresh_ambient_credentials_r2_cache(state_home: &Path, now: u64) {
    let rung_path = state_home.join("cortexkit/aft/gh-shim/rung-cache.json");
    fs::create_dir_all(rung_path.parent().expect("rung cache parent"))
        .expect("create shim state directory");
    fs::write(
        rung_path,
        serde_json::to_vec(&json!({
            "rung": "R2",
            "as_of_unix_secs": now,
            "inputs": {
                "connection_file": "ready",
                "agent_credentials_present": "env:GH_TOKEN",
                "catalog_holder": "prefrontal-core"
            },
            "manifest_version": 1
        }))
        .expect("serialize ambient-credential R2 rung cache"),
    )
    .expect("write ambient-credential R2 rung cache");
}

fn write_invalid_manifest(state_home: &Path, now: u64) {
    write_fresh_manifest(state_home, now);
    let manifest_path = state_home.join("cortexkit/aft/gh-shim/gh-routing-manifest.json");
    let mut envelope: Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("read manifest envelope"))
            .expect("parse manifest envelope");
    envelope["key_id"] = json!("gh-routing-untrusted-test-key");
    fs::write(
        manifest_path,
        serde_json::to_vec(&envelope).expect("serialize invalid manifest envelope"),
    )
    .expect("write invalid manifest envelope");
}

fn write_dead_connection_file(root: &Path) -> PathBuf {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve a loopback port");
    let port = listener
        .local_addr()
        .expect("read reserved loopback address")
        .port();
    drop(listener);

    let path = root.join("subc-connection.json");
    let connection = ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: None,
        endpoints: vec![Endpoint {
            host: "127.0.0.1".to_string(),
            port,
        }],
        key: vec![0x42; KEY_LEN],
        daemon_id: [0x24; DAEMON_ID_LEN],
        pid: std::process::id(),
        daemon_ver: "gh-shim-runtime-context-test".to_string(),
    };
    connection_file::write_atomic(&path, &connection).expect("write dead-daemon connection file");
    path
}

fn write_project_repo(root: &Path) -> PathBuf {
    write_project_repo_for(root, "project", "cortexkit/aft")
}

fn write_project_repo_for(root: &Path, name: &str, repository: &str) -> PathBuf {
    let project = root.join(name);
    fs::create_dir_all(&project).expect("create project directory");
    let initialized = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&project)
        .status()
        .expect("initialize project repository");
    assert!(initialized.success(), "git init failed: {initialized}");
    let remote_added = Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            &format!("https://github.com/{repository}.git"),
        ])
        .current_dir(&project)
        .status()
        .expect("configure project origin");
    assert!(
        remote_added.success(),
        "git remote add failed: {remote_added}"
    );
    project
}

fn write_upstream_gh(bin: &Path) {
    let gh = bin.join("gh");
    fs::create_dir_all(bin).expect("create fake upstream bin directory");
    fs::write(
        &gh,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$GH_SHIM_TEST_RECORD\"\nprintf 'r2-passthrough\\n'\nexit 73\n",
    )
    .expect("write fake upstream gh");
    let mut permissions = fs::metadata(&gh)
        .expect("read fake upstream gh metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&gh, permissions).expect("make fake upstream gh executable");
}

fn write_upstream_gh_user_api(bin: &Path) {
    let gh = bin.join("gh");
    fs::create_dir_all(bin).expect("create fake upstream bin directory");
    fs::write(
        &gh,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$GH_SHIM_TEST_RECORD\"\n[ \"$1\" = api ] || exit 73\nprintf '289616620\\n'\n",
    )
    .expect("write fake upstream gh API");
    let mut permissions = fs::metadata(&gh)
        .expect("read fake upstream gh metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&gh, permissions).expect("make fake upstream gh executable");
}

fn write_numeric_ids(state_home: &Path, ids: Value) {
    let path = state_home.join("cortexkit/aft/gh-shim/numeric-ids.json");
    fs::create_dir_all(path.parent().expect("numeric id cache parent"))
        .expect("create numeric id cache directory");
    fs::write(
        path,
        serde_json::to_vec(&ids).expect("serialize numeric ids"),
    )
    .expect("write numeric id cache");
}

fn write_user_config(config_home: &Path, connection_file: &Path, enabled: Option<bool>) {
    let config_dir = config_home.join("cortexkit");
    fs::create_dir_all(&config_dir).expect("create user config directory");
    let mut config = json!({
        "subc": { "connection_file": connection_file },
    });
    if let Some(enabled) = enabled {
        config["gh_shim"] = json!({ "enabled": enabled });
    }
    fs::write(
        config_dir.join("aft.jsonc"),
        serde_json::to_vec_pretty(&config).expect("serialize user config"),
    )
    .expect("write user config");
}

fn unclassified_refusal(manifest_version: u64) -> String {
    format!(
        "gh-shim: gh_shim_unclassified: no manifest declaration for this invocation (manifest {manifest_version}); GH_SHIM_BYPASS does not apply to undeclared invocations - this verb needs a manifest declaration\n"
    )
}

fn shim_status(
    project: &Path,
    config_home: &Path,
    state_home: &Path,
    home: &Path,
    upstream_bin: &Path,
    recorder: &Path,
) -> Value {
    let output = shim_command(
        &["--status"],
        project,
        config_home,
        state_home,
        home,
        upstream_bin,
        recorder,
    )
    .output()
    .expect("spawn gh shim status");
    assert!(
        output.status.success(),
        "status should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("parse gh shim status JSON")
}

fn snapshot_tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn collect(root: &Path, current: &Path, entries: &mut Vec<(PathBuf, Vec<u8>)>) {
        let Ok(directory) = fs::read_dir(current) else {
            return;
        };
        for entry in directory.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect(root, &path, entries);
            } else if let Ok(bytes) = fs::read(&path) {
                entries.push((path.strip_prefix(root).unwrap().to_path_buf(), bytes));
            }
        }
    }

    let mut entries = Vec::new();
    collect(root, root, &mut entries);
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries
}

fn assert_status_provenance(report: &Value) {
    assert!(report["executing_image"]
        .as_str()
        .is_some_and(|image| !image.is_empty()));
    assert!(report["cached_manifest"]
        .get("verified_by_key_id")
        .is_some());
    assert!(report["cached_manifest"]["compiled_trust_set_key_ids"]
        .as_array()
        .is_some_and(|ids| ids.iter().any(|id| id == "gh-routing-dev-test-key-v1")));
}

fn shim_command(
    args: &[&str],
    project: &Path,
    config_home: &Path,
    state_home: &Path,
    home: &Path,
    upstream_bin: &Path,
    recorder: &Path,
) -> Command {
    let inherited_path = std::env::var_os("PATH").expect("test PATH");
    let path = std::env::join_paths(
        std::iter::once(upstream_bin.to_path_buf()).chain(std::env::split_paths(&inherited_path)),
    )
    .expect("build test PATH");
    let mut shim = Command::new(aft_binary());
    shim.arg("gh-shim")
        .args(args)
        .current_dir(project)
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_STATE_HOME", state_home)
        .env("AFT_GH_SHIM_STATE_DIR", state_home.join("cortexkit/aft/gh-shim"))
        .env("HOME", home)
        // Direct shim invocations bypass the shared AftProcess helper. Give each
        // fixture its own root so an unexpected future configure path cannot
        // write an index into the developer's shared storage.
        .env("AFT_STORAGE_DIR", state_home.join("aft-test-storage"))
        .env("PATH", path)
        .env("GH_SHIM_TEST_RECORD", recorder)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_ENTERPRISE_TOKEN")
        .env_remove("GH_SHIM_BYPASS");
    shim
}

#[test]
fn gh_shim_child_override_leaves_operator_state_canary_untouched() {
    let temp = tempfile::tempdir().expect("create test root");
    let config_home = temp.path().join("config");
    let isolated_state = temp.path().join("isolated-state");
    let operator_xdg = temp.path().join("operator-xdg");
    let operator_home = temp.path().join("operator-home");
    let project = write_project_repo(temp.path());
    let connection_file = write_dead_connection_file(temp.path());
    let upstream_bin = temp.path().join("upstream-bin");
    let recorder = temp.path().join("upstream-invocations.txt");
    write_upstream_gh(&upstream_bin);
    write_fresh_manifest(&isolated_state, unix_seconds());
    write_user_config(&config_home, &connection_file, None);
    let before = snapshot_tree(&operator_xdg);

    let output = shim_command(
        &["issue", "list"],
        &project,
        &config_home,
        &isolated_state,
        &operator_home,
        &upstream_bin,
        &recorder,
    )
    // Point HOME and XDG_STATE_HOME at canary directories. The child's
    // dedicated state-location override must take precedence so that records
    // written by the child leave both operator directories unchanged.
    .env("XDG_STATE_HOME", &operator_xdg)
    .env("HOME", &operator_home)
    .output()
    .expect("spawn canary gh shim child");

    assert_eq!(output.status.code(), Some(73));
    assert_eq!(snapshot_tree(&operator_xdg), before);
    assert!(snapshot_tree(&operator_home).is_empty());
}

#[test]
fn gh_shim_s2_one_agent_many_repos_exercises_binding_and_failure_arms() {
    const AGENT_ID: &str = "agent_d444250e2d503c07";
    const BOUND_REPOS: [&str; 3] = [
        "cortexkit/cortexkit-e2e",
        "cortexkit/cortexkit-account",
        "cortexkit/aft",
    ];

    let fixture: Value = serde_json::from_str(include_str!("fixtures/gh_shim/s2-manifest-v1.json"))
        .expect("parse S2 manifest fixture");
    let bindings = fixture["bindings"].as_object().expect("S2 bindings object");
    assert_eq!(bindings.len(), BOUND_REPOS.len());
    assert!(bindings.values().all(|agent| agent == AGENT_ID));

    let temp = tempfile::tempdir().expect("create test root");
    let config_home = temp.path().join("config");
    let home = temp.path().join("home");
    let connection_file = write_dead_connection_file(temp.path());
    let upstream_bin = temp.path().join("upstream-bin");
    write_upstream_gh(&upstream_bin);
    write_user_config(&config_home, &connection_file, None);

    // Each repository gets an independent state universe so a persisted binding
    // from one invocation cannot satisfy another repository's assertion.
    for (index, repository) in BOUND_REPOS.into_iter().enumerate() {
        let state_home = temp.path().join(format!("bound-state-{index}"));
        let project =
            write_project_repo_for(temp.path(), &format!("bound-project-{index}"), repository);
        let recorder = temp.path().join(format!("bound-recorder-{index}.txt"));
        write_fresh_s2_manifest(&state_home, unix_seconds());

        let output = shim_command(
            &["issue", "comment", "42", "--body", "S2 fixture"],
            &project,
            &config_home,
            &state_home,
            &home,
            &upstream_bin,
            &recorder,
        )
        .output()
        .expect("spawn bound S2 governed invocation");
        assert_eq!(output.status.code(), Some(86));
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "gh-shim: gh_shim_governance_unavailable: the governance daemon is unreachable and this repository's actions are identity-governed; retry after the daemon returns\n"
        );
        assert!(
            !recorder.exists(),
            "a bound governed invocation must never reach ambient gh"
        );

        let status = shim_status(
            &project,
            &config_home,
            &state_home,
            &home,
            &upstream_bin,
            &recorder,
        );
        assert_eq!(status["agent_binding"]["repo"], repository);
        assert_eq!(status["agent_binding"]["agent_id"], AGENT_ID);
        assert_eq!(
            status["last_seam_refusal"]["code"],
            "gh_shim_governance_unavailable"
        );
        assert_eq!(status["cached_manifest"]["state"], "valid");
        assert_status_provenance(&status);
    }

    let unbound_state = temp.path().join("unbound-state");
    let unbound_project = write_project_repo_for(
        temp.path(),
        "unbound-project",
        "cortexkit/unmanifested-repository",
    );
    let unbound_recorder = temp.path().join("unbound-recorder.txt");
    write_fresh_s2_manifest(&unbound_state, unix_seconds());
    let unbound = shim_command(
        &["issue", "comment", "42", "--body", "S2 fixture"],
        &unbound_project,
        &config_home,
        &unbound_state,
        &home,
        &upstream_bin,
        &unbound_recorder,
    )
    .output()
    .expect("spawn unbound S2 governed invocation");
    assert_eq!(unbound.status.code(), Some(73));
    assert_eq!(String::from_utf8_lossy(&unbound.stdout), "r2-passthrough\n");
    assert!(unbound.stderr.is_empty());
    assert_eq!(
        fs::read_to_string(&unbound_recorder).expect("read unbound upstream invocation"),
        "issue comment 42 --body S2 fixture\n"
    );
    let unbound_status = shim_status(
        &unbound_project,
        &config_home,
        &unbound_state,
        &home,
        &upstream_bin,
        &unbound_recorder,
    );
    assert!(unbound_status["agent_binding"].is_null());
    assert!(unbound_status["last_seam_refusal"].is_null());
    assert_eq!(unbound_status["cached_manifest"]["state"], "valid");
    assert_status_provenance(&unbound_status);

    let regressed_state = temp.path().join("regressed-state");
    let regressed_project =
        write_project_repo_for(temp.path(), "regressed-project", BOUND_REPOS[0]);
    let regressed_recorder = temp.path().join("regressed-recorder.txt");
    write_fresh_s2_manifest(&regressed_state, unix_seconds());
    let happy_status = shim_status(
        &regressed_project,
        &config_home,
        &regressed_state,
        &home,
        &upstream_bin,
        &regressed_recorder,
    );
    assert_eq!(happy_status["cached_manifest"]["state"], "valid");
    assert_status_provenance(&happy_status);

    let manifest_path = regressed_state.join("cortexkit/aft/gh-shim/gh-routing-manifest.json");
    let mut envelope: Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("read S2 manifest envelope"))
            .expect("parse S2 manifest envelope");
    envelope["key_id"] = json!("unknown-s2-test-key");
    fs::write(
        &manifest_path,
        serde_json::to_vec(&envelope).expect("serialize untrusted S2 envelope"),
    )
    .expect("write untrusted S2 envelope");

    let regressed = shim_command(
        &["issue", "comment", "42", "--body", "S2 fixture"],
        &regressed_project,
        &config_home,
        &regressed_state,
        &home,
        &upstream_bin,
        &regressed_recorder,
    )
    .output()
    .expect("spawn untrusted S2 governed invocation");
    assert_eq!(regressed.status.code(), Some(86));
    let regressed_stderr = String::from_utf8_lossy(&regressed.stderr);
    assert!(regressed_stderr.contains("gh_shim_manifest_regressed"));
    assert!(regressed_stderr.contains("update aft, or install a manifest signed by a trusted key"));
    assert!(
        !regressed_recorder.exists(),
        "a regressed governed invocation must not reach ambient gh"
    );

    let regressed_status = shim_status(
        &regressed_project,
        &config_home,
        &regressed_state,
        &home,
        &upstream_bin,
        &regressed_recorder,
    );
    assert_eq!(regressed_status["cached_manifest"]["state"], "regressed");
    assert_ne!(
        regressed_status["cached_manifest"]["state"], happy_status["cached_manifest"]["state"],
        "the broken S2 fixture's status must disagree with its valid twin"
    );
    assert_eq!(
        regressed_status["cached_manifest"]["diagnostics"],
        json!([
            "gh_shim_status_manifest_regressed",
            "gh_shim_status_manifest_invalid"
        ])
    );
    assert_eq!(
        regressed_status["cached_manifest"]["diagnostic_guidance"],
        "the manifest may be newer than this aft build's trust set - update aft, or install a manifest signed by a trusted key"
    );
    assert!(regressed_status["cached_manifest"]["verified_by_key_id"].is_null());
    assert_status_provenance(&regressed_status);
}

#[test]
fn gh_shim_daemon_probe_from_sync_entry_is_r2_without_a_runtime_panic() {
    let temp = tempfile::tempdir().expect("create test root");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let home = temp.path().join("home");
    let project = write_project_repo(temp.path());
    let connection_file = write_dead_connection_file(temp.path());
    let upstream_bin = temp.path().join("upstream-bin");
    let recorder = temp.path().join("upstream-invocations.txt");
    write_upstream_gh(&upstream_bin);
    write_fresh_manifest(&state_home, unix_seconds());
    write_user_config(&config_home, &connection_file, None);

    let output = shim_command(
        &["issue", "list"],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .output()
    .expect("spawn gh shim");

    assert_eq!(
        output.status.code(),
        Some(73),
        "R2 must delegate to the upstream gh stand-in; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "r2-passthrough\n",
        "the shim should pass the command through after determining R2"
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("panicked at"),
        "the sync CLI probe must not require a pre-existing Tokio runtime: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        fs::read_to_string(&recorder).expect("read upstream invocation record"),
        "issue list\n"
    );

    let status = shim_command(
        &["--status"],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .output()
    .expect("spawn gh shim status");
    assert!(
        status.status.success(),
        "status should read the recorded rung: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let report: Value = serde_json::from_slice(&status.stdout).expect("parse gh shim status JSON");
    assert_eq!(report["last_rung"]["rung"], "R2");
    assert_eq!(
        report["last_rung"]["determination_inputs"]["daemon_unreachable"],
        "failed"
    );
    assert_eq!(
        report["last_rung"]["recorded_by_version"],
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(report["last_rung"]["recorded_by_repo_key"], "cortexkit/aft");
    assert!(report["last_rung"]["recorded_by_image_path"]
        .as_str()
        .is_some_and(|path| !path.is_empty()));
    assert_eq!(
        report["cached_manifest"]["verified_by_key_id"],
        "gh-routing-dev-test-key-v1"
    );
    assert!(report["cached_manifest"]["compiled_trust_set_key_ids"]
        .as_array()
        .is_some_and(|ids| ids.iter().any(|id| id == "gh-routing-dev-test-key-v1")));
}

#[test]
fn gh_shim_governed_manifest_passthroughs_no_verb_and_help_invocations() {
    let temp = tempfile::tempdir().expect("create test root");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let home = temp.path().join("home");
    let project = write_project_repo(temp.path());
    let connection_file = write_dead_connection_file(temp.path());
    let upstream_bin = temp.path().join("upstream-bin");
    let recorder = temp.path().join("upstream-invocations.txt");
    write_upstream_gh(&upstream_bin);
    let now = unix_seconds();
    write_fresh_manifest(&state_home, now);
    write_fresh_r3_cache(&state_home, now);
    write_user_config(&config_home, &connection_file, None);

    for args in [
        &[][..],
        &["--version"][..],
        &["--help"][..],
        &["-h"][..],
        &["help", "pr"][..],
    ] {
        let output = shim_command(
            args,
            &project,
            &config_home,
            &state_home,
            &home,
            &upstream_bin,
            &recorder,
        )
        .output()
        .expect("spawn governed gh shim invocation");
        assert_eq!(
            output.status.code(),
            Some(73),
            "upstream passthrough: {args:?}"
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "r2-passthrough\n");
        assert!(
            output.stderr.is_empty(),
            "unexpected shim refusal: {args:?}"
        );
    }

    let undeclared_write = shim_command(
        &["release", "publish", "v1.0.0"],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .output()
    .expect("spawn undeclared governed gh shim invocation");
    assert_eq!(undeclared_write.status.code(), Some(86));
    assert!(undeclared_write.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&undeclared_write.stderr),
        unclassified_refusal(1)
    );

    assert_eq!(
        fs::read_to_string(&recorder).expect("read upstream invocation record"),
        "\n--version\n--help\n-h\nhelp pr\n"
    );
}

#[test]
fn gh_shim_v9_admin_tuples_differ_from_raw_delete_and_keep_get_mechanical() {
    let temp = tempfile::tempdir().expect("create test root");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let home = temp.path().join("home");
    let project = write_project_repo(temp.path());
    let connection_file = write_dead_connection_file(temp.path());
    let upstream_bin = temp.path().join("upstream-bin");
    let recorder = temp.path().join("upstream-invocations.txt");
    write_upstream_gh(&upstream_bin);
    let now = unix_seconds();
    write_fresh_v9_manifest(&state_home, now);
    write_fresh_r3_cache_for_manifest(&state_home, now, 9);
    write_user_config(&config_home, &connection_file, None);

    let expected_admin_refusal =
        "gh-shim: gh_shim_admin_tier: this action requires GH_SHIM_BYPASS=operator\n";
    for args in [
        &["repo", "edit", "cortexkit/insula", "--visibility", "public"][..],
        &[
            "repo",
            "edit",
            "cortexkit/insula",
            "--visibility",
            "private",
        ][..],
        &["run", "delete", "123", "--repo", "cortexkit/insula"][..],
    ] {
        let output = shim_command(
            args,
            &project,
            &config_home,
            &state_home,
            &home,
            &upstream_bin,
            &recorder,
        )
        .output()
        .expect("spawn v9 admin gh shim invocation");
        assert_eq!(output.status.code(), Some(86));
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            expected_admin_refusal
        );
    }

    let raw_api_delete = shim_command(
        &[
            "api",
            "-X",
            "DELETE",
            "repos/cortexkit/insula/actions/runs/123",
        ],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .output()
    .expect("spawn raw API delete gh shim invocation");
    assert_eq!(raw_api_delete.status.code(), Some(86));
    assert!(raw_api_delete.stdout.is_empty());
    let raw_api_refusal = String::from_utf8_lossy(&raw_api_delete.stderr);
    assert_eq!(raw_api_refusal, unclassified_refusal(9));
    assert_ne!(
        expected_admin_refusal, raw_api_refusal,
        "native admin and raw API delete refusals must remain distinguishable"
    );

    let get_control = shim_command(
        &["api", "repos/cortexkit/insula", "--jq", ".name"],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .output()
    .expect("spawn mechanical GET gh shim invocation");
    assert_eq!(get_control.status.code(), Some(73));
    assert_eq!(
        String::from_utf8_lossy(&get_control.stdout),
        "r2-passthrough\n"
    );
    assert!(get_control.stderr.is_empty());
    assert_eq!(
        fs::read_to_string(&recorder).expect("read mechanical GET invocation record"),
        "api repos/cortexkit/insula --jq .name\n"
    );
}

#[test]
fn gh_shim_operator_bypass_does_not_lift_unclassified_refusal_and_keeps_admin_message_unchanged() {
    let temp = tempfile::tempdir().expect("create test root");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let home = temp.path().join("home");
    let project = write_project_repo(temp.path());
    let connection_file = write_dead_connection_file(temp.path());
    let upstream_bin = temp.path().join("upstream-bin");
    let recorder = temp.path().join("upstream-invocations.txt");
    write_upstream_gh(&upstream_bin);
    let now = unix_seconds();
    write_fresh_v9_manifest(&state_home, now);
    write_fresh_r3_cache_for_manifest(&state_home, now, 9);
    write_user_config(&config_home, &connection_file, None);

    let unclassified = shim_command(
        &[
            "api",
            "-X",
            "DELETE",
            "repos/cortexkit/insula/actions/runs/123",
        ],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .env("GH_SHIM_BYPASS", "operator")
    .output()
    .expect("spawn operator-bypassed unclassified invocation");
    assert_eq!(unclassified.status.code(), Some(86));
    assert!(unclassified.stdout.is_empty());
    let unclassified_stderr = String::from_utf8_lossy(&unclassified.stderr);
    assert!(unclassified_stderr.contains(
        "GH_SHIM_BYPASS does not apply to undeclared invocations - this verb needs a manifest declaration"
    ));
    assert_eq!(unclassified_stderr, unclassified_refusal(9));

    let admin = shim_command(
        &["repo", "edit", "cortexkit/insula", "--visibility", "public"],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .output()
    .expect("spawn non-bypassed admin invocation");
    assert_eq!(admin.status.code(), Some(86));
    assert!(admin.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&admin.stderr),
        "gh-shim: gh_shim_admin_tier: this action requires GH_SHIM_BYPASS=operator\n"
    );
    assert!(!recorder.exists());
}

#[test]
fn gh_shim_v10_workflow_run_admin_tuple_differs_from_raw_dispatch_and_is_version_gated() {
    let temp = tempfile::tempdir().expect("create test root");
    let config_home = temp.path().join("config");
    let home = temp.path().join("home");
    let connection_file = write_dead_connection_file(temp.path());
    let upstream_bin = temp.path().join("upstream-bin");
    let v10_state = temp.path().join("v10-state");
    let v10_recorder = temp.path().join("v10-upstream-invocations.txt");
    let project = write_project_repo(temp.path());
    write_upstream_gh(&upstream_bin);
    write_user_config(&config_home, &connection_file, None);
    let now = unix_seconds();
    write_fresh_v10_manifest(&v10_state, now, 10);
    write_fresh_r3_cache_for_manifest(&v10_state, now, 10);

    let expected_admin_refusal =
        "gh-shim: gh_shim_admin_tier: this action requires GH_SHIM_BYPASS=operator\n";
    let workflow_run = ["workflow", "run", "ci.yml", "--ref", "main"];
    let admin = shim_command(
        &workflow_run,
        &project,
        &config_home,
        &v10_state,
        &home,
        &upstream_bin,
        &v10_recorder,
    )
    .output()
    .expect("spawn v10 admin workflow invocation");
    assert_eq!(admin.status.code(), Some(86));
    assert!(admin.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&admin.stderr),
        expected_admin_refusal
    );

    let operator = shim_command(
        &workflow_run,
        &project,
        &config_home,
        &v10_state,
        &home,
        &upstream_bin,
        &v10_recorder,
    )
    .env("GH_SHIM_BYPASS", "operator")
    .output()
    .expect("spawn operator-bypassed v10 workflow invocation");
    assert_eq!(operator.status.code(), Some(73));
    assert_eq!(
        String::from_utf8_lossy(&operator.stdout),
        "r2-passthrough\n"
    );
    assert!(operator.stderr.is_empty());

    let raw_api_dispatch = [
        "api",
        "-X",
        "POST",
        "repos/cortexkit/aft/actions/workflows/ci.yml/dispatches",
    ];
    let raw_v10 = shim_command(
        &raw_api_dispatch,
        &project,
        &config_home,
        &v10_state,
        &home,
        &upstream_bin,
        &v10_recorder,
    )
    .output()
    .expect("spawn raw v10 API dispatch invocation");
    assert_eq!(raw_v10.status.code(), Some(86));
    assert!(raw_v10.stdout.is_empty());
    let raw_v10_refusal = String::from_utf8_lossy(&raw_v10.stderr);
    assert_eq!(raw_v10_refusal, unclassified_refusal(10));
    assert_ne!(
        expected_admin_refusal, raw_v10_refusal,
        "native workflow admin and raw API dispatch refusals must remain distinguishable"
    );
    assert_eq!(
        fs::read_to_string(&v10_recorder).expect("read v10 upstream invocation record"),
        "workflow run ci.yml --ref main\n"
    );

    // Reuse the v10-style declaration contents with manifest version 9 to
    // verify that classification is controlled by the manifest version, not by
    // the declaration contents.
    let v9_state = temp.path().join("v9-shaped-state");
    let v9_recorder = temp.path().join("v9-shaped-upstream-invocations.txt");
    write_fresh_v10_manifest(&v9_state, now, 9);
    write_fresh_r3_cache_for_manifest(&v9_state, now, 9);

    let workflow_v9 = shim_command(
        &workflow_run,
        &project,
        &config_home,
        &v9_state,
        &home,
        &upstream_bin,
        &v9_recorder,
    )
    .output()
    .expect("spawn manifest 9 workflow invocation");
    assert_eq!(workflow_v9.status.code(), Some(86));
    assert!(workflow_v9.stdout.is_empty());
    let workflow_v9_refusal = String::from_utf8_lossy(&workflow_v9.stderr);
    assert_eq!(workflow_v9_refusal, unclassified_refusal(9));
    assert_ne!(expected_admin_refusal, workflow_v9_refusal);

    let raw_v9 = shim_command(
        &raw_api_dispatch,
        &project,
        &config_home,
        &v9_state,
        &home,
        &upstream_bin,
        &v9_recorder,
    )
    .output()
    .expect("spawn raw manifest 9 API dispatch invocation");
    assert_eq!(raw_v9.status.code(), Some(86));
    assert!(raw_v9.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&raw_v9.stderr),
        unclassified_refusal(9)
    );
    assert!(!v9_recorder.exists());
}

#[test]
fn gh_shim_v10_comment_edit_last_is_governed_but_raw_comment_patch_is_unclassified() {
    let temp = tempfile::tempdir().expect("create test root");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let home = temp.path().join("home");
    let project = write_project_repo(temp.path());
    let connection_file = write_dead_connection_file(temp.path());
    let upstream_bin = temp.path().join("upstream-bin");
    let recorder = temp.path().join("upstream-invocations.txt");
    write_upstream_gh(&upstream_bin);
    let now = unix_seconds();
    write_fresh_v10_manifest(&state_home, now, 10);
    write_fresh_r3_cache_for_manifest(&state_home, now, 10);
    write_user_config(&config_home, &connection_file, None);

    let governed_stderr = "gh-shim: gh_shim_governance_unavailable: the governance daemon is unreachable and this repository's actions are identity-governed; retry after the daemon returns\n";
    let bare_create = shim_command(
        &["issue", "comment", "42", "--body", "new comment"],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .output()
    .expect("spawn bare governed comment invocation");
    assert_eq!(bare_create.status.code(), Some(86));
    assert_eq!(
        String::from_utf8_lossy(&bare_create.stderr),
        governed_stderr
    );

    for args in [
        &[
            "issue",
            "comment",
            "42",
            "--body",
            "replace the draft",
            "--edit-last",
        ][..],
        &[
            "pr",
            "comment",
            "7",
            "--body",
            "replace the draft",
            "--edit-last",
        ][..],
    ] {
        let output = shim_command(
            args,
            &project,
            &config_home,
            &state_home,
            &home,
            &upstream_bin,
            &recorder,
        )
        .output()
        .expect("spawn governed edit-last invocation");
        assert_eq!(
            output.status.code(),
            Some(86),
            "edit-last must stay governed"
        );
        assert_eq!(String::from_utf8_lossy(&output.stderr), governed_stderr);
        assert!(output.stdout.is_empty());
    }

    // Raw PATCH is deliberately outside the GET-only API seam: an issue-comment
    // id is repository-scoped and can identify a human contributor's comment.
    let raw_patch = shim_command(
        &[
            "api",
            "--method",
            "PATCH",
            "repos/cortexkit/aft/issues/comments/123",
        ],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .output()
    .expect("spawn raw comment PATCH invocation");
    assert_eq!(raw_patch.status.code(), Some(86));
    let raw_patch_refusal = String::from_utf8_lossy(&raw_patch.stderr);
    assert_eq!(raw_patch_refusal, unclassified_refusal(10));
    assert_ne!(raw_patch_refusal, governed_stderr);
    assert!(raw_patch.stdout.is_empty());

    // --delete-last performs a different mutation and is not an alias for the
    // authenticated-user-only edit operation performed by --edit-last.
    let delete_last = shim_command(
        &[
            "pr",
            "comment",
            "7",
            "--body",
            "replace the draft",
            "--delete-last",
        ],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .output()
    .expect("spawn unclassified delete-last invocation");
    assert_eq!(delete_last.status.code(), Some(86));
    assert_eq!(
        String::from_utf8_lossy(&delete_last.stderr),
        unclassified_refusal(10)
    );
    assert!(
        !recorder.exists(),
        "refused or governed writes must not reach gh"
    );
}

#[test]
fn gh_shim_v10_run_rerun_is_operator_bypassed_reads_passthrough_and_cancel_is_refused() {
    let temp = tempfile::tempdir().expect("create test root");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let home = temp.path().join("home");
    let project = write_project_repo(temp.path());
    let connection_file = write_dead_connection_file(temp.path());
    let upstream_bin = temp.path().join("upstream-bin");
    let recorder = temp.path().join("upstream-invocations.txt");
    write_upstream_gh(&upstream_bin);
    let now = unix_seconds();
    // The signed test manifest includes `run rerun` and `run cancel`, so this
    // test reaches the code-side review decision instead of failing earlier
    // because either action is absent from the manifest.
    write_fresh_v10_admin_manifest(&state_home, now);
    write_fresh_r3_cache_for_manifest(&state_home, now, 10);
    write_user_config(&config_home, &connection_file, None);

    let expected_admin_refusal =
        "gh-shim: gh_shim_admin_tier: this action requires GH_SHIM_BYPASS=operator\n";
    let rerun_without_bypass = shim_command(
        &["run", "rerun", "123", "--failed"],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .output()
    .expect("spawn non-bypassed rerun");
    assert_eq!(rerun_without_bypass.status.code(), Some(86));
    assert!(rerun_without_bypass.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&rerun_without_bypass.stderr),
        expected_admin_refusal
    );

    let rerun_with_bypass = shim_command(
        &["run", "rerun", "123", "--job", "17"],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .env("GH_SHIM_BYPASS", "operator")
    .output()
    .expect("spawn operator-bypassed rerun");
    assert_eq!(rerun_with_bypass.status.code(), Some(73));
    assert_eq!(
        String::from_utf8_lossy(&rerun_with_bypass.stdout),
        "r2-passthrough\n"
    );
    assert!(rerun_with_bypass.stderr.is_empty());

    let audit_path = state_home.join("cortexkit/aft/gh-shim/operator-bypass.jsonl");
    let audit_records = fs::read_to_string(audit_path)
        .expect("read rerun bypass audit")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("parse rerun bypass audit row"))
        .collect::<Vec<_>>();
    assert_eq!(audit_records.len(), 1);
    assert_eq!(audit_records[0]["tuple"], "run rerun");
    assert_eq!(audit_records[0]["repository"], "cortexkit/aft");

    for args in [&["run", "view", "123"][..], &["run", "watch", "123"][..]] {
        let read = shim_command(
            args,
            &project,
            &config_home,
            &state_home,
            &home,
            &upstream_bin,
            &recorder,
        )
        .output()
        .expect("spawn mechanical Actions read");
        assert_eq!(read.status.code(), Some(73), "read passthrough: {args:?}");
        assert_eq!(String::from_utf8_lossy(&read.stdout), "r2-passthrough\n");
        assert!(read.stderr.is_empty());
    }

    let cancel = shim_command(
        &["run", "cancel", "123"],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .env("GH_SHIM_BYPASS", "operator")
    .output()
    .expect("spawn operator-bypassed cancel");
    assert_eq!(cancel.status.code(), Some(86));
    assert!(cancel.stdout.is_empty());
    let cancel_refusal = String::from_utf8_lossy(&cancel.stderr);
    assert_eq!(cancel_refusal, unclassified_refusal(10));
    assert_ne!(
        expected_admin_refusal, cancel_refusal,
        "rerun's reviewed admin refusal and cancel's unclassified refusal must differ"
    );

    assert_eq!(
        fs::read_to_string(recorder).expect("read upstream invocation record"),
        "run rerun 123 --job 17\nrun view 123\nrun watch 123\n"
    );
}

#[test]
fn gh_shim_governed_binding_refuses_writes_when_daemon_is_unreachable() {
    let temp = tempfile::tempdir().expect("create test root");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let home = temp.path().join("home");
    let project = write_project_repo(temp.path());
    let connection_file = write_dead_connection_file(temp.path());
    let upstream_bin = temp.path().join("upstream-bin");
    let recorder = temp.path().join("upstream-invocations.txt");
    write_upstream_gh(&upstream_bin);
    write_fresh_manifest(&state_home, unix_seconds());
    write_user_config(&config_home, &connection_file, None);

    let expected_stderr = "gh-shim: gh_shim_governance_unavailable: the governance daemon is unreachable and this repository's actions are identity-governed; retry after the daemon returns\n";
    for args in [
        &["issue", "comment", "42", "--body", "hello"][..],
        &["pr", "merge", "42"][..],
    ] {
        let output = shim_command(
            args,
            &project,
            &config_home,
            &state_home,
            &home,
            &upstream_bin,
            &recorder,
        )
        .output()
        .expect("spawn governed gh shim invocation");
        assert_eq!(output.status.code(), Some(86));
        assert!(output.stdout.is_empty());
        assert_eq!(String::from_utf8_lossy(&output.stderr), expected_stderr);
        assert!(
            !recorder.exists(),
            "governed and admin actions must not reach upstream gh"
        );
    }

    let unclassified = shim_command(
        &["alias", "set", "shortcut", "issue list"],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .output()
    .expect("spawn unclassified gh shim invocation");
    assert_eq!(unclassified.status.code(), Some(86));
    assert!(unclassified.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&unclassified.stderr),
        unclassified_refusal(1)
    );
    assert!(
        !recorder.exists(),
        "unclassified actions on a governed repository must not reach upstream gh"
    );

    let status = shim_command(
        &["--status"],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .output()
    .expect("spawn gh shim status");
    assert!(status.status.success());
    let report: Value = serde_json::from_slice(&status.stdout).expect("parse gh shim status JSON");
    assert_eq!(
        report["last_seam_refusal"]["code"],
        "gh_shim_governance_unavailable"
    );

    let mechanical = shim_command(
        &["issue", "view", "42"],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .output()
    .expect("spawn mechanical gh shim invocation");
    assert_eq!(mechanical.status.code(), Some(73));
    assert_eq!(
        String::from_utf8_lossy(&mechanical.stdout),
        "r2-passthrough\n"
    );
    assert_eq!(
        fs::read_to_string(&recorder).expect("read upstream invocation record"),
        "issue view 42\n"
    );
}

#[test]
fn gh_shim_bound_write_refuses_at_r1_without_reason_string_allowlisting() {
    let temp = tempfile::tempdir().expect("create test root");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let home = temp.path().join("home");
    let project = write_project_repo(temp.path());
    let upstream_bin = temp.path().join("upstream-bin");
    let recorder = temp.path().join("upstream-invocations.txt");
    write_upstream_gh(&upstream_bin);
    write_fresh_manifest(&state_home, unix_seconds());

    let output = shim_command(
        &["issue", "comment", "42", "--body", "hello"],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .output()
    .expect("spawn R1 governed gh shim invocation");
    assert_eq!(output.status.code(), Some(86));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "gh-shim: gh_shim_governance_unavailable: the governance daemon is unreachable and this repository's actions are identity-governed; retry after the daemon returns\n"
    );
    assert!(
        !recorder.exists(),
        "a bound governed write at R1 must not reach ambient gh credentials"
    );

    let admin = shim_command(
        &["pr", "merge", "42"],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .env("GH_SHIM_BYPASS", "operator")
    .output()
    .expect("spawn R1 admin gh shim invocation");
    assert_eq!(admin.status.code(), Some(86));
    assert_eq!(
        String::from_utf8_lossy(&admin.stderr),
        "gh-shim: gh_shim_governance_unavailable: the governance daemon is unreachable and this repository's actions are identity-governed; retry after the daemon returns\n"
    );
    assert!(
        !recorder.exists(),
        "R1 must refuse admin actions before considering operator bypass"
    );
}

#[test]
fn gh_shim_bound_governed_write_refuses_ambient_credential_identity_ambiguity() {
    let temp = tempfile::tempdir().expect("create test root");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let home = temp.path().join("home");
    let project = write_project_repo(temp.path());
    let connection_file = write_dead_connection_file(temp.path());
    let upstream_bin = temp.path().join("upstream-bin");
    let recorder = temp.path().join("upstream-invocations.txt");
    write_upstream_gh(&upstream_bin);
    let now = unix_seconds();
    write_fresh_manifest(&state_home, now);
    write_fresh_ambient_credentials_r2_cache(&state_home, now);
    write_user_config(&config_home, &connection_file, None);

    let output = shim_command(
        &["issue", "comment", "42", "--body", "hello"],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .env("GH_TOKEN", "ambient-operator-credential")
    .output()
    .expect("spawn identity-ambiguous governed gh shim invocation");
    assert_eq!(output.status.code(), Some(86));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "gh-shim: gh_shim_governance_unavailable: the governance daemon is unreachable and this repository's actions are identity-governed; retry after the daemon returns\n"
    );
    assert!(
        !recorder.exists(),
        "ambient credentials must not receive a bound governed invocation"
    );
}

#[test]
fn gh_shim_without_manifest_keeps_unreachable_daemon_passthrough() {
    let temp = tempfile::tempdir().expect("create test root");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let home = temp.path().join("home");
    let project = write_project_repo(temp.path());
    let connection_file = write_dead_connection_file(temp.path());
    let upstream_bin = temp.path().join("upstream-bin");
    let recorder = temp.path().join("upstream-invocations.txt");
    write_upstream_gh(&upstream_bin);
    write_user_config(&config_home, &connection_file, None);

    let output = shim_command(
        &["issue", "comment", "42", "--body", "hello"],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .output()
    .expect("spawn dormant gh shim invocation");
    assert_eq!(output.status.code(), Some(73));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "r2-passthrough\n");
    assert!(output.stderr.is_empty());
    assert_eq!(
        fs::read_to_string(&recorder).expect("read upstream invocation record"),
        "issue comment 42 --body hello\n"
    );
}

#[test]
fn gh_shim_invalid_manifest_announces_ambient_credential_fallback() {
    let temp = tempfile::tempdir().expect("create test root");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let home = temp.path().join("home");
    let project = write_project_repo(temp.path());
    let connection_file = write_dead_connection_file(temp.path());
    let upstream_bin = temp.path().join("upstream-bin");
    let recorder = temp.path().join("upstream-invocations.txt");
    write_upstream_gh(&upstream_bin);
    write_invalid_manifest(&state_home, unix_seconds());
    write_user_config(&config_home, &connection_file, None);

    let output = shim_command(
        &["issue", "comment", "42", "--body", "hello"],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .output()
    .expect("spawn invalid-manifest gh shim invocation");

    assert_eq!(output.status.code(), Some(73));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "r2-passthrough\n");
    assert_eq!(
        output.stderr,
        b"gh-shim: manifest invalid (untrusted manifest key id gh-routing-untrusted-test-key); executing with ambient gh credentials\n"
    );
    assert_eq!(
        fs::read_to_string(&recorder).expect("read upstream invocation record"),
        "issue comment 42 --body hello\n"
    );
}

#[test]
fn gh_shim_disabled_by_config_overrides_governance_stickiness() {
    let temp = tempfile::tempdir().expect("create test root");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let home = temp.path().join("home");
    let project = write_project_repo(temp.path());
    let connection_file = write_dead_connection_file(temp.path());
    let upstream_bin = temp.path().join("upstream-bin");
    let recorder = temp.path().join("upstream-invocations.txt");
    write_upstream_gh(&upstream_bin);
    write_fresh_manifest(&state_home, unix_seconds());
    write_user_config(&config_home, &connection_file, Some(false));

    let output = shim_command(
        &["issue", "comment", "42", "--body", "hello"],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .output()
    .expect("spawn disabled gh shim invocation");
    assert_eq!(output.status.code(), Some(73));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "r2-passthrough\n");
    assert!(output.stderr.is_empty());
    assert_eq!(
        fs::read_to_string(&recorder).expect("read upstream invocation record"),
        "issue comment 42 --body hello\n"
    );
}

#[test]
fn co_author_line_uses_the_cached_manifest_binding_and_numeric_id_offline() {
    let temp = tempfile::tempdir().expect("create test root");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let home = temp.path().join("home");
    let project = write_project_repo(temp.path());
    let connection_file = write_dead_connection_file(temp.path());
    let upstream_bin = temp.path().join("upstream-bin");
    let recorder = temp.path().join("upstream-invocations.txt");
    write_upstream_gh(&upstream_bin);
    write_fresh_manifest(&state_home, unix_seconds());
    write_numeric_ids(&state_home, json!({ "alfonso-aft": 289616620 }));
    write_user_config(&config_home, &connection_file, None);

    let output = shim_command(
        &["--co-author-line"],
        &project,
        &config_home,
        &state_home,
        &home,
        &upstream_bin,
        &recorder,
    )
    .output()
    .expect("spawn co-author self-report");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Co-authored-by: alfonso-aft <289616620+alfonso-aft@users.noreply.github.com>\n"
    );
    assert!(output.stderr.is_empty());
    assert!(
        !recorder.exists(),
        "a warm numeric-id cache must stay offline"
    );
}

#[test]
fn co_author_line_resolves_a_missing_numeric_id_once_and_caches_it() {
    let temp = tempfile::tempdir().expect("create test root");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let home = temp.path().join("home");
    let project = write_project_repo(temp.path());
    let connection_file = write_dead_connection_file(temp.path());
    let upstream_bin = temp.path().join("upstream-bin");
    let recorder = temp.path().join("upstream-invocations.txt");
    write_upstream_gh_user_api(&upstream_bin);
    write_fresh_manifest(&state_home, unix_seconds());
    write_user_config(&config_home, &connection_file, None);

    for _ in 0..2 {
        let output = shim_command(
            &["--co-author-line"],
            &project,
            &config_home,
            &state_home,
            &home,
            &upstream_bin,
            &recorder,
        )
        .output()
        .expect("spawn co-author self-report");
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "Co-authored-by: alfonso-aft <289616620+alfonso-aft@users.noreply.github.com>\n"
        );
    }

    assert_eq!(
        fs::read_to_string(&recorder).expect("read API invocation record"),
        "api users/alfonso-aft --jq .id\n"
    );
    let ids: Value = serde_json::from_slice(
        &fs::read(state_home.join("cortexkit/aft/gh-shim/numeric-ids.json"))
            .expect("read numeric id cache"),
    )
    .expect("parse numeric id cache");
    assert_eq!(ids["alfonso-aft"], 289616620);
}

#[test]
fn gh_shim_v11_thread_state_verbs_stay_admin_and_v12_refuses_reason_and_delete_branch() {
    let temp = tempfile::tempdir().expect("create test root");
    let config_home = temp.path().join("config");
    let home = temp.path().join("home");
    let connection_file = write_dead_connection_file(temp.path());
    let upstream_bin = temp.path().join("upstream-bin");
    let project = write_project_repo(temp.path());
    write_upstream_gh(&upstream_bin);
    write_user_config(&config_home, &connection_file, None);
    let now = unix_seconds();

    let v11_state = temp.path().join("v11-state");
    let v11_recorder = temp.path().join("v11-upstream-invocations.txt");
    write_fresh_v11_manifest(&v11_state, now);
    write_fresh_r3_cache_for_manifest(&v11_state, now, 11);
    let expected_admin_refusal =
        "gh-shim: gh_shim_admin_tier: this action requires GH_SHIM_BYPASS=operator\n";
    for args in [
        &["issue", "close", "42"][..],
        &["issue", "reopen", "42"][..],
        &["pr", "close", "7"][..],
        &["pr", "reopen", "7"][..],
    ] {
        let output = shim_command(
            args,
            &project,
            &config_home,
            &v11_state,
            &home,
            &upstream_bin,
            &v11_recorder,
        )
        .output()
        .expect("spawn v11 admin thread-state invocation");
        assert_eq!(output.status.code(), Some(86));
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            expected_admin_refusal
        );
    }
    assert!(
        !v11_recorder.exists(),
        "v11 admin thread-state verbs must not reach upstream gh"
    );

    let v12_state = temp.path().join("v12-state");
    let v12_recorder = temp.path().join("v12-upstream-invocations.txt");
    write_fresh_v12_manifest(&v12_state, now);
    write_fresh_r3_cache_for_manifest(&v12_state, now, 12);

    let missing_reason = shim_command(
        &["issue", "close", "42", "--repo", "cortexkit/aft"],
        &project,
        &config_home,
        &v12_state,
        &home,
        &upstream_bin,
        &v12_recorder,
    )
    .output()
    .expect("spawn v12 issue close without --reason");
    assert_eq!(missing_reason.status.code(), Some(86));
    assert!(missing_reason.stdout.is_empty());
    let missing_stderr = String::from_utf8_lossy(&missing_reason.stderr);
    assert!(
        missing_stderr.contains("gh_shim_missing_reason"),
        "missing --reason must use the typed refusal: {missing_stderr}"
    );
    assert!(
        missing_stderr.contains("--reason"),
        "missing-reason refusal must name --reason: {missing_stderr}"
    );

    for flag in ["--delete-branch", "-d"] {
        let output = shim_command(
            &["pr", "close", "7", flag, "--repo", "cortexkit/aft"],
            &project,
            &config_home,
            &v12_state,
            &home,
            &upstream_bin,
            &v12_recorder,
        )
        .output()
        .expect("spawn v12 pr close with destructive flag");
        assert_eq!(output.status.code(), Some(86));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("gh_shim_destructive_flag"),
            "{flag} must use gh_shim_destructive_flag: {stderr}"
        );
        assert!(
            stderr.contains(flag),
            "destructive refusal must name {flag}: {stderr}"
        );
        assert!(
            stderr.contains("branch deletion stays undeclared"),
            "destructive refusal must keep branch deletion undeclared: {stderr}"
        );
    }
    assert!(
        !v12_recorder.exists(),
        "v12 pre-routing refusals must not reach upstream gh"
    );
}

#[test]
fn auto_child_hook_commits_the_cached_bound_identity_exactly_once_on_amend() {
    let temp = tempfile::tempdir().expect("create test root");
    let state_home = temp.path().join("state");
    let config_home = temp.path().join("config");
    let home = temp.path().join("home");
    let storage = temp.path().join("storage");
    let project = write_project_repo(temp.path());
    write_fresh_manifest(&state_home, unix_seconds());
    write_numeric_ids(&state_home, json!({ "alfonso-aft": 289616620 }));
    fs::write(project.join("tracked.txt"), "joint work\n").expect("write tracked file");
    for args in [
        &["config", "user.name", "AFT Test"][..],
        &["config", "user.email", "aft-test@example.test"][..],
        &["add", "tracked.txt"][..],
    ] {
        let status = Command::new("git")
            .args(args)
            .current_dir(&project)
            .status()
            .expect("run git setup command");
        assert!(status.success(), "git setup failed: {args:?}");
    }

    let binary = aft_binary();
    test_helpers::warm_executable(&binary, &["--version"]);
    let mut config = Config::default();
    config.gh_shim.enabled = false;
    config.gh_shim.binary_path = Some(binary);
    config.git.co_author = "auto".to_string();
    let inherited_path = std::env::var_os("PATH").expect("test PATH");
    let mut environment = std::collections::HashMap::from([
        (
            "PATH".to_string(),
            inherited_path.to_string_lossy().into_owned(),
        ),
        (
            "XDG_STATE_HOME".to_string(),
            state_home.to_string_lossy().into_owned(),
        ),
        (
            "XDG_CONFIG_HOME".to_string(),
            config_home.to_string_lossy().into_owned(),
        ),
        ("HOME".to_string(), home.to_string_lossy().into_owned()),
    ]);
    aft::agent_child_env::inject(&config, &storage, &mut environment)
        .expect("inject child Git environment");

    for args in [
        &["commit", "--quiet", "-m", "mason: joint work"][..],
        &["commit", "--quiet", "--amend", "--no-edit"][..],
    ] {
        let status = Command::new("git")
            .args(args)
            .current_dir(&project)
            .envs(&environment)
            .status()
            .expect("run governed commit");
        assert!(status.success(), "governed commit failed: {args:?}");
    }

    let output = Command::new("git")
        .args(["log", "-1", "--format=%B"])
        .current_dir(&project)
        .output()
        .expect("read commit message");
    assert!(output.status.success());
    let message = String::from_utf8(output.stdout).expect("commit message UTF-8");
    assert_eq!(message.matches("Co-authored-by:").count(), 1);
    assert!(message
        .contains("Co-authored-by: alfonso-aft <289616620+alfonso-aft@users.noreply.github.com>"));
}

#[test]
fn shim_invoked_as_gh_skips_its_managed_path_entry_and_execs_upstream_once() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("create test root");
    let project = write_project_repo(temp.path());
    let shims = temp.path().join("shims");
    let upstream = temp.path().join("upstream");
    let recorder = temp.path().join("upstream-invocations.txt");
    fs::create_dir_all(&shims).expect("create shims directory");
    symlink(aft_binary(), shims.join("gh")).expect("create managed gh link");
    write_upstream_gh(&upstream);
    let inherited = std::env::var_os("PATH").expect("test PATH");
    let path = std::env::join_paths(
        [shims.clone(), upstream]
            .into_iter()
            .chain(std::env::split_paths(&inherited)),
    )
    .expect("build shim PATH");

    let output = Command::new(shims.join("gh"))
        .args(["issue", "list"])
        .current_dir(project)
        .env("PATH", path)
        .env("AFT_GH_SHIMS_DIR", &shims)
        .env(
            "AFT_GH_SHIM_STATE_DIR",
            temp.path().join("state/cortexkit/aft/gh-shim"),
        )
        .env("GH_SHIM_TEST_RECORD", &recorder)
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("HOME", temp.path().join("home"))
        .output()
        .expect("spawn managed gh entry");

    assert_eq!(output.status.code(), Some(73));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "r2-passthrough\n");
    assert!(output.stderr.is_empty());
    assert_eq!(
        fs::read_to_string(recorder).expect("read upstream invocation record"),
        "issue list\n"
    );
}

#[test]
fn co_author_line_is_silently_empty_without_a_cached_manifest_binding() {
    let temp = tempfile::tempdir().expect("create test root");
    let project = write_project_repo(temp.path());
    let upstream = temp.path().join("upstream");
    let recorder = temp.path().join("upstream-invocations.txt");
    write_upstream_gh(&upstream);

    let output = shim_command(
        &["--co-author-line"],
        &project,
        &temp.path().join("config"),
        &temp.path().join("state"),
        &temp.path().join("home"),
        &upstream,
        &recorder,
    )
    .output()
    .expect("spawn inert co-author self-report");

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(!recorder.exists());
}
