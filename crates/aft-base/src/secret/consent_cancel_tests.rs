use super::*;
use std::sync::{atomic::Ordering, Arc, MutexGuard};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_wait_passes_cancellation_to_hold_and_withdraws_a_failed_await() {
    struct Transport {
        entered: tokio::sync::Notify,
        observed: std::sync::atomic::AtomicBool,
        withdrawals: std::sync::atomic::AtomicUsize,
        fail: bool,
    }
    impl SecretConsentTransport for Transport {
        fn file(&self, _: &SecretConsentRequest) -> ConsentOutcome {
            panic!("the explicit hold must receive cancellation")
        }
        fn hold(&self, _: &SecretConsentRequest, cancel: &Cancellation) -> ConsentOutcome {
            self.entered.notify_one();
            if self.fail {
                return ConsentOutcome::Unavailable {
                    detail: "await failed".into(),
                };
            }
            while !cancel.is_cancelled() {
                std::thread::park_timeout(std::time::Duration::from_millis(1));
            }
            self.observed
                .store(true, std::sync::atomic::Ordering::Release);
            ConsentOutcome::Cancelled
        }
        fn withhold(&self, _: &SecretConsentRequest) -> ConsentOutcome {
            self.withdrawals
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            ConsentOutcome::Cancelled
        }
    }
    for fail in [false, true] {
        let transport = std::sync::Arc::new(Transport {
            entered: Default::default(),
            observed: Default::default(),
            withdrawals: Default::default(),
            fail,
        });
        let cancel = Cancellation::default();
        let stamp = serde_json::from_value(serde_json::json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"kind":"head","ref":"default-hold","scope_epoch":1,"owner_authorized":true})).unwrap();
        let request = SecretConsentRequest {
            scope: crate::secret::SecretScope::from_stamp(&stamp),
            project_root: "scope".into(),
            session: "hold".into(),
            label: "test".into(),
            request_id: "id".into(),
            statement_b64url: "statement".into(),
            expires_at_ms: None,
        };
        let waiter = tokio::spawn(transport.clone().wait(request, cancel.clone()));
        transport.entered.notified().await;
        if !fail {
            cancel.cancel();
        }
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .unwrap()
            .unwrap();
        if fail {
            assert!(matches!(outcome, ConsentOutcome::Unavailable { .. }));
        } else {
            assert_eq!(outcome, ConsentOutcome::Cancelled);
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while !transport
                    .observed
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        assert_eq!(
            transport
                .withdrawals
                .load(std::sync::atomic::Ordering::Acquire),
            1
        );
    }
}

struct Held {
    filed: mpsc::UnboundedSender<(String, oneshot::Sender<ConsentOutcome>)>,
    withdrawn: mpsc::UnboundedSender<(String, CancellationReason)>,
}
impl SecretConsentTransport for Held {
    fn wait(
        self: Arc<Self>,
        request: SecretConsentRequest,
        cancel: Cancellation,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ConsentOutcome> + Send>> {
        Box::pin(async move {
            let (answer, received) = oneshot::channel();
            self.filed
                .send((request.request_id.clone(), answer))
                .unwrap();
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    self.withdrawn.send((request.request_id, cancel.reason().unwrap())).unwrap();
                    ConsentOutcome::Cancelled
                }
                Ok(answer) = received => answer,
            }
        })
    }
    fn file(&self, _: &SecretConsentRequest) -> ConsentOutcome {
        unreachable!("the shared coordinator uses wait")
    }
    fn withhold(&self, _: &SecretConsentRequest) -> ConsentOutcome {
        unreachable!("the test wait records its own withdrawal")
    }
}

struct Fixture {
    _lock: MutexGuard<'static, ()>,
    filed: mpsc::UnboundedReceiver<(String, oneshot::Sender<ConsentOutcome>)>,
    withdrawn: mpsc::UnboundedReceiver<(String, CancellationReason)>,
}

