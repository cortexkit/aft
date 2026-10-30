//! Synchronous adapters over daemon-attested module routes, not command-line
//! output parsing or request-supplied principal claims.

use std::path::{Path, PathBuf};
use std::time::Duration;

use cortexkit_project_identity::{
    EnumerateParams, EnumerateReply, Outcome, Request, ResolveParams, ResolveReply,
};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use subc_client_rs::{CallError, CallOptions, CloseRouteOptions, ConsumerOptions, SubcConsumer};
use subc_protocol::{BindIdentity, RouteTarget};

use super::door::{DoorError, JsonDoorTransport};
use super::ProjectRegistry;

#[derive(Debug, Clone)]
pub struct ModuleTransport {
    connection_file: PathBuf,
    identity: BindIdentity,
    target: RouteTarget,
}

impl ModuleTransport {
    pub fn engram(connection_file: PathBuf, identity: BindIdentity) -> Self {
        Self {
            connection_file,
            identity,
            target: RouteTarget::InternalService {
                module_id: "engram".into(),
                service_id: "session-door".into(),
            },
        }
    }
    pub fn entorhinal(connection_file: PathBuf, identity: BindIdentity) -> Self {
        Self {
            connection_file,
            identity,
            target: RouteTarget::ManagementSurface {
                module_id: "entorhinal".into(),
            },
        }
    }

    fn request(&self, body: Vec<u8>, timeout: Duration) -> Result<Value, DoorError> {
        let this = self.clone();
        // SDK consumers run an async receive loop. A dedicated thread avoids
        // entering a second Tokio runtime on an async caller's executor thread.
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| DoorError::unavailable(error.to_string()))?;
            runtime.block_on(async {
                let principal = crate::launch_nonce::consumer_identity().ok_or_else(|| {
                    DoorError::unavailable("AFT module launch identity is unavailable")
                })?;
                let consumer =
                    SubcConsumer::connect(&this.connection_file, ConsumerOptions::default())
                        .await
                        .map_err(|error| DoorError::unavailable(error.to_string()))?;
                let route = consumer
                    .open_route(
                        this.target,
                        this.identity,
                        CallOptions {
                            consumer_identity: Some(principal),
                            ..CallOptions::default()
                        },
                    )
                    .await
                    .map_err(|error| DoorError::unavailable(error.to_string()))?;
                let result = consumer
                    .request(
                        &route,
                        body,
                        CallOptions {
                            timeout,
                            ..CallOptions::default()
                        },
                    )
                    .await;
                let _ = consumer
                    .close_handle(&route, CloseRouteOptions::default())
                    .await;
                let _ = consumer.close().await;
                match result {
                    Ok(bytes) => serde_json::from_slice(&bytes)
                        .map_err(|error| DoorError::unavailable(error.to_string())),
                    Err(CallError::Module(body)) => serde_json::to_value(body)
                        .map_err(|error| DoorError::unavailable(error.to_string())),
                    Err(error) => Err(DoorError::unavailable(error.to_string())),
                }
            })
        })
        .join()
        .map_err(|_| DoorError::unavailable("module transport thread panicked"))?
    }
}

impl super::binding::SessionLookup for ModuleTransport {
    fn session_for_ref(
        &self,
        reference: &cortexkit_role_agent_data::refs::Ref,
    ) -> Result<cortexkit_agent_identity::AgentSessionForRefReply, super::participant::BindingFailure>
    {
        let request = Request::new(
            "agent.session_for_ref",
            cortexkit_agent_identity::AgentSessionForRefParams::new(
                reference.ref_kind.clone(),
                reference.r#ref.clone(),
            ),
        );
        let bytes = serde_json::to_vec(&request)
            .map_err(|error| super::participant::BindingFailure::Unavailable(error.to_string()))?;
        let reply = self
            .request(bytes, Duration::from_secs(30))
            .map_err(|error| super::participant::BindingFailure::Unavailable(error.code))?;
        decode_session_for_ref(reply)
    }
}

impl JsonDoorTransport for ModuleTransport {
    fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, DoorError> {
        let body = serde_json::to_vec(&json!({"method":method,"params":params}))
            .map_err(|error| DoorError::unavailable(error.to_string()))?;
        let reply = self.request(body, timeout)?;
        match serde_json::from_value::<Outcome<Value>>(reply)
            .map_err(|error| DoorError::unavailable(error.to_string()))?
        {
            Outcome::Success(success) => Ok(success.result),
            Outcome::Refusal(error) => Err(DoorError {
                code: error.code.into(),
                detail: error.detail.unwrap_or(Value::Null),
            }),
            _ => Err(DoorError::unavailable("unsupported module outcome")),
        }
    }
}

#[derive(Debug)]
pub struct EntorhinalRegistry(pub ModuleTransport);

impl EntorhinalRegistry {
    fn read<P: serde::Serialize, R: DeserializeOwned>(
        &self,
        request: Request<P>,
    ) -> Result<R, String> {
        let body = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
        let reply = self
            .0
            .request(body, Duration::from_secs(30))
            .map_err(|error| error.code)?;
        match serde_json::from_value::<Outcome<R>>(reply).map_err(|error| error.to_string())? {
            Outcome::Success(success) => Ok(success.result),
            Outcome::Refusal(error) => Err(format!("registry unavailable: {}", error.message)),
            _ => Err("registry unavailable: unsupported outcome".into()),
        }
    }
}

impl ProjectRegistry for EntorhinalRegistry {
    fn resolve(&self, path: &Path) -> Result<ResolveReply, String> {
        self.read(Request::new(
            "resolve",
            ResolveParams::new(path.to_str().ok_or("non-UTF8 path")?.to_owned(), None),
        ))
    }
    fn enumerate(&self) -> Result<EnumerateReply, String> {
        self.read(Request::new("enumerate", EnumerateParams::new(None)))
    }
}

/// Use the published envelope and refusal vocabulary for both DATA and ERROR
/// replies. A missing registration is final; service or decoding failures are
/// unavailable rather than evidence that the session belongs to someone else.
pub(super) fn decode_session_for_ref(
    reply: Value,
) -> Result<cortexkit_agent_identity::AgentSessionForRefReply, super::participant::BindingFailure> {
    use super::participant::BindingFailure;
    match serde_json::from_value::<Outcome<cortexkit_agent_identity::AgentSessionForRefReply>>(
        reply,
    )
    .map_err(|error| BindingFailure::Unavailable(error.to_string()))?
    {
        Outcome::Success(success) => Ok(success.result),
        Outcome::Refusal(error)
            if error.code == cortexkit_project_identity::RefusalCode::NotFound =>
        {
            Err(BindingFailure::NotBindable)
        }
        Outcome::Refusal(error) => Err(BindingFailure::Unavailable(error.message)),
        _ => Err(BindingFailure::Unavailable(
            "unsupported session lookup outcome".into(),
        )),
    }
}
