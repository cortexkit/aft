//! Request-addressed vault use and shared, cancellable approval cards.

use super::{
    ApiKeyTarget, ApprovalRequest, ConsentOutcome, SecretConsentRequest, SecretScope, SecretVault,
    UseAnswer,
};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

test_support_visibility! {
/// The phone's approval window is independent of upstream network deadlines.
pub(crate) trait ApprovalClock: Send + Sync {
    fn now_ms(&self) -> u64;
    fn deadline(&self) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}
}
struct WallClock;
impl ApprovalClock for WallClock {
    fn now_ms(&self) -> u64 {
        super::vault::now_ms()
    }
    fn deadline(&self) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(tokio::time::sleep(Duration::from_secs(600)))
    }
}

/// One absolute person-wait limit, shared by every label in an HTTP request.
pub(crate) struct RequestBudget {
    pub(crate) expires_at_ms: u64,
    deadline: Pin<Box<dyn Future<Output = ()> + Send>>,
}
impl RequestBudget {
    pub(crate) async fn expired(&mut self) {
        self.deadline.as_mut().await;
    }
}
#[rustfmt::skip]
test_support_visibility! {
#[derive(Clone, Copy, Debug)]
pub(crate) enum UseError {
    Declined,
    Expired,
    Cancelled,
    Unavailable,
    Indeterminate,
    InvalidPlan(&'static str),
}
}

#[derive(Clone, Debug)]
pub(crate) struct ApprovalMetadata {
    pub(crate) request_id: String,
    pub(crate) outcome_code: Option<&'static str>,
    pub(crate) granted: bool,
}

/// The approved local refusal text, shared by fetches and proxy requests.
pub(crate) fn consent_failure_message(label: &str, host: &str, reason: UseError) -> String {
    match reason {
        UseError::Declined => format!("secret {label}: approval was declined for {host}; the request was not sent"),
        UseError::Expired => format!("secret {label}: approval expired for {host}; the request was not sent"),
        UseError::Cancelled => format!("secret {label}: the request for {host} was cancelled; the request was not sent"),
        _ => format!("secret {label}: the approval request for {host} could not be filed; the request was not sent"),
    }
}

#[rustfmt::skip]
test_support_visibility! {
/// Shared progress for sockets whose idle budget excludes approval waits.
#[derive(Default)]
pub(crate) struct RequestActivity {
    waiting: AtomicUsize,
    last_activity: Mutex<Option<tokio::time::Instant>>,
}
}
impl RequestActivity {
    test_support_visibility! {
    pub(crate) fn begin_wait(&self) {
        self.waiting.fetch_add(1, Ordering::AcqRel);
    }
    }

    test_support_visibility! {
    pub(crate) fn finish_wait(&self) {
        // Publish progress before the count can reach zero, so a socket never
        // resumes against the deadline from before the approval completed.
        self.record_activity();
        self.waiting.fetch_sub(1, Ordering::AcqRel);
    }
    }

    test_support_visibility! {
    pub(crate) fn record_activity(&self) {
        let mut last = self
            .last_activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *last = Some(tokio::time::Instant::now());
    }
    }

    test_support_visibility! {
    pub(crate) fn snapshot(&self) -> (bool, Option<tokio::time::Instant>) {
        // Read the count first: observing the last waiter leave also observes
        // its previously published completion timestamp.
        let waiting = self.waiting.load(Ordering::Acquire) != 0;
        let last = *self
            .last_activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (waiting, last)
    }
    }
}

test_support_visibility! {
pub(crate) struct RequestSecrets {
    vault: Arc<dyn SecretVault>,
    scope: SecretScope,
    context: Option<(std::path::PathBuf, String)>,
    status: Mutex<BTreeMap<u64, Vec<String>>>,
    sequence: AtomicU64,
    clock: Arc<dyn ApprovalClock>,
    activity: Arc<RequestActivity>,
    once: Mutex<BTreeMap<String, bool>>,
    request_cancelled: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}
}
struct Waiting<'a> {
    owner: &'a RequestSecrets,
    sequence: u64,
}
impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.owner
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.sequence);
        self.owner.activity.finish_wait();
    }
}
impl RequestSecrets {
    #[rustfmt::skip]
    test_support_visibility! {
    pub(crate) fn new(
        vault: Arc<dyn SecretVault>,
        scope: SecretScope,
        context: Option<(std::path::PathBuf, String)>,
    ) -> Self {
        Self {
            vault,
            scope,
            context,
            status: Mutex::default(),
            sequence: AtomicU64::new(1),
            clock: Arc::new(WallClock),
            activity: Arc::default(),
            once: Mutex::default(),
            request_cancelled: None,
        }
    }
    }
    pub(crate) fn with_request_cancellation(
        mut self,
        cancelled: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        self.request_cancelled = Some(Arc::new(cancelled));
        self
    }

