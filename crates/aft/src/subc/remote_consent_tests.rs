use super::*;
use crate::subc::permissions::{self, RevokedGrant};
use crate::subc::test_support::{route_identity, test_root};

const TIMEOUT: Duration = Duration::from_secs(5);
const CALLER: &str = "remote-agent";
const PAIRING: &str = "pairing-A";

type Answer = oneshot::Sender<Value>;

struct Cingulate {
    cards: mpsc::UnboundedSender<(consent::ConsentCall, Answer)>,
    answers: StdMutex<HashMap<String, oneshot::Receiver<Value>>>,
}

impl consent::ConsentTransport for Cingulate {
    fn call(&self, call: consent::ConsentCall) -> consent::ConsentFuture {
        let (send, receive);
        match call.method {
            "consent.request" => {
                // The fake is strict about the requester contract, not just the
                // transport envelope. Extra fields would be rejected in service.
                let fields = call.params.as_object().unwrap();
                assert_eq!(fields.len(), 8);
                assert_eq!(fields["kind"], "tool_permission");
                assert!(matches!(
                    fields["operation"].as_str(),
                    Some("read" | "write")
                ));
                assert_eq!(fields["paths"], json!([call.project_root]));
                assert!(fields["args_hash"].is_string() && fields["tool"].is_string());
                assert!(fields["pattern"].is_string());
                let reason = fields["reason"].as_str().unwrap();
                assert!(!reason.trim().is_empty() && reason.chars().count() <= 160);
                assert!(!reason.contains(['\r', '\n', '\0', '\u{2028}', '\u{2029}']));
                let id = format!("acc_{}", fields["call_key"].as_str().unwrap());
                (send, receive) = oneshot::channel();
                assert!(self
                    .answers
                    .lock()
                    .unwrap()
                    .insert(id.clone(), receive)
                    .is_none());
                self.cards.send((call, send)).unwrap();
                Box::pin(async move { Ok(json!({"elicitation_id": id})) })
            }
            "consent.await" => {
                let id = call.params["elicitation_id"].as_str().unwrap();
                receive = self.answers.lock().unwrap().remove(id).unwrap();
                Box::pin(async move {
                    Ok(receive
                        .await
                        .unwrap_or(json!({"state":"refused:access_withdrawn"})))
                })
            }
            "consent.withdraw" => {
                Box::pin(async { Ok(json!({"state":"refused:access_withdrawn"})) })
            }
            method => panic!("unexpected request: {method}"),
        }
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: ProjectRootId,
    db: Db,
    registry: Arc<StdMutex<EnumerateReply>>,
    routes: HashMap<RouteChannel, RouteIdentity>,
    channels: HashMap<ProjectRootId, HashSet<RouteChannel>>,
    auth: Authorizer,
    cards: mpsc::UnboundedReceiver<(consent::ConsentCall, Answer)>,
}

impl Fixture {
    fn new() -> Self {
        let (dir, root) = test_root("remote-consent-folder");
        let db = Arc::new(StdMutex::new(
            crate::db::open(&dir.path().join("aft.db")).unwrap(),
        ));
        let mut auth = Authorizer::new(Some(db.clone()), dir.path().join("connection.json"));
        let (send, cards) = mpsc::unbounded_channel();
        auth.transport = Some(Arc::new(Cingulate {
            cards: send,
            answers: Default::default(),
        }));
        let registry = Arc::new(StdMutex::new(crate::subc::remote_calls::tests::registry(
            root.as_path(),
        )));
        let mut f = Self {
            _dir: dir,
            root: root.clone(),
            db,
            registry,
            routes: HashMap::new(),
            channels: HashMap::new(),
            auth,
            cards,
        };
        f.local(1, &root);
        f
    }

    fn local(&mut self, channel: u16, root: &ProjectRootId) {
        let channel = route_key(channel, 1);
        self.routes
            .insert(channel, route_identity(root, "local-proof-of-liveness"));
        self.channels
            .entry(root.clone())
            .or_default()
            .insert(channel);
    }

