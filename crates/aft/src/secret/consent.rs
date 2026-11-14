//! The secret consent entry point AFT's own tools dispatch to.
//!
//! A secret consent is not a filesystem permission. It is filed for every
//! eligible scope, including a bound codemode call, and it carries the vault's
//! own request id and statement so the phone screen can show what the vault
//! asked. Cingulate's allow alone grants nothing: the caller resolves again
//! afterwards, and only a vault grant allows a substitution.
//!
//! Secret questions share the Cingulate transport with filesystem permission
//! asks, but never become filesystem `AccessGrant`s. In particular, a bound
//! codemode call may ask about a secret under its head's authorization scope
//! even though it may not file a filesystem permission ask.

use crate::secret::SecretScope;

#[cfg(test)]
pub(crate) static TEST_TRANSPORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The consent kind a secret ask files under.
pub const SECRET_CONSENT_KIND: &str = "secret_use";

/// What the consent adapter is asked to file for one label.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretConsentRequest {
    /// The authorization scope the ask is filed under.
    pub scope: SecretScope,
    /// The originating route's project root, used only for the scoped transport.
    pub project_root: std::path::PathBuf,
    /// The originating route's session, used only for the scoped transport.
    pub session: String,
    /// The label the vault is waiting on.
    pub label: String,
    /// The vault's identifier for the pending decision.
    pub request_id: String,
    /// The vault's statement bytes, base64url without padding.
    pub statement_b64url: String,
    /// Bound the card by the HTTP request as well as the vault statement.
    pub expires_at_ms: Option<u64>,
}

impl std::fmt::Debug for SecretConsentRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretConsentRequest")
            .field("scope", &self.scope)
            .field("label", &self.label)
            .field("request_id", &self.request_id)
            .field("statement_b64url_len", &self.statement_b64url.len())
            .finish()
    }
}

/// What happened to a filed secret consent.
#[derive(Clone, PartialEq, Eq)]
pub enum ConsentOutcome {
    /// Cingulate allowed this request. Retry the vault once; only its release
    /// authorizes substitution.
    Answered,
    /// One-use approval. Preserve this distinction so a later card can be
    /// announced as replacing a consumed one-use approval.
    AnsweredOnce,
    Declined,
    Expired,
    /// The caller left or the card was withdrawn; this is not a phone answer.
    Cancelled,
    /// The card was answered, but no trustworthy decision could be read.
    /// This must not be retained as a person's answer or a filing failure.
    Indeterminate,
    /// Still pending when the call's hold ended: the call ends
    /// `access_pending` and the retried call asks again.
    Pending,
    /// The ask could not be filed or awaited: the call ends
    /// `access_unavailable`.
    Unavailable {
        /// Plain-text detail with no key material.
        detail: String,
    },
}

impl std::fmt::Debug for ConsentOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConsentOutcome::Answered => f.write_str("Answered"),
            ConsentOutcome::AnsweredOnce => f.write_str("AnsweredOnce"),
            ConsentOutcome::Declined => f.write_str("Declined"),
            ConsentOutcome::Expired => f.write_str("Expired"),
            ConsentOutcome::Cancelled => f.write_str("Cancelled"),
            ConsentOutcome::Indeterminate => f.write_str("Indeterminate"),
            ConsentOutcome::Pending => f.write_str("Pending"),
            ConsentOutcome::Unavailable { detail } => f
                .debug_struct("Unavailable")
                .field("detail", detail)
                .finish(),
        }
    }
}

