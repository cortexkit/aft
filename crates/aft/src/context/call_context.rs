//! Route-independent facts installed by daemon-side code for one synchronous call.

use crate::secret::{CallScopes, EligibilityForm, Ineligibility, SecretScope};
use std::{cell::RefCell, path::PathBuf};

#[derive(Default)]
pub(crate) struct CallContext {
    result_body: bool,
    scopes: Option<Result<(CallScopes, EligibilityForm), Ineligibility>>,
    consent_context: Option<(PathBuf, String)>,
    audit_scope: Option<SecretScope>,
}

thread_local! {
    static CURRENT: RefCell<CallContext> = RefCell::new(CallContext::default());
}

pub(crate) struct IdentityGuard(Option<(PathBuf, String)>, Option<SecretScope>);

pub(crate) fn install_identity(
    consent_context: Option<(PathBuf, String)>,
    audit_scope: Option<SecretScope>,
) -> IdentityGuard {
    CURRENT.with(|slot| {
        let mut context = slot.borrow_mut();
        IdentityGuard(
            std::mem::replace(&mut context.consent_context, consent_context),
            std::mem::replace(&mut context.audit_scope, audit_scope),
        )
    })
}

impl Drop for IdentityGuard {
    fn drop(&mut self) {
        CURRENT.with(|slot| {
            let mut context = slot.borrow_mut();
            context.consent_context = self.0.take();
            context.audit_scope = self.1.take();
        });
    }
}

pub(crate) fn current_secret_consent_context() -> Option<(PathBuf, String)> {
    CURRENT.with(|slot| slot.borrow().consent_context.clone())
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
