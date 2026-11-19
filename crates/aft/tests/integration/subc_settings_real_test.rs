//! Linux tests against the config.gate daemon. Use CK_SUBC_BIN for the ordinary
//! build and CK_SUBC_DEV_BIN for the test-only scripted approval build. Both
//! use isolated homes and an explicit binary, never the operator's daemon.
//! Cases that approve a settings stamp launch AFT on the supervised
//! registration channel. The unstamped case exercises the legacy TCP module
//! connection.

use serde_json::{json, Value};
use std::{
    fs,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use subc_client_rs::{ConsumerOptions, SubcConsumer};
use subc_protocol::manifest::ProviderRole;

fn fixture_aft_binary() -> &'static Path {
    static BINARY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BINARY.get_or_init(|| {
        let source = crate::test_helpers::aft_binary::source_binary();
        let scratch = tempfile::tempdir().unwrap();
        let stripped = scratch.path().join("aft");
        // To approve a settings stamp, the daemon hashes the entire executable
        // and limits it to 512 MiB. Keep the debug binary's test hooks, not its DWARF.
        let output = Command::new("objcopy")
            .arg("--strip-debug")
            .arg(source)
            .arg(&stripped)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "objcopy: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        crate::test_helpers::executable_cache::cached_executable_named(
            &fs::read(stripped).unwrap(),
            "aft",
        )
        .unwrap()
    })
}

