#![cfg(unix)]

use super::*;
use expiry::{Sweep, IDLE_MS};
use std::task::Poll;

const BUDGET: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Pause {
    entered: Notify,
    resume: Notify,
}

#[derive(Default)]
struct Controls {
    wait: Option<Pause>,
    withdraw: Option<Pause>,
    lose_withdraw_reply: bool,
    failed: AtomicBool,
    attempted: StdMutex<Vec<String>>,
    awaited: StdMutex<Vec<String>>,
}

struct Endpoint(Arc<FakeCingulate>, Arc<Controls>);

impl ConsentTransport for Endpoint {
    fn call(&self, call: ConsentCall) -> ConsentFuture {
        if call.method != "consent.await" {
            return SharedFake(Arc::clone(&self.0)).call(call);
        }
        let fake = Arc::clone(&self.0);
        let controls = Arc::clone(&self.1);
        Box::pin(async move {
            controls
                .awaited
                .lock()
                .unwrap()
                .push(call.params["elicitation_id"].as_str().unwrap().into());
            if let Some(pause) = &controls.wait {
                pause.entered.notify_one();
                pause.resume.notified().await;
            }
            Ok(fake
                .reply(call.method, &call.params)?
                .unwrap_or_else(|| json!({"state":"pending:head"})))
        })
    }

    fn withdraw_idle(&self, id: String, _timeout: Duration) -> ConsentFuture {
        let fake = Arc::clone(&self.0);
        let controls = Arc::clone(&self.1);
        Box::pin(async move {
            controls.attempted.lock().unwrap().push(id.clone());
            if let Some(pause) = &controls.withdraw {
                pause.entered.notify_one();
                pause.resume.notified().await;
            }
            if controls.failed.load(Ordering::SeqCst) {
                return Err(ConsentError::Unavailable("test withdrawal refused".into()));
            }
            let reply = fake
                .reply("consent.withdraw", &json!({"elicitation_id":id}))?
                .unwrap();
            if controls.lose_withdraw_reply {
                std::future::pending::<()>().await;
            }
            Ok(reply)
        })
    }
}

fn setup(controls: Controls) -> (Fixture, Arc<Endpoint>) {
    let fake = FakeCingulate::up();
    let f = fixture(Arc::clone(&fake));
    let endpoint = Arc::new(Endpoint(fake, Arc::new(controls)));
    install_test_transport(Some(endpoint.clone()));
    (f, endpoint)
}

fn file(path: &Path) -> AccessGrant {
    AccessGrant {
        path: path.to_path_buf(),
        kind: GrantKind::File,
        operation: AccessOperation::Read,
    }
}

async fn park_call(f: &Fixture) {
    let target = f.head_root.join("shared.txt");
    let call = scope_probe_call(f, &target);
    let owner = SettlementOwner::new(&call);
    let result = hold(
        &call,
        Permission::File(file(&target)),
        Instant::now(),
        &RouteHold::new(call.route),
        None,
        &owner.0,
    )
    .await;
    match result {
        Hold::Answer(reply) => assert_eq!(reply.data["code"], "access_pending", "{}", reply.data),
        _ => panic!("an unanswered call must return pending"),
    }
}

fn last_use(f: &Fixture) -> i64 {
    f.ctx
        .db()
        .unwrap()
        .lock()
        .unwrap()
        .query_row("SELECT created_at FROM access_asks", [], |row| row.get(0))
        .unwrap()
}

fn set_last_use(f: &Fixture, time: i64) {
    f.ctx
        .db()
        .unwrap()
        .lock()
        .unwrap()
        .execute("UPDATE access_asks SET created_at=?1", [time])
        .unwrap();
}

async fn sweep(f: &Fixture, endpoint: &Endpoint, time: i64) {
    Sweep::default()
        .run(
            &f.ctx.db().unwrap(),
            Some(endpoint),
            32,
            BUDGET,
            time,
            Instant::now,
        )
        .await
        .unwrap();
}

fn seed(f: &Fixture, id: &str, permission: Permission, elicitation: Option<&str>) {
    let key = AskKey {
        scope: access_grants::scope_key(&f.worker),
        args_hash: id.into(),
    };
    let db = f.ctx.db().unwrap();
    let db = db.lock().unwrap();
    asks::open_permission(&db, &key, id, &json!({}), &json!({}), &permission, 1).unwrap();
    if let Some(elicitation) = elicitation {
        asks::set_elicitation(&db, &key, id, elicitation).unwrap();
    }
}

