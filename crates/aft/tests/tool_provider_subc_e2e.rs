//! Tool-provider route selection and scoped project I/O through daemon 0.39.
//!
//! The daemon is embedded from the dev-only `subc-daemon` crate. It launches
//! AFT and a test-only scope owner with isolated configuration and storage.
//! The owner registers a workdir; project-free consumers then open scoped
//! routes through daemon admission, which attaches the registered scope's
//! workdir to the route bind AFT receives.
//!
//! Run the opt-in tests with:
//!
//! ```text
//! cargo test -p agent-file-tools --features test-scope-owner --test rest \
//!     tool_provider_subc_e2e -- --ignored --nocapture
//! ```

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use subc_client_rs::{CallError, CallOptions, ConsumerOptions, RouteHandle, SubcConsumer};
use subc_protocol::{BindIdentity, RouteTarget};

#[path = "helpers/aft_binary.rs"]
mod aft_binary;

/// An embedded daemon with supervised children, all using fixture-only paths.
/// Aborting its task drops the supervisor and its kill-on-drop children.
struct HermeticDaemon {
    task: tokio::task::JoinHandle<Result<(), subc_daemon::bootstrap::BootstrapError>>,
    connection_file: PathBuf,
}

impl HermeticDaemon {
    async fn start(home: &Path) -> Self {
        let connection_file = home.join("runtime/subc-connection.json");
        let config = subc_daemon::bootstrap::BootstrapConfig::new(&connection_file, 0)
            .with_daemon_config_path(home.join("config/cortexkit/subc.jsonc"))
            .expect("isolated daemon config")
            .with_capture_logs_dir(home.join("logs"))
            .with_terminal_journal_path(home.join("terminals.jsonl"));
        let task = tokio::spawn(subc_daemon::bootstrap::run_with_config(config));
        let deadline = Instant::now() + Duration::from_secs(60);
        while !connection_file.exists() {
            assert!(!task.is_finished(), "embedded daemon exited during startup");
            assert!(
                Instant::now() < deadline,
                "embedded daemon did not publish its connection file"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Self {
            task,
            connection_file,
        }
    }
}

#[test]
fn cached_test_daemon_keeps_ckdev_name() {
    let script = b"#!/bin/sh\nprintf '%s\\n' \"$0\"\n";
    let first =
        crate::test_helpers::executable_cache::cached_executable_named(script, "ckdev-subc")
            .unwrap();
    let second =
        crate::test_helpers::executable_cache::cached_executable_named(script, "ckdev-subc")
            .unwrap();
    assert_eq!(first, second);
    assert_eq!(first.file_name(), Some(std::ffi::OsStr::new("ckdev-subc")));
    assert_eq!(first.parent().unwrap().file_name().unwrap().len(), 64);
    let output = Command::new(&first)
        .output()
        .expect("run cached daemon fixture");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        first.to_string_lossy()
    );
}

impl Drop for HermeticDaemon {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn hermetic_home() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(temp.path()).unwrap();
    for dir in [
        "runtime",
        "config/cortexkit",
        "data",
        "state",
        "cache",
        "tmp",
        "home",
        "project/.cortexkit",
    ] {
        std::fs::create_dir_all(home.join(dir)).unwrap();
    }
    let module_env = json!({
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "HOME": home.join("home"),
        "XDG_RUNTIME_DIR": home.join("runtime"),
        "XDG_CONFIG_HOME": home.join("config"),
        "XDG_DATA_HOME": home.join("data"),
        "XDG_STATE_HOME": home.join("state"),
        "XDG_CACHE_HOME": home.join("cache"),
        "AFT_STORAGE_DIR": home.join("data/aft"),
        "TMPDIR": home.join("tmp"),
    });
    std::fs::write(
        home.join("config/cortexkit/subc.jsonc"),
        serde_json::to_vec_pretty(&json!({
            "version": 1,
            "storage": {"backend": "sqlite", "data_home": home.join("data")},
            "modules": {"aft": {
                "program": aft_binary::aft_binary(),
                "args": [],
                "enabled": true,
                "env": module_env,
            }, "aft-test-scope-owner": {
                "program": option_env!("CARGO_BIN_EXE_aft-test-scope-owner")
                    .expect("build the test with --features test-scope-owner"),
                "args": [],
                "enabled": true,
                "env": module_env,
            }},
        }))
        .unwrap(),
    )
    .unwrap();
    // Keep the module light: nothing here needs an index.
    let quiet = json!({
        "indexes": { "trigram": false, "semantic": false, "callgraph": false },
        "disabled_tools": [],
    });
    std::fs::write(
        home.join("config/cortexkit/aft.jsonc"),
        serde_json::to_vec(&quiet).unwrap(),
    )
    .unwrap();
    let project = home.join("project");
    std::fs::write(
        project.join(".cortexkit/aft.jsonc"),
        serde_json::to_vec(&quiet).unwrap(),
    )
    .unwrap();
    std::fs::write(project.join("seed.txt"), "seed line one\nseed line two\n").unwrap();
    (temp, home, project)
}

fn options(role_versions: Option<&[(&str, &str)]>) -> CallOptions {
    CallOptions {
        timeout: Duration::from_secs(60),
        route_retry_deadline: Duration::from_secs(60),
        role_versions: role_versions.map(|pairs| {
            pairs
                .iter()
                .map(|(role, version)| (role.to_string(), version.to_string()))
                .collect::<BTreeMap<_, _>>()
        }),
        ..CallOptions::default()
    }
}

async fn open(
    consumer: &SubcConsumer,
    project: &Path,
    session: &str,
    role_versions: Option<&[(&str, &str)]>,
) -> Result<RouteHandle, CallError> {
    let ready = project.parent().unwrap().join("scope-owner.ready");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !ready.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "test scope owner did not register its workdir"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let scope = subc_protocol::scope::ScopeSelector::new(
        subc_protocol::Principal::Reserved {
            module_id: "aft-test-scope-owner".into(),
        },
        "e2e-project",
        Some(1),
    );
    consumer
        .open_route_scoped(
            RouteTarget::ToolProvider {
                module_id: "aft".into(),
            },
            BindIdentity::new("runner", session),
            scope,
            options(role_versions),
        )
        .await
}

async fn request(
    consumer: &SubcConsumer,
    route: &RouteHandle,
    mut body: Value,
) -> Result<Vec<u8>, CallError> {
    // Scoped tool calls name their catalog preset; provider metadata does not.
    if body
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|name| name != "tool.catalog")
    {
        body.as_object_mut()
            .unwrap()
            .entry("preset")
            .or_insert(json!("head"));
    }
    consumer
        .request(route, serde_json::to_vec(&body).unwrap(), options(None))
        .await
}

