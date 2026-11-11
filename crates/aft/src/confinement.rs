//! Runtime adapter for the request-local read fence.

pub(crate) use crate::data_layer::read_fence::*;
use crate::{context::AppContext, protocol::Response};
use std::{path::Path, sync::Arc};

pub(crate) fn with_root<R>(
    ctx: &AppContext,
    id: &str,
    run: impl FnOnce() -> Result<R, Response>,
) -> Result<R, Response> {
    crate::data_layer::read_fence::with_root(id, || {
        ctx.with_force_restrict(id, || {
            let policy = ctx.access_policy(id)?;
            let root = ctx
                .config()
                .project_root
                .as_ref()
                .map(|root| {
                    crate::path_identity::normalize_path(&ctx.resolved_path_restriction_root(root))
                })
                .ok_or_else(|| refusal(id, Path::new(".")))?;
            let decision_root = root.clone();
            let gate = ReadGate::new(
                root,
                Arc::new(move |path| {
                    use crate::access_policy::{AccessDecision, AccessOperation};
                    let identity = crate::access_policy::policy_path(path);
                    identity.starts_with(&decision_root)
                        && policy.decide_resolved(AccessOperation::Read, &identity, false)
                            == AccessDecision::Allow
                }),
            );
            with_gate(gate, run)
        })
    })
}
