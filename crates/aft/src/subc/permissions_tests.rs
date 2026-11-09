use super::*;
use crate::access_policy::{AccessDecision, AccessGrant};
use crate::config::Config;
use crate::db::access_grants::{self as store, ScopeKey};
use std::sync::Mutex;

const OWNER: &str = r#"{"kind":"reserved","module_id":"prefrontal-core"}"#;

fn key(scope_ref: &str, epoch: u64) -> ScopeKey {
    ScopeKey {
        owner: OWNER.into(),
        scope_ref: scope_ref.into(),
        epoch,
    }
}

fn head() -> ScopeKey {
    key("scope:head-alf", 7)
}

fn worker() -> ScopeKey {
    key("scope:wkr-7f3a", 3)
}

/// aft.db with a head scope (agent `agent-alf`) and a worker scope it owns,
/// registered the way a daemon-stamped bind registers them.
fn database(dir: &Path) -> Mutex<crate::db::TrackedConnection> {
    let conn = crate::db::open(&dir.join("aft.db")).unwrap();
    store::register_scope(&conn, &head(), None, &Config::default()).unwrap();
    store::register_agent(&conn, &head(), "agent-alf", true).unwrap();
    store::register_scope(&conn, &worker(), Some(&head()), &Config::default()).unwrap();
    store::register_worker(&conn, &worker()).unwrap();
    Mutex::new(conn)
}

fn grant(path: &Path, kind: GrantKind, operation: AccessOperation) -> AccessGrant {
    AccessGrant {
        path: path.to_path_buf(),
        kind,
        operation,
    }
}

fn cingulate() -> Option<Principal> {
    Some(Principal::Reserved {
        module_id: "cingulate".into(),
    })
}

fn request(method: &str, params: Value) -> Request {
    admit(
        cingulate().as_ref(),
        &json!({"method": method, "params": params}),
    )
    .unwrap()
}

fn list_all(db: &Mutex<crate::db::TrackedConnection>) -> Vec<Value> {
    let reply = answer(Some(db), &request(LIST_OPERATION, json!({})), |_| 0).unwrap();
    reply["grants"].as_array().unwrap().clone()
}

fn revoke(db: &Mutex<crate::db::TrackedConnection>, id: &str, stopped: usize) -> Value {
    answer(
        Some(db),
        &request(REVOKE_OPERATION, json!({"id": id})),
        |_| stopped,
    )
    .unwrap()
}

/// The fields a `permissions/v1` row may carry, and those it must.
const ROW_FIELDS: [&str; 14] = [
    "id",
    "agent_id",
    "holder",
    "module",
    "subject_label",
    "scope",
    "scope_label",
    "group",
    "methods",
    "expires_at_ms",
    "granted_at_ms",
    "card_ref",
    "source",
    "granted_at_bound",
];
const REQUIRED_ROW_FIELDS: [&str; 7] = [
    "id",
    "holder",
    "module",
    "subject_label",
    "scope",
    "scope_label",
    "granted_at_ms",
];

fn assert_contract_row(row: &Value) {
    let object = row.as_object().expect("row is an object");
    for field in object.keys() {
        assert!(
            ROW_FIELDS.contains(&field.as_str()),
            "unknown field {field}: {row}"
        );
    }
    for field in REQUIRED_ROW_FIELDS {
        assert!(object.contains_key(field), "missing {field}: {row}");
    }
    assert_eq!(row["module"], "aft");
    assert!(row["granted_at_ms"].is_u64(), "{row}");
    // Saved answers last until revoked or until their scope ends.
    assert!(row.get("expires_at_ms").is_none(), "{row}");
    assert!(row.get("group").is_none() && row.get("methods").is_none());
    assert!(["read", "read_write", "file_read", "file_read_write"]
        .contains(&row["scope"].as_str().unwrap()));
    // The row is listed under the holder's agent (a worker's owning head).
    assert_eq!(row["agent_id"], row["holder"]["agent_id"], "{row}");
    match row["holder"]["kind"].as_str() {
        Some("agent") => assert_eq!(row["holder"].as_object().unwrap().len(), 2, "{row}"),
        Some("worker") => {
            assert_eq!(row["holder"].as_object().unwrap().len(), 3, "{row}");
            assert!(row["holder"]["scope_ref"].is_string(), "{row}");
        }
        other => panic!("AFT never holds a grant as {other:?}: {row}"),
    }
}

