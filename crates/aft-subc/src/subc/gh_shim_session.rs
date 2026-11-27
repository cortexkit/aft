//! GitHub policy and attribution wiring for an attached AFT module.

use super::*;
use crate::gh_shim::{manifest_sync, trust_root};

pub(super) fn start(
    connection_file: &Path,
    control: Arc<readiness::DaemonControl>,
) -> Option<manifest_sync::Worker> {
    crate::agent_child_env::coauthor::install(connection_file.to_path_buf());
    manifest_sync::start(connection_file.to_path_buf(), move || {
        let control = control.clone();
        Box::pin(async move {
            let op = trust_root::GET;
            if !control.advertises(op) {
                return Err(());
            }
            control
                .config_request(&json!({"op": op}))
                .await
                .map_err(|_| ())
        })
    })
}

pub(super) fn speech_launch_seed(
    identity: &RouteIdentity,
    route: RouteChannel,
    control: Option<&Arc<readiness::DaemonControl>>,
    routes: &HashMap<RouteChannel, RouteIdentity>,
) -> Option<crate::gh_shim_speech::service::LaunchSeed> {
    use crate::gh_shim_speech::model::{LaunchAuthority, RouteOrigin};
    let lifecycle = identity.scope.clone()?;
    let parent = lifecycle.parent.as_ref().and_then(|parent| {
        routes
            .values()
            .filter_map(|route| route.scope.as_ref())
            .find(|scope| {
                scope.kind == subc_protocol::scope::ScopeKind::Head
                    && scope.owner_authorized
                    && scope.owner == parent.owner
                    && scope.scope_ref == parent.scope_ref
                    && scope.scope_epoch == parent.scope_epoch
            })
    });
    let coauthor_agent =
        crate::agent_child_env::coauthor::agent_id(&lifecycle, parent).map(str::to_owned);
    let origin = |channel: RouteChannel| {
        control.and_then(|control| {
            Some(RouteOrigin {
                run_nonce: crate::gh_shim_speech::service::run_nonce()?.into(),
                connection_generation: format!("{:p}", Arc::as_ptr(control)),
                channel: u32::from(channel.channel),
                epoch: u64::from(channel.epoch),
            })
        })
    };
    let launch = if let Some(owner) = &identity.run_owner {
        if matches!(owner.kind, codemode::RunKind::Worker { .. }) {
            Err(crate::gh_shim_speech::Refusal::new(
                crate::gh_shim_speech::Code::AuthorityUnavailable,
                "owner_unresolved",
            ))
        } else {
            owner
                .owner_route
                .scope
                .clone()
                .ok_or_else(|| {
                    crate::gh_shim_speech::Refusal::new(
                        crate::gh_shim_speech::Code::AuthorityUnavailable,
                        "owner_unresolved",
                    )
                })
                .and_then(|head| {
                    LaunchAuthority::codemode(
                        head,
                        lifecycle.clone(),
                        origin(route),
                        owner.owner_route_channel.and_then(origin),
                    )
                })
        }
    } else {
        LaunchAuthority::classic(lifecycle.clone(), origin(route))
    };
    Some(crate::gh_shim_speech::service::LaunchSeed {
        launch,
        lifecycle: Some(lifecycle),
        coauthor_agent,
        session: identity.session.clone(),
        harness: identity.harness.clone(),
        project_root: identity.project_root.display().to_string(),
    })
}
