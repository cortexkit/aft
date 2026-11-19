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

#[cfg(any(test, feature = "test-support"))]
pub static TEST_TRANSPORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    /// The vault's approval expiry for a shared card. Cingulate reads this
    /// from the signed statement instead of from a separate field.
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
    /// The card was withdrawn; this is not a phone answer.
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

    /// Discard transport state for an expired vault request.
    fn forget_expired(
        &self,
        _request: &SecretConsentRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(std::future::ready(()))
    }
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
    drop(replace_transport(transport));
}

fn replace_transport(transport: Option<std::sync::Arc<dyn SecretConsentTransport>>) -> Cards {
    let mut installed = TRANSPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut cards = CARDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *installed = transport;
    for card in cards
        .values()
        .filter(|card| card.outcome.borrow().is_none())
    {
        card.cancel.cancel();
    }
    std::mem::take(&mut *cards)
}

pub async fn shutdown() {
    let cards = replace_transport(None);
    // Give all withdrawals the same bounded window, rather than waiting a
    // separate RPC timeout for each card before the daemon exits.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        for card in cards.values() {
            let mut outcome = card.outcome.subscribe();
            let _ = outcome.wait_for(Option::is_some).await;
        }
    })
    .await;
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
pub struct Cancellation(tokio::sync::watch::Sender<Option<CancellationReason>>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancellationReason {
    Expired,
    Cancelled,
}

impl CancellationReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
        }
    }
}

impl Default for Cancellation {
    fn default() -> Self {
        Self(tokio::sync::watch::channel(None).0)
    }
}
impl Cancellation {
    pub fn cancel(&self) {
        self.cancel_with_reason(CancellationReason::Cancelled);
    }
    pub fn cancel_with_reason(&self, reason: CancellationReason) {
        // Preserve the first cause when a deadline and a scope revocation race.
        self.0.send_if_modified(|current| {
            if current.is_none() {
                *current = Some(reason);
                true
            } else {
                false
            }
        });
    }
    pub fn reason(&self) -> Option<CancellationReason> {
        *self.0.borrow()
    }
    pub fn is_cancelled(&self) -> bool {
        self.reason().is_some()
    }
    pub async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        loop {
            if receiver.borrow_and_update().is_some() {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

// The vault deduplicates within the stamped authorization scope. Ending one
// run must not withdraw a card another run can use under that same authority.
#[derive(Clone)]
pub struct CardKey(pub SecretScope, pub String);

impl CardKey {
    fn identity(&self) -> (&str, &str, u64, &str) {
        (
            self.0.owner(),
            self.0.reference(),
            self.0.stamp().scope_epoch,
            &self.1,
        )
    }
}

impl PartialEq for CardKey {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
}

impl Eq for CardKey {}

impl std::hash::Hash for CardKey {
    fn hash<H: std::hash::Hasher>(&self, hasher: &mut H) {
        std::hash::Hash::hash(&self.identity(), hasher);
    }
}

type Cards = std::collections::HashMap<CardKey, std::sync::Arc<SharedCard>>;
const MAX_CLIENTLESS_CARDS: usize = 32;
const MAX_CARD_LIFETIME_MS: u64 = 600_000;

struct SharedCard {
    created: tokio::time::Instant,
    statement: String,
    expires: u64,
    deadline: tokio::time::Instant,
    outcome: tokio::sync::watch::Sender<Option<ConsentOutcome>>,
    outcome_code: std::sync::Mutex<Option<&'static str>>,
    cancel: Cancellation,
    waiters: std::sync::atomic::AtomicUsize,
}

impl SharedCard {
    fn is_pending(&self) -> bool {
        !self.cancel.is_cancelled() && self.outcome.borrow().is_none()
    }

    fn is_clientless(&self) -> bool {
        self.waiters.load(std::sync::atomic::Ordering::Acquire) == 0 && self.is_pending()
    }
}

pub fn pending_scope_stamps() -> Vec<subc_protocol::scope::ScopeStamp> {
    CARDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|(_, card)| card.is_pending())
        .map(|(key, _)| key.0.stamp().clone())
        .collect()
}

pub fn revoke_scope(stamp: &subc_protocol::scope::ScopeStamp) {
    let cards = CARDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (key, card) in cards.iter() {
        let held = key.0.stamp();
        if held.owner == stamp.owner
            && held.scope_ref == stamp.scope_ref
            && held.scope_epoch == stamp.scope_epoch
            && card.is_pending()
        {
            card.cancel.cancel();
        }
    }
}

pub fn record_outcome_code(scope: &SecretScope, request_id: &str, code: &str) {
    let cards = CARDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(card) = cards.get(&CardKey(scope.clone(), request_id.to_owned())) {
        *card
            .outcome_code
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(super::transport::diagnostics::safe_code(code));
    }
}
static CARDS: std::sync::LazyLock<std::sync::Mutex<Cards>> =
    std::sync::LazyLock::new(Default::default);

struct Waiter<'a> {
    card: std::sync::Arc<SharedCard>,
    request_id: &'a str,
    local_cancel: Option<&'a Cancellation>,
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        // Joins and answer publication use the same lock as eviction, so a
        // joining waiter or a recorded phone answer rules out eviction.
        let cards = CARDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let last = self
            .card
            .waiters
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
            == 1;
        if !self.card.is_pending() {
            return;
        }
        // A cancelled fetch tool call or its reply deadline still withdraws
        // the last waiter's card. An HTTP client closing its proxy socket does not.
        if let Some(reason) = self.local_cancel.and_then(Cancellation::reason) {
            if last {
                self.card.cancel.cancel_with_reason(reason);
            }
            return;
        }
        if last {
            let mut clientless = cards.values().filter(|card| card.is_clientless());
            if let Some(oldest) = clientless.next() {
                let (count, oldest) = clientless.fold((1, oldest), |(count, oldest), card| {
                    (
                        count + 1,
                        if card.created < oldest.created {
                            card
                        } else {
                            oldest
                        },
                    )
                });
                if count > MAX_CLIENTLESS_CARDS {
                    oldest
                        .cancel
                        .cancel_with_reason(CancellationReason::Expired);
                }
            }
        }
        if !self.card.cancel.is_cancelled() {
            crate::slog_info!(
                "secret consent waiter dropped; card kept until expiry: request_id={:?} expires_at_ms={}",
                self.request_id,
                self.card.expires,
            );
        }
    }
}

