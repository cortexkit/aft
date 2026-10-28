use serde_json::{json, Value};
use subc_protocol::scope::ScopeStamp;
use subc_protocol::session::ModuleControlResponseToModule;

use crate::db::access_grants::ScopeKey;

use super::{Code, Refusal, ScopeRole};

/// Only authenticated, selector-bound scope evidence can create a permanent end record.
#[derive(Clone, Debug)]
pub(crate) struct ValidatedEnd {
    key: ScopeKey,
    evidence: Value,
}

#[derive(Clone, Copy)]
pub(crate) enum ScopedRpcEndSource {
    Provider,
    RouteOpen,
}

impl ValidatedEnd {
    pub(crate) fn scoped_rpc(
        expected: &ScopeStamp,
        role: ScopeRole,
        selector: &subc_client_rs::ScopeSelector,
        request_generation: &str,
        current_generation: &str,
        source: ScopedRpcEndSource,
    ) -> Result<Self, Refusal> {
        let expected_selector = subc_client_rs::ScopeSelector::new(
            expected.owner.clone(),
            expected.scope_ref.clone(),
            Some(expected.scope_epoch),
        );
        let valid_kind = match role {
            ScopeRole::Run => {
                expected.kind == subc_protocol::scope::ScopeKind::Ephemeral
                    && expected.parent.is_some()
                    && expected.parent_state == Some(subc_protocol::scope::ParentState::Linked)
                    && expected.attributes.run_id.is_some()
            }
            ScopeRole::Authority => {
                (expected.kind == subc_protocol::scope::ScopeKind::Head
                    || (expected.kind == subc_protocol::scope::ScopeKind::Ephemeral
                        && expected.parent.is_none()))
                    && crate::gh_shim_relay::delegating_agent(expected).is_some()
            }
            ScopeRole::Head => {
                expected.kind == subc_protocol::scope::ScopeKind::Head
                    && crate::gh_shim_relay::delegating_agent(expected).is_some()
            }
        };
        if request_generation.is_empty()
            || request_generation != current_generation
            || selector != &expected_selector
            || !expected.owner_authorized
            || expected.scope_ref.is_empty()
            || !valid_kind
        {
            return Err(unknown("unverifiable", role));
        }
        let source = match source {
            ScopedRpcEndSource::Provider => "plexus.bot_request.scope_ended",
            ScopedRpcEndSource::RouteOpen => "subc.route.open.scope_ended",
        };
        Ok(Self {
            key: ScopeKey::from_stamp(expected),
            evidence: json!({"source":source,"generation":request_generation,"scope":expected,"scope_role":role}),
        })
    }
}

impl ValidatedEnd {
    pub(crate) fn key(&self) -> &ScopeKey {
        &self.key
    }

    pub(crate) fn evidence(&self) -> &Value {
        &self.evidence
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Answer {
    Live(ScopeStamp),
    Ended(ValidatedEnd),
}

fn unknown(reason: &str, role: ScopeRole) -> Refusal {
    Refusal::scoped(Code::ScopeUnknown, reason, role)
}

fn security_matches(expected: &ScopeStamp, record: &ScopeStamp) -> bool {
    record.owner == expected.owner
        && record.scope_ref == expected.scope_ref
        && record.scope_epoch == expected.scope_epoch
        && record.kind == expected.kind
        && record.owner_authorized == expected.owner_authorized
        && record.attributes == expected.attributes
        && record.parent == expected.parent
        && record.parent_state == expected.parent_state
}

/// Epoch is the cross-restart identity; connection generations only reject late replies.
pub(crate) fn validate_scope_identity(
    expected: &ScopeStamp,
    role: ScopeRole,
    reply: &ModuleControlResponseToModule,
    expected_generation: &str,
    reply_generation: &str,
    now_ms: u64,
) -> Result<Answer, Refusal> {
    if expected_generation.is_empty() || expected_generation != reply_generation {
        return Err(unknown("unverifiable", role));
    }
    let ModuleControlResponseToModule::ScopeDescribe {
        status,
        scope_epoch,
        scope,
        owner_synced,
        ..
    } = reply
    else {
        return Err(unknown("unverifiable", role));
    };
    if scope.as_ref().is_some_and(|record| {
        record.owner != expected.owner || record.scope_ref != expected.scope_ref
    }) {
        return Err(unknown("unverifiable", role));
    }
    let record_epoch = scope.as_ref().map(|record| record.scope_epoch);
    let record_kind = scope.as_ref().map(|record| record.kind);
    let expected_value =
        serde_json::to_value(expected).map_err(|_| unknown("unverifiable", role))?;
    let status_value = serde_json::to_value(status).map_err(|_| unknown("unverifiable", role))?;
    let status = status_value
        .as_str()
        .ok_or_else(|| unknown("unverifiable", role))?;
    if status == "live" {
        if !owner_synced {
            return Err(unknown("not_synced", role));
        }
        let record = scope
            .as_ref()
            .ok_or_else(|| unknown("unverifiable", role))?;
        let epoch = scope_epoch.ok_or_else(|| unknown("unverifiable", role))?;
        if record_epoch != Some(epoch) {
            return Err(unknown("unverifiable", role));
        }
        if epoch != expected.scope_epoch {
            return Err(Refusal::scoped(Code::ScopeEnded, "epoch_changed", role));
        }
        if record_kind != Some(expected.kind) || !security_matches(expected, record) {
            return Err(unknown("unverifiable", role));
        }
        let deadline = match (expected.expires_at_ms, record.expires_at_ms) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if deadline.is_some_and(|deadline| now_ms >= deadline) {
            return Err(Refusal::scoped(Code::ScopeEnded, "ended", role));
        }
        return Ok(Answer::Live(record.clone()));
    }
    if record_epoch.is_some_and(|epoch| epoch != expected.scope_epoch)
        || record_kind.is_some_and(|kind| kind != expected.kind)
        || scope_epoch.is_some_and(|epoch| epoch != expected.scope_epoch)
        || scope
            .as_ref()
            .is_some_and(|record| !security_matches(expected, record))
    {
        return Err(unknown("unverifiable", role));
    }
    let definitive = match status {
        "ended" => {
            scope_epoch == &Some(expected.scope_epoch) || record_epoch == Some(expected.scope_epoch)
        }
        "not_live" => *owner_synced,
        "not_synced" => return Err(unknown("not_synced", role)),
        _ => false,
    };
    if !definitive {
        return Err(unknown(
            if !owner_synced {
                "not_synced"
            } else {
                "unverifiable"
            },
            role,
        ));
    }
    Ok(Answer::Ended(ValidatedEnd {
        key: ScopeKey::from_stamp(expected),
        evidence: json!({"source":"scope.describe","status":status,"generation":reply_generation,"scope":expected_value}),
    }))
}

#[cfg(test)]
mod tests;