struct Daemon {
    child: Child,
    home: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Reap the fixture's whole process group, including its supervised AFT.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

impl Daemon {
    async fn launch(dev: bool, setup: impl FnOnce(&Path)) -> (Self, SubcConsumer) {
        let home = tempfile::tempdir().unwrap();
        for name in [
            "config/cortexkit",
            "data",
            "runtime",
            "state",
            "cache",
            "home",
            "tmp",
        ] {
            let dir = home.path().join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let fixture = home.path();
        let schema_path = fixture.join("aft-schema.json");
        let leaf = |kind, default: Value, apply| json!({"type":kind,"description":"Test setting","default":default,"x-ck-apply":apply});
        let mut disabled = leaf("array", json!([]), "module_restart");
        disabled["items"] = leaf("string", json!(""), "module_restart");
        let schema = json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object", "unevaluatedProperties":false,
            "x-ck-layers":["user"], "properties":{
                "disabled_tools":disabled,
                "memory":leaf("string", json!("balanced"), "live"),
                "formatter_timeout_secs":leaf("integer", json!(30), "live"),
            },
        });
        fs::write(&schema_path, schema.to_string()).unwrap();
        let module_env = json!({"PATH":"/usr/bin:/bin", "HOME":fixture.join("home"), "RUST_LOG":"info",
            "XDG_CONFIG_HOME":fixture.join("config"), "XDG_RUNTIME_DIR":fixture.join("runtime"),
            "XDG_DATA_HOME":fixture.join("data"), "XDG_STATE_HOME":fixture.join("state"),
            "XDG_CACHE_HOME":fixture.join("cache"), "TMPDIR":fixture.join("tmp"),
            "AFT_STORAGE_DIR":fixture.join("data/aft"), "AFT_TEST_SETTINGS_SCHEMA":schema_path});
        fs::write(fixture.join("config/cortexkit/subc.jsonc"), json!({
            "version":1,"port":0,"storage":{"backend":"sqlite","data_home":fixture.join("data")},
            "modules":{"aft":{"program":fixture_aft_binary(),"args":[],"enabled":true,"registration_channel":dev,"env":module_env}}
        }).to_string()).unwrap();
        setup(fixture);
        let binary_env = if dev {
            "CK_SUBC_DEV_BIN"
        } else {
            "CK_SUBC_BIN"
        };
        let binary = PathBuf::from(
            std::env::var_os(binary_env)
                .unwrap_or_else(|| panic!("set {binary_env} to the pinned Linux daemon")),
        );
        let binary = crate::test_helpers::executable_cache::cached_executable_named(
            &fs::read(binary).unwrap(),
            "ckdev-subc",
        )
        .unwrap();
        let mut command = Command::new(binary);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", fixture.join("home"))
            .env("XDG_CONFIG_HOME", fixture.join("config"))
            .env("XDG_RUNTIME_DIR", fixture.join("runtime"))
            .env("XDG_DATA_HOME", fixture.join("data"))
            .env("XDG_STATE_HOME", fixture.join("state"))
            .env("XDG_CACHE_HOME", fixture.join("cache"))
            .env("TMPDIR", fixture.join("tmp"))
            .env("SUBC_CGROUP_PLACEMENT", "disabled")
            .env("SUBC_PORT", "0")
            .arg("--config-store-root")
            .arg(fixture.join("authority"))
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                fs::File::create(fixture.join("daemon.stderr")).unwrap(),
            ));
        if dev {
            let script = fixture.join("operator");
            fs::write(&script, "approve\n".repeat(16)).unwrap();
            command
                .env("SUBC_TEST_CONFIG_GATE", "1")
                .env("SUBC_TEST_OPERATOR_SCRIPT", script)
                .env("SUBC_TEST_OPERATOR_EVENTS", fixture.join("operator-events"));
        }
        let child = command.spawn().unwrap();
        let mut daemon = Self { child, home };
        let connection = daemon.home.path().join("runtime/subc-connection.json");
        tokio::time::timeout(Duration::from_secs(30), async {
            while !connection.is_file() {
                assert!(
                    daemon.child.try_wait().unwrap().is_none(),
                    "daemon exited: {}",
                    daemon.log()
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("daemon connection deadline: {}", daemon.log()));
        let consumer = SubcConsumer::connect_local(&connection, ConsumerOptions::default())
            .await
            .unwrap();
        (daemon, consumer)
    }

    fn log(&self) -> String {
        fs::read_to_string(self.home.path().join("daemon.stderr")).unwrap_or_default()
    }
    fn user_file(&self) -> PathBuf {
        self.home.path().join("config/cortexkit/aft.jsonc")
    }
}

#[tokio::test]
#[ignore = "requires CK_SUBC_BIN with config.gate"]
async fn subc_settings_real_daemon_declares_then_filters_ready_catalog_and_applies_changed() {
    let (daemon, client) = Daemon::launch(false, |fixture| {
        fs::write(
            fixture.join("config/cortexkit/aft.jsonc"),
            r#"{"memory":"lean","disabled_tools":["aft_search"]}"#,
        )
        .unwrap();
    })
    .await;
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            let catalog = client.catalog_list().await.unwrap();
            check_bootstrap_error(&client).await;
            if let Some(aft) = catalog
                .modules
                .iter()
                .find(|entry| entry.module_id == "aft" && entry.ready)
            {
                let tools: Vec<_> = aft
                    .roles
                    .iter()
                    .filter_map(|role| match role {
                        ProviderRole::ToolProvider { tools, .. } => Some(tools),
                        _ => None,
                    })
                    .flatten()
                    .map(|tool| tool.name.as_str())
                    .collect();
                assert!(
                    tools.contains(&"read") || tools.contains(&"aft_read"),
                    "missing tool-provider catalog"
                );
                assert!(
                    !tools.contains(&"search") && !tools.contains(&"aft_search"),
                    "disabled search appeared after readiness"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("AFT did not become ready: {}", daemon.log()));
    let modules = client.config_modules().await.unwrap();
    assert!(!modules.stamped);
    assert!(modules
        .modules
        .iter()
        .any(|module| module.module_id == "aft" && module.schema_digest.is_some()));
    let health = client.supervisor_health_probe("aft").await.unwrap();
    assert_eq!(health.metrics.unwrap()["memory_profile"]["value"], "lean");

    fs::write(
        daemon.user_file(),
        r#"{"memory":"balanced","disabled_tools":["aft_search"]}"#,
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let health = client.supervisor_health_probe("aft").await.unwrap();
            if health
                .metrics
                .is_some_and(|value| value["memory_profile"]["value"] == "balanced")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "AFT did not apply the daemon's change hint: {}",
            daemon.log()
        )
    });
}

