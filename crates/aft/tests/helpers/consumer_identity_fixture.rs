//! Real-daemon peers for production vault transports using implicit launch identity.

use aft::secret::SecretUseTransport;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use subc_client_rs::{
    async_trait, BindDecision, HandlerOutcome, ModuleHandler, RequestCtx, RouteBindRequest,
    RouteHandle,
};
use subc_protocol::manifest::{
    Concurrency, ManagementOperation, ManagementOperationKind, ModuleManifest, ProviderRole,
};
use subc_protocol::scope::{ScopeCarrier, ScopeKind, ScopeRecord, ScopeRecordOutcome};
use subc_protocol::{BindIdentity, Flags, Frame, FrameType, ModuleHelloBody, Principal, Priority};
use subc_transport::{authenticate_client, connection_file, read_frame, write_frame};

struct Vault {
    bindings: Mutex<HashMap<RouteHandle, RouteBindRequest>>,
}

#[async_trait]
impl ModuleHandler for Vault {
    async fn on_bind(&self, request: &RouteBindRequest) -> BindDecision {
        assert_eq!(
            request.principal,
            Some(Principal::Reserved {
                module_id: "aft".into()
            })
        );
        self.bindings
            .lock()
            .unwrap()
            .insert(request.handle, request.clone());
        BindDecision::accept()
    }

    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        let request: Value = serde_json::from_slice(&body).unwrap();
        let bindings = self.bindings.lock().unwrap();
        let bind = &bindings[&ctx.route_handle()];
        HandlerOutcome::Response(
            serde_json::to_vec(&json!({"result":{
                "method":request["method"],
                "principal":bind.principal,
                "scope":bind.scope,
                "identity":bind.identity,
            }}))
            .unwrap(),
        )
    }
}

pub(super) async fn run() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let fixture = home.parent().unwrap();
    let connection = PathBuf::from(std::env::var_os("AFT_TEST_CONNECTION").unwrap());
    match std::env::var("SUBC_MODULE_ID").unwrap().as_str() {
        "aft" => run_aft(fixture, &connection).await,
        "claustrum" => {
            let manifest = ModuleManifest::builder("claustrum", "0.0.0")
                .provides(vec![ProviderRole::ManagementSurface {
                    operations: ["secret.use", "manifest.latest"]
                        .into_iter()
                        .map(|name| ManagementOperation {
                            name: name.into(),
                            kind: ManagementOperationKind::Query,
                            description: None,
                        })
                        .collect(),
                    config_schema: json!({}),
                    observability: vec![],
                    identity_scope: vec![],
                    concurrency: Concurrency::ModuleManaged,
                }])
                .build();
            let (_, serve) = subc_client_rs::serve_from_env_with_handle(
                manifest,
                Vault {
                    bindings: Mutex::default(),
                },
            )
            .await
            .unwrap();
            std::fs::write(fixture.join("identity-vault.ready"), b"ready").unwrap();
            serve.await.unwrap();
        }
        "aft-test-scope-owner" => {
            let (handle, serve) = subc_client_rs::serve_from_env_with_handle(
                ModuleManifest::builder("aft-test-scope-owner", "0.0.0").build(),
                super::ScopeOwner,
            )
            .await
            .unwrap();
            let serving = tokio::spawn(serve);
            let scope = ScopeRecord::new("identity-vault", 7, ScopeKind::Head)
                .with_workdir(Some(fixture.join("project").to_string_lossy().into_owned()))
                .with_carriers(vec![ScopeCarrier::new(Principal::Reserved {
                    module_id: "aft".into(),
                })
                .with_targets(Some(vec!["claustrum".into()]))]);
            let reply = handle.scope_sync(1, vec![scope]).await.unwrap();
            assert_ne!(
                reply.results[0].outcome,
                ScopeRecordOutcome::Refused,
                "{reply:?}"
            );
            std::fs::write(fixture.join("identity-owner.ready"), b"ready").unwrap();
            serving.await.unwrap().unwrap();
        }
        module => panic!("unexpected identity fixture module {module}"),
    }
}

async fn run_aft(fixture: &Path, connection: &Path) {
    // Match AFT's custom loop: register over TCP with the captured nonce, not
    // SDK serve_with_handle, which would adopt the inherited registration channel.
    let conn = connection_file::read_for_client(connection).unwrap();
    let endpoint = &conn.endpoints[0];
    let mut stream = tokio::net::TcpStream::connect(format!("{}:{}", endpoint.host, endpoint.port))
        .await
        .unwrap();
    authenticate_client(&mut stream, &conn, Duration::from_secs(5))
        .await
        .unwrap();
    let hello = ModuleHelloBody {
        manifest: ModuleManifest::builder("aft", "0.0.0").build(),
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: Some(aft::launch_nonce::current().unwrap().value().to_string()),
    };
    let flags = Flags::new(false, Priority::Interactive, false);
    write_frame(
        &mut stream,
        &Frame::build(
            FrameType::Hello,
            flags,
            0,
            0,
            1,
            serde_json::to_vec(&hello).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let ack = read_frame(&mut stream).await.unwrap().unwrap();
    assert_eq!(ack.header.ty, FrameType::HelloAck, "{ack:?}");
    let serving = tokio::spawn(async move {
        while let Some(frame) = read_frame(&mut stream).await.unwrap() {
            if frame.header.ty == FrameType::Ping {
                let pong = Frame::build(
                    FrameType::Pong,
                    frame.header.flags,
                    0,
                    0,
                    frame.header.corr,
                    vec![],
                )
                .unwrap();
                write_frame(&mut stream, &pong).await.unwrap();
            }
        }
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !fixture.join("identity-vault.ready").exists()
        || !fixture.join("identity-owner.ready").exists()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "identity peers did not start"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stamp = subc_protocol::scope::ScopeStamp::new(
        Principal::Reserved {
            module_id: "aft-test-scope-owner".into(),
        },
        "identity-vault",
        7,
        ScopeKind::Head,
    );
    let transport = aft::secret::transport::SubcSecretUseTransport::new(
        connection.to_path_buf(),
        BindIdentity::new("aft-secret-use", "identity-proof"),
        Duration::from_secs(5),
    )
    .expect("supervised AFT identity");
    let secret = tokio::task::spawn_blocking(move || transport.secret_use(json!({
        "stamp":stamp, "name":"test-key", "target":{"scheme":"https","host":"example.test","path":"/proof"},
    }))).await.unwrap().unwrap();
    let manifest = aft::secret::transport::manifest_latest_for_test(connection)
        .await
        .unwrap();
    std::fs::write(
        fixture.join("identity-outcomes.json"),
        serde_json::to_vec(&json!({
            "secret":serde_json::from_slice::<Value>(&secret).unwrap(),
            "manifest":serde_json::from_slice::<Value>(&manifest).unwrap(),
        }))
        .unwrap(),
    )
    .unwrap();
    serving.await.unwrap();
}
