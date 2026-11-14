//! Consumer identity readiness shared with the daemon registration channel.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use subc_client_rs::registration::ClaimError;
use subc_client_rs::{ConsumerError, ConsumerOptions, SubcConsumer};
use tokio::sync::Notify;

const READY_BUDGET: Duration = Duration::from_secs(5);

static REGISTRATION: OnceLock<IdentityReady> = OnceLock::new();

#[derive(Default)]
struct IdentityReady {
    installed: AtomicBool,
    changed: Notify,
}

impl IdentityReady {
    fn install(&self) {
        self.installed.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }

    fn is_ready(&self) -> bool {
        self.installed.load(Ordering::Acquire)
    }

    async fn wait(&self, budget: Duration) -> Result<(), ClaimError> {
        tokio::time::timeout(budget, async {
            // Subscribe before checking state so installation cannot be missed.
            let changed = self.changed.notified();
            if !self.is_ready() {
                changed.await;
            }
        })
        .await
        .map_err(|_| ClaimError::Unavailable)
    }
}

pub fn identity_ready() -> bool {
    REGISTRATION
        .get()
        .map_or_else(crate::launch_nonce::was_captured, IdentityReady::is_ready)
}

pub fn module_id() -> Option<String> {
    std::env::var(subc_protocol::SUBC_MODULE_ID_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Can use module identity now, or wait for the minter on the consumer path.
/// Preserve the legacy gate for processes not launched with module identity.
pub fn module_consumer_allowed() -> bool {
    module_id().is_some() && (REGISTRATION.get().is_some() || identity_ready())
}

pub async fn connect_consumer(
    path: &Path,
    options: ConsumerOptions,
) -> Result<SubcConsumer, ConsumerError> {
    if let Some(ready) = REGISTRATION.get() {
        ready
            .wait(READY_BUDGET)
            .await
            .map_err(|error| ConsumerError::Connect {
                path: path.to_path_buf(),
                endpoint: "claim binding".into(),
                source: std::io::Error::other(error),
            })?;
    }
    SubcConsumer::connect(path, options).await
}

/// A daemon registration channel was adopted; consumers must wait for its minter.
pub fn channel_adopted() {
    REGISTRATION.get_or_init(IdentityReady::default);
}

/// The daemon-backed claim minter is ready to serve waiting consumers.
pub fn minter_installed() {
    REGISTRATION.get_or_init(IdentityReady::default).install();
}
