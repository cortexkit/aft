//! Bounded `secret.use` calls over the daemon's attested consumer routes.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use subc_client_rs::{
    CallError, CallOptions, CloseRouteOptions, ConsumerIdentity, ConsumerOptions, RouteHandle,
    SubcConsumer, SubscribeOptions, Subscription,
};
use subc_protocol::scope::{ScopeSelector, ScopeStamp};
use subc_protocol::{BindIdentity, RouteTarget};
use zeroize::Zeroizing;

use super::vault::SecretUseTransport;

#[path = "transport_diagnostics.rs"]
mod diagnostics;
use diagnostics::{Failure, Stage};

const UNAVAILABLE: &str = "secret.use transport unavailable";
static CONNECTION_FILE: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

pub(crate) fn install_connection_file(path: &std::path::Path) {
    *CONNECTION_FILE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path.to_owned());
}

pub(crate) fn for_current_command() -> Option<SubcSecretUseTransport> {
    let path = CONNECTION_FILE.read().ok()?.clone()?;
    if !path.is_file() {
        return None;
    }
    let (_, session) = crate::subc::current_secret_consent_context()?;
    SubcSecretUseTransport::new(
        path,
        BindIdentity::new("aft-secret-use", session),
        Duration::from_secs(15),
    )
}

/// A request-addressed vault transport. Construct it at call time: a standalone
/// process has no attested AFT module identity. Never open its vault route under
/// the daemon's unprivileged `direct` principal instead.
/// One command owns one consumer connection; failed connections are discarded so
/// a later request can reconnect after the daemon returns.
#[derive(Clone)]
pub struct SubcSecretUseTransport {
    connection_file: PathBuf,
    identity: BindIdentity,
    timeout: Duration,
    worker: Arc<OnceLock<tokio::sync::mpsc::UnboundedSender<Work>>>,
    #[cfg(test)]
    test_principal: Option<ConsumerIdentity>,
}

struct Work {
    params: Value,
    principal: ConsumerIdentity,
    deadline: tokio::time::Instant,
    reply: std::sync::mpsc::Sender<Result<Zeroizing<Vec<u8>>, Failure>>,
    failure: Arc<Mutex<Option<Failure>>>,
}

struct ConnectionConfig {
    connection_file: PathBuf,
    identity: BindIdentity,
    timeout: Duration,
}

// Both scoped secret use and daemon-owned manifest delivery use the attested
// claustrum management surface. A manifest watch deliberately has no agent scope.
async fn open_vault_route(
    consumer: &SubcConsumer,
    identity: BindIdentity,
    principal: ConsumerIdentity,
    scope: Option<ScopeSelector>,
    timeout: Duration,
) -> Result<RouteHandle, CallError> {
    let target = RouteTarget::ManagementSurface {
        module_id: "claustrum".into(),
    };
    let options = CallOptions {
        consumer_identity: Some(principal),
        timeout,
        route_retry_deadline: timeout,
        ..CallOptions::default()
    };
    match scope {
        Some(scope) => {
            consumer
                .open_route_scoped(target, identity, scope, options)
                .await
        }
        None => consumer.open_route(target, identity, options).await,
    }
}

/// A held daemon-owned route on the existing vault transport. Closing it also
/// closes its consumer so reconnects never leave an old watch subscribed.
pub(crate) struct ManagementSession {
    consumer: SubcConsumer,
    route: RouteHandle,
}

impl ManagementSession {
    pub(crate) async fn connect(connection_file: &std::path::Path) -> Result<Self, ()> {
        let principal = aft_identity().ok_or(())?;
        Self::connect_as(connection_file, principal).await
    }

    async fn connect_as(
        connection_file: &std::path::Path,
        principal: ConsumerIdentity,
    ) -> Result<Self, ()> {
        let consumer =
            crate::fleet_status::connect_subc_consumer(connection_file, ConsumerOptions::default())
                .await
                .map_err(|_| ())?;
        let route = match open_vault_route(
            &consumer,
            BindIdentity::new("aft-manifest-sync", "daemon"),
            principal,
            None,
            Duration::from_secs(15),
        )
        .await
        {
            Ok(route) => route,
            Err(_) => {
                let _ = consumer.close().await;
                return Err(());
            }
        };
        Ok(Self { consumer, route })
    }

    #[cfg(all(test, unix))]
    pub(crate) async fn for_test(connection_file: &std::path::Path) -> Result<Self, ()> {
        Self::connect_as(
            connection_file,
            ConsumerIdentity {
                module_id: "aft".into(),
                launch_nonce: "manifest-sync-test-nonce".into(),
            },
        )
        .await
    }

    pub(crate) async fn request(&self, body: Vec<u8>) -> Result<Vec<u8>, CallError> {
        self.consumer
            .request(
                &self.route,
                body,
                CallOptions {
                    timeout: Duration::from_secs(15),
                    ..CallOptions::default()
                },
            )
            .await
    }

