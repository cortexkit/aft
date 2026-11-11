//! Foreground executor cancellation must reach a real placeholder fetch.
use super::*;
use crate::secret::transport::fake::{FakeClaustrum, Record, NONCE};
use crate::secret::{SecretVault, SubcSecretVault};
use crate::url_fetch::UrlFetchOptions;
use std::sync::atomic::AtomicUsize;

static FINISHED: AtomicUsize = AtomicUsize::new(0);
static NO_RUNTIME: AtomicBool = AtomicBool::new(false);
static FETCH_ENV: std::sync::LazyLock<StdMutex<Option<(Arc<dyn SecretVault>, UrlFetchOptions)>>> =
    std::sync::LazyLock::new(Default::default);

fn dispatch(request: RawRequest, ctx: &AppContext) -> Response {
    let (vault, options) = FETCH_ENV.lock().unwrap().clone().unwrap();
    NO_RUNTIME.store(
        tokio::runtime::Handle::try_current().is_err(),
        Ordering::SeqCst,
    );
    let response = crate::commands::with_secret_fetch_for_test(vault, options, || {
        crate::commands::outline::handle_outline(&request, ctx)
    });
    FINISHED.fetch_add(1, Ordering::SeqCst);
    response
}

#[derive(Default)]
struct Phone {
    requests: StdMutex<Vec<Value>>,
    answers: StdMutex<HashMap<String, Value>>,
    withdrawals: StdMutex<Vec<String>>,
}
impl ConsentTransport for Phone {
    fn call(&self, call: ConsentCall) -> ConsentFuture {
        match call.method {
            "consent.request" => {
                let mut requests = self.requests.lock().unwrap();
                requests.push(call.params.clone());
                let id = format!("el_{}", requests.len());
                Box::pin(async move { Ok(json!({"elicitation_id": id})) })
            }
            "consent.withdraw" => {
                self.withdrawals
                    .lock()
                    .unwrap()
                    .push(call.params["elicitation_id"].as_str().unwrap().into());
                Box::pin(async { Ok(json!({"state":"withdrawn"})) })
            }
            "consent.await" => {
                let id = call.params["elicitation_id"].as_str().unwrap().to_owned();
                let phone = PHONE.lock().unwrap().as_ref().unwrap().clone();
                Box::pin(async move {
                    loop {
                        if let Some(answer) = phone.answers.lock().unwrap().get(&id).cloned() {
                            return Ok(answer);
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
            }
            other => panic!("unexpected consent method {other}"),
        }
    }
}
static PHONE: std::sync::LazyLock<StdMutex<Option<Arc<Phone>>>> =
    std::sync::LazyLock::new(Default::default);

struct Restore;
impl Drop for Restore {
    fn drop(&mut self) {
        crate::secret::consent::install_transport(None);
        FETCH_ENV.lock().unwrap().take();
        if let Some(phone) = PHONE.lock().unwrap().take() {
            for index in 0..phone.requests.lock().unwrap().len() {
                phone
                    .answers
                    .lock()
                    .unwrap()
                    .insert(format!("el_{}", index + 1), json!({"state":"expired"}));
            }
        }
    }
}

fn server_tls() -> (rustls::ClientConfig, tokio_rustls::TlsAcceptor) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let key = rcgen::KeyPair::generate().unwrap();
    let certificate = rcgen::CertificateParams::new(vec!["api.secret.test".into()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate.der().clone()).unwrap();
    let client = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
        )
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    (client, acceptor)
}

async fn server() -> (
    std::net::SocketAddr,
    rustls::ClientConfig,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (client, acceptor) = server_tls();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let sends = Arc::new(AtomicUsize::new(0));
    let seen = sends.clone();
    let worker = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let mut tls = acceptor.accept(socket).await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = tls.read(&mut buffer).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
            }
            seen.fetch_add(1, Ordering::SeqCst);
            let _ = tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/markdown\r\nContent-Length: 8\r\nConnection: close\r\n\r\n# Fetch\n").await;
        }
    });
    (address, client, sends, worker)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_fetch_cancel_with_two_pending_labels_withdraws_both_and_a_late_allow_cannot_release_or_send(
) {
    held_foreground_fetch_late_allow(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_fetch_cancel_during_tls_setup_never_hands_the_keyed_request_to_http() {
    use crate::secret::SecretVault;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let _consent_lock = CONSENT_TEST_LOCK.lock().await;
    let _secret_lock = crate::secret::consent::TEST_TRANSPORT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // The uncancelled control uses the same certificate and held handshake,
    // so a TLS setup failure cannot make the no-request assertion pass.
    for cancel in [true, false] {
        let f = fixture(FakeCingulate::up());
        let _restore = Restore;
        let daemon = FakeClaustrum::start();
        {
            let mut state = daemon.state.lock().unwrap();
            state.pending_expires_at_ms = Some(crate::secret::vault::now_ms() + 600_000);
            state.once = true;
            state.record = Some(Record {
                name: "first".into(),
                host: "api.secret.test".into(),
                path: "/v1".into(),
                location: "query:first".into(),
                key: b"tls-stall-released-key-0123456789".to_vec(),
            });
        }
        let transport = crate::secret::transport::SubcSecretUseTransport::for_test(
            daemon.path.clone(),
            subc_protocol::BindIdentity::new("test", "session"),
            Duration::from_secs(2),
            subc_client_rs::ConsumerIdentity {
                module_id: "aft".into(),
                launch_nonce: NONCE.into(),
            },
        );
        let vault = Arc::new(SubcSecretVault::new(Arc::new(transport)));
        let scope = crate::secret::SecretScope::from_stamp(f.head.scope.as_ref().unwrap());
        let target = crate::secret::ApiKeyTarget {
            scheme: "https".into(),
            host: "api.secret.test".into(),
            port: Some(443),
            path: "/v1/doc.md".into(),
        };
        let crate::secret::UseAnswer::ApprovalRequired(pending) =
            vault.use_secret(&scope, "first", &target)
        else {
            panic!("approval required")
        };
        daemon
            .state
            .lock()
            .unwrap()
            .approve(&pending.statement_b64url);
        let (tls, acceptor) = server_tls();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted, connected) = tokio::sync::oneshot::channel();
        let (resume, resumed) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            accepted.send(()).unwrap();
            resumed.await.unwrap();
            let Ok(mut tls) = acceptor.accept(socket).await else {
                return Vec::new();
            };
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                match tokio::time::timeout(Duration::from_secs(2), tls.read(&mut byte)).await {
                    Ok(Ok(1)) => request.push(byte[0]),
                    _ => break,
                }
            }
            if !request.is_empty() {
                let _ = tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/markdown\r\nContent-Length: 8\r\nConnection: close\r\n\r\n# Fetch\n").await;
            }
            request
        });
        *FETCH_ENV.lock().unwrap() = Some((
            vault,
            UrlFetchOptions {
                tls_config: Some(tls),
                public_host_overrides: vec![(
                    "api.secret.test".into(),
                    vec!["93.184.216.34".parse().unwrap()],
                )],
                connect_overrides: vec![("api.secret.test".into(), address)],
                ..Default::default()
            },
        ));
        let url = format!(
            "https://api.secret.test:{}/v1/doc.md?first={{{{ck-secret:first}}}}",
            address.port()
        );
        let mut replies = f
            .send_tool(&f.head, "outline", json!({"target":url}), 91, dispatch)
            .await;
        tokio::time::timeout(Duration::from_secs(3), connected)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            daemon.state.lock().unwrap().calls,
            2,
            "the covered key was released before the held connection setup"
        );
        if cancel {
            assert!(cancel_active_tool_call(
                &f.active,
                &f.executor,
                route_key(41, 1),
                91,
                "cancel during TLS setup"
            )
            .is_some());
        }
        resume.send(()).unwrap();
        let request = tokio::time::timeout(Duration::from_secs(4), server)
            .await
            .unwrap()
            .unwrap();
        if cancel {
            assert!(
                request.is_empty(),
                "no HTTP request line may follow cancellation during TLS setup: {}",
                String::from_utf8_lossy(&request)
            );
        } else {
            assert!(
                request.starts_with(
                    b"GET /v1/doc.md?first=tls-stall-released-key-0123456789 HTTP/1.1\r\n"
                ),
                "the control must complete TLS and send the real keyed request"
            );
        }
        let _ = tokio::time::timeout(Duration::from_secs(3), replies.recv()).await;
        assert_eq!(
            daemon.state.lock().unwrap().calls,
            2,
            "neither cancellation nor a connect failure may retry the release"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_fetch_reply_deadline_with_two_pending_labels_withdraws_both_and_cannot_send_after_late_allow(
) {
    held_foreground_fetch_late_allow(true).await;
}

async fn held_foreground_fetch_late_allow(expire: bool) {
    let _consent_lock = CONSENT_TEST_LOCK.lock().await;
    let _secret_lock = crate::secret::consent::TEST_TRANSPORT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = fixture(FakeCingulate::up());
    let _restore = Restore;
    let daemon = FakeClaustrum::start();
    daemon.state.lock().unwrap().pending_expires_at_ms =
        Some(crate::secret::vault::now_ms() + 600_000);
    for label in ["first", "second"] {
        daemon.state.lock().unwrap().records.push(Record {
            name: label.into(),
            host: "api.secret.test".into(),
            path: "/v1".into(),
            location: format!("query:{label}"),
            key: format!("released-key-for-{label}-0000").into_bytes(),
        });
    }
    let transport = crate::secret::transport::SubcSecretUseTransport::for_test(
        daemon.path.clone(),
        subc_protocol::BindIdentity::new("test", "session"),
        Duration::from_secs(2),
        subc_client_rs::ConsumerIdentity {
            module_id: "aft".into(),
            launch_nonce: NONCE.into(),
        },
    );
    let phone = Arc::new(Phone::default());
    *PHONE.lock().unwrap() = Some(phone.clone());
    super::super::secret_consent::install_subc_transport(phone.clone());
    let (address, tls, sends, server) = server().await;
    *FETCH_ENV.lock().unwrap() = Some((
        Arc::new(SubcSecretVault::new(Arc::new(transport))),
        UrlFetchOptions {
            tls_config: Some(tls),
            public_host_overrides: vec![(
                "api.secret.test".into(),
                vec!["93.184.216.34".parse().unwrap()],
            )],
            connect_overrides: vec![("api.secret.test".into(), address)],
            ..Default::default()
        },
    ));
    let url = format!("https://api.secret.test:{}/v1/doc.md?first={{{{ck-secret:first}}}}&second={{{{ck-secret:second}}}}", address.port());
    let reply_deadline_ms = crate::secret::vault::now_ms() + 2000;
    if expire {
        f.deadlines.lock().unwrap().insert(91, reply_deadline_ms);
    }
    let mut replies = f
        .send_tool(&f.head, "outline", json!({"target":url}), 91, dispatch)
        .await;
    wait_for(|| phone.requests.lock().unwrap().len() == 2).await;
    assert_eq!(
        daemon.state.lock().unwrap().calls,
        2,
        "one initial approval-required ask per label"
    );
    assert_eq!(sends.load(Ordering::SeqCst), 0);
    if expire {
        assert!(phone
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request["expires_at_ms"].as_u64().unwrap() <= reply_deadline_ms));
    } else {
        assert!(cancel_active_tool_call(
            &f.active,
            &f.executor,
            route_key(41, 1),
            91,
            "cancel foreground fetch"
        )
        .is_some());
    }
    wait_for(|| phone.withdrawals.lock().unwrap().len() == 2).await;
    // Deliver both signed allows even after cancellation. Neither answer may
    // revive the executor job, re-ask the vault, or dispatch its HTTP request.
    let requests = phone.requests.lock().unwrap().clone();
    for (index, request) in requests.iter().enumerate() {
        let item = &request["secret_use"]["items"][0];
        daemon
            .state
            .lock()
            .unwrap()
            .approve(item["statement_b64url"].as_str().unwrap());
        phone.answers.lock().unwrap().insert(
            format!("el_{}", index + 1),
            json!({
                "state":"answered", "answered_choice_id":"allow", "secret_use_answer": {
                    "decision":"allow", "duration":"once", "envelope_sha256":"00".repeat(32),
                    "items":[{"request_id":item["request_id"],"decision":"allow"}]
                }
            }),
        );
    }
    wait_for(|| phone.withdrawals.lock().unwrap().len() == 2).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), replies.recv()).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        daemon.state.lock().unwrap().calls,
        2,
        "cancellation forbids even a releasing retry"
    );
    assert_eq!(
        sends.load(Ordering::SeqCst),
        0,
        "the late allow must not send an HTTP request"
    );
    server.abort();
}