#[tokio::test]
async fn consent_expiry_withdraws_an_idle_card_once_and_retry_opens_a_fresh_card() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let (f, endpoint) = setup(Controls::default());
    park_call(&f).await;
    let old = f.asks().pop().unwrap();
    let aged = last_use(&f) + IDLE_MS;
    sweep(&f, &endpoint, aged).await;
    assert_eq!(f.asks().len(), 1, "exactly 30 minutes is not yet expired");
    sweep(&f, &endpoint, aged + 1).await;
    sweep(&f, &endpoint, aged + 2).await;
    assert!(f.asks().is_empty());
    assert_eq!(
        f.fake.calls("consent.withdraw"),
        [json!({"elicitation_id":old.elicitation_id})]
    );
    park_call(&f).await;
    let fresh = f.asks().pop().unwrap();
    assert_ne!(fresh.elicitation_id, old.elicitation_id);
    assert_ne!(fresh.call_key, old.call_key);
}

#[tokio::test]
async fn consent_expiry_does_not_withdraw_a_card_with_an_active_waiter() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let (f, endpoint) = setup(Controls {
        wait: Some(Pause::default()),
        ..Controls::default()
    });
    let target = f.head_root.join("shared.txt");
    let mut replies = f.send_read(&f.worker, &target, 1).await;
    let pause = endpoint.1.wait.as_ref().unwrap();
    tokio::time::timeout(BUDGET, pause.entered.notified())
        .await
        .unwrap();
    let old = f.asks().pop().unwrap();
    sweep(&f, &endpoint, last_use(&f) + IDLE_MS + 1).await;
    assert!(endpoint.1.attempted.lock().unwrap().is_empty());
    assert_eq!(f.asks(), [old.clone()]);
    f.fake.answer(
        old.call_key.as_deref().unwrap(),
        json!({"state":"settled","outcome":"allow_once"}),
    );
    pause.resume.notify_one();
    let frame = tokio::time::timeout(BUDGET, replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(text(&serde_json::from_slice(&frame.body).unwrap()).contains("head-owned contents"));
}

#[tokio::test]
async fn consent_expiry_identical_retry_refreshes_the_idle_window() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let (f, endpoint) = setup(Controls::default());
    park_call(&f).await;
    let old = f.asks().pop().unwrap();
    let first = last_use(&f) - IDLE_MS / 2;
    set_last_use(&f, first);
    park_call(&f).await;
    assert_eq!(f.asks(), [old]);
    assert!(last_use(&f) > first);
    sweep(&f, &endpoint, first + IDLE_MS + 1).await;
    assert_eq!(f.asks().len(), 1);
    assert!(f.fake.calls("consent.withdraw").is_empty());
    sweep(&f, &endpoint, last_use(&f) + IDLE_MS + 1).await;
    assert!(f.asks().is_empty());
    assert_eq!(f.fake.calls("consent.withdraw").len(), 1);
}

#[tokio::test]
async fn consent_expiry_deletes_unpublished_backlog_without_touching_shell_asks() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let (f, endpoint) = setup(Controls::default());
    seed(&f, "file", Permission::File(file(&f.head_root)), None);
    seed(
        &f,
        "shell",
        Permission::Shell {
            command: "ls".into(),
            cwd: f.head_root.clone(),
            permissions: vec![],
        },
        Some("acc_shell"),
    );
    sweep(&f, &endpoint, IDLE_MS + 2).await;
    assert_eq!(
        f.asks()
            .into_iter()
            .map(|row| row.key.args_hash)
            .collect::<Vec<_>>(),
        ["shell"]
    );
    for method in ["consent.request", "consent.await", "consent.withdraw"] {
        assert!(f.fake.calls(method).is_empty());
    }
    assert!(endpoint.1.attempted.lock().unwrap().is_empty());
}

#[tokio::test]
async fn consent_expiry_failed_withdrawal_keeps_the_row_for_the_next_sweep() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let (f, endpoint) = setup(Controls::default());
    park_call(&f).await;
    let old = f.asks().pop().unwrap();
    let aged = last_use(&f) + IDLE_MS + 1;
    let mut worker = Sweep::default();
    endpoint.1.failed.store(true, Ordering::SeqCst);
    worker
        .run(
            &f.ctx.db().unwrap(),
            Some(&*endpoint),
            32,
            BUDGET,
            aged,
            Instant::now,
        )
        .await
        .unwrap();
    assert_eq!(f.asks(), [old]);
    endpoint.1.failed.store(false, Ordering::SeqCst);
    worker
        .run(
            &f.ctx.db().unwrap(),
            Some(&*endpoint),
            32,
            BUDGET,
            aged,
            Instant::now,
        )
        .await
        .unwrap();
    assert!(f.asks().is_empty());
    assert_eq!(endpoint.1.attempted.lock().unwrap().len(), 2);
    assert_eq!(f.fake.calls("consent.withdraw").len(), 1);
}