    pub(crate) async fn watch(&self, body: Vec<u8>) -> Result<Subscription, CallError> {
        self.consumer
            .subscribe_route(&self.route, body, SubscribeOptions::default())
            .await
    }

    pub(crate) async fn close(self) {
        let _ = self
            .consumer
            .close_handle(&self.route, CloseRouteOptions::default())
            .await;
        let _ = self.consumer.close().await;
    }
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
            #[cfg(test)]
            test_principal: None,
        })
    }

    #[cfg(all(test, unix))]
    pub(crate) fn for_test(
        connection_file: PathBuf,
        identity: BindIdentity,
        timeout: Duration,
        principal: ConsumerIdentity,
    ) -> Self {
        Self {
            connection_file,
            identity,
            timeout,
            worker: Arc::default(),
            test_principal: Some(principal),
        }
    }

    async fn send(
        config: &ConnectionConfig,
        consumer: &subc_client_rs::SubcConsumer,
        params: Value,
        principal: ConsumerIdentity,
    ) -> Result<Zeroizing<Vec<u8>>, Failure> {
        let target = &params["target"];
        if target["scheme"] != "https"
            || target
                .get("port")
                .is_some_and(|port| port.as_u64() != Some(443))
        {
            return Err(Failure::local(Stage::TargetRefused));
        }
        let stamp: ScopeStamp =
            serde_json::from_value(params.get("stamp").cloned().unwrap_or_default())
                .map_err(|_| Failure::local(Stage::StampDecode))?;
        let kind = stamp.kind;
        let scope = ScopeSelector::new(stamp.owner, stamp.scope_ref, Some(stamp.scope_epoch));
        let body = serde_json::to_vec(&json!({"method": "secret.use", "params": params}))
            .map_err(|_| Failure::local(Stage::Encode))?;
        let route = open_vault_route(
            consumer,
            config.identity.clone(),
            principal,
            Some(scope.clone()),
            config.timeout,
        )
        .await
        .map_err(|error| Failure::call(Stage::RouteOpen, &error).under_scope(kind, &scope))?;
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
                Ok(bytes)
            }
            // Preserve the distinction between a module Error frame and a
            // result refusal. Only allowlisted codes enter diagnostics: SDK
            // messages, detail and reply bytes could carry released material.
            Err(error @ CallError::Module(_)) => {
                let failure = Failure::call(Stage::ModuleError, &error);
                let CallError::Module(error) = error else {
                    unreachable!();
                };
                let code = Zeroizing::new(error.code);
                let _message = Zeroizing::new(error.message);
                // Retain only the classified sender error. Peer detail may
                // contain a credential and must never enter a second buffer.
                if code.as_str() == "invalid_params" {
                    Ok(Zeroizing::new(
                        br#"{"code":"invalid_params","message":""}"#.to_vec(),
                    ))
                } else {
                    Err(failure)
                }
            }
            Err(error) => Err(Failure::call(Stage::Request, &error)),
        }
    }

    fn dispatch(
        &self,
        params: Value,
        principal: ConsumerIdentity,
    ) -> Result<Zeroizing<Vec<u8>>, String> {
        self.dispatch_work(params, principal)
            .map_err(Failure::unavailable)
    }

    fn dispatch_work(
        &self,
        params: Value,
        principal: ConsumerIdentity,
    ) -> Result<Zeroizing<Vec<u8>>, Failure> {
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
        let failure = Arc::new(Mutex::new(None));
        sender
            .send(Work {
                params,
                principal,
                deadline: tokio::time::Instant::now() + self.timeout,
                reply,
                failure: Arc::clone(&failure),
            })
            .map_err(|_| Failure::local(Stage::Worker))?;
        answer.recv_timeout(self.timeout).unwrap_or_else(|error| {
            // The worker can be closing a failed connection when the caller's
            // deadline expires. Preserve that failure and log only at the caller,
            // so cleanup and deadline races never emit two warnings for one call.
            Err(failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .unwrap_or_else(|| {
                    Failure::local(match error {
                        std::sync::mpsc::RecvTimeoutError::Timeout => Stage::Deadline,
                        std::sync::mpsc::RecvTimeoutError::Disconnected => Stage::Worker,
                    })
                }))
        })
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
                        .map_err(|error| Failure::connect(&error))?,
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
            .unwrap_or_else(|_| Err(Failure::local(Stage::Deadline)));
            if let Err(error) = &result {
                *work
                    .failure
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.clone());
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
    fn secret_use(&self, params: Value) -> Result<Zeroizing<Vec<u8>>, String> {
        #[cfg(test)]
        if let Some(principal) = &self.test_principal {
            return self.dispatch(params, principal.clone());
        }
        let principal =
            aft_identity().ok_or_else(|| Failure::local(Stage::IdentityMissing).unavailable())?;
        self.dispatch(params, principal)
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "transport_fake.rs"]
pub(crate) mod fake;
