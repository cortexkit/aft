//! Route-independent facts installed by daemon-side code for one synchronous call.

use crate::secret::{
    CallScopes, EligibilityForm, Ineligibility, LabelOutcome, SecretConsentRequest, SecretScope,
};
use std::{cell::RefCell, sync::Arc};

/// A launch outcome that may end the call with `access_pending`, carrying
/// the ask it needs answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingSecretConsent {
    /// The ask filed under the authorization scope.
    pub(crate) request: SecretConsentRequest,
    /// The label being waited on.
    pub(crate) label: String,
    /// The exact message the held call returns.
    pub(crate) message: &'static str,
}

/// What one launch decided for one label, before anything starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LaunchDecision {
    /// The label may be used: the rule to follow and the grant to ask for
    /// material with.
    Proceed {
        /// The label's rule, as the vault resolved it.
        rule: secret_rules::location::LabelRule,
        /// The grant the material call names.
        grant_id: String,
        expires_at_ms: u64,
    },
    /// The launch is held until the user answers the filed ask.
    Pending(PendingSecretConsent),
    /// The launch ends with this outcome, whose code names the reason.
    Refuse(LabelOutcome),
}

pub(crate) type SecretResolver =
    Arc<dyn Fn(&dyn crate::secret::SecretVault, &str) -> LaunchDecision + Send + Sync>;

#[derive(Default)]
pub(crate) struct CallContext {
    result_body: bool,
    scopes: Option<Result<(CallScopes, EligibilityForm), Ineligibility>>,
    resolver: Option<SecretResolver>,
    audit_scope: Option<SecretScope>,
}

thread_local! {
    static CURRENT: RefCell<CallContext> = RefCell::new(CallContext::default());
}

pub(crate) struct IdentityGuard(Option<SecretResolver>, Option<SecretScope>);

pub(crate) fn install_identity(
    resolver: Option<SecretResolver>,
    audit_scope: Option<SecretScope>,
) -> IdentityGuard {
    CURRENT.with(|slot| {
        let mut context = slot.borrow_mut();
        IdentityGuard(
            std::mem::replace(&mut context.resolver, resolver),
            std::mem::replace(&mut context.audit_scope, audit_scope),
        )
    })
}

impl Drop for IdentityGuard {
    fn drop(&mut self) {
        CURRENT.with(|slot| {
            let mut context = slot.borrow_mut();
            context.resolver = self.0.take();
            context.audit_scope = self.1.take();
        });
    }
}

/// Resolve only with the identity installed by dispatch, never tool arguments.
pub(crate) fn resolve_current_secret_label(
    vault: &dyn crate::secret::SecretVault,
    label: &str,
) -> LaunchDecision {
    let resolver = CURRENT.with(|slot| slot.borrow().resolver.clone());
    match resolver {
        Some(resolve) => resolve(vault, label),
        None => LaunchDecision::Refuse(crate::secret::refuse_ineligible(
            label,
            Ineligibility::NoScope,
        )),
    }
}

/// The eligibility answer for the tool call executing on this thread.
/// An unresolved scope is never an eligible one.
pub(crate) fn current_call_scopes() -> Result<(CallScopes, EligibilityForm), Ineligibility> {
    CURRENT
        .with(|slot| slot.borrow().scopes.clone())
        .unwrap_or(Err(Ineligibility::NoScope))
}

pub(crate) fn current_secret_audit_scope() -> Option<SecretScope> {
    current_call_scopes()
        .ok()
        .map(|(scopes, _)| scopes.authorization)
        .or_else(|| CURRENT.with(|slot| slot.borrow().audit_scope.clone()))
}

/// Installs the eligibility answer for one call and restores the previous one
/// when it drops, so the executor lane never carries a stale answer into the
/// next call.
pub(crate) struct SecretScopeGuard(Option<Result<(CallScopes, EligibilityForm), Ineligibility>>);

impl SecretScopeGuard {
    pub(crate) fn install(
        scopes: Option<Result<(CallScopes, EligibilityForm), Ineligibility>>,
    ) -> Self {
        SecretScopeGuard(
            CURRENT.with(|slot| std::mem::replace(&mut slot.borrow_mut().scopes, scopes)),
        )
    }
}

impl Drop for SecretScopeGuard {
    fn drop(&mut self) {
        CURRENT.with(|slot| slot.borrow_mut().scopes = self.0.take());
    }
}

/// The executor installs the bind's policy; producers capture it before offloading.
/// This flag records whether the caller negotiated the tool-result/v1 envelope.
pub(crate) fn active() -> bool {
    CURRENT.with(|slot| slot.borrow().result_body)
}

pub(crate) struct Guard(bool);

pub(crate) fn enter(enabled: bool) -> Guard {
    Guard(CURRENT.with(|slot| std::mem::replace(&mut slot.borrow_mut().result_body, enabled)))
}

impl Drop for Guard {
    fn drop(&mut self) {
        CURRENT.with(|slot| slot.borrow_mut().result_body = self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_call_facts_restore_without_leaking_to_the_next_call() {
        assert!(!active());
        assert_eq!(current_call_scopes(), Err(Ineligibility::NoScope));
        {
            let _result = enter(true);
            let _scope = SecretScopeGuard::install(Some(Err(Ineligibility::FlowRead)));
            let _identity = install_identity(None, None);
            assert!(active());
            assert_eq!(current_call_scopes(), Err(Ineligibility::FlowRead));
            {
                let _result = enter(false);
                let _scope = SecretScopeGuard::install(None);
                let _identity = install_identity(None, None);
                assert!(!active());
                assert_eq!(current_call_scopes(), Err(Ineligibility::NoScope));
            }
            assert!(active());
            assert_eq!(current_call_scopes(), Err(Ineligibility::FlowRead));
        }
        assert!(!active());
        assert_eq!(current_call_scopes(), Err(Ineligibility::NoScope));
    }
}