#[test]
fn permissions_list_rows_for_agent_and_worker_grants() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(dir.path());
    let home = crate::access_policy::policy_path(dir.path()).join("home");
    let site = home.join("Work").join("site");
    let notes = home.join("Work").join("notes");
    {
        let conn = db.lock().unwrap();
        store::insert_from_card(
            &conn,
            &head(),
            &grant(&site, GrantKind::Subtree, AccessOperation::Read),
            Some("acc_1"),
        )
        .unwrap();
        // Widening the same folder keeps one row per folder per holder.
        store::insert_from_card(
            &conn,
            &head(),
            &grant(&site, GrantKind::Subtree, AccessOperation::Write),
            Some("acc_3"),
        )
        .unwrap();
        store::insert_from_card(
            &conn,
            &worker(),
            &grant(&notes, GrantKind::Subtree, AccessOperation::Read),
            Some("acc_2"),
        )
        .unwrap();
    }
    let rows = list(&db.lock().unwrap(), None, &[home.clone()]).unwrap();
    let rows = rows["grants"].as_array().unwrap().clone();
    assert_eq!(rows.len(), 2, "{rows:?}");
    rows.iter().for_each(assert_contract_row);
    // The label shows the folder the way the person's system writes it:
    // `~/Work/site` on macOS and Linux, `~\Work\site` on Windows.
    let label = |folder: &str| {
        if cfg!(windows) {
            format!(r"~\Work\{folder}")
        } else {
            format!("~/Work/{folder}")
        }
    };
    let agent = &rows[0];
    assert_eq!(
        agent["holder"],
        json!({"kind": "agent", "agent_id": "agent-alf"})
    );
    assert_eq!(agent["subject_label"], label("site"));
    assert_eq!(agent["scope"], "read_write");
    assert_eq!(
        agent["scope_label"],
        format!("read and change files in {}", label("site"))
    );
    assert_eq!(agent["card_ref"], "acc_3");
    let worker_row = &rows[1];
    assert_eq!(
        worker_row["holder"],
        json!({"kind": "worker", "agent_id": "agent-alf", "scope_ref": "scope:wkr-7f3a"})
    );
    assert_eq!(worker_row["agent_id"], "agent-alf");
    assert_eq!(
        worker_row["scope_label"],
        format!("read files in {}", label("notes"))
    );
    assert_ne!(agent["id"], worker_row["id"]);

    // The agent filter keeps the head's own grants and its workers'.
    let filtered = list(&db.lock().unwrap(), Some("agent-alf"), &[home.clone()]).unwrap();
    assert_eq!(filtered["grants"].as_array().unwrap().len(), 2);
    let other = list(&db.lock().unwrap(), Some("agent-other"), &[home]).unwrap();
    assert_eq!(other, json!({"grants": []}));
}

#[test]
fn permissions_list_drops_grants_whose_scope_ended() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(dir.path());
    let notes = crate::access_policy::policy_path(dir.path()).join("notes");
    store::insert(
        &db.lock().unwrap(),
        &worker(),
        &grant(&notes, GrantKind::Subtree, AccessOperation::Read),
    )
    .unwrap();
    let id = list_all(&db)[0]["id"].as_str().unwrap().to_owned();
    // A saved answer ends with the scope that holds it: the daemon confirmed
    // the worker ended, so the row drops out and its id is unknown.
    store::end_scope_grants(&db.lock().unwrap(), &worker()).unwrap();
    assert!(list_all(&db).is_empty());
    assert_eq!(revoke(&db, &id, 0), json!({"outcome": "not_found"}));
}

