//! Compatibility identities for the prefrontal-core → prefrontal module rename.

use subc_protocol::Principal;
use subc_protocol::{
    scope::{ScopeStamp, ScopeStatus},
    session::ModuleControlResponseToModule,
};

pub(crate) const LEGACY_MODULE_ID: &str = "prefrontal-core";
pub(crate) const MODULE_IDS: [&str; 2] = ["prefrontal", LEGACY_MODULE_ID];

pub(crate) fn is_module(module_id: &str) -> bool {
    MODULE_IDS.contains(&module_id)
}

pub(crate) fn is_principal(principal: &Principal) -> bool {
    matches!(principal, Principal::Reserved { module_id } if is_module(module_id))
}

/// The other owner used during the prefrontal module rename, if applicable.
pub(crate) fn counterpart_owner(owner: &Principal) -> Option<Principal> {
    let Principal::Reserved { module_id } = owner else {
        return None;
    };
    if !is_module(module_id) {
        return None;
    }
    MODULE_IDS
        .iter()
        .find(|id| **id != module_id)
        .map(|id| Principal::Reserved {
            module_id: (*id).into(),
        })
}

pub(crate) fn counterpart(stamp: &ScopeStamp) -> Option<ScopeStamp> {
    let mut other = stamp.clone();
    other.owner = counterpart_owner(&stamp.owner)?;
    Some(other)
}

/// A missing counterpart module cannot own a continuation. Preserve the
/// original single-owner wire exchange until the catalog lists the other name.
pub(crate) fn counterpart_in_catalog(stamp: &ScopeStamp, configured: &[String]) -> bool {
    let Some(Principal::Reserved { module_id }) = counterpart_owner(&stamp.owner) else {
        return false;
    };
    configured.contains(&module_id)
}

pub(crate) enum Continuation {
    Live(ScopeStamp),
    Hold,
    Absent,
}

/// A configured live reference continues the agent even if its epoch changed,
/// mirroring the ordinary scope.describe rebind rule for a single owner.
pub(crate) fn continuation(
    other: &ScopeStamp,
    reply: Option<&ModuleControlResponseToModule>,
) -> Continuation {
    match reply {
        Some(ModuleControlResponseToModule::ScopeDescribe {
            owner_configured: false,
            ..
        }) => Continuation::Absent,
        Some(ModuleControlResponseToModule::ScopeDescribe {
            status: ScopeStatus::Live,
            owner_configured: true,
            scope_epoch,
            ..
        }) => {
            let mut live = other.clone();
            live.scope_epoch = scope_epoch.unwrap_or(other.scope_epoch);
            Continuation::Live(live)
        }
        Some(
            ModuleControlResponseToModule::ScopeDescribe {
                status: ScopeStatus::Ended,
                ..
            }
            | ModuleControlResponseToModule::ScopeDescribe {
                status: ScopeStatus::NotLive,
                owner_synced: true,
                ..
            },
        ) => Continuation::Absent,
        _ => Continuation::Hold,
    }
}

pub(crate) fn retired(stamp: &ScopeStamp, reply: &ModuleControlResponseToModule) -> bool {
    is_principal(&stamp.owner)
        && matches!(
            reply,
            ModuleControlResponseToModule::ScopeDescribe {
                owner_configured: false,
                ..
            }
        )
}

/// Logs a continuity decision once per change. The grant sweep and scope
/// describes re-check the same scopes every few seconds, so logging each check
/// filled half the daemon log with identical lines; a repeated decision for the
/// same scope and counterpart says nothing new.
pub(crate) fn log_continuation(from: &ScopeStamp, to: &ScopeStamp, outcome: &str) {
    let line = format!("prefrontal scope continuity owner={:?} scope_ref={:?} scope_epoch={} counterpart_owner={:?} counterpart_ref={:?} counterpart_epoch={} outcome={outcome}", from.owner, from.scope_ref, from.scope_epoch, to.owner, to.scope_ref, to.scope_epoch);
    if continuity_line_is_new(format!("{:?}/{:?}", from.owner, from.scope_ref), &line) {
        log::info!("{line}");
    }
}

/// Remembers the last continuity line logged for each scope and reports
/// whether `line` differs from it.
fn continuity_line_is_new(key: String, line: &str) -> bool {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};
    // Bounded: a full map is cleared, which at worst logs each live decision once more.
    const MAX_REMEMBERED: usize = 4096;
    static LAST: LazyLock<Mutex<HashMap<String, String>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut last = LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if last.get(&key).map(String::as_str) == Some(line) {
        return false;
    }
    if last.len() >= MAX_REMEMBERED {
        last.clear();
    }
    last.insert(key, line.to_owned());
    true
}

