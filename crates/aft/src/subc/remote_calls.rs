//! Admission for Callosum tool routes. Their project keys are portable; their paths are not.

use super::*;
use crate::agent_data::transport::{EntorhinalRegistry, ModuleTransport};
use cortexkit_project_identity::{EnumerateReply, RootKeyKind as RegistryKeyKind};
use subc_protocol::scope::{
    FederatedPeerClass, FederatedProject, RemoteClaim, RootKeyKind, ScopeKind, ScopeStamp,
};

#[path = "remote_consent.rs"]
pub(super) mod authorization;
#[path = "remote_dispatch.rs"]
mod dispatch;
#[path = "remote_call_record.rs"]
mod record;
#[path = "remote_bash.rs"]
mod remote_bash;
#[path = "remote_call_service.rs"]
mod service;
pub(super) use remote_bash::Running as RunningBash;
pub(super) use service::{Binding, Service};

/// Success of the remote dispatch preflight, minted only after admission,
/// folder and tool checks, verification of the runner's schema and behavior
/// pin, transport checks, and creation of the audit record.
pub(super) struct Preflight {
    scope: ScopeStamp,
    session: String,
    folder: PathBuf,
    mode: crate::db::remote_grant_rows::Mode,
    tool: String,
    args_hash: String,
}

const MAX_PENDING_LOOKUPS: usize = 8;
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
tokio::task_local! {
    static TEST_REGISTRY: Arc<dyn Fn() -> EnumerateReply + Send + Sync>;
}

fn admission_refusal() -> ErrorBody {
    ErrorBody::new(
        "remote_call_not_admitted",
        "This computer's AFT can't take calls from other computers.",
    )
}

fn not_registered() -> ErrorBody {
    ErrorBody::new(
        "remote_folder_not_registered",
        "The folder isn't registered on this computer.",
    )
}

fn not_working() -> ErrorBody {
    ErrorBody::new(
        "remote_folder_not_working",
        "AFT isn't working in that folder here.",
    )
}

fn tool_unavailable() -> ErrorBody {
    ErrorBody::new(
        "remote_tool_not_available",
        "This tool isn't available to agents on other computers.",
    )
}

pub(super) fn decode_control(body: &[u8]) -> serde_json::Result<ModuleControlRequest> {
    serde_json::from_slice(body).or_else(|error| {
        // An undecodable daemon scope stamp must still reach call-time refusal
        // on a `fed:` tool route. Other binds keep their original decode error.
        without_undecodable_scope(body).ok_or(error)
    })
}

fn without_undecodable_scope(body: &[u8]) -> Option<ModuleControlRequest> {
    let mut envelope: Value = serde_json::from_slice(body).ok()?;
    if envelope.get("op")?.as_str()? != "route.bind"
        || !harness_forces_untrusted(envelope.pointer("/identity/harness")?.as_str()?)
    {
        return None;
    }
    let stamp = envelope.as_object_mut()?.remove("scope")?;
    if serde_json::from_value::<Option<ScopeStamp>>(stamp).is_ok() {
        return None;
    }
    match serde_json::from_value(envelope).ok()? {
        bind @ ModuleControlRequest::RouteBind {
            target: RouteTarget::ToolProvider { .. },
            ..
        } => Some(bind),
        _ => None,
    }
}

fn caller(facts: &subc_protocol::scope::FederatedAttributes) -> Option<&str> {
    match facts.remote_claim.as_ref()? {
        RemoteClaim::Agent { agent_id, .. } | RemoteClaim::Run { agent_id, .. } => Some(agent_id),
        RemoteClaim::Worker { on_behalf_of, .. } => on_behalf_of.as_deref(),
        _ => None,
    }
}

fn admit(scope: Option<&ScopeStamp>) -> Result<(&FederatedProject, &str), ErrorBody> {
    let facts = scope
        .filter(|stamp| stamp.kind == ScopeKind::Federated)
        .and_then(|stamp| stamp.attributes.federated.as_ref())
        .ok_or_else(admission_refusal)?;
    if facts.peer_class != FederatedPeerClass::Computer
        || facts.peer_box_id.is_none()
        || !matches!(facts.origin_principal, Some(Principal::Reserved { .. }))
        || facts.validate().is_err()
    {
        return Err(admission_refusal());
    }
    Ok((
        facts.project.as_ref().ok_or_else(admission_refusal)?,
        caller(facts).ok_or_else(admission_refusal)?,
    ))
}

fn registered_root(
    project: &FederatedProject,
    registry: &EnumerateReply,
) -> Result<PathBuf, ErrorBody> {
    let kind = match project.root_key.kind {
        RootKeyKind::Remote => RegistryKeyKind::Remote,
        RootKeyKind::Label => RegistryKeyKind::Label,
    };
    let mut matches = registry
        .projects
        .iter()
        .filter(|entry| entry.project_id == project.project_id)
        .flat_map(|entry| entry.root_records.iter().flatten())
        .filter(|record| {
            crate::agent_data::registry::usable(record)
                && record
                    .root_key
                    .as_ref()
                    .is_some_and(|key| key.kind == kind && key.root_key == project.root_key.key)
        });
    let record = matches.next().ok_or_else(not_registered)?;
    if matches.next().is_some() {
        return Err(not_registered());
    }
    Ok(PathBuf::from(&record.root))
}

fn locally_bound_root<'a>(
    registered: &Path,
    root_channels: &'a HashMap<ProjectRootId, HashSet<RouteChannel>>,
    routes: &HashMap<RouteChannel, RouteIdentity>,
) -> Result<&'a ProjectRootId, ErrorBody> {
    // Entorhinal stores the canonical spelling. Do not follow a replacement
    // symlink to a different root that happens to have a live bind.
    root_channels
        .iter()
        .find(|(root, channels)| {
            root.as_path() == registered
                && channels.iter().any(|channel| {
                    routes
                        .get(channel)
                        .is_some_and(|identity| !harness_forces_untrusted(&identity.harness))
                })
        })
        .map(|(root, _)| root)
        .ok_or_else(not_working)
}

async fn enumerate(
    #[cfg(test)] fixture: Option<Arc<dyn Fn() -> EnumerateReply + Send + Sync>>,
    connection_file: PathBuf,
) -> Result<EnumerateReply, String> {
    #[cfg(test)]
    if let Some(fixture) = fixture {
        return Ok(fixture());
    }
    EntorhinalRegistry(ModuleTransport::entorhinal(
        connection_file,
        subc_protocol::BindIdentity::new("aft", "aft-remote-folder-lookup"),
    ))
    .enumerate_async()
    .await
}

#[cfg(test)]
#[path = "remote_calls_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "remote_wire_tests.rs"]
mod wire_tests;