#[test]
fn permissions_list_all_live_sources_have_times_and_remain_revocable() {
    use crate::db::access_grant_rows::GrantSource;
    let dir = tempfile::tempdir().unwrap();
    let db = database(dir.path());
    for (index, source) in [
        Some(GrantSource::Person),
        Some(GrantSource::HeadGrant),
        Some(GrantSource::HeadPolicy),
        None,
    ]
    .into_iter()
    .enumerate()
    {
        store::insert_with_source(
            &db.lock().unwrap(),
            &worker(),
            &grant(
                &dir.path().join(format!("source-{index}")),
                GrantKind::Subtree,
                AccessOperation::Read,
            ),
            source.map(|_| "acc_source"),
            source,
        )
        .unwrap();
    }
    let rows = list_all(&db);
    assert_eq!(rows.len(), 4);
    for (row, source) in rows
        .iter()
        .zip(["person", "head_grant", "head_policy", "unknown"])
    {
        assert_contract_row(row);
        assert_eq!(row["source"], source);
        assert!(
            row["granted_at_ms"].is_i64(),
            "every listed grant needs its time"
        );
        if source == "unknown" {
            assert_eq!(row["granted_at_bound"], "at_or_before");
        } else {
            assert!(row.get("granted_at_bound").is_none());
        }
        assert_eq!(
            revoke(&db, row["id"].as_str().unwrap(), 0),
            json!({"outcome":"revoked"})
        );
    }
    assert!(list_all(&db).is_empty());
}

#[test]
fn permissions_legacy_card_time_is_exact_despite_unknown_source() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(dir.path());
    store::insert_from_card(
        &db.lock().unwrap(),
        &worker(),
        &grant(
            &dir.path().join("card"),
            GrantKind::Subtree,
            AccessOperation::Read,
        ),
        Some("acc_legacy"),
    )
    .unwrap();
    let rows = list_all(&db);
    assert_eq!(rows[0]["source"], "unknown");
    assert!(rows[0]["granted_at_ms"].is_i64());
    assert!(rows[0].get("granted_at_bound").is_none());
}

#[test]
fn permissions_revoke_makes_the_scope_ask_again_and_answers_not_found_twice() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(dir.path());
    let root = dir.path().join("project");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let outside = crate::access_policy::policy_path(&outside);
    store::insert_from_card(
        &db.lock().unwrap(),
        &worker(),
        &grant(&outside, GrantKind::Subtree, AccessOperation::Write),
        Some("acc_1"),
    )
    .unwrap();
    let decide = || {
        let mut policy = crate::access_policy::AccessPolicy::from_config(
            &Config::default(),
            &dir.path().join("store"),
            vec![crate::access_policy::policy_path(&root)],
            &dir.path().join("home"),
        );
        policy.grants = store::list(&db.lock().unwrap(), &worker()).unwrap();
        policy.decide(AccessOperation::Write, &outside.join("file.txt"), false)
    };
    assert_eq!(decide(), AccessDecision::Allow);
    let id = list_all(&db)[0]["id"].as_str().unwrap().to_owned();

    let mut stopped_for = Vec::new();
    let reply = answer(
        Some(&db),
        &request(REVOKE_OPERATION, json!({"id": id})),
        |row| {
            stopped_for.push((row.scope.clone(), row.path.clone()));
            0
        },
    )
    .unwrap();
    assert_eq!(reply, json!({"outcome": "revoked"}));
    assert_eq!(stopped_for, vec![(worker(), outside.clone())]);
    assert_eq!(decide(), AccessDecision::Ask, "the next call asks again");
    assert!(list_all(&db).is_empty());
    assert_eq!(revoke(&db, &id, 0), json!({"outcome": "not_found"}));
    assert_eq!(revoke(&db, "grt_99", 0), json!({"outcome": "not_found"}));
}

