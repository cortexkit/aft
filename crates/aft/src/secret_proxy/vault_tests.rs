//! Real proxy requests using routes authenticated as AFT's module identity.
//! A consent transport double records phone cards and supplies allow or decline.

use super::*;
use crate::secret::transport::fake::{FakeClaustrum, Record, NONCE};
use crate::secret::{
    ConsentOutcome, SecretConsentRequest, SecretConsentTransport, SubcSecretVault,
};
use std::sync::atomic::AtomicU64;

struct Clock {
    now: Arc<AtomicU64>,
    ticks: tokio::sync::watch::Sender<u64>,
}

impl Clock {
    fn new() -> Arc<Self> {
        let (ticks, _) = tokio::sync::watch::channel(1_700_000_000_000);
        Arc::new(Self {
            now: Arc::new(AtomicU64::new(1_700_000_000_000)),
            ticks,
        })
    }
    fn advance(&self, ms: u64) {
        let now = self.now.fetch_add(ms, Ordering::SeqCst) + ms;
        self.ticks.send_replace(now);
    }
}

impl crate::secret::requests::ApprovalClock for Clock {
    fn now_ms(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }
    fn deadline(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let end = self.now.load(Ordering::SeqCst) + 600_000;
        let mut ticks = self.ticks.subscribe();
        Box::pin(async move {
            while *ticks.borrow_and_update() < end {
                if ticks.changed().await.is_err() {
                    break;
                }
            }
        })
    }
}

#[derive(Default)]
struct PhoneState {
    requests: Vec<SecretConsentRequest>,
    answers: HashMap<String, ConsentOutcome>,
    closed: bool,
    withdrawn: Vec<String>,
}

struct Phone {
    runtime: tokio::runtime::Handle,
    state: std::sync::Mutex<PhoneState>,
    answer: std::sync::Condvar,
    filed: tokio::sync::Notify,
}

impl Phone {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            runtime: tokio::runtime::Handle::current(),
            state: Default::default(),
            answer: Default::default(),
            filed: Default::default(),
        })
    }
    async fn card(&self, count: usize) -> SecretConsentRequest {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let notified = self.filed.notified();
                if let Some(request) = self.state.lock().unwrap().requests.get(count - 1).cloned() {
                    return request;
                }
                notified.await;
            }
        })
        .await
        .expect("the proxy filed the expected card")
    }
    fn decide(&self, daemon: &FakeClaustrum, count: usize, answer: ConsentOutcome) {
        let mut state = self.state.lock().unwrap();
        let request = &state.requests[count - 1];
        if matches!(
            answer,
            ConsentOutcome::Answered | ConsentOutcome::AnsweredOnce
        ) {
            daemon
                .state
                .lock()
                .unwrap()
                .approve(&request.statement_b64url);
        }
        let id = request.request_id.clone();
        state.answers.insert(id, answer);
        self.answer.notify_all();
    }
    fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.answer.notify_all();
    }
}

impl SecretConsentTransport for Phone {
    fn runtime(&self) -> Option<tokio::runtime::Handle> {
        Some(self.runtime.clone())
    }
    fn file(&self, request: &SecretConsentRequest) -> ConsentOutcome {
        self.hold(request)
    }
    fn hold(&self, request: &SecretConsentRequest) -> ConsentOutcome {
        let mut state = self.state.lock().unwrap();
        assert!(
            !state
                .requests
                .iter()
                .any(|r| r.request_id == request.request_id),
            "never file the same vault id twice"
        );
        state.requests.push(request.clone());
        self.filed.notify_one();
        loop {
            if state.closed {
                return ConsentOutcome::Expired;
            }
            if let Some(answer) = state.answers.get(&request.request_id) {
                return answer.clone();
            }
            state = self.answer.wait(state).unwrap();
        }
    }
    fn withhold(&self, request: &SecretConsentRequest) -> ConsentOutcome {
        let mut state = self.state.lock().unwrap();
        state.withdrawn.push(request.request_id.clone());
        state
            .answers
            .insert(request.request_id.clone(), ConsentOutcome::Expired);
        self.answer.notify_all();
        ConsentOutcome::Expired
    }
}

