pub use aft_base::secret::call_context::*;

#[cfg(test)]
#[allow(unused_imports)]
use crate::secret::{CallScopes, EligibilityForm, Ineligibility, SecretScope};
#[cfg(test)]
#[allow(unused_imports)]
use std::{cell::RefCell, path::PathBuf};

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