fn refusal_code(error: &CallError) -> Option<String> {
    error
        .code()
        .map(str::to_string)
        .or_else(|| error.route_open_refusal().map(|body| body.code.clone()))
}

/// The reply's raw text with its request id, `subc-<channel>-<corr>`, replaced
/// by a placeholder. The id must name this route's channel; every other byte
/// is compared as sent.
fn without_request_id(reply: &[u8], route: &RouteHandle) -> String {
    let text = String::from_utf8(reply.to_vec()).expect("reply is UTF-8");
    let id = serde_json::from_str::<Value>(&text).unwrap()["structuredContent"]["id"]
        .as_str()
        .expect("reply carries a request id")
        .to_string();
    assert!(
        id.starts_with(&format!("subc-{}-", route.channel)),
        "{id} names route channel {}",
        route.channel
    );
    let quoted = format!("\"{id}\"");
    assert_eq!(text.matches(&quoted).count(), 1, "{text}");
    text.replace(&quoted, "\"<request-id>\"")
}

#[tokio::test]
#[ignore = "opt-in daemon 0.39 integration; run with --features test-scope-owner"]
async fn tool_provider_subc_e2e_role_versions_select_v1_and_leave_legacy_routes_unchanged() {
    let (_temp, home, project) = hermetic_home();
    let daemon = HermeticDaemon::start(&home).await;
    let consumer = SubcConsumer::connect(&daemon.connection_file, ConsumerOptions::default())
        .await
        .expect("connect to the hermetic daemon");

    // v1 route: the catalog is served, with system text naming its tools.
    let v1 = open(
        &consumer,
        &project,
        "e2e-v1",
        Some(&[("tool-provider", "v1")]),
    )
    .await
    .expect("open a tool-provider v1 route");
    let catalog: Value = serde_json::from_slice(
        &request(
            &consumer,
            &v1,
            json!({"name": "tool.catalog", "arguments": {"system_text": {"preset": "broca", "params": {"worker": false}}}}),
        )
        .await
        .expect("tool.catalog on the v1 route"),
    )
    .unwrap();
    let mut names: Vec<String> = catalog["tools"]
        .as_array()
        .expect("catalog tools")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.iter().any(|name| name == "read"), "{names:?}");
    names.sort();
    assert_eq!(catalog["system_text"]["tool_names"], json!(names));
    assert!(catalog["catalog_digest"]
        .as_str()
        .is_some_and(|d| !d.is_empty()));

    // The v1 grammar runs: a plain call succeeds, and an argument outside the
    // served schema is refused before anything runs.
    let status = request(&consumer, &v1, json!({"name": "status", "arguments": {}}))
        .await
        .expect("v1 status call");
    assert!(!status.is_empty());
    let refused = request(
        &consumer,
        &v1,
        json!({"name": "status", "arguments": {"not_served": true}}),
    )
    .await
    .expect_err("v1 admission refuses an unserved argument");
    assert_eq!(refusal_code(&refused).as_deref(), Some("invalid_request"));

    // Legacy routes: no role versions, and a declaration for some other role.
    let legacy = open(&consumer, &project, "e2e-legacy", None)
        .await
        .expect("open a legacy route");
    let unknown_op = request(
        &consumer,
        &legacy,
        json!({"op": "example.not_served", "params": {}}),
    )
    .await
    .expect_err("an operation the provider does not serve returns an error frame");
    assert_eq!(refusal_code(&unknown_op).as_deref(), Some("unknown_op"));
    let other_role = open(
        &consumer,
        &project,
        "e2e-legacy",
        Some(&[("other-role", "v1")]),
    )
    .await
    .expect("open a route declaring only another role");
    assert_ne!(
        (legacy.channel, legacy.epoch),
        (other_role.channel, other_role.epoch),
        "routes differing only in role versions are separate routes"
    );
    for route in [&legacy, &other_role] {
        // The legacy grammar tolerates extra plugin arguments ...
        request(
            &consumer,
            route,
            json!({"name": "status", "arguments": {"not_served": true}}),
        )
        .await
        .expect("a legacy route keeps the plugin grammar");
        // ... and never admits the v1 call operation.
        let refused = request(
            &consumer,
            route,
            json!({"op": "tool.call", "name": "status", "arguments": {}}),
        )
        .await
        .expect_err("tool.call is refused on a legacy route");
        assert_eq!(
            refusal_code(&refused).as_deref(),
            Some("unsupported_operation")
        );
    }
    // An ordinary plugin call returns the same bytes on both legacy routes,
    // apart from the request id AFT mints from each route's channel.
    let glob = json!({"name": "glob", "arguments": {"pattern": "*.txt"}});
    let legacy_bytes = request(&consumer, &legacy, glob.clone())
        .await
        .expect("legacy glob");
    let other_role_bytes = request(&consumer, &other_role, glob)
        .await
        .expect("glob on the other-role route");
    assert_eq!(
        without_request_id(&legacy_bytes, &legacy),
        without_request_id(&other_role_bytes, &other_role)
    );
    assert!(String::from_utf8_lossy(&legacy_bytes).contains("seed.txt"));

    // A tool-provider version AFT does not serve refuses the route.
    let unsupported = open(
        &consumer,
        &project,
        "e2e-v2",
        Some(&[("tool-provider", "v2")]),
    )
    .await
    .expect_err("an unserved tool-provider version refuses the route");
    let refusal = unsupported
        .route_open_refusal()
        .unwrap_or_else(|| panic!("the daemon relays AFT's bind refusal: {unsupported:?}"));
    assert_eq!(refusal.code, "unsupported_role_version");
    assert_eq!(
        refusal.detail.as_ref().map(|detail| &detail["versions"]),
        Some(&json!(["v1"]))
    );

    eprintln!(
        "v1 catalog: {} tools, digest {}; legacy glob bytes identical ({} bytes)",
        names.len(),
        catalog["catalog_digest"],
        legacy_bytes.len()
    );
    drop(consumer);
    drop(daemon);
}

