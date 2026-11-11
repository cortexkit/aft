//! Bounded `secret.use` calls over the daemon's attested consumer routes.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use subc_client_rs::{CallError, CallOptions, ConsumerIdentity, ConsumerOptions};
use subc_protocol::scope::{ScopeSelector, ScopeStamp};
use subc_protocol::{BindIdentity, RouteTarget};
use zeroize::Zeroizing;

use super::vault::SecretUseTransport;

const UNAVAILABLE: &str = "secret.use transport unavailable";
static CONNECTION_FILE: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

pub(crate) fn install_connection_file(path: &std::path::Path) {
    *CONNECTION_FILE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path.to_owned());
}

pub(crate) fn for_current_command() -> Option<SubcSecretUseTransport> {
    let path = CONNECTION_FILE.read().ok()?.clone()?;
    let (_, session) = crate::subc::current_secret_consent_context()?;
    SubcSecretUseTransport::new(
        path,
        BindIdentity::new("aft-secret-use", session),
        Duration::from_secs(15),
    )
}

/// A transport for a future request-addressed vault client. Construct it at call
/// time: a standalone process has no module identity and must not open as direct.
/// One command owns one consumer connection; failed connections are discarded so
/// a later request can reconnect after the daemon returns.
#[derive(Clone)]
pub struct SubcSecretUseTransport {
    connection_file: PathBuf,
    identity: BindIdentity,
    timeout: Duration,
    worker: Arc<OnceLock<tokio::sync::mpsc::UnboundedSender<Work>>>,
}

struct Work {
    params: Value,
    principal: ConsumerIdentity,
    deadline: tokio::time::Instant,
    reply: std::sync::mpsc::Sender<Result<Value, String>>,
}

struct ConnectionConfig {
    connection_file: PathBuf,
    identity: BindIdentity,
    timeout: Duration,
}

impl SubcSecretUseTransport {
    /// Use AFT's captured launch-nonce identity, never an agent-supplied principal.
    /// The deadline includes connection, scoped open, request, and cleanup.
    pub fn new(
        connection_file: PathBuf,
        identity: BindIdentity,
        timeout: Duration,
    ) -> Option<Self> {
        aft_identity()?;
        Some(Self {
            connection_file,
            identity,
            timeout,
            worker: Arc::default(),
        })
    }

    async fn send(
        config: &ConnectionConfig,
        consumer: &subc_client_rs::SubcConsumer,
        params: Value,
        principal: ConsumerIdentity,
    ) -> Result<Value, String> {
        let target = &params["target"];
        if target["scheme"] != "https"
            || target
                .get("port")
                .is_some_and(|port| port.as_u64() != Some(443))
        {
            return Err(UNAVAILABLE.to_owned());
        }
        let stamp: ScopeStamp =
            serde_json::from_value(params.get("stamp").cloned().unwrap_or_default())
                .map_err(|_| UNAVAILABLE.to_owned())?;
        let scope = ScopeSelector::new(stamp.owner, stamp.scope_ref, Some(stamp.scope_epoch));
        let body = serde_json::to_vec(&json!({"method": "secret.use", "params": params}))
            .map_err(|_| UNAVAILABLE.to_owned())?;
        let route = consumer
            .open_route_scoped(
                RouteTarget::ManagementSurface {
                    module_id: "claustrum".into(),
                },
                config.identity.clone(),
                scope,
                CallOptions {
                    consumer_identity: Some(principal),
                    timeout: config.timeout,
                    route_retry_deadline: config.timeout,
                    ..CallOptions::default()
                },
            )
            .await
            .map_err(|_| UNAVAILABLE.to_owned())?;
        let reply = consumer
            .request(
                &route,
                body,
                CallOptions {
                    timeout: config.timeout,
                    ..CallOptions::default()
                },
            )
            .await;
        match reply {
            Ok(bytes) => {
                let bytes = Zeroizing::new(bytes);
                serde_json::from_slice(&bytes).map_err(|_| UNAVAILABLE.to_owned())
            }
            // Preserve the distinction between a module Error frame and a
            // result refusal. Do not include SDK errors or reply bytes in
            // transport diagnostics: either could carry released material.
            Err(CallError::Module(error)) => Ok(json!({
                "code": error.code,
                "message": error.message,
            })),
            Err(_) => Err(UNAVAILABLE.to_owned()),
        }
    }

    fn dispatch(&self, params: Value, principal: ConsumerIdentity) -> Result<Value, String> {
        let sender = self.worker.get_or_init(|| {
            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
            let config = ConnectionConfig {
                connection_file: self.connection_file.clone(),
                identity: self.identity.clone(),
                timeout: self.timeout,
            };
            std::thread::spawn(move || {
                if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    runtime.block_on(Self::serve(config, receiver));
                }
            });
            sender
        });
        let (reply, answer) = std::sync::mpsc::channel();
        sender
            .send(Work {
                params,
                principal,
                deadline: tokio::time::Instant::now() + self.timeout,
                reply,
            })
            .map_err(|_| UNAVAILABLE.to_owned())?;
        answer
            .recv_timeout(self.timeout)
            .map_err(|_| UNAVAILABLE.to_owned())?
    }

    async fn serve(
        config: ConnectionConfig,
        mut receiver: tokio::sync::mpsc::UnboundedReceiver<Work>,
    ) {
        let mut consumer = None;
        while let Some(work) = receiver.recv().await {
            let result = tokio::time::timeout_at(work.deadline, async {
                if consumer.is_none() {
                    consumer = Some(
                        crate::fleet_status::connect_subc_consumer(
                            &config.connection_file,
                            ConsumerOptions::default(),
                        )
                        .await
                        .map_err(|_| UNAVAILABLE.to_owned())?,
                    );
                }
                Self::send(
                    &config,
                    consumer.as_ref().expect("connected consumer"),
                    work.params,
                    work.principal,
                )
                .await
            })
            .await
            .unwrap_or_else(|_| Err(UNAVAILABLE.to_owned()));
            if result.is_err() {
                if let Some(failed) = consumer.take() {
                    let _ = tokio::time::timeout(Duration::from_secs(1), failed.close()).await;
                }
            }
            let _ = work.reply.send(result);
        }
        if let Some(consumer) = consumer {
            let _ = tokio::time::timeout(Duration::from_secs(1), consumer.close()).await;
        }
    }
}

fn aft_identity() -> Option<ConsumerIdentity> {
    crate::launch_nonce::consumer_identity().filter(|identity| identity.module_id == "aft")
}

impl SecretUseTransport for SubcSecretUseTransport {
    fn secret_use(&self, params: Value) -> Result<Value, String> {
        let principal = aft_identity().ok_or_else(|| UNAVAILABLE.to_owned())?;
        self.dispatch(params, principal)
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "transport_fake.rs"]
pub(crate) mod fake;
