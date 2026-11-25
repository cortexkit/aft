//! Exercises the daemon's project-settings reader and review. A supervised
//! registry peer reports temporary project and workspace directories.
use super::*;
use std::os::unix::fs::MetadataExt;
use subc_client_rs::CallOptions;
use subc_protocol::{
    manifest::ModuleManifest,
    scope::{ScopeCarrier, ScopeKind, ScopeRecord, ScopeSelector},
    BindIdentity, Flags, Frame, FrameType, ModuleHelloBody, Principal, Priority, RouteTarget,
    PROTOCOL_VERSION,
};
use tokio::net::UnixStream;

const ID: &str = "settings-fixture-project";
const SCOPE: &str = "settings-fixture-scope";
const REGISTRY_TEST: &str = "subc_settings_real_test::project::settings_registry_peer";

fn file_id(path: &Path) -> String {
    let metadata = fs::metadata(path).unwrap();
    format!("unix:{}:{}", metadata.dev(), metadata.ino())
}

async fn send(stream: &mut UnixStream, kind: FrameType, corr: u64, body: Value) {
    let frame = Frame::build(
        kind,
        Flags::new(false, Priority::Interactive, false),
        0,
        0,
        corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    subc_transport::write_frame(stream, &frame).await.unwrap();
}

#[tokio::test]
#[ignore = "supervised project registry peer launched by the real-daemon test"]
async fn settings_registry_peer() {
    let fixture = PathBuf::from(std::env::var_os("AFT_SETTINGS_PROJECT_FIXTURE").unwrap());
    let project = fixture.join("project");
    let workspace = fixture.join("workspace");
    let socket = subc_os::registration_channel::take_registration_channel()
        .unwrap()
        .unwrap()
        .into_unix_stream();
    socket.set_nonblocking(true).unwrap();
    let mut stream = UnixStream::from_std(socket).unwrap();
    let mut manifest = ModuleManifest::builder("entorhinal", "1.0.0").build();
    manifest.ready = Some(true);
    let hello = ModuleHelloBody {
        manifest,
        protocol_ver: PROTOCOL_VERSION,
        control_ops: Some(vec!["projects.enumerate".to_string()]),
        launch_nonce: None,
    };
    send(&mut stream, FrameType::Hello, 1, json!(hello)).await;
    let hello = subc_transport::read_frame(&mut stream)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        hello.header.ty,
        FrameType::HelloAck,
        "{:?}",
        String::from_utf8_lossy(&hello.body)
    );
    let scope = ScopeRecord::new(SCOPE, 1, ScopeKind::Head)
        .with_workdir(Some(project.to_string_lossy().into_owned()))
        .with_project_id(Some(ID.into()))
        .with_carriers(vec![ScopeCarrier::new(Principal::Direct)]);
    let (_, reply) = rpc(
        &mut stream,
        json!({"op":"scope.sync","generation":1,"scopes":[scope]}),
    )
    .await;
    assert_eq!(reply["op"], "scope.sync", "{reply}");
    assert_ne!(reply["results"][0]["outcome"], "refused", "{reply}");
    fs::write(fixture.join("registry.ready"), "ready").unwrap();
    while let Some(frame) = subc_transport::read_frame(&mut stream).await.unwrap() {
        if frame.header.ty == FrameType::Goodbye {
            break;
        }
        if frame.header.ty == FrameType::Ping {
            let reply = Frame::build(
                FrameType::Pong,
                frame.header.flags,
                0,
                0,
                frame.header.corr,
                vec![],
            )
            .unwrap();
            subc_transport::write_frame(&mut stream, &reply)
                .await
                .unwrap();
            continue;
        }
        let input: Value = serde_json::from_slice(&frame.body).unwrap();
        if input["op"] == "health.check" {
            send(
                &mut stream,
                FrameType::Response,
                frame.header.corr,
                json!(subc_protocol::session::ModuleControlResponse::from(
                    subc_protocol::session::HealthReport::ok()
                )),
            )
            .await;
            continue;
        }
        assert_eq!(input["op"], "projects.enumerate", "{input}");
        let root = json!({"root":project, "remotes":[], "incarnation":{"kind":"token","value":"fixture-checkout"}, "registrationEpoch":null, "identity":"bound", "approval":{"state":"approved","journalSeq":1}, "fileId":file_id(&project)});
        send(&mut stream, FrameType::Response, frame.header.corr, json!({
            "op":"projects.enumerate", "result": {
                "projects":[{"projectId":ID,"name":"Fixture project","roots":[project],"implicit":false,"refKind":"local","workspaceId":"settings-fixture-workspace","rootRecords":[root]}],
                "workspaces":[{"workspaceId":"settings-fixture-workspace","name":"Fixture workspace","root":workspace,"fileId":file_id(&workspace)}],
                "generation":1, "incarnation":"settings-fixture-registry"
            }
        })).await;
    }
}