/// What a dispatcher is asked to do with one secret consent.
///
/// This is the seam between the adapter and the transport: the dispatcher files
/// the ask through the daemon's Cingulate client and reports what happened.
/// The trait exists so the outcome table can be tested without a daemon, and
/// so a later transport shape changes only the installed dispatcher.
pub trait SecretConsentTransport: Send + Sync + 'static {
    /// Keep cleanup on the daemon runtime, not a command's short-lived proxy.
    fn runtime(&self) -> Option<tokio::runtime::Handle> {
        None
    }
    /// Await one shared card. Production observes cancellation while awaiting
    /// the Cingulate consent service and withdraws while the route is still
    /// bound to the card's authorization scope.
    fn wait(
        self: std::sync::Arc<Self>,
        request: SecretConsentRequest,
        cancel: Cancellation,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ConsentOutcome> + Send>> {
        Box::pin(async move {
            let worker = self.clone();
            let ask = request.clone();
            let held_cancel = cancel.clone();
            let held = tokio::task::spawn_blocking(move || {
                if held_cancel.is_cancelled() {
                    ConsentOutcome::Cancelled
                } else {
                    worker.hold(&ask, &held_cancel)
                }
            });
            let outcome = tokio::select! {
                biased;
                () = cancel.cancelled() => ConsentOutcome::Cancelled,
                result = held => result.unwrap_or(ConsentOutcome::Unavailable { detail: "Cingulate could not settle the secret-use card".into() }),
            };
            if matches!(
                outcome,
                ConsentOutcome::Cancelled
                    | ConsentOutcome::Unavailable { .. }
                    | ConsentOutcome::Indeterminate
            ) {
                let _ = tokio::task::spawn_blocking(move || self.withhold(&request)).await;
            }
            outcome
        })
    }
    /// File `request` and wait for the answer, or for the hold to end.
    fn file(&self, request: &SecretConsentRequest) -> ConsentOutcome;

    /// File once and hold a proxy request for the full approval window.
    fn hold(&self, request: &SecretConsentRequest, cancel: &Cancellation) -> ConsentOutcome {
        if cancel.is_cancelled() {
            ConsentOutcome::Cancelled
        } else {
            self.file(request)
        }
    }

    /// Settle a pending ask the call left behind.
    fn withhold(&self, request: &SecretConsentRequest) -> ConsentOutcome;
}

/// The dispatcher installed for this process.
///
/// `None` means no secret consent transport is connected. A pending vault
/// answer then ends the call with `access_unavailable`, never a silent start.
static TRANSPORT: std::sync::LazyLock<
    std::sync::Mutex<Option<std::sync::Arc<dyn SecretConsentTransport>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

/// Install the secret consent transport.
pub fn install_transport(transport: Option<std::sync::Arc<dyn SecretConsentTransport>>) {
    CARDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    *TRANSPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = transport;
}