async fn check_bootstrap_error(client: &SubcConsumer) {
    let Ok(report) = client.supervisor_health_probe("aft").await else {
        return;
    };
    if let Some(reason) = report
        .metrics
        .and_then(|metrics| metrics["settings"]["reason"].as_str().map(str::to_owned))
    {
        if reason != "waiting_for_settings" && reason != "settings_catalog_starting" {
            let tail = client
                .supervisor_stderr_tail("aft", Some(30), Some(8192))
                .await;
            panic!("gated bootstrap failed ({reason}); module stderr: {tail:?}");
        }
    }
}

async fn desktop(daemon: &Daemon) -> tokio::net::UnixStream {
    let connection = subc_transport::connection_file::read_with_local_endpoint(
        daemon.home.path().join("runtime/subc-connection.json"),
    )
    .unwrap();
    let Some(subc_transport::LocalEndpoint::Unix { path }) = connection.local_endpoint else {
        panic!("isolated desktop fixture requires the daemon's local endpoint");
    };
    let mut stream = tokio::net::UnixStream::connect(path).await.unwrap();
    subc_transport::authenticate_client_with_role(
        &mut stream,
        &connection.info,
        Duration::from_secs(5),
        "desktop-app",
    )
    .await
    .unwrap();
    stream
}

async fn rpc(
    stream: &mut tokio::net::UnixStream,
    body: Value,
) -> (subc_protocol::FrameType, Value) {
    use subc_protocol::{Flags, Frame, FrameType, Priority};
    let frame = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        0,
        0,
        2,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    subc_transport::write_frame(stream, &frame).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(20), subc_transport::read_frame(stream))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(reply.header.corr, 2);
    (
        reply.header.ty,
        serde_json::from_slice(&reply.body).unwrap(),
    )
}

async fn approve(stream: &mut tokio::net::UnixStream, op: &str) -> u64 {
    let (_, plan) = rpc(stream, json!({"op":op})).await;
    assert!(plan["view"]["digest"].is_string(), "{op}: {plan}");
    let (_, applied) = rpc(
        stream,
        json!({"op":"config.apply","digest":plan["view"]["digest"]}),
    )
    .await;
    applied["generation"]
        .as_u64()
        .unwrap_or_else(|| panic!("{op} approval: {applied}"))
}

