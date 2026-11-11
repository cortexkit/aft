//! Request-addressed vault use and shared, cancellable approval cards.

use super::{
    ApiKeyTarget, ApprovalRequest, ConsentOutcome, SecretConsentRequest, SecretScope, SecretVault,
    UseAnswer,
};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The phone's approval window is independent of upstream network deadlines.
pub(crate) trait ApprovalClock: Send + Sync {
    fn now_ms(&self) -> u64;
    fn deadline(&self) -> Pin<Box<dyn Future<Output = ()> + Send>>;
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
#[derive(Clone, Copy, Debug)]
pub(crate) enum UseError {
    NotApproved,
    Unavailable,
    InvalidPlan(&'static str),
}

pub(crate) struct RequestSecrets {
    vault: Arc<dyn SecretVault>,
    scope: SecretScope,
    context: Option<(std::path::PathBuf, String)>,
    status: Mutex<BTreeMap<u64, Vec<String>>>,
    sequence: AtomicU64,
    clock: Arc<dyn ApprovalClock>,
    active: Arc<std::sync::atomic::AtomicUsize>,
    once: Mutex<BTreeMap<String, bool>>,
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
        self.owner.active.fetch_sub(1, Ordering::AcqRel);
    }
}
impl RequestSecrets {
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
            active: Arc::default(),
            once: Mutex::default(),
        }
    }
    pub(crate) fn budget(&self) -> RequestBudget {
        RequestBudget {
            expires_at_ms: self.clock.now_ms().saturating_add(600_000),
            deadline: self.clock.deadline(),
        }
    }
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
    pub(crate) fn activity(&self) -> Arc<std::sync::atomic::AtomicUsize> {
        self.active.clone()
    }
    #[cfg(test)]
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
        tokio::task::spawn_blocking(move || vault.use_secret(&scope, &label, &target))
            .await
            .unwrap_or(UseAnswer::Unavailable)
    }

    pub(crate) async fn approve(
        &self,
        label: &str,
        target: &ApiKeyTarget,
        pending: ApprovalRequest,
        expires_at_ms: u64,
    ) -> Result<ConsentOutcome, UseError> {
        let Some((project_root, session)) = &self.context else {
            return Err(UseError::Unavailable);
        };
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        self.active.fetch_add(1, Ordering::AcqRel);
        // Drop also runs when a client cancels, the command stops, or another
        // label refuses; none of those paths may leave a waiting status behind.
        let _waiting = Waiting {
            owner: self,
            sequence,
        };
        let mut lines = vec![format!("secret {label}: waiting for approval on the phone for {} (requests wait up to 10 minutes)", target.host)];
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
        let answer = super::consent::wait_consent(SecretConsentRequest {
            scope: self.scope.clone(),
            project_root: project_root.clone(),
            session: session.clone(),
            label: label.into(),
            request_id: pending.request_id,
            statement_b64url: pending.statement_b64url,
            expires_at_ms: Some(expires_at_ms.min(pending.expires_at_ms)),
        })
        .await;
        match answer {
            ConsentOutcome::Answered | ConsentOutcome::AnsweredOnce => Ok(answer),
            ConsentOutcome::Declined | ConsentOutcome::Expired => Err(UseError::NotApproved),
            _ => Err(UseError::Unavailable),
        }
    }

    /// The caller supplies a fresh verified connection before a releasing retry.
    /// No mutex is held while asking the vault or waiting for a person's answer.
    #[cfg(all(test, unix))]
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
        mut before_retry: F,
    ) -> Result<UseAnswer, UseError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<(), UseError>>,
        P: Fn(&str) -> Result<(), UseError>,
        O: Fn(&[u8]) -> Result<(), UseError>,
    {
        let mut answered = std::collections::HashSet::new();
        let mut once = false;
        loop {
            let answer = self.ask(label, target).await;
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
            once = self.approve(label, target, pending, expires_at_ms).await?
                == ConsentOutcome::AnsweredOnce;
            before_retry().await?;
        }
    }
}