impl Fixture {
    fn new() -> Self {
        let lock = TEST_TRANSPORT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (filed, filed_rx) = mpsc::unbounded_channel();
        let (withdrawn, withdrawn_rx) = mpsc::unbounded_channel();
        install_transport(Some(Arc::new(Held { filed, withdrawn })));
        Self {
            _lock: lock,
            filed: filed_rx,
            withdrawn: withdrawn_rx,
        }
    }

    async fn card(&mut self, request: &SecretConsentRequest) -> oneshot::Sender<ConsentOutcome> {
        let (id, answer) = self.filed.recv().await.unwrap();
        assert_eq!(id, request.request_id);
        answer
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        install_transport(None);
    }
}

fn request(index: usize, ttl_ms: u64) -> SecretConsentRequest {
    let stamp: subc_protocol::scope::ScopeStamp = serde_json::from_value(serde_json::json!({
        "owner":{"kind":"reserved","module_id":"prefrontal-core"},
        "kind":"head", "ref":"cancel-cache", "scope_epoch":1, "owner_authorized":true
    }))
    .unwrap();
    SecretConsentRequest {
        scope: super::super::SecretScope::from_stamp(&stamp),
        project_root: "project".into(),
        session: "session".into(),
        label: "key".into(),
        request_id: format!("01HF7YAT{index:018}"),
        statement_b64url: "c3RhdGVtZW50".into(),
        expires_at_ms: Some(super::super::vault::now_ms() + ttl_ms),
    }
}

fn shared(request: &SecretConsentRequest) -> Arc<SharedCard> {
    CARDS.lock().unwrap()[&CardKey(request.scope.clone(), request.request_id.clone())].clone()
}

async fn hang_up<T>(caller: tokio::task::JoinHandle<T>) {
    caller.abort();
    assert!(matches!(caller.await, Err(error) if error.is_cancelled()));
}

#[tokio::test(start_paused = true)]
async fn a_secret_retry_eight_seconds_after_a_sixty_second_hangup_joins_the_same_card() {
    let mut f = Fixture::new();
    let request = request(1, 600_000);
    let caller = tokio::spawn(wait_consent(request.clone()));
    let phone = f.card(&request).await;
    tokio::time::advance(Duration::from_secs(60)).await;
    hang_up(caller).await;
    tokio::time::advance(Duration::from_secs(8)).await;
    let mut retry_request = request.clone();
    let mut retry_stamp = request.scope.stamp().clone();
    retry_stamp.attributes.agent_id = Some("another-route-binding".into());
    retry_request.scope = SecretScope::from_stamp(&retry_stamp);
    let retry = tokio::spawn(wait_consent(retry_request));
    tokio::task::yield_now().await;
    assert_eq!(shared(&request).waiters.load(Ordering::Acquire), 1);
    assert!(
        f.withdrawn.try_recv().is_err(),
        "hang-up must not withdraw the card"
    );
    assert!(
        f.filed.try_recv().is_err(),
        "retry must not file a second card"
    );
    assert!(
        !retry.is_finished(),
        "the retry must still wait for the phone"
    );
    phone.send(ConsentOutcome::AnsweredOnce).unwrap();
    assert_eq!(retry.await.unwrap(), ConsentOutcome::AnsweredOnce);
}

#[tokio::test(start_paused = true)]
async fn secret_approval_and_denial_answered_without_a_client_are_cached_only_until_expiry() {
    let mut f = Fixture::new();
    for (index, answer) in [ConsentOutcome::AnsweredOnce, ConsentOutcome::Declined]
        .into_iter()
        .enumerate()
    {
        let request = request(index, 600_000);
        let caller = tokio::spawn(wait_consent(request.clone()));
        let phone = f.card(&request).await;
        let card = shared(&request);
        hang_up(caller).await;
        phone.send(answer.clone()).unwrap();
        let mut recorded = card.outcome.subscribe();
        assert_eq!(
            *recorded.wait_for(Option::is_some).await.unwrap(),
            Some(answer.clone())
        );
        assert_eq!(wait_consent(request.clone()).await, answer);
        assert!(f.filed.try_recv().is_err());
        assert!(f.withdrawn.try_recv().is_err());
        tokio::time::sleep_until(card.deadline + Duration::from_millis(1)).await;
        assert!(!CARDS
            .lock()
            .unwrap()
            .contains_key(&CardKey(request.scope, request.request_id)));
    }
}

