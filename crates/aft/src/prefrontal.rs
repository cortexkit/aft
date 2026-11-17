//! Compatibility identities for the prefrontal-core → prefrontal module rename.

use subc_protocol::Principal;

pub(crate) const LEGACY_MODULE_ID: &str = "prefrontal-core";
pub(crate) const MODULE_IDS: [&str; 2] = ["prefrontal", LEGACY_MODULE_ID];

pub(crate) fn is_module(module_id: &str) -> bool {
    MODULE_IDS.contains(&module_id)
}

pub(crate) fn is_principal(principal: &Principal) -> bool {
    matches!(principal, Principal::Reserved { module_id } if is_module(module_id))
}

/// Prefer the renamed module when advertised; old catalogs keep the old target.
pub(crate) fn catalog_module<'a>(ids: impl IntoIterator<Item = &'a str>) -> &'static str {
    let ids: Vec<_> = ids.into_iter().collect();
    MODULE_IDS
        .into_iter()
        .find(|id| ids.contains(id))
        .unwrap_or(LEGACY_MODULE_ID)
}

/// Grant, scope and role storage keeps one key across the rename. Do not use
/// this key for call-ledger rows awaiting completion, bus subjects or history:
/// those retain the original wire identity.
pub(crate) fn grant_owner_key(principal: &Principal) -> String {
    let canonical;
    let principal = if is_principal(principal) {
        canonical = Principal::Reserved {
            module_id: LEGACY_MODULE_ID.into(),
        };
        &canonical
    } else {
        principal
    };
    serde_json::to_string(principal).expect("scope owner serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefrontal_rename_admits_only_reserved_aliases() {
        for id in MODULE_IDS {
            assert!(is_principal(&Principal::Reserved {
                module_id: id.into()
            }));
        }
        assert!(!is_principal(&Principal::Direct));
        assert!(!is_principal(&Principal::Unverified));
        assert!(!is_principal(&Principal::Reserved {
            module_id: "other".into()
        }));
        assert!(!is_module("reserved:prefrontal"));
    }

    #[test]
    fn prefrontal_rename_catalog_targets_follow_both_states_and_fallback() {
        assert_eq!(catalog_module(["prefrontal-core"]), "prefrontal-core");
        assert_eq!(catalog_module(["prefrontal"]), "prefrontal");
        assert_eq!(
            catalog_module(["prefrontal-core", "prefrontal"]),
            "prefrontal"
        );
        assert_eq!(catalog_module(["other"]), "prefrontal-core");
    }
}