#[derive(Default)]
struct HeldVault {
    entered: AtomicBool,
    released: StdMutex<bool>,
    wake: std::sync::Condvar,
}
impl HeldVault {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}
impl SecretVault for HeldVault {
    fn use_secret(
        &self,
        _: &crate::secret::SecretScope,
        _: &str,
        _: &crate::secret::ApiKeyTarget,
    ) -> crate::secret::UseAnswer {
        self.entered.store(true, Ordering::SeqCst);
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.wake.wait(released).unwrap();
        }
        crate::secret::UseAnswer::Unavailable
    }
    fn audit(
        &self,
        _: &secret_rules::audit::AuditRecordV1,
    ) -> Result<(), crate::secret::AuditError> {
        Ok(())
    }
    fn can_release_material(&self) -> bool {
        false
    }
}
struct ReleaseHeld(Arc<HeldVault>);
impl Drop for ReleaseHeld {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_fetch_cancellation_does_not_wait_for_local_runtime_blocking_work_to_finish() {
    let _consent_lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let _restore = Restore;
    FINISHED.store(0, Ordering::SeqCst);
    NO_RUNTIME.store(false, Ordering::SeqCst);
    let vault = Arc::new(HeldVault::default());
    let _release = ReleaseHeld(vault.clone());
    *FETCH_ENV.lock().unwrap() = Some((
        vault.clone(),
        UrlFetchOptions {
            public_host_overrides: vec![(
                "api.secret.test".into(),
                vec!["93.184.216.34".parse().unwrap()],
            )],
            ..Default::default()
        },
    ));
    let _replies = f
        .send_tool(
            &f.head,
            "outline",
            json!({"target":"https://api.secret.test/v1/doc.md?first={{ck-secret:first}}"}),
            92,
            dispatch,
        )
        .await;
    wait_for(|| vault.entered.load(Ordering::SeqCst)).await;
    assert!(
        NO_RUNTIME.load(Ordering::SeqCst),
        "the foreground executor really uses the no-current-runtime branch"
    );
    assert!(cancel_active_tool_call(
        &f.active,
        &f.executor,
        route_key(41, 1),
        92,
        "cancel blocked lookup"
    )
    .is_some());
    tokio::time::timeout(Duration::from_secs(2), async {
        while FINISHED.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("runtime destruction must not wait for the vault transport's blocking timeout");
    assert!(
        !*vault.released.lock().unwrap(),
        "the blocked lookup is still held when the caller finishes"
    );
    vault.release();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_fetch_refuses_non_query_locations_before_any_card_or_releasing_retry() {
    let _consent_lock = CONSENT_TEST_LOCK.lock().await;
    let _secret_lock = crate::secret::consent::TEST_TRANSPORT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for covered in [false, true] {
        let f = fixture(FakeCingulate::up());
        let _restore = Restore;
        let daemon = FakeClaustrum::start();
        {
            let mut state = daemon.state.lock().unwrap();
            state.pending_expires_at_ms = Some(crate::secret::vault::now_ms() + 600_000);
            state.record = Some(Record {
                name: "first".into(),
                host: "api.secret.test".into(),
                path: "/v1".into(),
                location: "authorization_bearer".into(),
                key: b"must-never-leave-fetch-0123456789".to_vec(),
            });
        }
        let transport = crate::secret::transport::SubcSecretUseTransport::for_test(
            daemon.path.clone(),
            subc_protocol::BindIdentity::new("test", "session"),
            Duration::from_secs(2),
            subc_client_rs::ConsumerIdentity {
                module_id: "aft".into(),
                launch_nonce: NONCE.into(),
            },
        );
        let vault = Arc::new(SubcSecretVault::new(Arc::new(transport)));
        if covered {
            let scope = crate::secret::SecretScope::from_stamp(f.head.scope.as_ref().unwrap());
            let ask = vault.clone();
            let answer = tokio::task::spawn_blocking(move || {
                ask.use_secret(
                    &scope,
                    "first",
                    &crate::secret::ApiKeyTarget {
                        scheme: "https".into(),
                        host: "api.secret.test".into(),
                        port: Some(443),
                        path: "/v1/doc.md".into(),
                    },
                )
            })
            .await
            .unwrap();
            let crate::secret::UseAnswer::ApprovalRequired(pending) = answer else {
                panic!("setup needs one real vault statement")
            };
            daemon
                .state
                .lock()
                .unwrap()
                .approve(&pending.statement_b64url);
        }
        let phone = Arc::new(Phone::default());
        *PHONE.lock().unwrap() = Some(phone.clone());
        super::super::secret_consent::install_subc_transport(phone.clone());
        let (address, tls, sends, server) = server().await;
        *FETCH_ENV.lock().unwrap() = Some((
            vault,
            UrlFetchOptions {
                tls_config: Some(tls),
                public_host_overrides: vec![(
                    "api.secret.test".into(),
                    vec!["93.184.216.34".parse().unwrap()],
                )],
                connect_overrides: vec![("api.secret.test".into(), address)],
                ..Default::default()
            },
        ));
        let url = format!(
            "https://api.secret.test:{}/v1/doc.md?first={{{{ck-secret:first}}}}",
            address.port()
        );
        let reply = f
            .checkout_tool(&f.head, "outline", json!({"target":url}), 93, dispatch)
            .await;
        assert_eq!(reply["structuredContent"]["success"], false, "{reply}");
        assert!(
            phone.requests.lock().unwrap().is_empty(),
            "a pending non-query statement must be refused before a phone card is filed"
        );
        assert_eq!(
            daemon.state.lock().unwrap().calls,
            if covered { 2 } else { 1 },
            "including the setup ask, there is no releasing retry after refusing the location"
        );
        assert_eq!(
            sends.load(Ordering::SeqCst),
            0,
            "even an already-covered release is dropped unsent"
        );
        server.abort();
    }
}