#[test]
fn permissions_revoke_reports_stopping_only_when_running_work_lost_access() {
    let dir = tempfile::tempdir().unwrap();
    let db = database(dir.path());
    let folder = crate::access_policy::policy_path(dir.path()).join("a");
    for scope in [head(), worker()] {
        store::insert(
            &db.lock().unwrap(),
            &scope,
            &grant(&folder, GrantKind::Subtree, AccessOperation::Read),
        )
        .unwrap();
    }
    let ids: Vec<String> = list_all(&db)
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        revoke(&db, &ids[0], 1),
        json!({"outcome": "revoked", "stopping": true})
    );
    assert_eq!(revoke(&db, &ids[1], 0), json!({"outcome": "revoked"}));
}

#[test]
fn permissions_refuse_callers_other_than_cingulate_and_direct() {
    let body = json!({"method": REVOKE_OPERATION, "params": {"id": "aftg_1"}});
    for principal in [
        Some(Principal::Reserved {
            module_id: "plexus".into(),
        }),
        Some(Principal::Reserved {
            module_id: "prefrontal-core".into(),
        }),
        Some(Principal::Reserved {
            module_id: "aft".into(),
        }),
        Some(Principal::Unverified),
        None,
    ] {
        for body in [&body, &json!({"method": LIST_OPERATION, "params": {}})] {
            let refused = admit(principal.as_ref(), body).err().unwrap();
            assert_eq!(
                refused.code, "permissions_operator_required",
                "{principal:?}"
            );
        }
    }
    assert!(admit(cingulate().as_ref(), &body).is_ok());
    assert!(admit(Some(&Principal::Direct), &body).is_ok());
    // An admitted caller still sends well-formed parameters.
    for bad in [
        json!({"method": REVOKE_OPERATION, "params": {}}),
        json!({"method": REVOKE_OPERATION, "params": {"id": 7}}),
        json!({"method": LIST_OPERATION, "params": {"agent_id": 7}}),
        json!({"method": LIST_OPERATION, "params": []}),
    ] {
        assert_eq!(
            admit(cingulate().as_ref(), &bad).err().unwrap().code,
            "invalid_params"
        );
    }
    assert!(!is_request(&json!({"method": "permissions.list_all"})));
    assert!(is_request(&json!({"op": LIST_OPERATION})));
}

#[test]
fn permissions_subject_labels_show_home_as_tilde() {
    let homes = [PathBuf::from("/Users/person")];
    assert_eq!(display_path(Path::new("/Users/person"), &homes), "~");
    assert_eq!(
        display_path(Path::new("/Users/person/Work/x"), &homes),
        Path::new("~").join("Work/x").display().to_string()
    );
    assert_eq!(
        display_path(Path::new("/Users/personal/x"), &homes),
        "/Users/personal/x"
    );
    assert_eq!(display_path(Path::new("/srv/data"), &[]), "/srv/data");
}

