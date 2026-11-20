//! Recognition of Prefrontal's reserved module identity.

use subc_protocol::Principal;

pub const MODULE_ID: &str = "prefrontal";

pub fn is_module(module_id: &str) -> bool {
    module_id == MODULE_ID
}

pub fn is_principal(principal: &Principal) -> bool {
    matches!(principal, Principal::Reserved { module_id } if is_module(module_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_prefrontal_is_recognized_as_prefrontal() {
        assert!(is_principal(&Principal::Reserved {
            module_id: MODULE_ID.into(),
        }));
        for id in ["prefrontal-core", "other", "reserved:prefrontal"] {
            assert!(!is_module(id));
            assert!(!is_principal(&Principal::Reserved {
                module_id: id.into(),
            }));
        }
        assert!(!is_principal(&Principal::Direct));
        assert!(!is_principal(&Principal::Unverified));
    }
}
