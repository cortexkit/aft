use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use subc_client_rs::registration::{self, ClaimError, ClaimMinter};
use subc_client_rs::{ConsumerError, ConsumerOptions, ModuleStream, SubcConsumer};
use subc_protocol::claim::{ClaimMintReply, ClaimMintRequest, ClaimRateLimitedDetail};
use subc_protocol::{error_codes, ErrorBody, FrameType};
use subc_transport::{authenticate_client, connection_file};
use tokio::sync::Notify;
use tokio_util::sync::{CancellationToken, DropGuard};

use super::health::DispatchPathMetrics;
use super::readiness::{ControlError, DaemonControl};
use super::{SubcError, AUTH_DEADLINE};

const READY_BUDGET: Duration = Duration::from_secs(5);
const MINT_BUDGET: Duration = Duration::from_secs(5);

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

pub(crate) fn identity_ready() -> bool {
    REGISTRATION
        .get()
        .map_or_else(crate::launch_nonce::was_captured, IdentityReady::is_ready)
}

pub(crate) fn module_id() -> Option<String> {
    std::env::var(subc_protocol::SUBC_MODULE_ID_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Can use module identity now, or wait for the minter on the consumer path.
/// Preserve the legacy gate for processes not launched with module identity.
pub(crate) fn module_consumer_allowed() -> bool {
    module_id().is_some() && (REGISTRATION.get().is_some() || identity_ready())
}

pub(crate) async fn connect_consumer(
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

/// The stream is registered with this runtime's I/O driver during adoption.
/// Move both to the module thread so the original driver stays alive and is polled.
pub struct AdoptedRegistration {
    pub(super) runtime: tokio::runtime::Runtime,
    pub(super) metrics: Arc<DispatchPathMetrics>,
    pub(super) stream: Box<dyn ModuleStream>,
}

impl AdoptedRegistration {
    /// Call on the single-threaded startup path, before any child can inherit
    /// the handoff. The SDK installs the stream on the entered I/O driver.
    pub fn adopt() -> Result<Option<Self>, SubcError> {
        let metrics = Arc::new(DispatchPathMetrics::new());
        let runtime = metrics.frame_loop_runtime().map_err(SubcError::Runtime)?;
        let stream = {
            let _entered = runtime.enter();
            registration::adopt_channel().map_err(SubcError::Registration)?
        };
        match stream {
            Some(stream) => {
                crate::launch_nonce::strip_environment();
                REGISTRATION.get_or_init(IdentityReady::default);
                Ok(Some(Self {
                    runtime,
                    metrics,
                    stream,
                }))
            }
            None => Ok(None),
        }
    }
}

pub(super) async fn authenticate(
    mut stream: Box<dyn ModuleStream>,
    path: &Path,
) -> Result<Box<dyn ModuleStream>, SubcError> {
    let conn =
        connection_file::read_for_client(path).map_err(|source| SubcError::ConnectionFile {
            path: path.to_path_buf(),
            source,
        })?;
    authenticate_client(&mut stream, &conn, AUTH_DEADLINE)
        .await
        .map_err(|source| SubcError::Auth {
            endpoint: "registration channel".into(),
            source,
        })?;
    Ok(stream)
}

pub(super) fn install_minter(control: &Arc<DaemonControl>) -> Result<DropGuard, SubcError> {
    let closed = CancellationToken::new();
    registration::install_claim_minter(Arc::new(Minter {
        control: Arc::downgrade(control),
        closed: closed.clone(),
    }))
    .map_err(SubcError::Registration)?;
    REGISTRATION.get_or_init(IdentityReady::default).install();
    Ok(closed.drop_guard())
}

struct Minter {
    control: std::sync::Weak<DaemonControl>,
    closed: CancellationToken,
}

#[subc_client_rs::async_trait]
impl ClaimMinter for Minter {
    async fn mint(&self, request: ClaimMintRequest) -> Result<ClaimMintReply, ClaimError> {
        let control = self.control.upgrade().ok_or(ClaimError::Unavailable)?;
        tokio::select! {
            biased;
            _ = self.closed.cancelled() => Err(ClaimError::Unavailable),
            result = tokio::time::timeout(MINT_BUDGET, mint(&control, &request)) => {
                result.map_err(|_| ClaimError::Timeout)?
            }
        }
    }
}

async fn mint(
    control: &DaemonControl,
    request: &ClaimMintRequest,
) -> Result<ClaimMintReply, ClaimError> {
    loop {
        let frame = control
            .raw_request(request)
            .await
            .map_err(|error| match error {
                ControlError::ConnectionClosed => ClaimError::Unavailable,
                _ => ClaimError::Protocol,
            })?;
        match frame.header.ty {
            FrameType::Response => {
                // ClaimMintReply also validates the required `claim.mint` op.
                return serde_json::from_slice(&frame.body).map_err(|_| ClaimError::Protocol);
            }
            FrameType::Error => {
                let error: ErrorBody =
                    serde_json::from_slice(&frame.body).map_err(|_| ClaimError::Protocol)?;
                if error.code != error_codes::CLAIM_RATE_LIMITED {
                    return Err(ClaimError::Refused { code: error.code });
                }
                let detail: ClaimRateLimitedDetail =
                    serde_json::from_value(error.detail.ok_or(ClaimError::Protocol)?)
                        .map_err(|_| ClaimError::Protocol)?;
                tokio::time::sleep(Duration::from_millis(detail.retry_after_ms)).await;
            }
            _ => return Err(ClaimError::Protocol),
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[path = "registration_tests.rs"]
mod tests;