fn configure_fixture(fixture: &Path) {
    for root in ["project", "workspace"] {
        fs::create_dir_all(fixture.join(root).join(".cortexkit")).unwrap();
    }
    let registry_bin = fixture.join("registry-fixture");
    let strip = Command::new("objcopy")
        .arg("--strip-debug")
        .arg(std::env::current_exe().unwrap())
        .arg(&registry_bin)
        .output()
        .unwrap();
    assert!(
        strip.status.success(),
        "{}",
        String::from_utf8_lossy(&strip.stderr)
    );
    let daemon_config = fixture.join("config/cortexkit/subc.jsonc");
    let mut config: Value = serde_json::from_slice(&fs::read(&daemon_config).unwrap()).unwrap();
    config["modules"]["entorhinal"] = json!({
        "program":registry_bin,"args":["--ignored","--exact",REGISTRY_TEST,"--nocapture"],"enabled":true,
        "registration_channel":true,"env":{"AFT_SETTINGS_PROJECT_FIXTURE":fixture}
    });
    fs::write(daemon_config, config.to_string()).unwrap();
    fs::write(fixture.join("config/cortexkit/aft.jsonc"), r#"{"bash":{"env":{"SETTINGS_USER":"from_user"}},"indexes":{"trigram":false,"semantic":false,"callgraph":false}}"#).unwrap();
    fs::write(
        fixture.join("workspace/.cortexkit/aft.jsonc"),
        r#"{"bash":{"env":{"SETTINGS_WORKSPACE":"from_workspace"}}}"#,
    )
    .unwrap();
}

#[tokio::test]
#[ignore = "requires CK_SUBC_DEV_BIN with project settings and scripted Linux approval"]
async fn subc_settings_real_daemon_reviews_and_delivers_project_settings() {
    let (daemon, consumer) = Daemon::launch(true, configure_fixture).await;
    if tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if daemon.home.path().join("registry.ready").exists()
                && consumer
                    .catalog_list()
                    .await
                    .unwrap()
                    .modules
                    .iter()
                    .any(|row| row.module_id == "aft" && row.ready)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .is_err()
    {
        let registry = consumer
            .supervisor_stderr_tail("entorhinal", Some(22), Some(4096))
            .await;
        let aft = consumer
            .supervisor_stderr_tail("aft", Some(22), Some(4096))
            .await;
        panic!(
            "registry and settings not ready: registry={registry:?} aft={aft:?}; {}",
            daemon.log()
        );
    }
    let mut operator = desktop(&daemon).await;
    approve(&mut operator, "config.stamp").await;
    let route = consumer
        .open_route_scoped(
            RouteTarget::ToolProvider {
                module_id: "aft".into(),
            },
            BindIdentity::new("runner", "settings-project-test"),
            ScopeSelector::new(
                Principal::Reserved {
                    module_id: "entorhinal".into(),
                },
                SCOPE,
                Some(1),
            ),
            CallOptions::default(),
        )
        .await
        .unwrap();
    let project_file = daemon.home.path().join("project/.cortexkit/aft.jsonc");
    for (candidate, path) in [
        (
            json!({"search":{"rerank":{"backend":"onnx"}}}),
            "/search/rerank/backend",
        ),
        (
            json!({"bash":{"changed_files":"diff"}}),
            "/bash/changed_files",
        ),
        (json!({"bash":{"env":{"PATH":"refused"}}}), "/bash/env/PATH"),
    ] {
        fs::write(&project_file, candidate.to_string()).unwrap();
        let (_, reply) = rpc(&mut operator, json!({"op":"config.plan"})).await;
        let reply = reply.to_string();
        assert!(
            reply.contains("module_review_refused") && reply.contains(path),
            "{reply}"
        );
    }
    fs::write(
        &project_file,
        r#"{"bash":{"env":{"SETTINGS_PROJECT":"from_project"}}}"#,
    )
    .unwrap();
    approve(&mut operator, "config.plan").await;
    eventually(&daemon, "AFT applies the merged project environment", || async {
        let body = json!({"name":"bash","arguments":{"command":"printf '%s:%s:%s' \"$SETTINGS_USER\" \"$SETTINGS_WORKSPACE\" \"$SETTINGS_PROJECT\""}});
        let output = consumer.request(&route, serde_json::to_vec(&body).unwrap(), CallOptions::default()).await.ok()?;
        String::from_utf8_lossy(&output).contains("from_user:from_workspace:from_project").then_some(())
    }).await;
}