/// The published sample of AFT's `permissions/v1` rows
/// (`test-vectors/permissions-v1/aft-provider-list.json`): `permissions.list`
/// from this handler for an agent grant and two worker grants (one saved from
/// a card, one from the worker's launch plan, which has no card). Ids and
/// times are pinned in the database so the reply is reproducible.
#[cfg(unix)]
#[test]
fn permissions_list_matches_the_published_sample() {
    let sample: Value = serde_json::from_str(include_str!(
        "../../../../test-vectors/permissions-v1/aft-provider-list.json"
    ))
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = database(dir.path());
    let home = PathBuf::from("/Users/person");
    {
        let conn = db.lock().unwrap();
        for (scope, path, kind, operation, card) in [
            (
                head(),
                "Work/site",
                GrantKind::Subtree,
                AccessOperation::Write,
                Some("acc_1"),
            ),
            (
                worker(),
                "Work/notes",
                GrantKind::Subtree,
                AccessOperation::Read,
                Some("acc_2"),
            ),
            (
                worker(),
                "Work/ref/spec.md",
                GrantKind::File,
                AccessOperation::Read,
                None,
            ),
        ] {
            store::insert_from_card(
                &conn,
                &scope,
                &grant(&home.join(path), kind, operation),
                card,
            )
            .unwrap();
        }
        for (index, path) in ["Work/site", "Work/notes", "Work/ref/spec.md"]
            .into_iter()
            .enumerate()
        {
            conn.execute(
                "UPDATE access_grants SET grant_id=?1, granted_at_ms=1000000 WHERE path=?2",
                rusqlite::params![
                    format!("aftg_{:024}", index + 1),
                    home.join(path).to_string_lossy()
                ],
            )
            .unwrap();
        }
    }
    assert_eq!(sample["method"], LIST_OPERATION);
    assert_eq!(sample["route"]["principal"], "reserved:cingulate");
    let mut reply = list(&db.lock().unwrap(), None, &[home]).unwrap();
    reply["grants"]
        .as_array()
        .unwrap()
        .iter()
        .for_each(assert_contract_row);
    // The older pinned sample predates additive provenance fields. Keep its
    // original row contract pinned; the sources sample pins the new fields.
    for row in reply["grants"].as_array_mut().unwrap() {
        row.as_object_mut().unwrap().remove("source");
        row.as_object_mut().unwrap().remove("granted_at_bound");
    }
    assert_eq!(reply, sample["reply"]);
}

