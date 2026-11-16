//! Opt-in SDK compatibility against a staged release daemon, never a live install.
#![cfg(target_os = "linux")]

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::json;
use subc_client_rs::{CallError, CallOptions, CloseRouteOptions, ConsumerOptions, SubcConsumer};
use subc_protocol::{BindIdentity, RouteTarget};

#[path = "helpers/aft_binary.rs"]
mod aft_binary;

struct ReleaseDaemon(Child);

impl Drop for ReleaseDaemon {
    fn drop(&mut self) {
        // The isolated group also contains modules supervised by this fixture.
        unsafe { libc::killpg(self.0.id() as libc::pid_t, libc::SIGKILL) };
        let _ = self.0.wait();
    }
}

#[tokio::test]
#[ignore = "requires AFT_TEST_SUBC_CORE_BINARY pointing to a staged Linux release"]
async fn release_daemon_0_21_3_rejects_catalog_watch_but_opens_management_route() {
    let binary = PathBuf::from(
        std::env::var_os("AFT_TEST_SUBC_CORE_BINARY").expect("stage a release daemon explicitly"),
    );
    let binary = crate::test_helpers::executable_cache::cached_executable_named(
        &std::fs::read(binary).expect("read staged release"),
        "ckdev-subc",
    )
    .expect("cache isolated development-named daemon");
    let temp = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(temp.path()).unwrap();
    for directory in [
        "runtime",
        "config/cortexkit",
        "data",
        "state",
        "cache",
        "tmp",
        "home",
    ] {
        std::fs::create_dir_all(home.join(directory)).unwrap();
    }
    let module_env = json!({
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "HOME": home.join("home"),
        "XDG_RUNTIME_DIR": home.join("runtime"),
        "XDG_CONFIG_HOME": home.join("config"),
        "XDG_DATA_HOME": home.join("data"),
        "XDG_STATE_HOME": home.join("state"),
        "XDG_CACHE_HOME": home.join("cache"),
        "TMPDIR": home.join("tmp"),
        "AFT_STORAGE_DIR": home.join("data/aft"),
    });
    std::fs::write(
        home.join("config/cortexkit/subc.jsonc"),
        serde_json::to_vec(&json!({
            "version": 1, "port": 0,
            "storage": {"backend": "sqlite", "data_home": home.join("data")},
            "modules": {"aft": {"program": aft_binary::aft_binary(), "enabled": true,
                "args": [], "env": module_env}},
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        home.join("config/cortexkit/aft.jsonc"),
        serde_json::to_vec(&json!({
            "indexes": {"trigram": false, "semantic": false, "callgraph": false},
        }))
        .unwrap(),
    )
    .unwrap();
    let stderr = std::fs::File::create(home.join("daemon.stderr")).unwrap();
    let mut daemon = ReleaseDaemon(
        Command::new(binary)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", home.join("home"))
            .env("XDG_RUNTIME_DIR", home.join("runtime"))
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("XDG_STATE_HOME", home.join("state"))
            .env("XDG_CACHE_HOME", home.join("cache"))
            .env("TMPDIR", home.join("tmp"))
            .env("SUBC_PORT", "0")
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .expect("start isolated release daemon"),
    );
    let connection_file = home.join("runtime/subc-connection.json");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !connection_file.exists() {
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon exited: {}",
            std::fs::read_to_string(home.join("daemon.stderr")).unwrap()
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "release daemon startup timeout"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let consumer = SubcConsumer::connect(&connection_file, ConsumerOptions::default())
        .await
        .expect("connect SDK to release daemon");
    let description = consumer
        .server_describe()
        .await
        .expect("describe release daemon");
    // Release 0.21.3 describes protocol 2, build b86f6d1, and catalog-events/v1,
    // but not catalog.watch or route-close-reasons/v1. The SDK must keep its
    // old-daemon path; only the newer live daemon supplies the generation feed.
    let close_reasons_supported = description
        .capabilities
        .iter()
        .any(|capability| capability == "route-close-reasons/v1");
    eprintln!("release server.describe: {description:?}; SDK close reasons supported: {close_reasons_supported}");
    assert_eq!(description.protocol_ver, 2);
    assert_eq!(
        description.build_git_sha.as_deref(),
        Some("b86f6d159cd7286e5edd3baa9b9671cdd9d73f7a")
    );
    assert!(description
        .capabilities
        .iter()
        .any(|capability| capability == "catalog-events/v1"));
    assert!(!close_reasons_supported);
    let mut watch = consumer.catalog_watch().await.expect("open catalog.watch");
    let refused = tokio::time::timeout(Duration::from_secs(5), watch.next())
        .await
        .expect("initial generation deadline")
        .expect_err("v0.21.3 does not implement catalog.watch");
    let CallError::Module(refused) = refused else {
        panic!("expected a definite old-daemon refusal, got {refused:?}");
    };
    assert_eq!(refused.code, "unknown_control_op");
    assert!(refused.message.contains("unknown variant `catalog.watch`"));
    loop {
        let catalog = consumer.catalog_list().await.expect("read release catalog");
        if catalog.modules.iter().any(|entry| entry.module_id == "aft") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "AFT registration timeout"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let route = consumer
        .open_route(
            RouteTarget::ManagementSurface {
                module_id: "aft".into(),
            },
            BindIdentity::new("compatibility", "release-smoke"),
            CallOptions::default(),
        )
        .await
        .expect("open AFT management surface through release daemon");
    consumer
        .close_handle(&route, CloseRouteOptions::default())
        .await
        .expect("close management route");
}