    fn cancelled(&self) -> bool {
        self.request_cancelled.as_ref().is_some_and(|check| check())
    }
    pub(crate) fn budget(&self) -> RequestBudget {
        RequestBudget {
            expires_at_ms: self.clock.now_ms().saturating_add(600_000),
            deadline: self.clock.deadline(),
        }
    }
    #[rustfmt::skip]
    test_support_visibility! {
    #[cfg(unix)]
    pub(crate) fn status(&self) -> Vec<String> {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .flatten()
            .cloned()
            .collect()
    }
    }
    #[rustfmt::skip]
    test_support_visibility! {
    pub(crate) fn activity(&self) -> Arc<RequestActivity> {
        self.activity.clone()
    }
    }
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn with_clock(mut self, clock: Option<Arc<dyn ApprovalClock>>) -> Self {
        if let Some(clock) = clock {
            self.clock = clock;
        }
        self
    }

    pub(crate) async fn ask(&self, label: &str, target: &ApiKeyTarget) -> UseAnswer {
        let (vault, scope, label, target) = (
            self.vault.clone(),
            self.scope.clone(),
            label.to_owned(),
            target.clone(),
        );
        let cancelled = self.request_cancelled.clone();
        tokio::task::spawn_blocking(move || {
            if cancelled.as_ref().is_some_and(|check| check()) {
                UseAnswer::Unavailable
            } else {
                vault.use_secret(&scope, &label, &target)
            }
        })
        .await
        .unwrap_or(UseAnswer::Unavailable)
    }

