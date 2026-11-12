//! Supervised peers for the real-daemon runner-notice transport test.

use serde_json::json;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use subc_client_rs::{
    async_trait, BindDecision, HandlerOutcome, ModuleHandler, RequestCtx, RouteBindRequest,
    RouteHandle,
};
use subc_protocol::manifest::{
    Concurrency, ManagementOperation, ManagementOperationKind, ModuleManifest, ProviderRole,
};
use subc_protocol::scope::{
    ScopeCarrier, ScopeKind, ScopeRecord, ScopeRecordOutcome, ScopeSelector,
};
use subc_protocol::Principal;

struct Logger(Mutex<std::fs::File>);
impl log::Log for Logger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &log::Record<'_>) {
        writeln!(
            self.0.lock().unwrap(),
            "{} {}",
            record.level(),
            record.args()
        )
        .unwrap();
    }
    fn flush(&self) {}
}

struct Peer {
    fixture: PathBuf,
    records: bool,
    module: String,
    bindings: Mutex<HashMap<RouteHandle, RouteBindRequest>>,
    recoveries: Arc<Mutex<HashMap<String, Arc<aft::subc::RunnerNoticeRecoveryForTest>>>>,
}

#[async_trait]
impl ModuleHandler for Peer {
    async fn on_bind(&self, request: &RouteBindRequest) -> BindDecision {
        self.bindings
            .lock()
            .unwrap()
            .insert(request.handle, request.clone());
        BindDecision::accept()
    }

    async fn on_bound(&self, handle: &RouteHandle) {
        let bind = self.bindings.lock().unwrap()[handle].clone();
        if let Some(recovery) = self.recoveries.lock().unwrap().get(&bind.identity.session) {
            recovery.bound(&bind);
        }
    }

    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        assert!(self.records, "only the runner receives data requests");
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        if body["method"] == "session.head" {
            return HandlerOutcome::Response(
                cortexkit_role_llm_runner::wire::reply_body(
                    &cortexkit_role_llm_runner::read::HeadMeta::empty(1, "fixture"),
                )
                .unwrap(),
            );
        }
        assert_eq!(body["method"], "session.send");
        assert_eq!(body["params"]["notice"], true);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.fixture.join("runner-received.jsonl"))
            .unwrap();
        let binding = self.bindings.lock().unwrap()[&ctx.route_handle()]
            .identity
            .clone();
        writeln!(file, "{}", json!({"module":self.module,"harness":binding.harness,"session":binding.session,"request":body})).unwrap();
        HandlerOutcome::Response(
            cortexkit_role_llm_runner::wire::reply_body(
                &cortexkit_role_llm_runner::send::SendReply::new("pending")
                    .with_submission_id("notice-1"),
            )
            .unwrap(),
        )
    }
}