pub(crate) fn retained_outcome(reply: Option<&ModuleControlResponseToModule>) -> &'static str {
    if matches!(
        reply,
        Some(ModuleControlResponseToModule::ScopeDescribe {
            status: ScopeStatus::Live,
            owner_configured: true,
            ..
        })
    ) {
        "source_live"
    } else {
        "hold_source"
    }
}

static CONFIGURED: std::sync::LazyLock<tokio::sync::watch::Sender<Vec<String>>> =
    std::sync::LazyLock::new(|| tokio::sync::watch::channel(Vec::new()).0);

pub(crate) fn observe_catalog<'a>(ids: impl IntoIterator<Item = &'a str>) {
    let ids: std::collections::HashSet<_> = ids.into_iter().collect();
    let names: Vec<_> = MODULE_IDS
        .iter()
        .filter(|id| ids.contains(**id))
        .map(|id| (*id).to_string())
        .collect();
    CONFIGURED.send_if_modified(|current| {
        if *current == names {
            false
        } else {
            *current = names;
            true
        }
    });
}

pub(crate) fn catalog_updates() -> tokio::sync::watch::Receiver<Vec<String>> {
    CONFIGURED.subscribe()
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

    #[test]
    fn prefrontal_scope_counterpart_is_exactly_the_other_reserved_owner() {
        for (owner, expected) in [
            ("prefrontal-core", "prefrontal"),
            ("prefrontal", "prefrontal-core"),
        ] {
            assert_eq!(
                counterpart_owner(&Principal::Reserved {
                    module_id: owner.into()
                }),
                Some(Principal::Reserved {
                    module_id: expected.into()
                })
            );
        }
        assert_eq!(counterpart_owner(&Principal::Direct), None);
        assert_eq!(
            counterpart_owner(&Principal::Reserved {
                module_id: "other".into()
            }),
            None
        );
    }

    #[test]
    fn prefrontal_scope_continuity_logs_each_decision_once_per_change() {
        let key = || "continuity-log-dedupe-agent".to_owned();
        let decisions = [
            "outcome=source_live",
            "outcome=source_live",
            "outcome=hold_source",
            "outcome=hold_source",
            "outcome=source_live",
        ]
        .map(|line| continuity_line_is_new(key(), line));
        assert_eq!(decisions, [true, false, true, false, true]);
        // Another scope keeps its own last line.
        assert!(continuity_line_is_new(
            "continuity-log-dedupe-other".into(),
            "outcome=source_live"
        ));
    }

    #[test]
    fn prefrontal_scope_counterpart_probe_requires_its_catalog_entry() {
        let old = ScopeStamp::new(
            Principal::Reserved {
                module_id: "prefrontal-core".into(),
            },
            "agent",
            7,
            subc_protocol::scope::ScopeKind::Head,
        );
        let new = counterpart(&old).unwrap();
        assert!(!counterpart_in_catalog(&old, &[]));
        assert!(!counterpart_in_catalog(&old, &["prefrontal-core".into()]));
        assert!(counterpart_in_catalog(&old, &["prefrontal".into()]));
        assert!(counterpart_in_catalog(&new, &["prefrontal-core".into()]));
        assert!(!counterpart_in_catalog(&new, &["prefrontal".into()]));
    }

    #[test]
    fn prefrontal_scope_continuation_requires_a_configured_live_owner() {
        let old = ScopeStamp::new(
            Principal::Reserved {
                module_id: "prefrontal-core".into(),
            },
            "agent",
            7,
            subc_protocol::scope::ScopeKind::Head,
        );
        let other = counterpart(&old).unwrap();
        for (status, configured, synced, expected) in [
            (ScopeStatus::Live, true, true, "live"),
            (ScopeStatus::Live, false, true, "absent"),
            (ScopeStatus::NotLive, true, false, "hold"),
            (ScopeStatus::NotLive, false, false, "absent"),
            (ScopeStatus::NotLive, true, true, "absent"),
            (ScopeStatus::Ended, true, true, "absent"),
        ] {
            let reply = ModuleControlResponseToModule::ScopeDescribe {
                status,
                scope_epoch: Some(8),
                daemon_incarnation: "test".into(),
                owner_synced: synced,
                owner_configured: configured,
                scope: None,
            };
            let actual = match continuation(&other, Some(&reply)) {
                Continuation::Live(live) => {
                    assert_eq!(live.scope_epoch, 8);
                    assert_eq!(live.owner, other.owner);
                    "live"
                }
                Continuation::Hold => "hold",
                Continuation::Absent => "absent",
            };
            assert_eq!(actual, expected);
        }
        assert!(matches!(continuation(&other, None), Continuation::Hold));
    }
}
