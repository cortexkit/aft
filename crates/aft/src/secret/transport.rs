//! Bounded `secret.use` calls over the daemon's attested consumer routes.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use subc_client_rs::{CallError, CallOptions, ConsumerIdentity, ConsumerOptions};
use subc_protocol::scope::{ScopeSelector, ScopeStamp};
use subc_protocol::{BindIdentity, RouteTarget};
use zeroize::Zeroizing;

use super::vault::SecretUseTransport;

const UNAVAILABLE: &str = "secret.use transport unavailable";

/// A transport for a future request-addressed vault client. Construct it at call
/// time: a standalone process has no module identity and must not open as direct.
/// No connection or vault availability is cached across calls.
#[derive(Clone)]
pub struct SubcSecretUseTransport {
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
        })
    }

    async fn send(&self, params: Value, principal: ConsumerIdentity) -> Result<Value, String> {
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
        let consumer = crate::fleet_status::connect_subc_consumer(
            &self.connection_file,
            ConsumerOptions::default(),
        )
        .await
        .map_err(|_| UNAVAILABLE.to_owned())?;
        // Each call owns its consumer, so closing a route cannot interrupt a
        // concurrent use of an SDK-cached handle. Dropping the call's runtime on
        // timeout also drops the connection and all of its routes.
        let result = async {
            let route = consumer
                .open_route_scoped(
                    RouteTarget::ManagementSurface {
                        module_id: "claustrum".into(),
                    },
                    self.identity.clone(),
                    scope,
                    CallOptions {
                        consumer_identity: Some(principal),
                        timeout: self.timeout,
                        route_retry_deadline: self.timeout,
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
                        timeout: self.timeout,
                        ..CallOptions::default()
                    },
                )
                .await;
            let result = match reply {
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
            };
            let _ = consumer
                .close_handle(&route, subc_client_rs::CloseRouteOptions::default())
                .await;
            result
        }
        .await;
        let _ = consumer.close().await;
        result
    }

    fn dispatch(&self, params: Value, principal: ConsumerIdentity) -> Result<Value, String> {
        let this = self.clone();
        // The seam is synchronous and is also used outside Tokio. A dedicated
        // runtime avoids entering a nested runtime on the caller's executor thread.
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| UNAVAILABLE.to_owned())?;
            runtime.block_on(async {
                tokio::time::timeout(this.timeout, this.send(params, principal))
                    .await
                    .map_err(|_| UNAVAILABLE.to_owned())?
            })
        })
        .join()
        .map_err(|_| UNAVAILABLE.to_owned())?
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
