//! Tool-provider v1 route selection through a real subc daemon.
//!
//! The consumer declares its role versions on `route.open`; the daemon
//! forwards them, unverified, to AFT's bind. This test runs the configured
//! `ck-subc` bytes from a stable content-addressed test cache under the
//! `ckdev-subc` name (subc-daemon 0.29 or later, which advertises
//! `route-role-versions/v1`) in a temporary, isolated home, runs it with the
//! `aft` binary this package builds as its module, and checks from the
//! consumer's side that:
//!
//! - a route opened with `role_versions: {"tool-provider": "v1"}` is served
//!   the catalog and admits calls under the v1 grammar;
//! - a route opened without role versions, and one that declares only some
//!   other role, keep the legacy plugin grammar, and an ordinary plugin call
//!   on them returns the same bytes;
//! - a tool-provider version AFT does not serve refuses the route.
//!
//! The TypeScript bridge rig cannot drive this: `@cortexkit/subc-client`
//! 0.20 has no role-versions option on `routeOpen`, so the consumer here is
//! `subc-client-rs`, whose `CallOptions::role_versions` sends the field.
//!
//! Ignored by default because it needs a daemon binary. Run with, e.g.:
//!
//! ```text
//! AFT_E2E_SUBC_BIN=~/.local/share/cortexkit/bin/ck-subc \
//! cargo test -p agent-file-tools --test rest tool_provider_subc_e2e -- \
//!     --ignored --nocapture
//! ```
//!
//! The daemon never touches the operator's own: every XDG directory, `HOME`
//! and `TMPDIR` point into a temporary directory, the port is kernel-assigned,
//! and the daemon runs in its own process group, which is killed at the end.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use subc_client_rs::{CallError, CallOptions, ConsumerOptions, RouteHandle, SubcConsumer};
use subc_protocol::{BindIdentity, RouteTarget};

#[path = "helpers/aft_binary.rs"]
mod aft_binary;

/// A `ckdev-subc` daemon in its own process group under an isolated home.
/// Dropping it kills the group, which includes the aft module it spawned.
struct HermeticDaemon {
    child: Child,
    connection_file: PathBuf,
}

impl HermeticDaemon {
    fn start(subc_bin: &Path, home: &Path) -> Self {
        let runtime = home.join("runtime");
        let connection_file = runtime.join("subc-connection.json");
        let test_bin = cache_test_daemon(subc_bin);
        let child = Command::new(&test_bin)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", home.join("home"))
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("XDG_STATE_HOME", home.join("state"))
            .env("XDG_CACHE_HOME", home.join("cache"))
            .env("TMPDIR", home.join("tmp"))
            .env("SUBC_PORT", "0")
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                std::fs::File::create(home.join("subc.stderr.log")).unwrap(),
            ))
            .spawn()
            .expect("start hermetic ckdev-subc");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !connection_file.exists() {
            assert!(
                Instant::now() < deadline,
                "hermetic ckdev-subc did not publish its connection file"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        Self {
            child,
            connection_file,
        }
    }
}

fn cache_test_daemon(subc_bin: &Path) -> PathBuf {
    let bytes = std::fs::read(subc_bin).expect("read daemon for the executable cache");
    crate::test_helpers::executable_cache::cached_executable_named(&bytes, "ckdev-subc")
        .expect("cache immutable test daemon")
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
        // The daemon's pid is its process group id (process_group(0)), so
        // this also stops the aft module the daemon spawned.
        unsafe {
            libc::killpg(self.child.id() as libc::pid_t, libc::SIGKILL);
        }
        let _ = self.child.wait();
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
    std::fs::write(
        home.join("config/cortexkit/subc.jsonc"),
        serde_json::to_vec_pretty(&json!({
            "version": 1,
            "storage": {"backend": "sqlite", "data_home": home.join("data")},
            "modules": {"aft": {
                "program": aft_binary::aft_binary(),
                "args": [],
                "enabled": true,
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
    consumer
        .open_route(
            RouteTarget::ToolProvider {
                module_id: "aft".into(),
            },
            BindIdentity::new(project, "runner", session),
            options(role_versions),
        )
        .await
}

async fn request(
    consumer: &SubcConsumer,
    route: &RouteHandle,
    body: Value,
) -> Result<Vec<u8>, CallError> {
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

/// Compare model-only replies as sent. Plugin replies additionally carry an
/// internal request id; normalize only that id, checking its route channel.
fn without_request_id(reply: &[u8], channel: u16) -> String {
    let text = String::from_utf8(reply.to_vec()).expect("reply is UTF-8");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert!(body["content"].is_array(), "{text}");
    assert!(body["isError"].is_boolean(), "{text}");
    let Some(structured) = body.get("structuredContent") else {
        return text;
    };
    let id = structured["id"]
        .as_str()
        .expect("plugin reply carries a request id")
        .to_string();
    assert!(
        id.starts_with(&format!("subc-{channel}-")),
        "{id} names route channel {channel}",
    );
    let quoted = format!("\"{id}\"");
    assert_eq!(text.matches(&quoted).count(), 1, "{text}");
    text.replace(&quoted, "\"<request-id>\"")
}

#[test]
fn legacy_reply_comparison_preserves_model_only_bytes() {
    let reply = br#"{ "content":[{"type":"text","text":"seed.txt"}], "isError":false }"#;
    assert_eq!(without_request_id(reply, 7).as_bytes(), reply);
}

#[test]
fn legacy_reply_comparison_normalizes_only_the_plugin_request_id() {
    let reply = br#"{"content":[{"type":"text","text":"seed.txt"}],"isError":false,"structuredContent":{"id":"subc-7-42","success":true,"text":"seed.txt"}}"#;
    assert_eq!(
        without_request_id(reply, 7),
        r#"{"content":[{"type":"text","text":"seed.txt"}],"isError":false,"structuredContent":{"id":"<request-id>","success":true,"text":"seed.txt"}}"#
    );
}

#[test]
#[should_panic(expected = "names route channel")]
fn legacy_reply_comparison_rejects_a_plugin_id_from_another_route() {
    without_request_id(
        br#"{"content":[],"isError":false,"structuredContent":{"id":"subc-8-42"}}"#,
        7,
    );
}

#[tokio::test]
#[ignore = "needs a real ck-subc daemon (subc-daemon 0.29+); set AFT_E2E_SUBC_BIN"]
async fn tool_provider_subc_e2e_role_versions_select_v1_and_leave_legacy_routes_unchanged() {
    let subc_bin = PathBuf::from(
        std::env::var_os("AFT_E2E_SUBC_BIN").expect("set AFT_E2E_SUBC_BIN to a ck-subc binary"),
    );
    let (_temp, home, project) = hermetic_home();
    let daemon = HermeticDaemon::start(&subc_bin, &home);
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
        without_request_id(&legacy_bytes, legacy.channel),
        without_request_id(&other_role_bytes, other_role.channel)
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