#[tokio::test(start_paused = true)]
async fn a_clientless_secret_card_is_withdrawn_at_the_vault_expiry_without_a_retry() {
    let mut f = Fixture::new();
    let request = request(1, 60_000);
    let caller = tokio::spawn(wait_consent(request.clone()));
    let _phone = f.card(&request).await;
    hang_up(caller).await;
    let card = shared(&request);
    tokio::time::advance(Duration::from_secs(59)).await;
    assert!(f.withdrawn.try_recv().is_err());
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), f.withdrawn.recv())
            .await
            .expect("the vault deadline ends the phone hold")
            .unwrap(),
        (request.request_id, CancellationReason::Expired)
    );
    assert!(tokio::time::Instant::now() >= card.deadline);
}

#[tokio::test(start_paused = true)]
async fn explicit_local_cancel_is_cached_as_cancelled_not_a_secret_decline_or_expiry() {
    let mut f = Fixture::new();
    let request = request(1, 600_000);
    let cancel = Cancellation::default();
    let caller = {
        let request = request.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move { wait_consent_with_code(request, Some(&cancel)).await })
    };
    let _phone = f.card(&request).await;
    cancel.cancel();
    hang_up(caller).await;
    assert_eq!(
        f.withdrawn.recv().await.unwrap(),
        (request.request_id.clone(), CancellationReason::Cancelled)
    );
    assert_eq!(wait_consent(request).await, ConsentOutcome::Cancelled);
    assert!(
        f.filed.try_recv().is_err(),
        "a withdrawn card is not silently refiled"
    );
}

#[tokio::test(start_paused = true)]
async fn shutdown_withdraws_clientless_secret_cards_before_returning() {
    let mut f = Fixture::new();
    let request = request(1, 600_000);
    let caller = tokio::spawn(wait_consent(request.clone()));
    let _phone = f.card(&request).await;
    hang_up(caller).await;
    assert!(f.withdrawn.try_recv().is_err());
    shutdown().await;
    assert_eq!(
        f.withdrawn.try_recv().unwrap(),
        (request.request_id, CancellationReason::Cancelled)
    );
    assert!(CARDS.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn secret_card_cap_withdraws_only_the_oldest_unanswered_clientless_card() {
    let mut f = Fixture::new();
    let first = request(0, 600_000);
    let live = tokio::spawn(wait_consent(first.clone()));
    let live_phone = f.card(&first).await;
    let answered = request(1, 600_000);
    let done = tokio::spawn(wait_consent(answered.clone()));
    f.card(&answered)
        .await
        .send(ConsentOutcome::Declined)
        .unwrap();
    assert_eq!(done.await.unwrap(), ConsentOutcome::Declined);

    let mut phones = Vec::new();
    for index in 2..=MAX_CLIENTLESS_CARDS + 2 {
        tokio::time::advance(Duration::from_millis(1)).await;
        let request = request(index, 600_000);
        let caller = tokio::spawn(wait_consent(request.clone()));
        phones.push(f.card(&request).await);
        hang_up(caller).await;
        if index <= MAX_CLIENTLESS_CARDS + 1 {
            assert!(
                f.withdrawn.try_recv().is_err(),
                "at most 32 cards need no eviction"
            );
        }
    }
    assert_eq!(
        f.withdrawn.recv().await.unwrap(),
        (request(2, 600_000).request_id, CancellationReason::Expired)
    );
    assert!(f.withdrawn.try_recv().is_err());
    assert_eq!(wait_consent(answered).await, ConsentOutcome::Declined);
    live_phone.send(ConsentOutcome::Answered).unwrap();
    assert_eq!(live.await.unwrap(), ConsentOutcome::Answered);
    assert_eq!(
        CARDS
            .lock()
            .unwrap()
            .values()
            .filter(|card| card.is_clientless())
            .count(),
        MAX_CLIENTLESS_CARDS
    );
}
