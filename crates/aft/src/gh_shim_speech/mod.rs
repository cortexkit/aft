//! Launch-bound bot-write authority, separate from worker reads and grants.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub(crate) mod invocation;
pub(crate) mod model;
pub(crate) mod partner;
pub(crate) mod scope;
pub(crate) mod service;
pub(crate) mod start_view;

pub(crate) const REFUSAL_STAGE: &str = "task_authority";

pub(crate) const IDENTITY_TEXT: &str = "this command's bot identity no longer matches the repository's bot ({reason}); start the command again from a live session.";
pub(crate) const POLICY_TEXT: &str = "this GitHub write is not allowed by the routing policy the command started under, or by the current one ({reason}); use a permitted operation, or ask the operator to update the policy and start the command again.";
pub(crate) const TEMPORARY_TEXT: &str = "bot writes from background commands are briefly unavailable ({reason}); retry in a few seconds.";
pub(crate) const UNAVAILABLE_TEXT: &str = "this command's bot-write permission cannot be verified ({reason}); start the command again from a live session, and tell the operator if it repeats.";
pub(crate) const ENDED_TEXT: &str = "the {scope_role} this command started from has ended ({reason}), so it can no longer write as the bot; start the command again from a live session.";
pub(crate) const UNKNOWN_TEXT: &str = "AFT could not confirm that the {scope_role} this command started from is still active ({reason}); retry in a minute.";
pub(crate) const REVOCATION_NOT_DURABLE: &str = "bot_write_revocation_not_durable: the block on this command's bot writes was not saved and lasts only until AFT restarts; check the command's state.";
// Worker reads retain their separate broker policy; worker writes always refuse.
pub(crate) const WORKER_WRITE_REFUSED: &str =
    "workers cannot write to GitHub; report what should be posted to your parent instead";
pub(crate) const WORKER_WRITE_CODE: &str = "gh_shim_worker_write_refused";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Code {
    IdentityConflict,
    PolicyDenied,
    AuthorityUnavailable,
    ScopeEnded,
    ScopeUnknown,
}

impl Code {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::IdentityConflict => "task_gh_identity_conflict",
            Self::PolicyDenied => "task_gh_policy_denied",
            Self::AuthorityUnavailable => "task_gh_authority_unavailable",
            Self::ScopeEnded => "task_gh_scope_ended",
            Self::ScopeUnknown => "task_gh_scope_unknown",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "task_gh_identity_conflict" => Self::IdentityConflict,
            "task_gh_policy_denied" => Self::PolicyDenied,
            "task_gh_authority_unavailable" => Self::AuthorityUnavailable,
            "task_gh_scope_ended" => Self::ScopeEnded,
            "task_gh_scope_unknown" => Self::ScopeUnknown,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ScopeRole {
    Authority,
    Head,
    Run,
}

impl ScopeRole {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Authority => "session",
            Self::Head => "head session",
            Self::Run => "codemode program",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Refusal {
    pub(crate) code: Code,
    pub(crate) reason: String,
    pub(crate) role: Option<ScopeRole>,
    pub(crate) detail: Option<String>,
}

impl Refusal {
    pub(crate) fn new(code: Code, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
            role: None,
            detail: None,
        }
    }

    pub(crate) fn scoped(code: Code, reason: impl Into<String>, role: ScopeRole) -> Self {
        Self {
            code,
            reason: reason.into(),
            role: Some(role),
            detail: None,
        }
    }

    pub(crate) fn text(&self) -> String {
        let template = match self.code {
            Code::IdentityConflict => IDENTITY_TEXT,
            Code::PolicyDenied => POLICY_TEXT,
            Code::AuthorityUnavailable => match self.reason.as_str() {
                "serving_lock_unavailable"
                | "storage_busy"
                | "revocation_pending"
                | "dispatch_queue_full"
                | "reservation_timeout"
                | "route_closed"
                | "relay_unavailable" => TEMPORARY_TEXT,
                _ => UNAVAILABLE_TEXT,
            },
            Code::ScopeEnded => ENDED_TEXT,
            Code::ScopeUnknown => UNKNOWN_TEXT,
        };
        template.replace("{reason}", &self.reason).replace(
            "{scope_role}",
            self.role.unwrap_or(ScopeRole::Authority).label(),
        )
    }

    pub(crate) fn agent_text(&self) -> String {
        format!("{}: {}", self.code.as_str(), self.text())
    }

    pub(crate) fn wire_data(&self) -> Value {
        let mut value = json!({
            "refusal_code": self.code.as_str(),
            "reason": self.reason,
            "stage": REFUSAL_STAGE,
            "message": self.text(),
        });
        if let Some(role) = self.role {
            value["scope_role"] = json!(role);
        }
        if let Some(detail) = &self.detail {
            value["detail"] = json!(detail);
        }
        value
    }
}

#[cfg(test)]
mod tests;