#[tokio::test]
#[ignore = "opt-in daemon 0.39 integration; run with --features test-scope-owner"]
async fn tool_provider_subc_e2e_project_free_bind_uses_owned_scope_workdir() {
    let (_temp, home, project) = hermetic_home();
    let daemon = HermeticDaemon::start(&home).await;
    let consumer = SubcConsumer::connect(&daemon.connection_file, ConsumerOptions::default())
        .await
        .expect("connect embedded daemon 0.39");
    let route = open(
        &consumer,
        &project,
        "project-free-e2e",
        Some(&[("tool-provider", "v1")]),
    )
    .await
    .expect("open project-free scoped route");
    for (name, arguments) in [
        ("read", json!({"path":"seed.txt"})),
        ("grep", json!({"pattern":"seed line one", "path":"."})),
    ] {
        let bytes = request(
            &consumer,
            &route,
            json!({"name":name, "arguments":arguments}),
        )
        .await
        .unwrap_or_else(|error| panic!("scoped {name}: {error}"));
        let reply: Value = serde_json::from_slice(&bytes).unwrap();
        assert_ne!(reply["isError"], true, "{reply}");
        assert!(
            String::from_utf8_lossy(&bytes).contains("seed line one"),
            "{reply}"
        );
    }
    consumer.close().await;
    drop(daemon);
}
