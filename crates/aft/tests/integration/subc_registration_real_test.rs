//! Run on Linux with CK_SUBC_BIN built at Subconscious's pinned settings commit,
//! bbce149305db52c022c6e1f62a2ed9ae51d16e99. Never use the operator's daemon.

use serde_json::{json, Value};
use std::fs;
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use subc_client_rs::{
    BindDecision, ConsumerOptions, HandlerOutcome, ModuleHandler, RequestCtx, RouteBindRequest,
    SubcConsumer,
};
use subc_protocol::manifest::{
    Concurrency, ManagementOperation, ManagementOperationKind, ModuleManifest, ProviderRole,
};
use subc_protocol::Principal;

const PEER: &str = "AFT_REGISTRATION_TEST_PEER";
const PEER_TEST: &str = "subc_registration_real_test::registration_real_daemon_peer";

struct Peer {
    witness: PathBuf,
    method: &'static str,
}

#[subc_client_rs::async_trait]
impl ModuleHandler for Peer {
    async fn on_bind(&self, request: &RouteBindRequest) -> BindDecision {
        assert_eq!(
            request.principal,
            Some(Principal::Reserved {
                module_id: "aft".into()
            })
        );
        BindDecision::accept()
    }

    async fn handle(&self, _: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        let request: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(request["method"], self.method);
        fs::write(&self.witness, self.method).unwrap();
        // The witness records a request after the daemon identified the caller
        // as AFT. The fake never issues credentials or contacts another service.
        HandlerOutcome::Response(br#"{"result":null}"#.to_vec())
    }
}

#[tokio::test]
#[ignore = "supervised helper process for registration_real_daemon_binds_vault_and_bus"]
async fn registration_real_daemon_peer() {
    let module = std::env::var(PEER).expect("spawn only through the registration fixture");
    let method = match module.as_str() {
        "claustrum" => "manifest.latest",
        "ckbus" => "ckbus.credential",
        _ => panic!("unexpected peer {module}"),
    };
    let manifest = ModuleManifest::builder(&module, "0.0.0")
        .provides(vec![ProviderRole::ManagementSurface {
            operations: vec![ManagementOperation {
                name: method.into(),
                kind: ManagementOperationKind::Query,
                description: None,
            }],
            config_schema: json!({}),
            observability: vec![],
            identity_scope: vec![],
            concurrency: Concurrency::ModuleManaged,
        }])
        .build();
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let witness = home.join(format!("{module}.attested"));
    let (_, serve) = subc_client_rs::serve_from_env_with_handle(manifest, Peer { witness, method })
        .await
        .unwrap();
    serve.await.unwrap();
}

struct Daemon {
    child: Child,
    fixture: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // The fixture's daemon starts in its own process group.
        unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn diagnostics(&self) -> String {
        fs::read_to_string(self.fixture.path().join("daemon.stderr")).unwrap_or_default()
    }
}

fn isolate_directories(root: &Path) -> Vec<(&'static str, PathBuf)> {
    let dirs = [
        ("HOME", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_RUNTIME_DIR", "runtime"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
        ("TMPDIR", "tmp"),
    ];
    dirs.into_iter()
        .map(|(key, subdir)| {
            let dir = root.join(subdir);
            fs::create_dir_all(&dir).unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
            (key, dir)
        })
        .collect()
}

fn launch() -> Daemon {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let dirs = isolate_directories(root);
    let mut env: Value = serde_json::to_value(
        dirs.iter()
            .cloned()
            .collect::<std::collections::BTreeMap<_, _>>(),
    )
    .unwrap();
    env["PATH"] = json!("/usr/bin:/bin");
    env["RUST_LOG"] = json!("info");
    env["AFT_STORAGE_DIR"] = json!(root.join("data/aft"));
    let module = crate::test_helpers::aft_binary::aft_binary();
    // The supervisor appends the connection-file arguments. Use a launcher
    // so the libtest child receives only its own harness arguments.
    let launcher = root.join("peer");
    fs::write(&launcher, format!("#!/bin/sh\nexec \"$AFT_REGISTRATION_TEST_EXE\" --ignored --exact {PEER_TEST} --nocapture\n")).unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).unwrap();
    let peer = |id: &str| {
        let mut env = env.clone();
        env[PEER] = json!(id);
        env["AFT_REGISTRATION_TEST_EXE"] = json!(std::env::current_exe().unwrap());
        json!({"program": launcher, "args": [], "enabled": true, "env": env})
    };
    let config = root.join("config/cortexkit");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("subc.jsonc"), json!({
        "version":1, "port":0, "storage":{"backend":"sqlite","data_home":root.join("data")},
        "modules": {
            "aft":{"program":module,"args":[],"enabled":true,"registration_channel":true,"env":env},
            "claustrum": peer("claustrum"), "ckbus": peer("ckbus"),
        }
    }).to_string()).unwrap();

    let binary =
        std::env::var_os("CK_SUBC_BIN").expect("set CK_SUBC_BIN to the pinned Linux daemon");
    let child = Command::new(binary)
        .env_clear()
        .envs(dirs)
        .env("PATH", "/usr/bin:/bin")
        .env("SUBC_CGROUP_PLACEMENT", "disabled")
        .env("SUBC_PORT", "0")
        .arg("--config-store-root")
        .arg(root.join("authority"))
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(root.join("daemon.stderr")).unwrap())
        .spawn()
        .unwrap();
    Daemon { child, fixture }
}

#[tokio::test]
#[ignore = "requires CK_SUBC_BIN built from the pinned Linux settings daemon commit"]
async fn registration_real_daemon_binds_vault_and_bus() {
    let mut daemon = launch();
    let root = daemon.fixture.path().to_path_buf();
    let path = root.join("runtime/subc-connection.json");
    tokio::time::timeout(Duration::from_secs(30), async {
        while !path.is_file() {
            assert!(
                daemon.child.try_wait().unwrap().is_none(),
                "{}",
                daemon.diagnostics()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "daemon never wrote connection file: {}",
            daemon.diagnostics()
        )
    });
    let client = SubcConsumer::connect_local(&path, ConsumerOptions::default())
        .await
        .unwrap();
    if tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            let catalog = client.catalog_list().await.unwrap();
            let ready = catalog
                .modules
                .iter()
                .any(|module| module.module_id == "aft" && module.ready);
            if ready
                && ["claustrum", "ckbus"]
                    .into_iter()
                    .all(|id| root.join(format!("home/{id}.attested")).is_file())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .is_err()
    {
        let mut tails = Vec::new();
        for id in ["aft", "claustrum", "ckbus"] {
            tails.push((
                id,
                client
                    .supervisor_stderr_tail(id, Some(35), Some(8192))
                    .await,
            ));
        }
        panic!(
            "AFT never became ready with both attested consumers: {}\n{tails:?}",
            daemon.diagnostics()
        );
    }
}