pub(super) async fn run() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let fixture = home.parent().unwrap().to_path_buf();
    let module = std::env::var("SUBC_MODULE_ID").unwrap();
    let aliases = std::env::var_os("AFT_TEST_RUNNER_ALIAS").is_some();
    if module == "aft" {
        static LOGGER: OnceLock<Logger> = OnceLock::new();
        let logger = LOGGER.get_or_init(|| {
            Logger(Mutex::new(
                std::fs::File::create(fixture.join("runner-transport.log")).unwrap(),
            ))
        });
        log::set_logger(logger).unwrap();
        log::set_max_level(log::LevelFilter::Info);
    }
    let mut manifest = ModuleManifest::builder(&module, "0.0.0");
    if module == "broca" || module == "thalamus-harness" || (aliases && module == "aft") {
        manifest = manifest
            .provides(vec![ProviderRole::ManagementSurface {
                operations: vec![ManagementOperation {
                    name: "session.send".into(),
                    kind: ManagementOperationKind::Mutate,
                    description: None,
                }],
                config_schema: json!({}),
                observability: vec![],
                identity_scope: vec![],
                concurrency: Concurrency::ModuleManaged,
            }])
            .role_declarations(std::collections::BTreeMap::from([(
                "llm-runner".into(),
                json!({"versions":["v1"]}).as_object().unwrap().clone(),
            )]));
    }
    let recoveries = Arc::new(Mutex::new(HashMap::new()));
    let (handle, serve) = subc_client_rs::serve_from_env_with_handle(
        manifest.build(),
        Peer {
            fixture: fixture.clone(),
            records: module == "broca" || module == "thalamus-harness",
            module: module.clone(),
            bindings: Default::default(),
            recoveries: recoveries.clone(),
        },
    )
    .await
    .unwrap();
    let serving = tokio::spawn(serve);
    if module == "aft-test-scope-owner" {
        let denied = ScopeRecord::new("notice-denied", 1, ScopeKind::Head)
            .with_workdir(Some(fixture.join("project").to_string_lossy().into_owned()));
        let allowed = ScopeRecord::new("notice-allowed", 1, ScopeKind::Worker)
            .with_workdir(Some(fixture.join("project").to_string_lossy().into_owned()))
            .with_carriers(vec![ScopeCarrier::new(Principal::Reserved {
                module_id: "aft".into(),
            })
            .with_targets(Some(if aliases {
                vec!["broca".into(), "thalamus-harness".into()]
            } else {
                vec!["broca".into()]
            }))]);
        let allowed = if aliases {
            allowed.with_carriers(vec![
                ScopeCarrier::new(Principal::Reserved {
                    module_id: "aft".into(),
                })
                .with_targets(Some(vec!["broca".into(), "thalamus-harness".into()])),
                ScopeCarrier::new(Principal::Reserved {
                    module_id: "thalamus-harness".into(),
                })
                .with_targets(Some(vec!["aft".into()])),
            ])
        } else {
            allowed
        };
        let reply = handle.scope_sync(1, vec![denied, allowed]).await.unwrap();
        assert_eq!(reply.results.len(), 2, "{reply:?}");
        for result in &reply.results {
            assert_ne!(result.outcome, ScopeRecordOutcome::Refused, "{reply:?}");
        }
        std::fs::write(fixture.join("notice-owner.ready"), b"ready").unwrap();
    } else if module == "broca" {
        std::fs::write(fixture.join("notice-runner.ready"), b"ready").unwrap();
    } else if module == "thalamus-harness" {
        std::fs::write(fixture.join("notice-thalamus.ready"), b"ready").unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !fixture.join("notice-waiting-for-harness").exists() {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let connection = PathBuf::from(std::env::var_os("AFT_TEST_CONNECTION").unwrap());
        let consumer = subc_client_rs::SubcConsumer::connect(&connection, Default::default())
            .await
            .unwrap();
        let route = consumer
            .open_route_scoped(
                subc_protocol::RouteTarget::ManagementSurface {
                    module_id: "aft".into(),
                },
                subc_protocol::BindIdentity::new("opencode2", "bash-00000000000d0012"),
                ScopeSelector::new(
                    Principal::Reserved {
                        module_id: "aft-test-scope-owner".into(),
                    },
                    "notice-allowed",
                    Some(1),
                ),
                subc_client_rs::CallOptions {
                    consumer_identity: aft::launch_nonce::consumer_identity(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        while !fixture.join("runner-outcomes.json").exists() {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        consumer
            .close_handle(&route, Default::default())
            .await
            .unwrap();
        consumer.close().await.unwrap();
    } else {
        assert_eq!(module, "aft");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !fixture.join("notice-owner.ready").exists()
            || !fixture.join("notice-runner.ready").exists()
            || (aliases && !fixture.join("notice-thalamus.ready").exists())
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "notice peers did not become ready"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let connection = PathBuf::from(std::env::var_os("AFT_TEST_CONNECTION").unwrap());
        if aliases {
            run_aliases(&fixture, &connection, &recoveries).await;
            serving.await.unwrap().unwrap();
            return;
        }
        let scope = |name: &str| {
            ScopeSelector::new(
                Principal::Reserved {
                    module_id: "aft-test-scope-owner".into(),
                },
                name,
                Some(1),
            )
        };
        let denied = aft::subc::runner_notice_transport_for_test(
            &connection,
            "notice-denied",
            scope("notice-denied"),
        )
        .await;
        let denied_again = aft::subc::runner_notice_transport_for_test(
            &connection,
            "notice-denied",
            scope("notice-denied"),
        )
        .await;
        let accepted = aft::subc::runner_notice_transport_for_test(
            &connection,
            "notice-allowed",
            scope("notice-allowed"),
        )
        .await;
        std::fs::write(
            fixture.join("runner-outcomes.json"),
            serde_json::to_vec(
                &json!({"denied":denied,"denied_again":denied_again,"allowed":accepted}),
            )
            .unwrap(),
        )
        .unwrap();
    }
    serving.await.unwrap().unwrap();
}

async fn run_aliases(
    fixture: &std::path::Path,
    connection: &std::path::Path,
    recoveries: &Arc<Mutex<HashMap<String, Arc<aft::subc::RunnerNoticeRecoveryForTest>>>>,
) {
    use subc_protocol::scope::ScopeStamp;
    let cases = [
        ("bash-00000000000d0011", "broca", "runner"),
        ("bash-00000000000d0012", "thalamus-harness", "runner"),
        ("bash-00000000000d0013", "thalamus-harness", "pi"),
    ];
    for (task, module, raw) in cases {
        let recovery = Arc::new(aft::subc::RunnerNoticeRecoveryForTest::new(
            connection,
            &fixture.join("notice-storage").join(task),
            &fixture.join("project"),
            task,
            module,
            raw,
            ScopeStamp::new(
                Principal::Reserved {
                    module_id: "aft-test-scope-owner".into(),
                },
                "notice-allowed",
                1,
                ScopeKind::Worker,
            ),
        ));
        recoveries
            .lock()
            .unwrap()
            .insert(task.into(), recovery.clone());
        recovery.offer();
    }
    let pending = recoveries.lock().unwrap()["bash-00000000000d0012"].clone();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !pending.waiting_for_harness() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "legacy head did not await a harness"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!pending.delivered());
    let before = std::fs::read_to_string(fixture.join("runner-received.jsonl")).unwrap_or_default();
    assert!(
        !before.contains("aft:bash:bash-00000000000d0012"),
        "an unresolved head must not be sent"
    );
    std::fs::write(fixture.join("notice-waiting-for-harness"), b"pending").unwrap();
    while !recoveries
        .lock()
        .unwrap()
        .values()
        .all(|recovery| recovery.delivered())
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "recovered completions were not delivered"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    std::fs::write(
        fixture.join("runner-outcomes.json"),
        serde_json::to_vec(
            &json!({"broca":true,"thalamus":true,"concrete":true,"waited_for_harness":true}),
        )
        .unwrap(),
    )
    .unwrap();
}
