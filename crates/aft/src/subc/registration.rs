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

/// Open a daemon consumer connection on behalf of `owner`, a fixed label naming
/// the part of AFT that holds it (see [`track_consumer`]).
pub(crate) async fn connect_consumer(
    owner: &'static str,
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
    match SubcConsumer::connect(path, options).await {
        Ok(consumer) => {
            track_consumer(owner, &consumer);
            Ok(consumer)
        }
        Err(error) => {
            log_claim_connect_failure(owner, &error);
            Err(error)
        }
    }
}

/// The claim failure behind a consumer connect error, if it was one. The SDK
/// wraps every claim failure as a `Connect` error whose source is the
/// [`ClaimError`], and its message reads the same for every cause.
fn connect_claim_error(error: &ConsumerError) -> Option<&ClaimError> {
    match error {
        ConsumerError::Connect { source, .. } => source.get_ref()?.downcast_ref::<ClaimError>(),
        _ => None,
    }
}

/// A short fixed label for a claim failure, for logs only. The daemon's
/// refusal code is a fixed identifier, never a message or secret.
fn claim_cause(error: &ClaimError) -> String {
    match error {
        ClaimError::Refused { code } => format!("refused code={code}"),
        ClaimError::Unavailable => "unavailable".into(),
        ClaimError::Timeout => "timeout".into(),
        ClaimError::Protocol => "protocol".into(),
        ClaimError::Handoff => "handoff".into(),
        ClaimError::PrefixChildConsumer => "prefix_child_consumer".into(),
        _ => "other".into(),
    }
}

/// Log why a consumer connection failed to bind its claim. Callers' own error
/// text stays unchanged; this line only records the cause the SDK's message hides.
pub(crate) fn log_claim_connect_failure(owner: &'static str, error: &ConsumerError) {
    if let Some(claim) = connect_claim_error(error) {
        log::info!(
            "subc consumer claim failed: owner={owner} cause={}",
            claim_cause(claim)
        );
    }
}

/// Live daemon consumer connections, counted per owner label.
#[derive(Default)]
struct ConsumerGauge {
    counts: std::sync::Mutex<std::collections::BTreeMap<&'static str, usize>>,
    changed: AtomicBool,
    reporter: std::sync::Once,
}

static CONSUMER_GAUGE: OnceLock<ConsumerGauge> = OnceLock::new();

/// How often the per-owner counts are logged, at most, when they changed.
const CONSUMER_GAUGE_LOG_INTERVAL: Duration = Duration::from_secs(60);

fn consumer_gauge() -> &'static ConsumerGauge {
    CONSUMER_GAUGE.get_or_init(ConsumerGauge::default)
}

impl ConsumerGauge {
    fn adjust(&self, owner: &'static str, opened: bool) {
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = counts.entry(owner).or_default();
        *count = if opened {
            count.saturating_add(1)
        } else {
            count.saturating_sub(1)
        };
        self.changed.store(true, Ordering::Release);
    }

    fn render(&self) -> String {
        let counts = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut line = String::new();
        for (owner, count) in counts.iter() {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&format!("{owner}={count}"));
        }
        line
    }

    /// One process-wide thread logs the counts once a minute, only after a
    /// change, so a growing owner shows in the log without a line per connect.
    fn start_reporter(&'static self) {
        self.reporter.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("aft-subc-consumer-gauge".into())
                .spawn(move || loop {
                    std::thread::sleep(CONSUMER_GAUGE_LOG_INTERVAL);
                    if self.changed.swap(false, Ordering::AcqRel) {
                        log::info!("subc consumers live: {}", self.render());
                    }
                });
        });
    }
}

/// Counts one live connection for its owner and uncounts it when dropped.
struct LiveConsumer {
    owner: &'static str,
}

impl Drop for LiveConsumer {
    fn drop(&mut self) {
        consumer_gauge().adjust(self.owner, false);
    }
}

/// Count `consumer` as a live connection held by `owner` until the SDK
/// releases it. The count lives in a connection-state callback, which the
/// SDK drops together with the connection's shared state: after the last
/// `SubcConsumer` handle, its subscriptions and its background tasks are gone.
/// A consumer that is kept alive therefore stays counted.
pub(crate) fn track_consumer(owner: &'static str, consumer: &SubcConsumer) {
    let gauge = consumer_gauge();
    gauge.start_reporter();
    gauge.adjust(owner, true);
    let live = LiveConsumer { owner };
    consumer.on_connection_state(move |_| {
        let _ = &live;
    });
}

/// The live consumer connections per owner label, for diagnostics and tests.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn live_consumers() -> std::collections::BTreeMap<&'static str, usize> {
    consumer_gauge()
        .counts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
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