    fn call(&self, tool: &str, mode: Mode) -> Preflight {
        self.call_for(
            tool,
            mode,
            PAIRING,
            "remote-scope",
            json!({"kind":"agent", "agent_id":CALLER}),
        )
    }

    fn call_for(
        &self,
        tool: &str,
        mode: Mode,
        pairing: &str,
        scope_ref: &str,
        claim: Value,
    ) -> Preflight {
        let mut stamp = crate::subc::remote_calls::tests::stamp(claim);
        stamp["ref"] = json!(scope_ref);
        stamp["attributes"]["federated"]["pairing_id"] = json!(pairing);
        Preflight {
            scope: serde_json::from_value(stamp).unwrap(),
            session: "remote-caller-session".into(),
            folder: self.root.as_path().into(),
            mode,
            tool: tool.into(),
            args_hash: "abc123".into(),
        }
    }

    fn worker(&self, scope_ref: &str, mode: Mode) -> Preflight {
        self.call_for(if mode == Mode::Read {"read"} else {"edit"}, mode, PAIRING, scope_ref, json!({
            "kind":"worker", "owner_module_id":"worker-owner", "worker_ref":"c".repeat(64), "on_behalf_of":CALLER,
        }))
    }

    async fn request(&mut self, call: Preflight) -> oneshot::Receiver<Decision> {
        let registry = self.registry.clone();
        TEST_REGISTRY
            .scope(Arc::new(move || registry.lock().unwrap().clone()), async {
                self.auth.request(call)
            })
            .await
    }

    async fn card(&mut self) -> (consent::ConsentCall, Answer) {
        tokio::time::timeout(TIMEOUT, self.cards.recv())
            .await
            .expect("expected a new card")
            .unwrap()
    }

    async fn finish(&mut self) {
        let completion = tokio::time::timeout(TIMEOUT, self.auth.next())
            .await
            .expect("expected a completed request");
        let registry = self.registry.clone();
        TEST_REGISTRY
            .scope(Arc::new(move || registry.lock().unwrap().clone()), async {
                self.auth.finish(completion, &self.channels, &self.routes);
            })
            .await;
    }

