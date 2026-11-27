//! GitHub policy and attribution wiring for an attached AFT module.

use super::*;
use crate::gh_shim::{manifest_sync, trust_root};

pub(super) fn with_refresh_worker(
    a0: A0Egress,
    connection_file: &Path,
    control: Arc<readiness::DaemonControl>,
) -> A0Egress {
    crate::agent_child_env::coauthor::install(connection_file.to_path_buf());
    a0.with_manifest_sync(manifest_sync::start(
        connection_file.to_path_buf(),
        move || {
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
        },
    ))
}

pub(super) fn speech_launch_seed(
    identity: &RouteIdentity,
    route: RouteChannel,
    control: Option<&Arc<readiness::DaemonControl>>,
    routes: &HashMap<RouteChannel, RouteIdentity>,
) -> Option<crate::gh_shim_speech::service::LaunchSeed> {
    use crate::gh_shim_speech::model::{LaunchAuthority, RouteOrigin};
    let lifecycle = identity.scope.clone()?;
    let worker_parent = identity.run_owner.as_ref().and_then(|owner| {
        matches!(owner.kind, codemode::RunKind::Worker { .. })
            .then_some(owner.owner_route.scope.as_ref())
            .flatten()
    });
    let heads = routes.values().filter_map(|route| route.scope.as_ref());
    let coauthor_agent =
        crate::agent_child_env::coauthor::agent_id(&lifecycle, worker_parent, heads)
            .map(str::to_owned);
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