fn scope_inventory_changed() {
    crate::subc::scope_watch::changed(crate::subc::scope_watch::Source::Parked);
}

async fn run_card(
    key: CardKey,
    card: std::sync::Arc<SharedCard>,
    transport: std::sync::Arc<dyn SecretConsentTransport>,
    request: SecretConsentRequest,
) {
    let wait = transport.clone().wait(request.clone(), card.cancel.clone());
    let expiry = tokio::time::sleep_until(card.deadline);
    tokio::pin!(wait, expiry);
    let answer = tokio::select! {
        biased;
        () = &mut expiry => {
            card.cancel.cancel_with_reason(CancellationReason::Expired);
            wait.await
        }
        answer = &mut wait => answer,
    };
    {
        let _cards = CARDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        card.outcome.send_replace(Some(answer));
    }
    scope_inventory_changed();
    expiry.await;
    transport.forget_expired(&request).await;
    let mut cards = CARDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if cards
        .get(&key)
        .is_some_and(|current| std::sync::Arc::ptr_eq(current, &card))
    {
        cards.remove(&key);
    }
}

/// Every HTTP request joins the same recorded card outcome without locking
/// another request's vault lookup or charging it another waiter's queue time.
#[rustfmt::skip]
test_support_visibility! {
#[cfg(any(test, feature = "test-support"))]
pub(crate) async fn wait_consent(request: SecretConsentRequest) -> ConsentOutcome {
    wait_consent_with_code(request, None).await.0
}
}

pub(crate) async fn wait_consent_with_code(
    mut request: SecretConsentRequest,
    local_cancel: Option<&Cancellation>,
) -> (ConsentOutcome, Option<&'static str>) {
    let card = {
        use std::collections::hash_map::Entry;
        let installed = TRANSPORT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(transport) = installed.clone() else {
            return (
                ConsentOutcome::Unavailable {
                    detail: "no secret consent transport is connected".into(),
                },
                None,
            );
        };
        let now = super::vault::now_ms();
        let expires = request
            .expires_at_ms
            .unwrap_or(u64::MAX)
            .min(now.saturating_add(MAX_CARD_LIFETIME_MS));
        if expires <= now {
            return (ConsentOutcome::Expired, None);
        }
        request.expires_at_ms = Some(expires);
        let mut cards = CARDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cards.retain(|_, card| {
            card.waiters.load(std::sync::atomic::Ordering::Acquire) != 0
                || !matches!(
                    &*card.outcome.borrow(),
                    Some(ConsentOutcome::Unavailable { .. } | ConsentOutcome::Indeterminate)
                )
        });
        let key = CardKey(request.scope.clone(), request.request_id.clone());
        let card = match cards.entry(key.clone()) {
            Entry::Occupied(entry) => {
                let card = entry.get();
                if card.statement != request.statement_b64url {
                    return (
                        ConsentOutcome::Unavailable {
                            detail: "Cingulate could not file the secret-use card".into(),
                        },
                        None,
                    );
                }
                if tokio::time::Instant::now() >= card.deadline {
                    return (ConsentOutcome::Expired, None);
                }
                if card.is_pending() {
                    crate::slog_info!(
                        "secret consent retry attached to existing card: request_id={:?}",
                        request.request_id,
                    );
                }
                card.clone()
            }
            Entry::Vacant(entry) => {
                let runtime = transport
                    .runtime()
                    .unwrap_or_else(tokio::runtime::Handle::current);
                let created = tokio::time::Instant::now();
                let card = std::sync::Arc::new(SharedCard {
                    created,
                    statement: request.statement_b64url.clone(),
                    expires,
                    deadline: created + std::time::Duration::from_millis(expires - now),
                    outcome: tokio::sync::watch::channel(None).0,
                    outcome_code: Default::default(),
                    cancel: Default::default(),
                    waiters: Default::default(),
                });
                entry.insert(card.clone());
                scope_inventory_changed();
                runtime.spawn(run_card(key, card.clone(), transport, request.clone()));
                card
            }
        };
        card.waiters
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        card
    };
    let _waiter = Waiter {
        card: card.clone(),
        request_id: &request.request_id,
        local_cancel,
    };
    let mut receiver = card.outcome.subscribe();
    loop {
        if let Some(answer) = receiver.borrow_and_update().clone() {
            let code = *card
                .outcome_code
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            return (answer, code);
        }
        if receiver.changed().await.is_err() {
            return (
                ConsentOutcome::Unavailable {
                    detail: "Cingulate could not settle the secret-use card".into(),
                },
                None,
            );
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
/// card's expiry itself from the statements' `request_expires_at_ms`.
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
