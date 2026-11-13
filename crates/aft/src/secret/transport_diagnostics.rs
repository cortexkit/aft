//! Secret-free diagnostics retained across the transport worker boundary.

use std::fmt;

use subc_client_rs::{CallError, ConsumerError, NotSentCause};
use subc_protocol::scope::{ScopeKind, ScopeSelector};
use subc_protocol::{error_codes, Principal};

#[derive(Clone, Copy)]
pub(super) enum Stage {
    IdentityMissing,
    TargetRefused,
    StampDecode,
    Encode,
    Connect,
    RouteOpen,
    Request,
    ModuleError,
    Worker,
    Deadline,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Self::IdentityMissing => "identity_missing",
            Self::TargetRefused => "target_refused",
            Self::StampDecode => "stamp_decode",
            Self::Encode => "encode",
            Self::Connect => "connect",
            Self::RouteOpen => "route_open",
            Self::Request => "request",
            Self::ModuleError => "module_error",
            Self::Worker => "worker",
            Self::Deadline => "deadline",
        }
    }
}

#[derive(Clone)]
pub(super) struct Failure {
    stage: Stage,
    variant: Option<&'static str>,
    code: Option<&'static str>,
    scope: Option<(ScopeKind, String, String, Option<u64>)>,
}

impl Failure {
    pub(super) fn local(stage: Stage) -> Self {
        Self {
            stage,
            variant: None,
            code: None,
            scope: None,
        }
    }

    pub(super) fn connect(error: &ConsumerError) -> Self {
        let variant = match error {
            ConsumerError::Discovery { .. } => "Discovery",
            ConsumerError::ConnectionFile { .. } => "ConnectionFile",
            ConsumerError::NoEndpoint { .. } => "NoEndpoint",
            ConsumerError::Connect { .. } => "Connect",
            ConsumerError::Auth { .. } => "Auth",
            ConsumerError::Closed => "Closed",
        };
        Self {
            variant: Some(variant),
            ..Self::local(Stage::Connect)
        }
    }

    pub(super) fn call(stage: Stage, error: &CallError) -> Self {
        let variant = match error {
            CallError::DaemonTooOld { .. } => "DaemonTooOld",
            CallError::NotSent(_) => "NotSent",
            CallError::OutcomeUnknown(_) => "OutcomeUnknown",
            CallError::Module(_) => "Module",
            CallError::SubscriptionBackpressure(_) => "SubscriptionBackpressure",
            CallError::StaleRouteHandle(_) => "StaleRouteHandle",
            CallError::CapabilityUnprovided { .. } => "CapabilityUnprovided",
            CallError::CapabilityAmbiguous { .. } => "CapabilityAmbiguous",
            CallError::InvalidCapabilityIdentifier { .. } => "InvalidCapabilityIdentifier",
        };
        // route_open_refusal retains the daemon code even when code() is None.
        // Neither accessor requires formatting the SDK error's free-text source.
        let refusal = error.route_open_refusal().map(|body| body.code.as_str());
        let cause = error.not_sent_cause();
        let cause_code = match &cause {
            Some(NotSentCause::RouteOpenRefused { code })
            | Some(NotSentCause::DaemonRefusedBeforeForward { code }) => Some(code.as_str()),
            _ => None,
        };
        Self {
            variant: Some(variant),
            code: refusal.or(error.code()).or(cause_code).map(safe_code),
            ..Self::local(stage)
        }
    }

    pub(super) fn under_scope(mut self, kind: ScopeKind, scope: &ScopeSelector) -> Self {
        let owner = match &scope.owner {
            Principal::Reserved { module_id } => format!("reserved:{module_id}"),
            Principal::Direct => "direct".into(),
            Principal::Unverified => "unverified".into(),
        };
        // Retain only route identifiers, never the stamp or a scope bind proof.
        self.scope = Some((kind, owner, scope.scope_ref.clone(), scope.scope_epoch));
        self
    }

    pub(super) fn unavailable(self) -> String {
        crate::slog_warn!("{self}");
        super::UNAVAILABLE.to_owned()
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "secret.use transport failed stage={}", self.stage.name())?;
        if let Some(variant) = self.variant {
            write!(f, " variant={variant}")?;
        }
        if let Some(code) = self.code {
            write!(f, " code={code}")?;
        }
        if let Some((kind, owner, reference, epoch)) = &self.scope {
            let kind = match kind {
                ScopeKind::Head => "head",
                ScopeKind::Worker => "worker",
                ScopeKind::Ephemeral => "ephemeral",
            };
            write!(
                f,
                " scope_kind={kind} scope_owner={owner:?} scope_ref={reference:?}"
            )?;
            if let Some(epoch) = epoch {
                write!(f, " scope_epoch={epoch}")?;
            }
        }
        Ok(())
    }
}

fn safe_code(code: &str) -> &'static str {
    // ErrorBody.code is an unconstrained peer string in subc-protocol 0.35.0.
    // Return only known literals so even a credential in that field is discarded.
    const CODES: &[&str] = &[
        error_codes::UNKNOWN_CHANNEL,
        error_codes::STALE_ROUTE_EPOCH,
        error_codes::ROUTE_BUSY,
        error_codes::FRAME_TOO_LARGE,
        error_codes::UNKNOWN_MODULE,
        error_codes::MODULE_REMOVED,
        error_codes::MODULE_RELOADING,
        error_codes::MODULE_WARMING,
        error_codes::TARGET_UNAVAILABLE,
        error_codes::TARGET_FLOW_UNSUPPORTED,
        error_codes::TARGET_AGENT_RUN_UNSUPPORTED,
        error_codes::MODULE_TIMEOUT,
        error_codes::MODULE_NO_PROTOCOL,
        error_codes::INVALID_REQUEST,
        error_codes::SCOPE_NOT_SYNCED,
        error_codes::SCOPE_CHANGED,
        error_codes::SESSION_ADDRESS_UNAVAILABLE,
        error_codes::BIND_BUSY,
        error_codes::SCOPE_EPOCH_REQUIRED,
        error_codes::SCOPE_NOT_LIVE,
        error_codes::SCOPE_ENDED,
        error_codes::SCOPE_NOT_CARRIER,
        error_codes::SCOPE_BIND_PROOF_REQUIRED,
        error_codes::SCOPE_BIND_PROOF_MISMATCH,
        error_codes::SCOPE_UNSUPPORTED,
        error_codes::OPERATOR_DECLINED,
        error_codes::OPERATOR_PRESENCE_UNAVAILABLE,
        error_codes::OPERATOR_SUMMARY_INVALID,
        error_codes::OPERATOR_REQUEST_NOT_PERMITTED,
        // Additional route-open / resolver codes named by subc-client-rs 0.35.0.
        "delegation_not_registered",
        "daemon_too_old",
        "capability_unprovided",
        "capability_ambiguous",
        "invalid_capability_identifier",
        // Claustrum's secret.use v1 contract (secret/wire.rs and vault.rs).
        "invalid_params",
        "not_found",
        "limit_exceeded",
        "secret_setup_incomplete",
        "pending_limit",
        "store_error",
        "transient",
        "unknown_method",
    ];
    CODES
        .iter()
        .copied()
        .find(|known| *known == code)
        .unwrap_or("unlisted")
}
