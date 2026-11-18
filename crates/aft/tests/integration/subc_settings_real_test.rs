//! Linux tests against the config.gate daemon. CK_SUBC_BIN is an explicit
//! opt-in so no test can attach to the operator's default daemon.

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
    async fn launch(setup: impl FnOnce(&Path)) -> (Self, SubcConsumer) {
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
        let module_env = json!({"PATH":"/usr/bin:/bin", "HOME":fixture.join("home"),
            "XDG_CONFIG_HOME":fixture.join("config"), "XDG_RUNTIME_DIR":fixture.join("runtime"),
            "XDG_DATA_HOME":fixture.join("data"), "XDG_STATE_HOME":fixture.join("state"),
            "XDG_CACHE_HOME":fixture.join("cache"), "TMPDIR":fixture.join("tmp"),
            "AFT_STORAGE_DIR":fixture.join("data/aft"), "AFT_TEST_SETTINGS_SCHEMA":schema_path});
        fs::write(fixture.join("config/cortexkit/subc.jsonc"), json!({
            "version":1,"port":0,"storage":{"backend":"sqlite","data_home":fixture.join("data")},
            "modules":{"aft":{"program":crate::test_helpers::aft_binary::aft_binary(),"args":[],"enabled":true,"env":module_env}}
        }).to_string()).unwrap();
        setup(fixture);
        let binary = PathBuf::from(
            std::env::var_os("CK_SUBC_BIN").expect("set CK_SUBC_BIN to the pinned Linux daemon"),
        );
        let binary = crate::test_helpers::executable_cache::cached_executable_named(
            &fs::read(binary).unwrap(),
            "ckdev-subc",
        )
        .unwrap();
        let child = Command::new(binary)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", fixture.join("home"))
            .env("XDG_CONFIG_HOME", fixture.join("config"))
            .env("XDG_RUNTIME_DIR", fixture.join("runtime"))
            .env("XDG_DATA_HOME", fixture.join("data"))
            .env("XDG_STATE_HOME", fixture.join("state"))
            .env("XDG_CACHE_HOME", fixture.join("cache"))
            .env("TMPDIR", fixture.join("tmp"))
            .env("SUBC_PORT", "0")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                fs::File::create(fixture.join("daemon.stderr")).unwrap(),
            ))
            .spawn()
            .unwrap();
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
        let consumer = SubcConsumer::connect(&connection, ConsumerOptions::default())
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
    let (daemon, client) = Daemon::launch(|fixture| {
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
