use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use subc_client_rs::registration::{self, ClaimError, ClaimMinter};
use subc_client_rs::ModuleStream;
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
use subc_client_rs::{ConsumerError, ConsumerOptions, SubcConsumer};
use subc_protocol::claim::{ClaimMintReply, ClaimMintRequest, ClaimRateLimitedDetail};
use subc_protocol::{error_codes, ErrorBody, FrameType};
use subc_transport::{authenticate_client, connection_file};
use tokio_util::sync::{CancellationToken, DropGuard};

use super::health::DispatchPathMetrics;
use super::readiness::{ControlError, DaemonControl};
use super::{SubcError, AUTH_DEADLINE};

use aft_base::registration::claim_cause;
#[cfg(test)]
pub(crate) use aft_base::registration::live_consumers;
pub(crate) use aft_base::registration::{
    connect_consumer, count_live, identity_ready, module_consumer_allowed, module_id,
};

const MINT_BUDGET: Duration = Duration::from_secs(5);

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
                aft_base::registration::channel_adopted();
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
    aft_base::registration::minter_installed();
    Ok(closed.drop_guard())
}

struct Minter {
    control: std::sync::Weak<DaemonControl>,
    closed: CancellationToken,
}

#[subc_client_rs::async_trait]
impl ClaimMinter for Minter {
    async fn mint(&self, request: ClaimMintRequest) -> Result<ClaimMintReply, ClaimError> {
        let result = match self.control.upgrade() {
            None => Err(ClaimError::Unavailable),
            Some(control) => tokio::select! {
                biased;
                _ = self.closed.cancelled() => Err(ClaimError::Unavailable),
                result = tokio::time::timeout(MINT_BUDGET, mint(&control, &request)) => {
                    result.unwrap_or(Err(ClaimError::Timeout))
                }
            },
        };
        if let Err(error) = &result {
            log::info!("subc claim mint failed: cause={}", claim_cause(error));
        }
        result
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