struct PhoneGuard {
    phone: Arc<Phone>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl PhoneGuard {
    fn install() -> Self {
        let lock = crate::secret::consent::TEST_TRANSPORT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let phone = Phone::new();
        crate::secret::consent::install_transport(Some(phone.clone()));
        Self { phone, _lock: lock }
    }
}
impl Drop for PhoneGuard {
    fn drop(&mut self) {
        self.phone.close();
        crate::secret::consent::install_transport(None);
    }
}

fn record(location: &str) -> Record {
    Record {
        name: "test".into(),
        host: "localhost".into(),
        path: "/approved".into(),
        location: location.into(),
        key: K1.to_vec(),
    }
}

fn live_config(daemon: &FakeClaustrum, port: u16, clock: Arc<Clock>) -> ProxyConfig {
    static SEQUENCE: AtomicU64 = AtomicU64::new(1);
    let auth = scope(&format!(
        "live-vault-{}",
        SEQUENCE.fetch_add(1, Ordering::SeqCst)
    ));
    let transport = crate::secret::transport::SubcSecretUseTransport::for_test(
        daemon.path.clone(),
        subc_protocol::BindIdentity::new("test", "session"),
        Duration::from_secs(2),
        subc_client_rs::ConsumerIdentity {
            module_id: "aft".into(),
            launch_nonce: NONCE.into(),
        },
    );
    ProxyConfig {
        vault: Arc::new(SubcSecretVault::new(Arc::new(transport))),
        authorization_scope: auth,
        lifetime_scope: scope("run"),
        scanned_labels: vec!["test".into()],
        consent_context: Some((std::path::PathBuf::from("project"), "session".into())),
        test_https_port: Some(port),
        approval_clock: Some(clock),
        origin: "live-proxy-task".into(),
    }
}

fn request_in_thread(
    mut client: Client,
    overlap: bool,
) -> std::thread::JoinHandle<(Value, Client)> {
    std::thread::spawn(move || {
        let reply = if overlap {
            client.command(json!({"op":"overlap"}))
        } else {
            client.request(
                "/approved",
                json!({"authorization":format!("Bearer {MARKER}")}),
                "",
            )
        };
        (reply, client)
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_vault_release_injects_only_the_deposit_location_and_every_use_is_audited() {
    let daemon = FakeClaustrum::start();
    daemon.state.lock().unwrap().record = Some(record("authorization_bearer"));
    let phone = PhoneGuard::install();
    let server = Server::new("echo-request").await;
    let cfg = live_config(&daemon, server.address.port(), Clock::new());
    let expected_scope = cfg.authorization_scope.clone();
    let proxy = SecretProxy::start_with_tls(cfg, server.client_tls.clone()).unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(false);
    let worker = request_in_thread(client, false);
    let card = phone.phone.card(1).await;
    assert_eq!(card.scope, expected_scope);
    assert!(
        server.seen.lock().is_empty(),
        "approval holds before any upstream request"
    );
    assert_eq!(proxy.approval_status(), vec!["secret test: waiting for approval on the phone for localhost (requests wait up to 10 minutes)"]);
    phone.phone.decide(&daemon, 1, ConsentOutcome::Answered);
    let (reply, mut client) = worker.join().unwrap();
    assert_eq!(reply["status"], 200);
    assert!(!reply.to_string().contains(std::str::from_utf8(K1).unwrap()));
    let next = client.request(
        &format!("/approved/next?key={MARKER}"),
        json!({"authorization":format!("Bearer {MARKER}"), "x-literal":MARKER}),
        "",
    );
    assert_eq!(next["status"], 200);
    let seen = server.seen.lock();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[1]["headers"]["authorization"],
        format!("Bearer {}", std::str::from_utf8(K1).unwrap())
    );
    assert_eq!(seen[1]["headers"]["x-literal"], MARKER);
    assert!(seen[1]["target"].as_str().unwrap().contains(MARKER));
    let state = daemon.state.lock().unwrap();
    assert_eq!(state.calls, 3, "initial ask, approved retry, next request");
    assert_eq!(
        state.connections, 1,
        "one command owns one reused daemon consumer"
    );
    assert_eq!(
        state.params[2]["target"],
        json!({"scheme":"https", "host":"localhost", "port":443, "path":"/approved/next"})
    );
    assert_eq!(phone.phone.state.lock().unwrap().requests.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_vault_decline_and_expiry_answer_403_without_sending_or_retrying() {
    for decision in [ConsentOutcome::Declined, ConsentOutcome::Expired] {
        let daemon = FakeClaustrum::start();
        daemon.state.lock().unwrap().record = Some(record("authorization_bearer"));
        let phone = PhoneGuard::install();
        let server = Server::new("echo-request").await;
        let proxy = SecretProxy::start_with_tls(
            live_config(&daemon, server.address.port(), Clock::new()),
            server.client_tls.clone(),
        )
        .unwrap();
        let mut client = Client::spawn(&proxy, &server);
        proxy.arm_command(client.process.id()).unwrap();
        client.connect(false);
        let worker = request_in_thread(client, false);
        phone.phone.card(1).await;
        phone.phone.decide(&daemon, 1, decision);
        let (reply, mut client) = worker.join().unwrap();
        assert_eq!(reply["status"], 403);
        assert_eq!(
            reply["body"],
            "secret test was not approved for localhost; the request was not sent"
        );
        assert!(server.seen.lock().is_empty());
        assert_eq!(
            daemon.state.lock().unwrap().calls,
            1,
            "an answered decline/expiry never retries"
        );
        let next = client.request(
            "/approved",
            json!({"authorization":format!("Bearer {MARKER}")}),
            "",
        );
        assert_eq!(
            next["status"], 403,
            "a matching retry sees the same unanswered vault id"
        );
        assert_eq!(
            phone.phone.state.lock().unwrap().requests.len(),
            1,
            "the same request_id is never refiled"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_vault_injected_ten_minute_deadline_answers_403_locally() {
    let daemon = FakeClaustrum::start();
    daemon.state.lock().unwrap().record = Some(record("authorization_bearer"));
    let phone = PhoneGuard::install();
    let clock = Clock::new();
    let server = Server::new("echo-request").await;
    let proxy = SecretProxy::start_with_tls(
        live_config(&daemon, server.address.port(), clock.clone()),
        server.client_tls.clone(),
    )
    .unwrap();
    proxy
        .state
        .test_idle_timeout_ms
        .store(100, Ordering::Release);
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(false);
    let worker = request_in_thread(client, false);
    phone.phone.card(1).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        !worker.is_finished(),
        "a held approval outlives the socket idle timeout"
    );
    clock.advance(599_999);
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        !worker.is_finished(),
        "the full ten-minute window remains available"
    );
    clock.advance(1);
    let (reply, _client) = worker.join().unwrap();
    assert_eq!(reply["status"], 403);
    assert_eq!(
        reply["body"],
        "secret test was not approved for localhost; the request was not sent"
    );
    assert!(server.seen.lock().is_empty());
    assert!(proxy.approval_status().is_empty());
    phone.phone.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_vault_concurrent_requests_share_one_card_and_once_queues_cards_serially() {
    for once in [false, true] {
        let daemon = FakeClaustrum::start();
        {
            let mut state = daemon.state.lock().unwrap();
            state.record = Some(record("authorization_bearer"));
            state.once = once;
        }
        let phone = PhoneGuard::install();
        let server = Server::new("echo-request").await;
        let proxy = SecretProxy::start_with_tls(
            live_config(&daemon, server.address.port(), Clock::new()),
            server.client_tls.clone(),
        )
        .unwrap();
        let mut client = Client::spawn(&proxy, &server);
        proxy.arm_command(client.process.id()).unwrap();
        client.connect(true);
        let worker = request_in_thread(client, true);
        let first = phone.phone.card(1).await;
        assert!(server.seen.lock().is_empty());
        assert_eq!(phone.phone.state.lock().unwrap().requests.len(), 1);
        phone.phone.decide(
            &daemon,
            1,
            if once {
                ConsentOutcome::AnsweredOnce
            } else {
                ConsentOutcome::Answered
            },
        );
        if once {
            let second = phone.phone.card(2).await;
            assert_ne!(first.request_id, second.request_id);
            assert!(!worker.is_finished());
            assert!(proxy.approval_status().iter().any(|line| line
                == "secret test: the approval covered one request; another card was sent"));
            phone.phone.decide(&daemon, 2, ConsentOutcome::AnsweredOnce);
        }
        let (reply, _client) = worker.join().unwrap();
        assert!(
            reply.get("a").is_some(),
            "both HTTP/2 requests completed: {reply}"
        );
        assert!(!reply.to_string().contains(std::str::from_utf8(K1).unwrap()));
        assert_eq!(server.seen.lock().len(), 2);
        assert_eq!(
            phone.phone.state.lock().unwrap().requests.len(),
            if once { 2 } else { 1 }
        );
        assert_eq!(daemon.state.lock().unwrap().calls, if once { 5 } else { 4 });
        assert_eq!(daemon.state.lock().unwrap().connections, 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_vault_wrong_host_is_literal_and_never_files_a_card() {
    let daemon = FakeClaustrum::start();
    let mut other = record("authorization_bearer");
    other.host = "other.test".into();
    daemon.state.lock().unwrap().record = Some(other);
    let phone = PhoneGuard::install();
    let server = Server::new("echo-request").await;
    let proxy = SecretProxy::start_with_tls(
        live_config(&daemon, server.address.port(), Clock::new()),
        server.client_tls.clone(),
    )
    .unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(false);
    assert_eq!(
        client.request(
            "/approved",
            json!({"authorization":format!("Bearer {MARKER}")}),
            ""
        )["status"],
        200
    );
    assert_eq!(
        server.seen.lock()[0]["headers"]["authorization"],
        format!("Bearer {MARKER}")
    );
    assert_eq!(daemon.state.lock().unwrap().calls, 1);
    assert!(phone.phone.state.lock().unwrap().requests.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_vault_redirect_to_another_host_asks_again_and_sends_no_key() {
    let daemon = FakeClaustrum::start();
    daemon.state.lock().unwrap().record = Some(record("authorization_bearer"));
    let phone = PhoneGuard::install();
    let server = Server::new("redirect").await;
    let proxy = SecretProxy::start_with_tls(
        live_config(&daemon, server.address.port(), Clock::new()),
        server.client_tls.clone(),
    )
    .unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(false);
    let worker = request_in_thread(client, false);
    phone.phone.card(1).await;
    phone.phone.decide(&daemon, 1, ConsentOutcome::Answered);
    let (first, mut client) = worker.join().unwrap();
    assert_eq!(first["status"], 302);
    let location = url::Url::parse(first["headers"]["location"].as_str().unwrap()).unwrap();
    assert_eq!(location.host_str(), Some("other.test"));
    proxy
        .state
        .test_upstream
        .lock()
        .insert("other.test".into(), server.address);
    assert_eq!(
        client.command(
            json!({"op":"connect", "authority":format!("other.test:{}", server.address.port())})
        )["ok"],
        true
    );
    let second = client.command(json!({"op":"request", "authority":format!("other.test:{}", server.address.port()), "path":location.path(), "headers":{"authorization":format!("Bearer {MARKER}")}}));
    assert_eq!(second["status"], 200);
    assert_eq!(
        server.seen.lock()[1]["headers"]["authorization"],
        format!("Bearer {MARKER}")
    );
    assert_eq!(daemon.state.lock().unwrap().calls, 3);
    assert_eq!(phone.phone.state.lock().unwrap().requests.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_vault_fetch_uses_the_concrete_target_and_holds_for_the_phone() {
    let daemon = FakeClaustrum::start();
    daemon.state.lock().unwrap().record = Some(record("query:key"));
    let phone = PhoneGuard::install();
    let server = Server::new("echo-request-h1").await;
    let cfg = live_config(&daemon, server.address.port(), Clock::new());
    let root = tempfile::tempdir().unwrap();
    let project = root.path().to_owned();
    let options = crate::url_fetch::UrlFetchOptions {
        allow_private: true,
        tls_config: Some(server.client_tls.clone()),
        ..Default::default()
    };
    let url = format!(
        "https://localhost:{}/approved/doc.md?key={MARKER}#local-fragment",
        server.address.port()
    );
    let port = server.address.port();
    let worker = std::thread::spawn(move || {
        crate::subc::with_secret_identity_for_test(
            &project,
            cfg.authorization_scope.stamp().clone(),
            || {
                let secrets = crate::url_fetch::SecretFetch {
                    vault: cfg.vault.clone(),
                    authorization: Some(cfg.authorization_scope.clone()),
                    labels: vec!["test".into()],
                    test_https_port: Some(port),
                    call_id: "fetch-live-vault".into(),
                };
                let result =
                    crate::url_fetch::fetch_url_with_secrets(&url, &project, options, &secrets);
                result.map(|path| std::fs::read_to_string(path).unwrap())
            },
        )
    });
    phone.phone.card(1).await;
    assert!(server.seen.lock().is_empty());
    phone.phone.decide(&daemon, 1, ConsentOutcome::Answered);
    let document = worker
        .join()
        .unwrap()
        .expect("the approved document was fetched");
    assert!(!document.contains(std::str::from_utf8(K1).unwrap()));
    assert!(document.contains(MARKER));
    let seen = server.seen.lock();
    assert_eq!(seen.len(), 1);
    assert!(seen[0]["target"]
        .as_str()
        .unwrap()
        .contains(std::str::from_utf8(K1).unwrap()));
    assert!(!seen[0]["target"]
        .as_str()
        .unwrap()
        .contains("local-fragment"));
    assert_eq!(daemon.state.lock().unwrap().calls, 2);
    assert_eq!(daemon.state.lock().unwrap().connections, 1);
    assert_eq!(
        daemon.state.lock().unwrap().params[0]["target"],
        json!({"scheme":"https", "host":"localhost", "port":443, "path":"/approved/doc.md"})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_vault_new_card_after_longer_approval_does_not_claim_it_was_once() {
    let daemon = FakeClaustrum::start();
    {
        let mut state = daemon.state.lock().unwrap();
        state.record = Some(record("authorization_bearer"));
        state.once = true;
    }
    let phone = PhoneGuard::install();
    let server = Server::new("echo-request").await;
    let proxy = SecretProxy::start_with_tls(
        live_config(&daemon, server.address.port(), Clock::new()),
        server.client_tls.clone(),
    )
    .unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(false);
    let first = request_in_thread(client, false);
    phone.phone.card(1).await;
    phone.phone.decide(&daemon, 1, ConsentOutcome::Answered);
    let (reply, client) = first.join().unwrap();
    assert_eq!(reply["status"], 200);
    // The fixture expires coverage after one request despite the longer phone
    // answer, standing in for a revoked grant before another request arrives.
    let second = request_in_thread(client, false);
    phone.phone.card(2).await;
    assert_eq!(proxy.approval_status(), vec!["secret test: waiting for approval on the phone for localhost (requests wait up to 10 minutes)"]);
    phone.phone.decide(&daemon, 2, ConsentOutcome::Declined);
    let (reply, _client) = second.join().unwrap();
    assert_eq!(reply["status"], 403);
    assert_eq!(server.seen.lock().len(), 1);
}

async fn condition(mut check: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
fn addressed(host: &str) -> crate::secret::ApiKeyTarget {
    crate::secret::ApiKeyTarget {
        scheme: "https".into(),
        host: host.into(),
        port: Some(443),
        path: "/approved".into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_label_does_not_block_an_immediate_vault_answer_for_another_target() {
    let daemon = FakeClaustrum::start();
    daemon.state.lock().unwrap().record = Some(record("authorization_bearer"));
    let phone = PhoneGuard::install();
    let cfg = live_config(&daemon, 443, Clock::new());
    let secrets = Arc::new(crate::secret::requests::RequestSecrets::new(
        cfg.vault,
        cfg.authorization_scope,
        cfg.consent_context,
    ));
    let held = secrets.clone();
    let first = tokio::spawn(async move {
        held.use_in_request("test", &addressed("localhost"), u64::MAX, || async {
            Ok(())
        })
        .await
    });
    phone.phone.card(1).await;
    let answer = tokio::time::timeout(
        Duration::from_millis(500),
        secrets.use_in_request("test", &addressed("other.test"), u64::MAX, || async {
            Ok(())
        }),
    )
    .await
    .expect("a pending card must not lock another vault lookup")
    .unwrap();
    assert!(matches!(answer, crate::secret::UseAnswer::NotFound));
    assert_eq!(
        daemon.state.lock().unwrap().calls,
        2,
        "count NotFound asks too"
    );
    let statement = phone.phone.state.lock().unwrap().requests[0]
        .statement_b64url
        .clone();
    daemon.state.lock().unwrap().approve(&statement);
    let released = tokio::time::timeout(
        Duration::from_millis(500),
        secrets.use_in_request("test", &addressed("localhost"), u64::MAX, || async {
            Ok(())
        }),
    )
    .await
    .expect("an already-covered request must not wait on the older card")
    .unwrap();
    assert!(matches!(released, crate::secret::UseAnswer::Released(_)));
    assert!(!first.is_finished());
    first.abort();
    let _ = first.await;
    condition(|| phone.phone.state.lock().unwrap().withdrawn.len() == 1).await;
    assert!(
        secrets.status().is_empty(),
        "cancelled waiter removes its waiting line in Drop"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_one_waiter_keeps_the_pending_card_and_a_retry_joins_until_the_last_waiter_leaves(
) {
    let daemon = FakeClaustrum::start();
    daemon.state.lock().unwrap().record = Some(record("authorization_bearer"));
    let phone = PhoneGuard::install();
    let cfg = live_config(&daemon, 443, Clock::new());
    let secrets = Arc::new(crate::secret::requests::RequestSecrets::new(
        cfg.vault,
        cfg.authorization_scope,
        cfg.consent_context,
    ));
    let spawn = || {
        let secrets = secrets.clone();
        tokio::spawn(async move {
            secrets
                .use_in_request("test", &addressed("localhost"), u64::MAX, || async {
                    Ok(())
                })
                .await
        })
    };
    let first = spawn();
    phone.phone.card(1).await;
    let second = spawn();
    condition(|| secrets.status().len() == 2).await;
    first.abort();
    let _ = first.await;
    assert!(phone.phone.state.lock().unwrap().withdrawn.is_empty());
    let retry = spawn();
    condition(|| secrets.status().len() == 2).await;
    assert!(
        !second.is_finished() && !retry.is_finished(),
        "a cancelled peer is not a declined card"
    );
    assert_eq!(phone.phone.state.lock().unwrap().requests.len(), 1);
    second.abort();
    let _ = second.await;
    assert!(phone.phone.state.lock().unwrap().withdrawn.is_empty());
    retry.abort();
    let _ = retry.await;
    condition(|| phone.phone.state.lock().unwrap().withdrawn.len() == 1).await;
    assert!(secrets.status().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_labels_file_concurrently_and_share_one_absolute_deadline() {
    let daemon = FakeClaustrum::start();
    let mut second = record("header:x-second");
    second.name = "second".into();
    second.key = K2.to_vec();
    daemon.state.lock().unwrap().records = vec![record("authorization_bearer"), second];
    let phone = PhoneGuard::install();
    let clock = Clock::new();
    let server = Server::new("echo-request").await;
    let mut cfg = live_config(&daemon, server.address.port(), clock.clone());
    cfg.scanned_labels.push("second".into());
    let proxy = SecretProxy::start_with_tls(cfg, server.client_tls.clone()).unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(false);
    let worker = std::thread::spawn(move || {
        let reply = client.request(
            "/approved",
            json!({"authorization":format!("Bearer {MARKER}"),"x-second":"{{ck-secret:second}}"}),
            "",
        );
        (reply, client)
    });
    phone.phone.card(2).await;
    let requests = phone.phone.state.lock().unwrap().requests.clone();
    assert!(
        requests
            .iter()
            .all(|r| r.expires_at_ms == Some(1_700_000_600_000)),
        "each filed card expires by the request's one deadline"
    );
    clock.advance(599_999);
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!worker.is_finished());
    let first_index = requests.iter().position(|r| r.label == "test").unwrap() + 1;
    phone
        .phone
        .decide(&daemon, first_index, ConsentOutcome::AnsweredOnce);
    condition(|| daemon.state.lock().unwrap().calls >= 3).await;
    assert!(server.seen.lock().is_empty());
    clock.advance(1);
    let (reply, _client) = worker.join().unwrap();
    assert_eq!(reply["status"], 403);
    assert!(
        server.seen.lock().is_empty(),
        "the earlier release is spent, not sent or reused when another label times out"
    );
    condition(|| phone.phone.state.lock().unwrap().withdrawn.len() == 1).await;
    assert!(proxy.approval_status().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approval_status_and_denial_scrub_a_label_that_equals_an_earlier_released_key() {
    let daemon = FakeClaustrum::start();
    let name = std::str::from_utf8(K1).unwrap();
    let mut second = record("header:x-second");
    second.name = name.into();
    second.key = K2.to_vec();
    daemon.state.lock().unwrap().records = vec![record("authorization_bearer"), second];
    let phone = PhoneGuard::install();
    let server = Server::new("echo-request").await;
    let mut cfg = live_config(&daemon, server.address.port(), Clock::new());
    cfg.scanned_labels.push(name.into());
    let proxy = SecretProxy::start_with_tls(cfg, server.client_tls.clone()).unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(false);
    let first = request_in_thread(client, false);
    phone.phone.card(1).await;
    phone.phone.decide(&daemon, 1, ConsentOutcome::Answered);
    let (reply, mut client) = first.join().unwrap();
    assert_eq!(reply["status"], 200);
    let marker = format!("{{{{ck-secret:{name}}}}}");
    let second = std::thread::spawn(move || {
        let reply = client.request("/approved", json!({"x-second":marker}), "");
        (reply, client)
    });
    phone.phone.card(2).await;
    assert!(!proxy.approval_status().is_empty());
    assert!(proxy
        .approval_status()
        .iter()
        .all(|line| !line.contains(name)));
    phone.phone.decide(&daemon, 2, ConsentOutcome::Declined);
    let (reply, _client) = second.join().unwrap();
    assert_eq!(reply["status"], 403);
    assert!(
        !reply.to_string().contains(name),
        "a local denial must use the command's released-key scrubber"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_json_structure_refuses_before_any_vault_ask_or_phone_card() {
    let daemon = FakeClaustrum::start();
    daemon.state.lock().unwrap().record = Some(record("json_pointer:/key"));
    let phone = PhoneGuard::install();
    let server = Server::new("echo-request").await;
    let proxy = SecretProxy::start_with_tls(
        live_config(&daemon, server.address.port(), Clock::new()),
        server.client_tls.clone(),
    )
    .unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(false);
    for body in [
        "{not json {{ck-secret:test}}",
        "{\"key\":\"{{ck-secret:test}}\",\"ke\\u0079\":\"other\"}",
    ] {
        let reply = client.request(
            "/approved",
            json!({"content-type":"application/json"}),
            body,
        );
        assert_eq!(reply["status"], 403);
    }
    assert_eq!(
        daemon.state.lock().unwrap().calls,
        0,
        "structure checks must precede releasing secret.use"
    );
    assert!(phone.phone.state.lock().unwrap().requests.is_empty());
    assert!(server.seen.lock().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fetch_and_proxy_request_share_one_once_card_then_raise_exactly_one_successor() {
    let daemon = FakeClaustrum::start();
    {
        let mut state = daemon.state.lock().unwrap();
        state.record = Some(record("query:key"));
        state.once = true;
    }
    let phone = PhoneGuard::install();
    let server = Server::new("echo-request-h1").await;
    let cfg = live_config(&daemon, server.address.port(), Clock::new());
    let fetch_vault = cfg.vault.clone();
    let fetch_scope = cfg.authorization_scope.clone();
    let proxy = SecretProxy::start_with_tls(cfg, server.client_tls.clone()).unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(false);
    let proxy_worker = std::thread::spawn(move || {
        let reply = client.request(&format!("/approved?key={MARKER}"), json!({}), "");
        (reply, client)
    });
    phone.phone.card(1).await;
    let root = tempfile::tempdir().unwrap();
    let project = root.path().to_owned();
    let port = server.address.port();
    let options = crate::url_fetch::UrlFetchOptions {
        allow_private: true,
        tls_config: Some(server.client_tls.clone()),
        ..Default::default()
    };
    let fetch_worker = std::thread::spawn(move || {
        crate::subc::with_secret_identity_for_test(&project, fetch_scope.stamp().clone(), || {
            let secrets = crate::url_fetch::SecretFetch {
                vault: fetch_vault,
                authorization: Some(fetch_scope),
                labels: vec!["test".into()],
                test_https_port: Some(port),
                call_id: "concurrent-fetch".into(),
            };
            crate::url_fetch::fetch_url_with_secrets(
                &format!("https://localhost:{port}/approved/doc.md?key={MARKER}"),
                &project,
                options,
                &secrets,
            )
        })
    });
    condition(|| daemon.state.lock().unwrap().calls >= 2).await;
    assert_eq!(phone.phone.state.lock().unwrap().requests.len(), 1);
    phone.phone.decide(&daemon, 1, ConsentOutcome::AnsweredOnce);
    phone.phone.card(2).await;
    condition(|| server.seen.lock().len() == 1).await;
    assert_eq!(
        server.seen.lock().len(),
        1,
        "one once approval can dispatch only one waiting request"
    );
    phone.phone.decide(&daemon, 2, ConsentOutcome::AnsweredOnce);
    let (reply, _client) = proxy_worker.join().unwrap();
    assert_eq!(reply["status"], 200);
    assert!(fetch_worker.join().unwrap().is_ok());
    assert_eq!(phone.phone.state.lock().unwrap().requests.len(), 2);
    assert_eq!(server.seen.lock().len(), 2);
    assert!(server.seen.lock().iter().all(|r| r["target"]
        .as_str()
        .unwrap()
        .contains(std::str::from_utf8(K1).unwrap())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_a_proxy_with_a_held_request_withdraws_the_card_and_removes_status() {
    let daemon = FakeClaustrum::start();
    daemon.state.lock().unwrap().record = Some(record("authorization_bearer"));
    let phone = PhoneGuard::install();
    let server = Server::new("echo-request").await;
    let mut proxy = SecretProxy::start_with_tls(
        live_config(&daemon, server.address.port(), Clock::new()),
        server.client_tls.clone(),
    )
    .unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(false);
    let worker = request_in_thread(client, false);
    phone.phone.card(1).await;
    assert!(!proxy.approval_status().is_empty());
    proxy.stop();
    condition(|| phone.phone.state.lock().unwrap().withdrawn.len() == 1).await;
    assert!(proxy.approval_status().is_empty());
    assert!(server.seen.lock().is_empty());
    let _ = worker.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_statement_location_is_planned_before_filing_or_consuming_a_release() {
    let daemon = FakeClaustrum::start();
    daemon.state.lock().unwrap().record = Some(record("json_pointer:/key"));
    let phone = PhoneGuard::install();
    let server = Server::new("echo-request").await;
    let proxy = SecretProxy::start_with_tls(
        live_config(&daemon, server.address.port(), Clock::new()),
        server.client_tls.clone(),
    )
    .unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(false);
    let reply = client.request(
        "/approved",
        json!({"content-type":"application/json"}),
        "{\"key\":42,\"other\":\"{{ck-secret:test}}\"}",
    );
    assert_eq!(reply["status"], 403);
    assert_eq!(
        daemon.state.lock().unwrap().calls,
        1,
        "read location from approval_required, without a releasing retry"
    );
    assert!(phone.phone.state.lock().unwrap().requests.is_empty());
    assert!(server.seen.lock().is_empty());
}