    #[rustfmt::skip]
    test_support_visibility! {
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) async fn approve(
        &self,
        label: &str,
        target: &ApiKeyTarget,
        pending: ApprovalRequest,
        expires_at_ms: u64,
    ) -> Result<ConsentOutcome, UseError> {
        self.approve_with_metadata(label, target, pending, expires_at_ms, &|_| {})
            .await
    }
}

    async fn approve_with_metadata(
        &self,
        label: &str,
        target: &ApiKeyTarget,
        pending: ApprovalRequest,
        expires_at_ms: u64,
        observe: &(impl Fn(ApprovalMetadata) + Sync),
    ) -> Result<ConsentOutcome, UseError> {
        let request_id = pending.request_id.clone();
        observe(ApprovalMetadata {
            request_id: request_id.clone(),
            outcome_code: None,
            granted: false,
        });
        let Some((project_root, session)) = &self.context else {
            return Err(UseError::Unavailable);
        };
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        self.activity.begin_wait();
        // Drop also runs when a client cancels, the command stops, or another
        // label refuses; none of those paths may leave a waiting status behind.
        let _waiting = Waiting {
            owner: self,
            sequence,
        };
        let mut lines = vec![format!(
            "secret {label}: waiting for your approval for {} (requests wait up to 10 minutes)",
            target.host
        )];
        if self
            .once
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(label)
            == Some(&true)
        {
            lines.push(format!(
                "secret {label}: the approval covered one request; another card was sent"
            ));
        }
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(sequence, lines);
        let (answer, outcome_code) = super::consent::wait_consent_with_code(SecretConsentRequest {
            scope: self.scope.clone(),
            project_root: project_root.clone(),
            session: session.clone(),
            label: label.into(),
            request_id: pending.request_id,
            statement_b64url: pending.statement_b64url,
            expires_at_ms: Some(expires_at_ms.min(pending.expires_at_ms)),
        })
        .await;
        observe(ApprovalMetadata {
            request_id,
            outcome_code,
            granted: matches!(
                answer,
                ConsentOutcome::Answered | ConsentOutcome::AnsweredOnce
            ),
        });
        match answer {
            ConsentOutcome::Answered | ConsentOutcome::AnsweredOnce => Ok(answer),
            ConsentOutcome::Declined => Err(UseError::Declined),
            ConsentOutcome::Expired => Err(UseError::Expired),
            ConsentOutcome::Cancelled => Err(UseError::Cancelled),
            ConsentOutcome::Indeterminate => Err(UseError::Indeterminate),
            _ => Err(UseError::Unavailable),
        }
    }

    /// The caller supplies a fresh verified connection before a releasing retry.
    /// No mutex is held while asking the vault or waiting for a person's answer.
    #[cfg(all(any(test, feature = "test-support"), unix))]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn use_in_request<F, Fut>(
        &self,
        label: &str,
        target: &ApiKeyTarget,
        expires_at_ms: u64,
        before_retry: F,
    ) -> Result<UseAnswer, UseError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<(), UseError>>,
    {
        self.use_planned_in_request(
            label,
            target,
            expires_at_ms,
            |_| Ok(()),
            |_| Ok(()),
            before_retry,
        )
        .await
    }

    pub(crate) async fn use_planned_in_request<F, Fut, P, O>(
        &self,
        label: &str,
        target: &ApiKeyTarget,
        expires_at_ms: u64,
        plan: P,
        observe: O,
        before_retry: F,
    ) -> Result<UseAnswer, UseError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<(), UseError>>,
        P: Fn(&str) -> Result<(), UseError>,
        O: Fn(&[u8]) -> Result<(), UseError>,
    {
        self.use_planned_with_metadata(
            label,
            target,
            expires_at_ms,
            plan,
            observe,
            before_retry,
            |_| {},
        )
        .await
    }

    pub(crate) async fn use_planned_with_metadata<F, Fut, P, O, M>(
        &self,
        label: &str,
        target: &ApiKeyTarget,
        expires_at_ms: u64,
        plan: P,
        observe: O,
        mut before_retry: F,
        metadata: M,
    ) -> Result<UseAnswer, UseError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<(), UseError>>,
        P: Fn(&str) -> Result<(), UseError>,
        O: Fn(&[u8]) -> Result<(), UseError>,
        M: Fn(ApprovalMetadata) + Sync,
    {
        let mut answered = std::collections::HashSet::new();
        let mut once = false;
        loop {
            if self.cancelled() {
                return Err(UseError::Unavailable);
            }
            let answer = self.ask(label, target).await;
            if self.cancelled() {
                return Err(UseError::Unavailable);
            }
            let UseAnswer::ApprovalRequired(pending) = answer else {
                if let UseAnswer::Released(released) = &answer {
                    observe(released.material())?;
                    plan(&released.location)?;
                }
                if matches!(answer, UseAnswer::Released(_)) {
                    self.once
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(label.into(), once);
                }
                return Ok(answer);
            };
            // An allowed card that still cannot release fails closed, but only
            // recorded decline/expiry (or the request deadline) means denial.
            if !answered.insert(pending.request_id.clone()) {
                return Err(UseError::Unavailable);
            }
            // A pending statement already names the location. Check its plan
            // before asking the person or consuming a subsequent release.
            use base64::Engine;
            if let Ok(bytes) =
                base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&pending.statement_b64url)
            {
                if let Ok(statement) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    if let Some(location) = statement["use"]["location"].as_str() {
                        plan(location)?;
                    }
                }
            }
            if once {
                self.once
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(label.into(), true);
            }
            once = self
                .approve_with_metadata(label, target, pending, expires_at_ms, &metadata)
                .await?
                == ConsentOutcome::AnsweredOnce;
            before_retry().await?;
        }
    }
}