/// Cingulate is not a first-party AFT principal, so its management route is
/// admitted for the `permissions/v1` operations alone. A first-party route
/// that is not the operator's is refused by name.
#[tokio::test]
async fn permissions_cingulate_route_serves_only_permissions_operations() {
    use super::super::{
        control_flags, read_frame, run_module_loop, write_frame, App, Executor, Frame, FrameType,
        ModuleControlRequest, QuiescedLsp, RouteTarget, HELLO_CORR,
    };
    use std::sync::Arc;
    use std::time::Duration;

    async fn receive(daemon: &mut tokio::io::DuplexStream, channel: u16, corr: u64) -> Frame {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let frame = read_frame(daemon).await.unwrap().unwrap();
                if frame.header.channel == channel && frame.header.corr == corr {
                    return frame;
                }
            }
        })
        .await
        .expect("permissions route response timed out")
    }

    async fn request(
        daemon: &mut tokio::io::DuplexStream,
        channel: u16,
        corr: u64,
        body: Value,
    ) -> Frame {
        let frame = Frame::build(
            FrameType::Request,
            control_flags(),
            channel,
            u32::from(channel != 0),
            corr,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        write_frame(daemon, &frame).await.unwrap();
        receive(daemon, channel, corr).await
    }

    fn error(frame: &Frame) -> ErrorBody {
        assert_eq!(frame.header.ty, FrameType::Error, "{frame:?}");
        serde_json::from_slice(&frame.body).unwrap()
    }

    fn result(frame: &Frame) -> Value {
        assert_eq!(frame.header.ty, FrameType::Response, "{frame:?}");
        serde_json::from_slice::<Value>(&frame.body).unwrap()["result"].clone()
    }

    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("module-storage");
    let app = App::default_shared();
    let executor = Arc::new(Executor::new());
    let module_app = app.clone();
    let module_executor = executor.clone();
    let (mut daemon, module) = tokio::io::duplex(64 * 1024);
    let module_storage = storage.clone();
    let module_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (read, write) = tokio::io::split(module);
        runtime.block_on(run_module_loop(
            read,
            write,
            &module_storage.join("absent-connection.json"),
            module_app,
            module_executor,
            |_, _| panic!("permissions routes must not execute commands"),
            None,
            false,
            usize::MAX,
            None,
            &module_storage,
            QuiescedLsp::ReapInBackground,
            None,
            None,
        ))
    });
    assert_eq!(
        receive(&mut daemon, 0, HELLO_CORR).await.header.ty,
        FrameType::Hello
    );
    write_frame(
        &mut daemon,
        &Frame::build(
            FrameType::HelloAck,
            control_flags(),
            0,
            0,
            HELLO_CORR,
            b"{}".to_vec(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    // Share the database the module opens itself: on macOS its startup opens
    // `<storage>/aft.db` (to withdraw earlier builds' macOS cards), and that
    // open replaces a handle installed under any other path. Opening the same
    // path returns the one shared handle, whichever side opens it first.
    std::fs::create_dir_all(&storage).unwrap();
    let db = app.open_db(&storage.join("aft.db")).unwrap();
    let folder = crate::access_policy::policy_path(dir.path()).join("notes");
    {
        let conn = db.lock().unwrap();
        store::register_scope(&conn, &head(), None, &Config::default()).unwrap();
        store::register_agent(&conn, &head(), "agent-alf", true).unwrap();
        store::insert_from_card(
            &conn,
            &head(),
            &grant(&folder, GrantKind::Subtree, AccessOperation::Read),
            Some("acc_1"),
        )
        .unwrap();
    }

    let bind = |channel: u16, principal: Option<Principal>| {
        serde_json::to_value(ModuleControlRequest::RouteBind {
            route_channel: channel,
            epoch: 1,
            target: RouteTarget::ManagementSurface {
                module_id: "aft".into(),
            },
            identity: subc_protocol::BindIdentity::new(
                "cingulate",
                format!("permissions:{channel}"),
            ),
            principal,
            consumer_capabilities: None,
            admission_facts: Default::default(),
            scope: None,
            role_versions: None,
            call_mode: None,
        })
        .unwrap()
    };

    let ack = request(&mut daemon, 0, 41, bind(41, cingulate())).await;
    assert_eq!(ack.header.ty, FrameType::Response, "{ack:?}");
    let listed = result(
        &request(
            &mut daemon,
            41,
            1,
            json!({"method": LIST_OPERATION, "params": {}}),
        )
        .await,
    );
    let rows = listed["grants"].as_array().unwrap();
    assert!(
        Arc::ptr_eq(&app.db().unwrap(), &db),
        "the handler reads the module's one shared database"
    );
    assert_eq!(rows.len(), 1, "{listed}");
    assert_contract_row(&rows[0]);
    let id = rows[0]["id"].clone();

    // The route admits nothing else, not even another management operation.
    for body in [
        json!({"op": "memory.census"}),
        json!({"method": "diagnostics.findings", "params": {}}),
        json!({"op": "tool.call", "name": "read", "arguments": {"path": "x"}}),
        json!({"method": "permissions.list_all", "params": {}}),
    ] {
        assert_eq!(
            error(&request(&mut daemon, 41, 2, body).await).code,
            "route_not_admitted"
        );
    }

    // A first-party route that is not the operator's is refused by name.
    let ack = request(
        &mut daemon,
        0,
        42,
        bind(
            42,
            Some(Principal::Reserved {
                module_id: "prefrontal-core".into(),
            }),
        ),
    )
    .await;
    assert_eq!(ack.header.ty, FrameType::Response, "{ack:?}");
    let refused = request(
        &mut daemon,
        42,
        3,
        json!({"method": REVOKE_OPERATION, "params": {"id": id}}),
    )
    .await;
    assert_eq!(error(&refused).code, "permissions_operator_required");

    let revoke_body = json!({"method": REVOKE_OPERATION, "params": {"id": id}});
    assert_eq!(
        result(&request(&mut daemon, 41, 4, revoke_body.clone()).await),
        json!({"outcome": "revoked"})
    );
    assert_eq!(
        result(&request(&mut daemon, 41, 5, revoke_body).await),
        json!({"outcome": "not_found"})
    );
    assert!(store::list(&db.lock().unwrap(), &head())
        .unwrap()
        .is_empty());

    drop(daemon);
    module_thread.join().unwrap().unwrap();
}
