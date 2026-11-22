use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use subc_protocol::{ErrorBody, Frame, FrameType};
use tokio::sync::mpsc;

use super::super::{
    consent, control_flags, route_key, test_support, AuthenticatedPrincipal, DecodedFrame,
    DispatchPathMetrics, PhaseTrace, ProjectRootId, PROTOCOL_VERSION,
};
use super::{LedgerEdge, RouteRole};
use crate::context::Executor;
use crate::db::call_ledger::{self as ledger, Key, RecordedFrame, ScopeIdentity, State};

const THALAMUS: &str = "reserved:thalamus-harness";
const HEAD_OWNER: &str = "reserved:prefrontal-core";
const HEAD_SESSION: &str = "head-session";

async fn exchange(
    executor: &Arc<Executor>,
    root: &ProjectRootId,
    channel: u16,
    carrier: &str,
    name: &str,
    arguments: Value,
) -> (FrameType, Value) {
    let mut identity = test_support::route_identity(root, HEAD_SESSION);
    let data = Arc::get_mut(&mut identity.0).unwrap();
    data.role = RouteRole::ToolProviderV1;
    data.held_reply = true;
    data.held_update = true;
    data.scope = Some(
        serde_json::from_value(json!({
            "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
            "ref": HEAD_SESSION, "scope_epoch": 1, "kind": "head", "owner_authorized": true,
        }))
        .unwrap(),
    );
    let AuthenticatedPrincipal::RouteBind { principal_id, .. } = &mut data.spawn_principal else {
        unreachable!()
    };
    *principal_id = Some(carrier.into());
    let routes = [(route_key(channel, 1), identity)].into();
    let (edge, _ready) = LedgerEdge::new();
    let (writer, mut replies) = mpsc::channel(8);
    edge.defer(
        DecodedFrame {
            parsed_body: Default::default(),
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
            frame: Frame::build(
                FrameType::Request,
                control_flags(),
                channel,
                1,
                7,
                serde_json::to_vec(&json!({"name": name, "arguments": arguments})).unwrap(),
            )
            .unwrap(),
            phase_trace: PhaseTrace::new(Instant::now()),
        },
        &routes,
        executor,
        &writer,
        &Arc::new(DispatchPathMetrics::new()),
    )
    .ok()
    .expect("the v1 ledger edge must handle custody requests");
    let reply = tokio::time::timeout(Duration::from_secs(5), replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply.header.channel, channel);
    assert_eq!(reply.header.corr, 7);
    (
        reply.header.ty,
        serde_json::from_slice(&reply.body).unwrap(),
    )
}

#[tokio::test]
async fn late_results_head_bound_ack_preserves_the_retired_cursor() {
    let (dir, root) = test_support::test_root("late-results-custody");
    let db = Arc::new(Mutex::new(
        crate::db::open(&dir.path().join("aft.db")).unwrap(),
    ));
    let scope = ScopeIdentity {
        owner: HEAD_OWNER.into(),
        scope_ref: HEAD_SESSION.into(),
        scope_epoch: 1,
    };
    let expired = Key {
        carrier: THALAMUS.into(),
        call_key: "expired-call".into(),
    };
    let running = Key {
        carrier: THALAMUS.into(),
        call_key: "running-call".into(),
    };
    {
        let conn = db.lock().unwrap();
        let now = ledger::now_ms();
        let old = now - ledger::RETENTION_MS;
        for key in [&expired, &running] {
            ledger::admit(&conn, key, "digest", Some(&scope), State::DispatchStarted).unwrap();
        }
        assert!(ledger::settle(
            &conn,
            &expired,
            &RecordedFrame {
                ty: FrameType::Response,
                body: br#"{"content":[{"type":"text","text":"done"}]}"#.to_vec(),
            },
            "ok",
            None,
            old,
        )
        .unwrap());
        ledger::reduce_expired(&conn, &expired, now).unwrap();
        let held =
            consent::held_frame(PROTOCOL_VERSION, route_key(41, 1), 7, control_flags()).unwrap();
        assert!(ledger::hold_permission(&conn, &running, &held.body, true).unwrap());
        assert!(ledger::record_running_update(&conn, &running, "approval", now).unwrap());
    }
    let ctx = test_support::test_ctx();
    ctx.set_db(db.clone());
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root.clone(), ctx));

    let (ty, first) = exchange(
        &executor,
        &root,
        41,
        THALAMUS,
        "late_results",
        json!({"since": null}),
    )
    .await;
    assert_eq!(ty, FrameType::Response, "{first}");
    assert_eq!(first["entries"].as_array().unwrap().len(), 2);
    assert_eq!(first["entries"][0]["call_key"], expired.call_key);
    assert_eq!(first["entries"][0]["kind"], "expired");
    assert_eq!(first["entries"][0]["reduced"], true);
    assert_eq!(first["entries"][1]["call_key"], running.call_key);
    assert_eq!(first["entries"][1]["held"], json!({"reason": "running"}));
    let through = first["cursor"].clone();
    assert_eq!(through["seq"], 2);

    let (ty, other) = exchange(
        &executor,
        &root,
        42,
        HEAD_OWNER,
        "late_results",
        json!({"since": null}),
    )
    .await;
    assert_eq!(ty, FrameType::Response, "{other}");
    assert_eq!(other["entries"], json!([]));
    let (ty, error) = exchange(
        &executor,
        &root,
        43,
        HEAD_OWNER,
        "late_results.ack",
        json!({"through": through}),
    )
    .await;
    assert_eq!(ty, FrameType::Error, "{error}");
    assert_eq!(error["code"], "cursor_incarnation_changed");
    assert!(ledger::get(&db.lock().unwrap(), &expired)
        .unwrap()
        .is_some());

    let ack = exchange(
        &executor,
        &root,
        44,
        THALAMUS,
        "late_results.ack",
        json!({"through": through}),
    )
    .await;
    assert_eq!(ack, (FrameType::Response, json!({})));
    {
        let conn = db.lock().unwrap();
        assert!(ledger::get(&conn, &expired).unwrap().is_none());
        let entry: Option<String> = conn
            .query_row(
                "SELECT entry FROM call_ledger_updates WHERE carrier=?1 AND call_key=?2",
                [&running.carrier, &running.call_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(entry, None);
    }
    let page = exchange(
        &executor,
        &root,
        45,
        THALAMUS,
        "late_results",
        json!({"since": through}),
    )
    .await;
    assert_eq!(
        page,
        (
            FrameType::Response,
            json!({"entries": [], "cursor": through, "more": false})
        )
    );

    for since in [
        json!({"provider_incarnation": "foreign", "seq": 2}),
        json!({"provider_incarnation": through["provider_incarnation"], "seq": 3}),
    ] {
        let (ty, reply) = exchange(
            &executor,
            &root,
            46,
            THALAMUS,
            "late_results",
            json!({"since": since}),
        )
        .await;
        assert_eq!(ty, FrameType::Error, "{reply}");
        let error: ErrorBody = serde_json::from_value(reply).unwrap();
        assert_eq!(error.code, "cursor_incarnation_changed");
        assert_eq!(
            error.detail.unwrap()["provider_incarnation"],
            through["provider_incarnation"]
        );
    }
}
