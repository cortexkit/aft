use std::{collections::VecDeque, sync::Mutex};

use super::*;

/// Which fake module a recorded call went to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Target {
    Plexus,
}

/// Fake plexus. Every call's body is recorded; replies are scripted and
/// default to a completed reply.
/// `scoped` says whether this fake can open its plexus route under a scope.
#[derive(Default)]
pub(super) struct FakeTransport {
    calls: Mutex<Vec<(Target, String, Value)>>,
    plexus_replies: Mutex<VecDeque<Result<Value, TransportError>>>,
    scoped: bool,
    plexus_scopes: Mutex<Vec<Option<ScopeSelector>>>,
    pub(super) read_binding: Mutex<Option<Result<String, String>>>,
    pub(super) read_roots: Mutex<Vec<String>>,
}

impl FakeTransport {
    pub(super) fn scoped() -> Self {
        Self {
            scoped: true,
            ..Self::default()
        }
    }

    /// The scope each plexus call's route was opened under, in call order.
    #[cfg(test)]
    pub(super) fn plexus_scopes(&self) -> Vec<Option<ScopeSelector>> {
        self.plexus_scopes.lock().unwrap().clone()
    }

    pub(super) fn push_plexus(&self, reply: Result<Value, TransportError>) {
        self.plexus_replies.lock().unwrap().push_back(reply);
    }

    pub(super) fn calls(&self) -> Vec<(Target, String, Value)> {
        self.calls.lock().unwrap().clone()
    }

    #[cfg(test)]
    pub(super) fn count(&self, target: Target) -> usize {
        self.calls()
            .iter()
            .filter(|(called, _, _)| *called == target)
            .count()
    }
}

impl RelayTransport for FakeTransport {
    fn read_repository(&self, ticket: &crate::gh_shim_ticket::Redeemed) -> Result<String, String> {
        let project_root = &ticket.project_root;
        self.read_roots
            .lock()
            .unwrap()
            .push(project_root.to_string());
        self.read_binding
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| Ok("cortexkit/aft".into()))
    }
    async fn plexus(
        &self,
        _project_root: &str,
        session: &str,
        scope: Option<&ScopeSelector>,
        body: Value,
    ) -> Result<Value, TransportError> {
        assert!(
            scope.is_none() || self.scoped,
            "the relay asked a transport that cannot open scoped routes for one"
        );
        self.plexus_scopes.lock().unwrap().push(scope.cloned());
        self.calls
            .lock()
            .unwrap()
            .push((Target::Plexus, session.to_string(), body));
        let scripted = self.plexus_replies.lock().unwrap().pop_front();
        scripted.unwrap_or_else(|| {
            Ok(json!({"repo_binding_generation": 1, "result": {"status": "completed"}}))
        })
    }

    fn opens_scoped_routes(&self) -> bool {
        self.scoped
    }
}

pub(super) fn golden() -> Value {
    serde_json::from_str(include_str!(
        "../../fixtures/plexus/seen-marker-reaction.json"
    ))
    .unwrap()
}

pub(super) fn envelope() -> Value {
    golden()["request"]["request"].clone()
}

pub(super) fn run_with_scopes<T: RelayTransport>(
    transport: &T,
    operation: &str,
    params: Value,
    route_scopes: &RouteScopes,
) -> (RelayReply, Vec<String>) {
    let lines = Mutex::new(Vec::new());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let reply = runtime.block_on(relay_transport_contract(
        transport,
        RelayCall {
            operation,
            params: &params,
            first_party: true,
            route_scopes,
        },
        &|line| lines.lock().unwrap().push(line.to_string()),
    ));
    (reply, lines.into_inner().unwrap())
}

pub(super) fn bot_params(ticket: &str, nonce: &str) -> Value {
    json!({"ticket": ticket, "request_nonce": nonce, "request": envelope()})
}

/// A Prefrontal-owned stamp as the daemon puts it on a head's tool route.
pub(super) fn stamp(agent: Option<&str>, delegates: bool, owner_authorized: bool) -> ScopeStamp {
    ScopeStamp::new(
        subc_protocol::Principal::Reserved {
            module_id: "prefrontal".to_string(),
        },
        "head-ref-1",
        7,
        subc_protocol::scope::ScopeKind::Head,
    )
    .with_attributes(
        subc_protocol::scope::ScopeAttributes::new()
            .with_agent_id(agent.map(str::to_string))
            .with_delegates(delegates),
    )
    .with_owner_authorized(owner_authorized)
}

pub(super) fn delegating_stamp() -> ScopeStamp {
    stamp(Some("agent-fixture"), true, true)
}

pub(super) fn scopes_for(session: &str, stamp: Option<ScopeStamp>) -> RouteScopes {
    let mut scopes = RouteScopes::default();
    scopes.push(session, stamp);
    scopes
}

#[cfg(all(unix, feature = "test-support"))]
pub fn assert_spawned_ticket_relays(ticket: &str, task_id: &str) {
    let golden = golden();
    let transport = FakeTransport::scoped();
    transport.push_plexus(Ok(golden["success"].clone()));
    let nonce = golden["request"]["request_nonce"].as_str().unwrap();
    let (reply, lines) = run_with_scopes(
        &transport,
        BOT_REQUEST_OPERATION,
        bot_params(ticket, nonce),
        &scopes_for("ses-agent", Some(delegating_stamp())),
    );

    assert_eq!(reply, RelayReply::relayed(golden["success"].clone()));
    let calls = transport.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, Target::Plexus);
    assert_eq!(calls[0].1, "ses-agent");
    // The fixture request still carries an `assertion` field. AFT's write is
    // that request with `assertion` removed and `op` set to `bot_request`.
    let mut expected_arguments = golden["request"].clone();
    expected_arguments["op"] = json!("bot_request");
    expected_arguments
        .as_object_mut()
        .unwrap()
        .remove("assertion");
    assert_eq!(
        calls[0].2,
        json!({"name": "github", "arguments": expected_arguments})
    );
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].contains(&format!("task={task_id}")),
        "{}",
        lines[0]
    );
}