#[tokio::test]
async fn consent_expiry_respects_the_query_limit_and_checks_the_deadline_between_rows() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let (f, endpoint) = setup(Controls::default());
    for i in 0..4 {
        seed(
            &f,
            &format!("file-{i}"),
            Permission::File(file(&f.head_root)),
            Some(&format!("acc_{i}")),
        );
    }
    let mut worker = Sweep::default();
    let db = f.ctx.db().unwrap();
    worker
        .run(&db, Some(&*endpoint), 2, BUDGET, IDLE_MS + 2, Instant::now)
        .await
        .unwrap();
    assert_eq!(f.asks().len(), 2);
    let start = Instant::now();
    let now = || {
        if endpoint.1.attempted.lock().unwrap().len() >= 3 {
            start + BUDGET
        } else {
            start
        }
    };
    worker
        .run(&db, Some(&*endpoint), 2, BUDGET, IDLE_MS + 2, now)
        .await
        .unwrap();
    assert_eq!(
        f.asks().len(),
        1,
        "the deadline expired after the first RPC of this page"
    );
    worker
        .run(&db, Some(&*endpoint), 2, BUDGET, IDLE_MS + 2, Instant::now)
        .await
        .unwrap();
    assert!(f.asks().is_empty());
    assert_eq!(endpoint.1.attempted.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn consent_expiry_retry_racing_the_sweep_never_awaits_the_withdrawn_card() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let (f, endpoint) = setup(Controls {
        withdraw: Some(Pause::default()),
        ..Controls::default()
    });
    park_call(&f).await;
    let old = f.asks()[0].elicitation_id.clone().unwrap();
    let aged = last_use(&f) + IDLE_MS + 1;
    let db = f.ctx.db().unwrap();
    let transport = Arc::clone(&endpoint);
    let worker = tokio::spawn(async move {
        Sweep::default()
            .run(&db, Some(&*transport), 32, BUDGET, aged, Instant::now)
            .await
            .unwrap();
    });
    let pause = endpoint.1.withdraw.as_ref().unwrap();
    tokio::time::timeout(BUDGET, pause.entered.notified())
        .await
        .unwrap();
    let mut retry = Box::pin(park_call(&f));
    std::future::poll_fn(|cx| {
        assert!(retry.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    pause.resume.notify_one();
    worker.await.unwrap();
    retry.await;
    let fresh = f.asks()[0].elicitation_id.clone().unwrap();
    assert_ne!(fresh, old);
    assert_eq!(*endpoint.1.awaited.lock().unwrap(), [old, fresh]);
    assert_eq!(f.fake.calls("consent.withdraw").len(), 1);
}

#[tokio::test]
async fn consent_expiry_lost_withdrawal_reply_refiles_and_honors_the_fresh_answer() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let (f, endpoint) = setup(Controls {
        lose_withdraw_reply: true,
        ..Controls::default()
    });
    park_call(&f).await;
    let old = f.asks().pop().unwrap();
    let now = Instant::now();
    tokio::time::pause();
    Sweep::default()
        .run(
            &f.ctx.db().unwrap(),
            Some(&*endpoint),
            32,
            BUDGET,
            last_use(&f) + IDLE_MS + 1,
            || now,
        )
        .await
        .unwrap();
    tokio::time::resume();
    assert_eq!(
        f.fake.calls("consent.withdraw").len(),
        1,
        "the remove reached Cingulate"
    );
    assert_eq!(
        f.asks(),
        [old.clone()],
        "the sweep did not receive its reply"
    );

    let pause = Arc::new(Controls {
        wait: Some(Pause::default()),
        ..Controls::default()
    });
    let endpoint = Arc::new(Endpoint(Arc::clone(&f.fake), Arc::clone(&pause)));
    install_test_transport(Some(endpoint));
    let pause = pause.wait.as_ref().unwrap();
    let target = f.head_root.join("shared.txt");
    let arguments = scope_probe_call(&f, &target).arguments;
    let mut replies = f
        .send_tool(&f.worker, "read", arguments, 1, read_dispatch)
        .await;
    tokio::time::timeout(BUDGET, pause.entered.notified())
        .await
        .unwrap();
    assert_eq!(
        f.asks(),
        [old.clone()],
        "first the retry picks up the old card"
    );
    pause.resume.notify_one();
    tokio::time::timeout(BUDGET, pause.entered.notified())
        .await
        .unwrap();
    let [new]: [asks::AskRecord; 1] = f
        .asks()
        .try_into()
        .expect("only the replacement card remains");
    assert_eq!(new.key, old.key);
    assert_ne!(new.call_key, old.call_key);
    assert_ne!(new.elicitation_id, old.elicitation_id);
    f.fake.answer(
        new.call_key.as_deref().unwrap(),
        json!({"state":"settled","outcome":"allow_once"}),
    );
    pause.resume.notify_one();
    let reply = tokio::time::timeout(BUDGET, replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(text(&serde_json::from_slice(&reply.body).unwrap()).contains("head-owned contents"));
    assert!(f.asks().is_empty());
    assert_eq!(
        f.fake
            .calls("consent.request")
            .iter()
            .map(|request| request["call_key"].clone())
            .collect::<Vec<_>>(),
        [
            json!(old.call_key),
            json!(old.call_key),
            json!(new.call_key)
        ]
    );
}
