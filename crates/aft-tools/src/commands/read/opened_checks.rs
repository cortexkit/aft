//! Read checks that judge the opened file rather than the requested name.

use std::path::{Path, PathBuf};

use crate::access_policy::{current_ask_route, AccessOperation, AskRoute};
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

/// The general temp root that admitted `identity`, when the access policy
/// applied its temp guard to this read: a standalone plugin route, or a
/// known plugin host whose route went missing, the same condition under which
/// the policy installs its temp guard roots. Reads below that root must open
/// the exact file the guard checked.
pub(super) fn general_temp_root(ctx: &AppContext, identity: &Path) -> Option<PathBuf> {
    let standalone = matches!(current_ask_route(), AskRoute::Plugin { .. });
    if !standalone && !ctx.missing_plugin_route() {
        return None;
    }
    let overridden = crate::access_policy::temp::roots_override();
    crate::access_policy::temp::guard_roots(overridden.as_deref())
        .into_iter()
        .find(|root| identity.starts_with(root))
}

#[cfg(test)]
thread_local! {
    static AFTER_AUTHORIZATION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `hook` once, after the next read on this thread is authorized and
/// before it opens anything, so a test can replace the file in that gap.
#[cfg(test)]
pub(crate) fn after_authorization_for_test(hook: impl FnOnce() + 'static) {
    AFTER_AUTHORIZATION.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
pub(super) fn authorized() {
    if let Some(hook) = AFTER_AUTHORIZATION.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}

/// The floor refusal for an opened file that is also a protected file: the
/// same answer the protected name gets, rendered for the requested name.
pub(super) fn floor_refusal(
    ctx: &AppContext,
    req: &RawRequest,
    original: &Path,
    protected: Option<&Path>,
    fallback: String,
) -> Response {
    let message = match ctx.access_policy(&req.id) {
        Ok(policy) => policy.floor_denial_message(
            AccessOperation::Read,
            protected.unwrap_or(original),
            original,
            false,
        ),
        Err(_) => fallback,
    };
    Response::error(&req.id, "denied_floor", message)
}
