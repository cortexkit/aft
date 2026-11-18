//! Admission for Callosum tool routes. Their project keys are portable; their paths are not.

use super::*;
use crate::agent_data::transport::{EntorhinalRegistry, ModuleTransport};
use cortexkit_project_identity::{EnumerateReply, RootKeyKind as RegistryKeyKind};
use subc_protocol::scope::{
    FederatedPeerClass, FederatedProject, RemoteClaim, RootKeyKind, ScopeKind, ScopeStamp,
};

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
    let caller = match facts.remote_claim.as_ref() {
        Some(RemoteClaim::Agent { agent_id, .. } | RemoteClaim::Run { agent_id, .. }) => agent_id,
        Some(RemoteClaim::Worker {
            on_behalf_of: Some(agent_id),
            ..
        }) => agent_id,
        _ => return Err(admission_refusal()),
    };
    Ok((
        facts.project.as_ref().ok_or_else(admission_refusal)?,
        caller,
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

#[derive(Default)]
pub(super) struct FolderLookups(tokio::task::JoinSet<(Frame, Result<PathBuf, ErrorBody>)>);

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

impl FolderLookups {
    pub(super) async fn request(
        &mut self,
        tx: &WriterSender,
        metrics: &Arc<DispatchPathMetrics>,
        frame: &Frame,
        body: Option<&Value>,
        identity: &ManagementIdentity,
        connection_file: &Path,
    ) -> Result<(), SubcError> {
        if !body.is_some_and(route_request_is_tool_call) {
            return send_provider_error(tx, metrics, frame, tool_unavailable()).await;
        }
        let project = match admit(identity.scope.as_ref()) {
            Ok((project, _caller_id)) if self.0.len() < MAX_PENDING_LOOKUPS => project.clone(),
            _ => return send_provider_error(tx, metrics, frame, admission_refusal()).await,
        };
        let frame = frame.clone();
        let lookup = enumerate(
            #[cfg(test)]
            TEST_REGISTRY.try_with(Arc::clone).ok(),
            connection_file.to_path_buf(),
        );
        let task = ResponseTaskGuard::new(metrics);
        self.0.spawn(async move {
            let _task = task;
            let result = match tokio::time::timeout(LOOKUP_TIMEOUT, lookup).await {
                Ok(Ok(entries)) => registered_root(&project, &entries),
                _ => Err(admission_refusal()),
            };
            (frame, result)
        });
        Ok(())
    }

    pub(super) async fn next(&mut self) -> Option<(Frame, Result<PathBuf, ErrorBody>)> {
        while let Some(next) = self.0.join_next().await {
            match next {
                Ok(completion) => return Some(completion),
                Err(error) => log::warn!("remote folder lookup task failed: {error}"),
            }
        }
        None
    }
}

pub(super) async fn finish_lookup(
    tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    (frame, folder): (Frame, Result<PathBuf, ErrorBody>),
    management_routes: &HashMap<RouteChannel, ManagementIdentity>,
    root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
    routes: &HashMap<RouteChannel, RouteIdentity>,
) -> Result<(), SubcError> {
    if !management_routes.contains_key(&route_key(frame.header.channel, frame.header.epoch)) {
        return Ok(());
    }
    // Resolve against the current live binds, not the ones present before the
    // registry request. No admitted remote tool can execute yet.
    let error = match folder.and_then(|path| locally_bound_root(&path, root_channels, routes)) {
        Ok(_) => tool_unavailable(),
        Err(error) => error,
    };
    send_provider_error(tx, metrics, &frame, error).await
}

#[cfg(test)]
#[path = "remote_calls_tests.rs"]
mod tests;
