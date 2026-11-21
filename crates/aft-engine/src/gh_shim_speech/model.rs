use serde::{Deserialize, Serialize};
use subc_protocol::scope::{ParentState, ScopeKind, ScopeStamp};

use super::{Code, Refusal, ScopeRole};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[doc(hidden)]
pub struct RouteOrigin {
    pub run_nonce: String,
    pub connection_generation: String,
    pub channel: u32,
    pub epoch: u64,
}

/// This value comes from the authenticated dispatch context, never tool arguments.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[doc(hidden)]
pub struct LaunchAuthority {
    pub(crate) authority: ScopeStamp,
    pub(crate) run: Option<ScopeStamp>,
    pub(crate) origin: Option<RouteOrigin>,
    pub(crate) head_origin: Option<RouteOrigin>,
}

impl LaunchAuthority {
    #[doc(hidden)]
    pub fn classic(scope: ScopeStamp, origin: Option<RouteOrigin>) -> Result<Self, Refusal> {
        if scope.kind != ScopeKind::Head {
            return Err(Refusal::new(
                Code::AuthorityUnavailable,
                "scope_kind_unsupported",
            ));
        }
        let value = Self {
            authority: scope,
            run: None,
            origin,
            head_origin: None,
        };
        value.validate()?;
        Ok(value)
    }

    #[doc(hidden)]
    pub fn codemode(
        head: ScopeStamp,
        run: ScopeStamp,
        run_origin: Option<RouteOrigin>,
        head_origin: Option<RouteOrigin>,
    ) -> Result<Self, Refusal> {
        let value = Self {
            authority: head,
            run: Some(run),
            origin: run_origin,
            head_origin,
        };
        value.validate()?;
        Ok(value)
    }

    pub(crate) fn validate(&self) -> Result<(), Refusal> {
        let invalid = || Refusal::new(Code::AuthorityUnavailable, "owner_unresolved");
        let Some(agent) = crate::gh_shim_relay::delegating_agent(&self.authority)
            .filter(|agent| !agent.is_empty())
        else {
            return Err(Refusal::new(
                Code::AuthorityUnavailable,
                "start_delegation_missing",
            ));
        };
        if self.authority.scope_ref.is_empty() || self.authority.kind != ScopeKind::Head {
            return Err(invalid());
        }
        if let Some(run) = &self.run {
            let parent = run.parent.as_ref().ok_or_else(invalid)?;
            if self.authority.kind != ScopeKind::Head
                || run.kind != ScopeKind::Ephemeral
                || !run.owner_authorized
                || run.parent_state != Some(ParentState::Linked)
                || run.attributes.agent_id.as_deref() != Some(agent)
                || run.attributes.run_id.as_deref().is_none_or(str::is_empty)
                || parent.owner != self.authority.owner
                || parent.scope_ref != self.authority.scope_ref
                || parent.scope_epoch != self.authority.scope_epoch
                || run.scope_ref.is_empty()
                || run.expires_at_ms.is_none()
            {
                return Err(invalid());
            }
        }
        Ok(())
    }

    pub(crate) fn agent(&self) -> Result<&str, Refusal> {
        self.validate()?;
        self.authority
            .attributes
            .agent_id
            .as_deref()
            .ok_or_else(|| Refusal::new(Code::AuthorityUnavailable, "start_delegation_missing"))
    }

    pub(crate) fn required_scopes(&self) -> Vec<(ScopeRole, &ScopeStamp)> {
        match &self.run {
            Some(run) => vec![(ScopeRole::Head, &self.authority), (ScopeRole::Run, run)],
            None => vec![(ScopeRole::Authority, &self.authority)],
        }
    }
}

#[path = "authority_types.rs"]
pub(crate) mod authority_types;
pub(crate) use authority_types::{AuthorityRecord, Binding, ExecutionSeal, PolicySnapshot};
