//! Serialize approval waits for a label without caching any released material.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use super::{ApiKeyTarget, SecretConsentRequest, SecretScope, SecretVault, UseAnswer};

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

#[derive(Default)]
struct History {
    filed: Option<(String, u64)>,
    used_after_card: bool,
}
#[derive(Default)]
struct Slot {
    history: tokio::sync::Mutex<History>,
    expires: AtomicU64,
}
type SlotKey = (String, String, u64, String);
static SLOTS: LazyLock<Mutex<HashMap<SlotKey, Arc<Slot>>>> = LazyLock::new(Mutex::default);

pub(crate) struct RequestSecrets {
    vault: Arc<dyn SecretVault>,
    scope: SecretScope,
    context: Option<(std::path::PathBuf, String)>,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    status: Mutex<BTreeMap<String, (u64, Vec<String>)>>,
    sequence: AtomicU64,
    clock: Arc<dyn ApprovalClock>,
    active: Arc<std::sync::atomic::AtomicUsize>,
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
            slots: Mutex::default(),
            status: Mutex::default(),
            sequence: AtomicU64::new(1),
            clock: Arc::new(WallClock),
            active: Arc::default(),
        }
    }

    fn slot(&self, label: &str) -> Arc<Slot> {
        let mut local = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        local
            .entry(label.into())
            .or_insert_with(|| {
                let mut slots = SLOTS
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                slots.retain(|_, slot| {
                    Arc::strong_count(slot) > 1
                        || slot.expires.load(Ordering::Acquire) > self.clock.now_ms()
                });
                let key = (
                    self.scope.owner().into(),
                    self.scope.reference().into(),
                    self.scope.stamp().scope_epoch,
                    label.to_owned(),
                );
                if let Some(slot) = slots.get(&key) {
                    return slot.clone();
                }
                let slot = Arc::new(Slot::default());
                slots.insert(key, slot.clone());
                slot
            })
            .clone()
    }

    pub(crate) fn status(&self) -> Vec<String> {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .flat_map(|(_, lines)| lines)
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

    pub(crate) async fn use_for_request(
        &self,
        label: &str,
        target: &ApiKeyTarget,
    ) -> Result<UseAnswer, ()> {
        struct Active(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Active {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        self.active.fetch_add(1, Ordering::AcqRel);
        let _active = Active(self.active.clone());
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let result = tokio::select! {
            result = self.use_serial(label, target, sequence) => result,
            () = self.clock.deadline() => Err(()),
        };
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if status
            .get(label)
            .is_some_and(|(owner, _)| *owner == sequence)
        {
            status.remove(label);
        }
        result
    }

    async fn ask(&self, label: &str, target: &ApiKeyTarget) -> UseAnswer {
        let (vault, scope, label, target) = (
            self.vault.clone(),
            self.scope.clone(),
            label.to_owned(),
            target.clone(),
        );
        let (send, receive) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = send.send(vault.use_secret(&scope, &label, &target));
        });
        receive.await.unwrap_or(UseAnswer::Unavailable)
    }

    async fn use_serial(
        &self,
        label: &str,
        target: &ApiKeyTarget,
        sequence: u64,
    ) -> Result<UseAnswer, ()> {
        let slot = self.slot(label);
        let mut history = slot.history.lock().await;
        if history
            .filed
            .as_ref()
            .is_some_and(|(_, expires)| *expires <= self.clock.now_ms())
        {
            history.filed = None;
        }
        let answer = self.ask(label, target).await;
        let UseAnswer::ApprovalRequired(pending) = answer else {
            return Ok(answer);
        };
        if history
            .filed
            .as_ref()
            .is_some_and(|(id, _)| id == &pending.request_id)
        {
            return Err(());
        }
        let Some((project_root, session)) = &self.context else {
            return Err(());
        };
        let request = SecretConsentRequest {
            scope: self.scope.clone(),
            project_root: project_root.clone(),
            session: session.clone(),
            label: label.into(),
            request_id: pending.request_id.clone(),
            statement_b64url: pending.statement_b64url,
        };
        let mut lines = vec![format!("secret {label}: waiting for approval on the phone for {} (requests wait up to 10 minutes)", target.host)];
        if history.used_after_card {
            lines.push(format!(
                "secret {label}: the approval covered one request; another card was sent"
            ));
        }
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(label.into(), (sequence, lines));
        history.filed = Some((pending.request_id.clone(), pending.expires_at_ms));
        slot.expires.store(pending.expires_at_ms, Ordering::Release);
        let (send, receive) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = send.send(super::consent::hold_consent(&request));
        });
        if receive.await.ok() != Some(super::ConsentOutcome::Answered) {
            return Err(());
        }
        let answer = self.ask(label, target).await;
        match answer {
            UseAnswer::ApprovalRequired(_) => Err(()),
            UseAnswer::Released(_) => {
                history.used_after_card = true;
                Ok(answer)
            }
            _ => Ok(answer),
        }
    }
}
