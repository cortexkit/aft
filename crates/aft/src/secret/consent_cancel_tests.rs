use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Condvar, Mutex,
};
use std::time::Duration;

#[derive(Default)]
struct Held {
    filed: AtomicUsize,
    withdrawn: Mutex<bool>,
    changed: Condvar,
}
impl SecretConsentTransport for Held {
    fn file(&self, _: &SecretConsentRequest) -> ConsentOutcome {
        self.filed.fetch_add(1, Ordering::SeqCst);
        let mut withdrawn = self.withdrawn.lock().unwrap();
        while !*withdrawn {
            withdrawn = self.changed.wait(withdrawn).unwrap();
        }
        // A compatibility transport can report withdrawal as Expired. The
        // coordinator still knows its own cancellation was not a phone answer.
        ConsentOutcome::Expired
    }
    fn withhold(&self, _: &SecretConsentRequest) -> ConsentOutcome {
        *self.withdrawn.lock().unwrap() = true;
        self.changed.notify_all();
        ConsentOutcome::Expired
    }
}
struct Restore(Arc<Held>);
impl Drop for Restore {
    fn drop(&mut self) {
        *self.0.withdrawn.lock().unwrap() = true;
        self.0.changed.notify_all();
        install_transport(None);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_shared_card_is_cached_as_cancelled_not_a_user_decline_or_expiry() {
    let _lock = TEST_TRANSPORT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let service = Arc::new(Held::default());
    let _restore = Restore(service.clone());
    install_transport(Some(service.clone()));
    let stamp: subc_protocol::scope::ScopeStamp = serde_json::from_value(serde_json::json!({
        "owner":{"kind":"reserved","module_id":"prefrontal-core"},
        "kind":"head", "ref":"cancel-cache", "scope_epoch":1, "owner_authorized":true
    }))
    .unwrap();
    let request = SecretConsentRequest {
        scope: super::super::SecretScope::from_stamp(&stamp),
        project_root: "project".into(),
        session: "session".into(),
        label: "key".into(),
        request_id: "01HF7YAT000000000000000001".into(),
        statement_b64url: "c3RhdGVtZW50".into(),
        expires_at_ms: Some(super::super::vault::now_ms() + 600_000),
    };
    let caller = tokio::spawn(wait_consent(request.clone()));
    tokio::time::timeout(Duration::from_secs(2), async {
        while service.filed.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    caller.abort();
    let _ = caller.await;
    let key = (request.scope.clone(), request.request_id.clone());
    let card = CARDS.lock().unwrap().get(&key).unwrap().clone();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if card.outcome.borrow().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(*card.outcome.borrow(), Some(ConsentOutcome::Cancelled));
    assert_eq!(wait_consent(request).await, ConsentOutcome::Cancelled);
    assert_eq!(
        service.filed.load(Ordering::SeqCst),
        1,
        "a withdrawn card is not silently refiled as a fresh phone request"
    );
}
