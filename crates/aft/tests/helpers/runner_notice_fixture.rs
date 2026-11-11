//! Supervised peers for the real-daemon runner-notice transport test.

use serde_json::json;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use subc_client_rs::{async_trait, HandlerOutcome, ModuleHandler, RequestCtx};
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
}

#[async_trait]
impl ModuleHandler for Peer {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        assert!(self.records, "only the runner receives data requests");
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["method"], "session.send");
        assert_eq!(body["params"]["notice"], true);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.fixture.join("runner-received.jsonl"))
            .unwrap();
        writeln!(file, "{body}").unwrap();
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
    if module == "broca" {
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
    let (handle, serve) = subc_client_rs::serve_from_env_with_handle(
        manifest.build(),
        Peer {
            fixture: fixture.clone(),
            records: module == "broca",
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
            .with_targets(Some(vec!["broca".into()]))]);
        let reply = handle.scope_sync(1, vec![denied, allowed]).await.unwrap();
        assert_eq!(reply.results.len(), 2, "{reply:?}");
        for result in &reply.results {
            assert_ne!(result.outcome, ScopeRecordOutcome::Refused, "{reply:?}");
        }
        std::fs::write(fixture.join("notice-owner.ready"), b"ready").unwrap();
    } else if module == "broca" {
        std::fs::write(fixture.join("notice-runner.ready"), b"ready").unwrap();
    } else {
        assert_eq!(module, "aft");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !fixture.join("notice-owner.ready").exists()
            || !fixture.join("notice-runner.ready").exists()
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "notice peers did not become ready"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let connection = PathBuf::from(std::env::var_os("AFT_TEST_CONNECTION").unwrap());
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
