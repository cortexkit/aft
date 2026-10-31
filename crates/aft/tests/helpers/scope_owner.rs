//! A daemon-launched scope owner for the opt-in real-daemon transport tests.
//! This binary is built only with the `test-scope-owner` feature.

use subc_client_rs::{async_trait, HandlerOutcome, ModuleHandler, RequestCtx};
use subc_protocol::manifest::ModuleManifest;
use subc_protocol::scope::{ScopeCarrier, ScopeKind, ScopeRecord, ScopeRecordOutcome};
use subc_protocol::Principal;

struct ScopeOwner;

#[async_trait]
impl ModuleHandler for ScopeOwner {
    async fn handle(&self, _ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(b"{}".to_vec())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("isolated test home"));
    let fixture = home.parent().expect("fixture directory");
    let project = fixture.join("project");
    let manifest = ModuleManifest::builder("aft-test-scope-owner", "0.0.0").build();
    let (handle, serve) = subc_client_rs::serve_from_env_with_handle(manifest, ScopeOwner)
        .await
        .expect("connect test scope owner");
    let serving = tokio::spawn(serve);
    let scope = ScopeRecord::new("e2e-project", 1, ScopeKind::Head)
        .with_workdir(Some(project.to_string_lossy().into_owned()))
        .with_carriers(vec![ScopeCarrier::new(Principal::Direct)]);
    let reply = handle
        .scope_sync(1, vec![scope])
        .await
        .expect("register test scope");
    assert_eq!(reply.results.len(), 1, "{reply:?}");
    assert_ne!(
        reply.results[0].outcome,
        ScopeRecordOutcome::Refused,
        "{reply:?}"
    );
    std::fs::write(fixture.join("scope-owner.ready"), b"ready").expect("publish scope readiness");
    serving
        .await
        .expect("serve task")
        .expect("serve scope owner");
}