fn transport() -> Option<std::sync::Arc<dyn SecretConsentTransport>> {
    TRANSPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// File `request` and report what happened.
///
/// The caller has already decided the asking call is eligible, so the scope in
/// `request` is the one the daemon stamped.
pub fn file_consent(request: &SecretConsentRequest) -> ConsentOutcome {
    match transport() {
        Some(transport) => transport.file(request),
        None => ConsentOutcome::Unavailable {
            detail: "no secret consent transport is connected".into(),
        },
    }
}

/// Cancellation shared by a request's labels and its approval worker.
#[derive(Clone)]
pub struct Cancellation(tokio::sync::watch::Sender<bool>);
impl Default for Cancellation {
    fn default() -> Self {
        Self(tokio::sync::watch::channel(false).0)
    }
}
impl Cancellation {
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    pub async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        loop {
            if *receiver.borrow_and_update() {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

type CardKey = (SecretScope, String);
struct SharedCard {
    created: std::time::Instant,
    statement: String,
    expires: u64,
    outcome: tokio::sync::watch::Sender<Option<ConsentOutcome>>,
    cancel: Cancellation,
    waiters: std::sync::atomic::AtomicUsize,
}
static CARDS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<CardKey, std::sync::Arc<SharedCard>>>,
> = std::sync::LazyLock::new(Default::default);

struct Waiter(std::sync::Arc<SharedCard>);
impl Drop for Waiter {
    fn drop(&mut self) {
        // Join increments use this same lock: a joining waiter must be counted
        // before the last departure can cancel the shared phone card.
        let _cards = CARDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self
            .0
            .waiters
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
            == 1
            && self.0.outcome.borrow().is_none()
        {
            self.0.cancel.cancel();
        }
    }
}

#[cfg(test)]
mod waiter_tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn a_last_waiter_cannot_decrement_or_cancel_while_a_join_holds_the_cards_lock() {
        let card = Arc::new(SharedCard {
            created: Instant::now(),
            statement: "statement".into(),
            expires: u64::MAX,
            outcome: tokio::sync::watch::channel(None).0,
            cancel: Default::default(),
            waiters: AtomicUsize::new(1),
        });
        let cards = CARDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let leaving = card.clone();
        let (started, start) = std::sync::mpsc::channel();
        let (finished, finish) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            drop(Waiter(leaving));
            finished.send(()).unwrap();
        });
        start.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            matches!(
                finish.recv_timeout(Duration::from_millis(100)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            "the decrement/cancel decision must wait for the joining critical section"
        );
        assert_eq!(card.waiters.load(Ordering::Acquire), 1);
        assert!(!card.cancel.is_cancelled());
        card.waiters.fetch_add(1, Ordering::AcqRel);
        drop(cards);
        finish.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        assert_eq!(card.waiters.load(Ordering::Acquire), 1);
        assert!(
            !card.cancel.is_cancelled(),
            "the already-counted join still owns the card"
        );
        drop(Waiter(card.clone()));
        assert!(card.cancel.is_cancelled());
    }
}

/// Every HTTP request joins the same recorded card outcome without locking
/// another request's vault lookup or charging it another waiter's queue time.
pub(crate) async fn wait_consent(request: SecretConsentRequest) -> ConsentOutcome {
    let Some(transport) = transport() else {
        return ConsentOutcome::Unavailable {
            detail: "no secret consent transport is connected".into(),
        };
    };
    let card = {
        let mut cards = CARDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cards.retain(|_, card| {
            if card.waiters.load(std::sync::atomic::Ordering::Acquire) == 0
                && matches!(
                    &*card.outcome.borrow(),
                    Some(ConsentOutcome::Unavailable { .. } | ConsentOutcome::Indeterminate)
                )
            {
                return false;
            }
            card.created.elapsed() < std::time::Duration::from_secs(600)
                || card.expires > super::vault::now_ms()
                || card.waiters.load(std::sync::atomic::Ordering::Acquire) > 0
        });
        let card = cards
            .entry((request.scope.clone(), request.request_id.clone()))
            .or_insert_with(|| {
                let card = std::sync::Arc::new(SharedCard {
                    created: std::time::Instant::now(),
                    statement: request.statement_b64url.clone(),
                    expires: request.expires_at_ms.unwrap_or(u64::MAX),
                    outcome: tokio::sync::watch::channel(None).0,
                    cancel: Default::default(),
                    waiters: Default::default(),
                });
                let worker = card.clone();
                let request = request.clone();
                transport
                    .runtime()
                    .unwrap_or_else(tokio::runtime::Handle::current)
                    .spawn(async move {
                        let answer = transport.wait(request, worker.cancel.clone()).await;
                        worker.outcome.send_replace(Some(answer));
                    });
                card
            })
            .clone();
        card.waiters
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        card
    };
    let _waiter = Waiter(card.clone());
    if card.statement != request.statement_b64url {
        return ConsentOutcome::Unavailable {
            detail: "Cingulate could not file the secret-use card".into(),
        };
    }
    let mut receiver = card.outcome.subscribe();
    loop {
        if let Some(answer) = receiver.borrow_and_update().clone() {
            return answer;
        }
        if receiver.changed().await.is_err() {
            return ConsentOutcome::Unavailable {
                detail: "Cingulate could not settle the secret-use card".into(),
            };
        }
    }
}

/// Settle a pending secret ask.
pub fn withhold_consent(request: &SecretConsentRequest) -> ConsentOutcome {
    match transport() {
        Some(transport) => transport.withhold(request),
        None => ConsentOutcome::Unavailable {
            detail: "no secret consent transport is connected".into(),
        },
    }
}

/// The `consent.request` parameter shape a secret ask carries.
///
/// Cingulate renders the vault's statement, not filer-authored prose. Forward
/// exactly one item without decoding or re-encoding the statement; the daemon
/// carries the authorization stamp outside this body.
///
/// The body is exactly `{kind, secret_use}`: Cingulate decodes secret-use
/// filings with unknown fields refused (`envelope_invalid`), and sets the
/// card's expiry itself from the statements' `request_expires_at_ms`. The
/// local hold still ends at `expires_at_ms`, and an abandoned card is withdrawn.
pub fn request_params(request: &SecretConsentRequest) -> serde_json::Value {
    serde_json::json!({
        "kind": SECRET_CONSENT_KIND,
        "secret_use": {
            "items": [{
                "request_id": request.request_id,
                "statement_b64url": request.statement_b64url,
            }],
        },
    })
}

#[cfg(test)]
#[path = "consent_cancel_tests.rs"]
mod cancellation_cache_tests;
