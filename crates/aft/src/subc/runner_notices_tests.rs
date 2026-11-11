use super::*;
use cortexkit_role_llm_runner::{
    send::SendReply,
    wire::{read_reply, reply_body},
};

fn owner(principal: &str, harness: &str) -> crate::bash_background::TaskIdentity {
    crate::bash_background::TaskIdentity {
        principal: Some(principal.into()),
        harness: harness.into(),
        session: "session-7".into(),
    }
}

#[test]
fn runner_alias_switch_preserves_task_ownership_and_persisted_keys() {
    let principal = |raw: &str| AuthenticatedPrincipal::RouteBind {
        trust: crate::sandbox_spawn::PrincipalTrust::FirstParty,
        route_channel: 1,
        route_epoch: 1,
        project_root: PathBuf::from("/project"),
        harness: raw.into(),
        session_id: "session-7".into(),
        principal_id: Some("reserved:broca".into()),
    };
    let canonical =
        crate::bash_background::TaskIdentity::from_principal(&principal("runner")).unwrap();
    for raw in ["runner", "opencode2", "broca"] {
        let identity =
            crate::bash_background::TaskIdentity::from_principal(&principal(raw)).unwrap();
        assert_eq!(identity, canonical);
        assert_eq!(
            super::super::tool_provider::ledger_carrier(&principal(raw)),
            super::super::tool_provider::ledger_carrier(&principal("runner"))
        );
        let saved: crate::bash_background::TaskIdentity = serde_json::from_value(
            json!({"principal":"reserved:broca", "harness":raw, "session":"session-7"}),
        )
        .unwrap();
        assert_eq!(saved, canonical);
        assert_eq!(
            serde_json::to_vec(&identity).unwrap(),
            serde_json::to_vec(&canonical).unwrap()
        );
    }
}

#[derive(Default)]
struct FakeRunner {
    calls: Vec<Value>,
    stored: HashMap<String, Vec<u8>>,
    notices: usize,
    lose_first_response: bool,
    refuse: Option<&'static str>,
}

impl FakeRunner {
    fn bind(
        &self,
        address: &Address,
        versions: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), &'static str> {
        if versions != &std::collections::BTreeMap::from([("llm-runner".into(), "v1".into())]) {
            return Err("role_version_unsupported");
        }
        if !matches!(address.module.as_str(), "thalamus-harness" | "broca") {
            return Err("module_not_found");
        }
        let identity = serde_json::to_value(&address.identity).unwrap();
        assert!(identity.get("project_root").is_none());
        Ok(())
    }

    fn call(&mut self, bytes: &[u8]) -> Outcome {
        let body: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(body.as_object().unwrap().len(), 2);
        assert_eq!(body["method"], "session.send");
        let request: SendRequest = serde_json::from_value(body["params"].clone()).unwrap();
        request.check_prompt().unwrap();
        assert_eq!(serde_json::to_value(&request).unwrap()["notice"], true);
        self.calls.push(body);
        if let Some(code) = self.refuse {
            return refusal(code);
        }
        let bytes = serde_json::to_vec(&request).unwrap();
        if let Some(first) = self.stored.get(&request.send_id) {
            if first != &bytes {
                return refusal("send_id_reuse");
            }
        } else {
            self.notices += 1;
            self.stored.insert(request.send_id.clone(), bytes);
        }
        if self.lose_first_response && self.calls.len() == 1 {
            return Outcome::Retry;
        }
        let response = reply_body(&SendReply::new("pending").with_submission_id("send-7")).unwrap();
        read_reply::<SendReply>(&response).unwrap();
        Outcome::Accepted
    }
}

#[test]
fn runner_notices_retry_unknown_outcome_with_same_id_and_dedupe() {
    let request = request("aft:bash:bash-7".into(), "completion".into(), true, false);
    let mut runner = FakeRunner {
        lose_first_response: true,
        ..Default::default()
    };
    let mut delays = Vec::new();
    assert!(deliver(
        &request.send_id,
        0,
        |_| Some(runner.call(&envelope(&request))),
        |delay| delays.push(delay)
    ));
    assert_eq!(runner.calls.len(), 2);
    assert_eq!(runner.notices, 1);
    assert_eq!(runner.calls[0], runner.calls[1]);
    assert_eq!(delays, [Duration::from_millis(250)]);
}