    fn list(&self, agent: Option<&str>) -> Vec<Value> {
        permissions::list(&self.db.lock().unwrap(), agent, &[]).unwrap()["grants"]
            .as_array()
            .unwrap()
            .clone()
    }
}

fn allow(answer: Answer, save: bool) {
    answer.send(json!({"state":"settled","answered_by":"user", "outcome": if save {"allow_scope"} else {"allow_once"}})).unwrap();
}

async fn authority(result: oneshot::Receiver<Decision>) -> Authorization {
    result
        .await
        .unwrap()
        .unwrap_or_else(|error| panic!("{}: {}", error.code, error.message))
        .1
}

async fn refusal(result: oneshot::Receiver<Decision>) -> ErrorBody {
    match result.await.unwrap() {
        Err(error) => error,
        Ok(_) => panic!("unauthorized call was released"),
    }
}

#[tokio::test]
async fn non_user_answer_denies_all_waiters_and_fresh_card_uses_remote_scope_and_agent_id() {
    let mut f = Fixture::new();
    let first = f.call("read", Mode::Read);
    let asking_scope = consent::selector(&first.scope);
    let mut one = f.request(first).await;
    let (card, answer) = f.card().await;
    let first_key = card.params["call_key"].clone();
    assert_eq!(card.scope, asking_scope);
    assert_eq!(card.session, "remote-caller-session");
    assert_eq!(card.project_root, f.root.as_path());
    assert_eq!(
        card.params["pattern"],
        format!("{}/**", f.root.as_path().display())
    );
    assert_eq!(
        card.params["reason"],
        format!(
            "{CALLER} on Computer A wants to read files in {}.",
            f.root.as_path().file_name().unwrap().to_string_lossy()
        )
    );
    assert_eq!(card.params["operation"], "read");
    assert_eq!(card.params["tool"], "read");
    assert!(card.params.get("head_policy").is_none());
    let mut two = f.request(f.call("grep", Mode::Read)).await;
    assert!(f.cards.try_recv().is_err());
    assert!(matches!(
        one.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        two.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    answer
        .send(json!({"state":"settled","answered_by":"head","outcome":"allow_scope"}))
        .unwrap();
    f.finish().await;
    for result in [one, two] {
        assert_eq!(
            refusal(result).await.message,
            "Denied by the person on this computer."
        );
    }
    assert!(f.list(None).is_empty());
    let three = f.request(f.call("read", Mode::Read)).await;
    let (card, answer) = f.card().await;
    assert_ne!(card.params["call_key"], first_key);
    allow(answer, false);
    f.finish().await;
    assert_eq!(authority(three).await, Authorization::Once);
}

#[tokio::test]
async fn daemon_edge_allow_once_stays_with_cancelled_card_owner() {
    let mut f = Fixture::new();
    let one = f.request(f.call("read", Mode::Read)).await;
    let (first, answer) = f.card().await;
    let mut two = f.request(f.call("grep", Mode::Read)).await;
    drop(one);

    allow(answer, false);
    f.finish().await;
    assert!(
        matches!(two.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
        "the first card must not authorize the queued call"
    );

    let (second, answer) = f.card().await;
    assert_eq!(second.params["tool"], "grep");
    assert_ne!(second.params["call_key"], first.params["call_key"]);
    allow(answer, false);
    f.finish().await;
    assert_eq!(authority(two).await, Authorization::Once);
}

#[tokio::test]
async fn allow_once_is_fifo_then_always_allow_lists_filters_upgrades_and_revokes() {
    let mut f = Fixture::new();
    let one = f.request(f.call("read", Mode::Read)).await;
    let mut two = f.request(f.call("grep", Mode::Read)).await;
    let (first, answer) = f.card().await;
    allow(answer, false);
    f.finish().await;
    assert_eq!(authority(one).await, Authorization::Once);
    assert!(matches!(
        two.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    let (second, answer) = f.card().await;
    assert_ne!(second.params["call_key"], first.params["call_key"]);
    assert_eq!(second.params["tool"], "grep");
    let three = f.request(f.call("glob", Mode::Read)).await;
    allow(answer, true);
    f.finish().await;
    let saved = authority(two).await;
    assert_eq!(authority(three).await, saved);
    let Authorization::Saved(id) = saved else {
        panic!("not saved")
    };
    let rows = f.list(Some(CALLER));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["holder"], json!({"kind":"agent","agent_id":CALLER}));
    assert_eq!(
        rows[0]["subject_label"],
        f.root.as_path().display().to_string()
    );
    assert_eq!(rows[0]["scope"], "read");
    assert_eq!(rows[0]["scope_label"], "remote-agent on Computer A");
    assert!(f.list(Some("different-agent")).is_empty());

    let edit = f.request(f.call("edit", Mode::ReadWrite)).await;
    let (card, answer) = f.card().await;
    assert_eq!(card.params["operation"], "write");
    assert!(card.params["reason"]
        .as_str()
        .unwrap()
        .contains("wants to read and edit files in"));
    answer
        .send(json!({"state":"settled","answered_by":"user","outcome":"deny"}))
        .unwrap();
    f.finish().await;
    assert_eq!(refusal(edit).await.code, "remote_access_denied");
    assert_eq!(f.list(None)[0]["scope"], "read");
    let bash = f.request(f.call("bash", Mode::ReadWrite)).await;
    let (card, answer) = f.card().await;
    assert_eq!(card.params["operation"], "write");
    assert!(card.params.get("command").is_none());
    allow(answer, true);
    f.finish().await;
    assert_eq!(authority(bash).await, Authorization::Saved(id.clone()));
    assert_eq!(f.list(None).len(), 1);
    assert_eq!(f.list(None)[0]["scope"], "read_write");
    let read = f.request(f.call("read", Mode::Read)).await;
    f.finish().await;
    assert_eq!(authority(read).await, Authorization::Saved(id.clone()));
    assert!(f.cards.try_recv().is_err());

    let request = permissions::admit(
        Some(&Principal::Direct),
        &json!({"method":"permissions.revoke", "params":{"id":id}}),
    )
    .unwrap();
    let revoked = permissions::answer(Some(&f.db), &request, |grant| {
        assert!(matches!(grant, RevokedGrant::Remote(revoked) if revoked == id));
        assert!(
            f.list(None).is_empty(),
            "grant deleted before stop callback"
        );
        1
    })
    .unwrap();
    assert_eq!(revoked, json!({"outcome":"revoked", "stopping":true}));
    let _next = f.request(f.call("read", Mode::Read)).await;
    let _ = f.card().await;
}

#[tokio::test]
async fn approval_rechecks_mapping_and_local_binds_before_saving() {
    let mut f = Fixture::new();
    let pending = f.request(f.call("edit", Mode::ReadWrite)).await;
    let (_, answer) = f.card().await;
    let channel = route_key(1, 1);
    f.routes.remove(&channel);
    f.channels.get_mut(&f.root).unwrap().remove(&channel);
    // A remaining remote channel at the root cannot prove local liveness.
    let remote = route_key(2, 1);
    let mut route = route_identity(&f.root, "still-remote");
    Arc::get_mut(&mut route.0).unwrap().harness = "fed:remote-only".into();
    f.routes.insert(remote, route);
    f.channels.get_mut(&f.root).unwrap().insert(remote);
    allow(answer, true);
    f.finish().await;
    assert_eq!(refusal(pending).await.code, "remote_folder_not_working");
    assert!(f.list(None).is_empty());

    f.local(1, &f.root.clone());
    let pending = f.request(f.call("edit", Mode::ReadWrite)).await;
    let (old, answer) = f.card().await;
    let (_other_dir, other) = test_root("remote-mapped-folder");
    f.local(3, &other);
    *f.registry.lock().unwrap() = crate::subc::remote_calls::tests::registry(other.as_path());
    allow(answer, true);
    f.finish().await;
    assert_eq!(refusal(pending).await.code, "remote_folder_not_registered");
    assert!(f.list(None).is_empty());
    f.root = other;
    let next = f.request(f.call("edit", Mode::ReadWrite)).await;
    let (new, answer) = f.card().await;
    assert_ne!(old.params["call_key"], new.params["call_key"]);
    assert_eq!(new.project_root, f.root.as_path());
    allow(answer, false);
    f.finish().await;
    assert_eq!(authority(next).await, Authorization::Once);
}

#[tokio::test]
async fn remapped_write_grant_covers_nothing_until_replaced_by_a_new_read_answer() {
    let mut f = Fixture::new();
    let write = f.request(f.call("edit", Mode::ReadWrite)).await;
    let (_, answer) = f.card().await;
    allow(answer, true);
    f.finish().await;
    let old_grant = authority(write).await;
    let (_other_dir, other) = test_root("remote-remapped-folder");
    f.local(2, &other);
    *f.registry.lock().unwrap() = crate::subc::remote_calls::tests::registry(other.as_path());
    f.root = other;
    let mut read = f.request(f.call("read", Mode::Read)).await;
    let (card, answer) = f.card().await;
    assert_eq!(card.project_root, f.root.as_path());
    assert_eq!(card.params["operation"], "read");
    assert!(matches!(
        read.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    allow(answer, true);
    f.finish().await;
    assert_ne!(authority(read).await, old_grant);
    let rows = f.list(None);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["scope"], "read");
    assert_eq!(
        rows[0]["subject_label"],
        f.root.as_path().display().to_string()
    );
    let _edit = f.request(f.call("edit", Mode::ReadWrite)).await;
    assert_eq!(f.card().await.0.params["operation"], "write");
}

#[tokio::test]
async fn worker_runs_dont_inherit_or_save_grants_and_expire_with_their_stamp() {
    let mut f = Fixture::new();
    let saved = f.request(f.call("edit", Mode::ReadWrite)).await;
    let (_, answer) = f.card().await;
    allow(answer, true);
    f.finish().await;
    assert!(matches!(authority(saved).await, Authorization::Saved(_)));
    let rows = f.list(None);

    let a1 = f.request(f.worker("worker-a", Mode::Read)).await;
    let a2 = f.request(f.worker("worker-a", Mode::Read)).await;
    let (a, answer_a) = f.card().await;
    let b = f.request(f.worker("worker-b", Mode::Read)).await;
    let (other, answer_b) = f.card().await;
    assert_ne!(a.params["call_key"], other.params["call_key"]);
    assert_ne!(a.scope, other.scope);
    allow(answer_a, false);
    f.finish().await;
    assert_eq!(authority(a1).await, Authorization::WorkerRun);
    assert_eq!(authority(a2).await, Authorization::WorkerRun);
    let again = f.request(f.worker("worker-a", Mode::Read)).await;
    assert_eq!(authority(again).await, Authorization::WorkerRun);
    assert!(f.cards.try_recv().is_err());
    allow(answer_b, true);
    f.finish().await;
    assert_eq!(authority(b).await, Authorization::WorkerRun);

    let edit = f.worker("worker-a", Mode::ReadWrite);
    let scope = edit.scope.clone();
    let waiting = f.request(edit).await;
    let (upgrade, answer) = f.card().await;
    assert_eq!(upgrade.scope, a.scope);
    assert_eq!(upgrade.params["operation"], "write");
    f.auth.end_scope(PAIRING, &scope);
    let _ = answer.send(json!({"state":"settled","answered_by":"user","outcome":"allow_scope"}));
    assert_eq!(refusal(waiting).await.code, "remote_access_denied");
    f.finish().await;
    assert_eq!(f.list(None), rows);
    let next_scope = f.request(f.worker("worker-new", Mode::Read)).await;
    let (_, answer) = f.card().await;
    allow(answer, true);
    f.finish().await;
    assert_eq!(authority(next_scope).await, Authorization::WorkerRun);
    assert_eq!(f.list(None), rows);
}

#[tokio::test]
async fn pairings_with_equal_scope_tuples_get_separate_cards_and_bounded_reasons() {
    let mut f = Fixture::new();
    let (_dir, long_root) = test_root(&"x".repeat(180));
    f.local(7, &long_root);
    *f.registry.lock().unwrap() = crate::subc::remote_calls::tests::registry(long_root.as_path());
    f.root = long_root;
    let a = f.call("read", Mode::Read);
    let mut b = f.call_for(
        "read",
        Mode::Read,
        "pairing-B",
        "remote-scope",
        json!({
            "kind":"agent", "agent_id":CALLER, "agent_name": "ä".repeat(80),
        }),
    );
    b.scope.attributes.federated.as_mut().unwrap().peer_name = "λ".repeat(80);
    let first = f.request(a).await;
    let (first_card, first_answer) = f.card().await;
    let second = f.request(b).await;
    let (second_card, second_answer) = f.card().await;
    assert_eq!(first_card.scope, second_card.scope);
    assert_ne!(
        first_card.params["call_key"],
        second_card.params["call_key"]
    );
    let message = second_card.params["reason"].as_str().unwrap();
    assert_eq!(message.chars().count(), 160);
    assert!(message.starts_with("… on … wants to read files in "));
    allow(first_answer, true);
    f.finish().await;
    assert!(matches!(authority(first).await, Authorization::Saved(_)));
    allow(second_answer, false);
    f.finish().await;
    assert_eq!(authority(second).await, Authorization::Once);
    assert_eq!(f.list(None).len(), 1);
    assert_eq!(display("a\nb\u{2028}c\t"), "a b c ");
}