async fn eventually<T, Fut>(daemon: &Daemon, label: &str, mut probe: impl FnMut() -> Fut) -> T
where
    Fut: std::future::Future<Output = Option<T>>,
{
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(result) = probe().await {
                return result;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{label}: {}", daemon.log()))
}

#[tokio::test]
#[ignore = "requires CK_SUBC_DEV_BIN with scripted Linux approval"]
async fn subc_settings_real_dev_daemon_reviews_applies_and_serves_declaration_pending() {
    let (daemon, consumer) = Daemon::launch(true, |fixture| {
        fs::write(
            fixture.join("config/cortexkit/aft.jsonc"),
            r#"{"memory":"lean"}"#,
        )
        .unwrap();
    })
    .await;
    eventually(&daemon, "initial AFT readiness", || async {
        check_bootstrap_error(&consumer).await;
        consumer
            .catalog_list()
            .await
            .ok()?
            .modules
            .iter()
            .any(|row| row.module_id == "aft" && row.ready)
            .then_some(())
    })
    .await;
    let mut operator = desktop(&daemon).await;
    let generation = approve(&mut operator, "config.stamp").await;
    assert_eq!(generation, 1);
    eventually(&daemon, "stamped generation activation", || async {
        consumer
            .config_modules()
            .await
            .ok()?
            .modules
            .iter()
            .any(|row| row.module_id == "aft" && row.active_generation == Some(generation))
            .then_some(())
    })
    .await;

    // The daemon's schema accepts this integer. Only AFT enforces its upper bound.
    fs::write(
        daemon.user_file(),
        r#"{"memory":"lean","formatter_timeout_secs":601}"#,
    )
    .unwrap();
    let (kind, refused) = rpc(&mut operator, json!({"op":"config.plan"})).await;
    assert_eq!(kind, subc_protocol::FrameType::Error, "{refused}");
    assert_eq!(refused["code"], "module_review_refused", "{refused}");
    assert!(
        refused.to_string().contains("/formatter_timeout_secs"),
        "{refused}"
    );
    assert!(!refused.to_string().contains("601"), "{refused}");

    fs::write(daemon.user_file(), r#"{"memory":"balanced"}"#).unwrap();
    let generation = approve(&mut operator, "config.plan").await;
    eventually(&daemon, "approved live generation activation", || async {
        consumer
            .config_modules()
            .await
            .ok()?
            .modules
            .iter()
            .any(|row| row.module_id == "aft" && row.active_generation == Some(generation))
            .then_some(())
    })
    .await;

    let path = daemon.home.path().join("aft-schema.json");
    let mut changed: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    changed["properties"]["format_on_edit"] = json!({"type":"boolean","description":"Format after edit","default":true,"x-ck-apply":"live"});
    fs::write(path, changed.to_string()).unwrap();
    fs::write(daemon.user_file(), r#"{"memory":"lean"}"#).unwrap();
    // The schema and settings file changed, but the approved launch command did not.
    let (kind, reply) = rpc(
        &mut operator,
        json!({"op":"supervisor.restart","module_id":"aft"}),
    )
    .await;
    assert_ne!(kind, subc_protocol::FrameType::Error, "{reply}");
    eventually(
        &daemon,
        "new declaration uses last approved settings",
        || async {
            let health = consumer.supervisor_health_probe("aft").await.ok()?;
            let metrics = health.metrics?;
            (metrics["settings"]["declaration_pending"] == true
                && metrics["memory_profile"]["value"] == "balanced")
                .then_some(())
        },
    )
    .await;
    let settings = consumer.config_modules().await.unwrap();
    assert!(settings.modules.iter().any(|row| row.module_id == "aft"
        && row.active_generation == Some(generation)
        && row.declaration_pending.is_some()));
}

#[tokio::test]
#[ignore = "requires CK_SUBC_DEV_BIN with scripted Linux approval"]
async fn subc_settings_real_dev_daemon_no_approved_config_keeps_aft_not_ready() {
    let (daemon, consumer) = Daemon::launch(true, |fixture| {
        // Stamp before the supervised process declares its settings. The
        // launch recipe is approved, but no AFT settings generation exists.
        let path = fixture.join("config/cortexkit/subc.jsonc");
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        config["modules"]["aft"]["env"]["AFT_TEST_SETTINGS_HELLO_MARKER"] =
            json!(fixture.join("stamped"));
        fs::write(path, config.to_string()).unwrap();
        fs::write(
            fixture.join("config/cortexkit/aft.jsonc"),
            r#"{"memory":"lean"}"#,
        )
        .unwrap();
    })
    .await;
    let mut operator = desktop(&daemon).await;
    assert_eq!(approve(&mut operator, "config.stamp").await, 1);
    fs::write(daemon.home.path().join("stamped"), b"").unwrap();
    eventually(
        &daemon,
        "AFT stayed registered but not ready without an approved view",
        || async {
            let health = consumer.supervisor_health_probe("aft").await.ok()?;
            let metrics = health.metrics?;
            (metrics["settings"]["reason"] == "no_approved_config").then_some(())
        },
    )
    .await;
    let catalog = consumer.catalog_list().await.unwrap();
    assert!(catalog
        .modules
        .iter()
        .any(|row| row.module_id == "aft" && !row.ready));
}