#[test]
fn runner_notices_final_refusals_stop_without_retry() {
    for code in [
        "scope_owner_mismatch",
        "expired",
        "session_released",
        "send_id_reuse",
        "delivery_unsupported",
        "invalid_params",
    ] {
        let request = request("aft:bash:bash-7".into(), "completion".into(), true, false);
        let mut runner = FakeRunner {
            refuse: Some(code),
            ..Default::default()
        };
        assert!(deliver(
            &request.send_id,
            0,
            |_| Some(runner.call(&envelope(&request))),
            |_| panic!("final refusal retried")
        ));
        assert_eq!(runner.calls.len(), 1, "{code}");
    }
}

#[test]
fn runner_notices_retry_budget_survives_persisted_offer_count() {
    let mut calls = 0;
    assert!(deliver(
        "aft:bash:bash-7",
        MAX_ATTEMPTS - 1,
        |_| {
            calls += 1;
            Some(Outcome::Retry)
        },
        |_| panic!("no attempts remain")
    ));
    assert_eq!(calls, 1);
    assert!(deliver(
        "aft:bash:bash-7",
        MAX_ATTEMPTS,
        |_| panic!("restart reset budget"),
        |_| {}
    ));
}

#[test]
fn runner_notices_raw_harness_bind_back_and_worker_delivery() {
    let stamp = subc_protocol::scope::ScopeStamp::new(
        Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        "worker-7",
        3,
        subc_protocol::scope::ScopeKind::Worker,
    );
    for (principal, raw, worker) in [
        ("reserved:thalamus-harness", "opencode2", false),
        ("reserved:thalamus-harness", "pi", false),
        ("reserved:broca", "broca", true),
    ] {
        let owner = owner(principal, "runner");
        let address = Address::new(&owner, raw.into(), None).unwrap();
        assert_eq!(address.identity.harness, raw);
        assert_eq!(address.identity.session, "session-7");
        assert_eq!(address.worker, worker);
        let versions = role_versions();
        assert_eq!(
            serde_json::to_value(target(&address)).unwrap(),
            json!({"kind":"management_surface", "module_id":address.module})
        );
        let runner = FakeRunner::default();
        runner.bind(&address, &versions).unwrap();
        assert!(runner
            .bind(
                &address,
                &std::collections::BTreeMap::from([("llm-runner".into(), "v2".into())])
            )
            .is_err());
        assert!(runner
            .bind(
                &address,
                &std::collections::BTreeMap::from([("tool-provider".into(), "v1".into())])
            )
            .is_err());
        let request = request("aft:bash:7".into(), "text".into(), true, address.worker);
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(
            value["delivery"],
            if worker {
                json!(["tool_result", "steer"])
            } else {
                json!(["tool_result", "steer", "idle_start"])
            }
        );
        let scoped = Address::new(&owner, raw.into(), Some(&stamp)).unwrap();
        assert!(scoped.worker);
        assert_eq!(
            scoped.scope.unwrap(),
            ScopeSelector::new(stamp.owner.clone(), "worker-7", Some(3))
        );
    }
    assert!(Address::new(&owner("reserved:thalamus-pi", "pi"), "pi".into(), None).is_none());
    assert!(!is_runner(&owner("direct", "opencode")));
    assert!(!is_runner(&owner("direct", "pi")));
}

#[test]
fn runner_notices_only_completions_on_heads_may_start_idle_runs() {
    for completion in [true, false] {
        for worker in [true, false] {
            let request = request("test".into(), "text".into(), completion, worker);
            let value = serde_json::to_value(request).unwrap();
            assert_eq!(
                value["delivery"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("idle_start")),
                completion && !worker
            );
        }
    }
    assert!(read_reply::<SendReply>(br#"{"result":{"state":"pending"},"extra":true}"#).is_err());
    assert!(read_reply::<SendReply>(br#"{"state":"pending"}"#).is_err());
}

#[test]
fn runner_notices_readable_sender_grant_can_disable_idle_start() {
    use cortexkit_role_llm_runner::read::{HeadMeta, HeadSender};
    let head = HeadMeta::empty(1, "incarnation");
    assert_eq!(idle_start_grant(&head), None);
    let limited = head.clone().with_sender(HeadSender::new(
        vec!["tool_result".into(), "steer".into()],
        false,
    ));
    assert_eq!(idle_start_grant(&limited), Some(false));
    let full = head.with_sender(HeadSender::new(
        vec!["tool_result".into(), "steer".into(), "idle_start".into()],
        false,
    ));
    assert_eq!(idle_start_grant(&full), Some(true));
}
