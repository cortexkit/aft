//! V3: the launch outcome for one label, and the first-failing-label rule.
//!
//! Each outcome maps to one reply code (or to "proceed"), and with several
//! labels the first failing one decides: no child, no job and no proxy start
//! while any label is pending or refused.
//!
//! The codes this module names are the ones the bash and fetch launch paths
//! already return for a held or refused call, plus two of the campaign's own.

use super::eligibility::Ineligibility;
use super::vault::{MaterialError, Resolution};

/// The reply code for a launch refused because its scope is not eligible
/// (V2). It outranks every vault answer, because an ineligible call never
/// reaches the vault at all.
pub const SECRET_REFUSED: &str = "secret_refused";
/// The reply code for a label the vault cannot resolve, including every
/// production `resolve` until the vault ships (V5).
pub const SECRETS_UNAVAILABLE: &str = "secrets_unavailable";
/// The reply code for a call held on a secret consent the user has not
/// answered yet (V3). It carries its own message, below.
pub const SECRET_ACCESS_PENDING: &str = "access_pending";
/// The reply code for a secret consent that could not be filed (V3).
pub const SECRET_ACCESS_UNAVAILABLE: &str = "access_unavailable";

/// The held call's message, one constant because every agent-visible sentence
/// is reviewed as one string (V3).
pub const SECRET_ACCESS_PENDING_MESSAGE: &str = "Waiting for approval to use a secret. The request has gone to the user. End your turn now; you will be resumed with the answer, and then you should run this same call again.";

/// What a launch does for one label (V3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LabelOutcome {
    /// The rule may be followed; substitute at the declared location.
    Proceed,
    /// The vault answered `ApprovalRequired`: file one consent ask carrying
    /// the vault's request id and statement, then hold the call.
    FileConsent {
        /// The vault's identifier for the pending decision.
        request_id: String,
        /// The vault's statement bytes, base64url without padding.
        statement_b64url: String,
    },
    /// The call ends with this code and message; nothing runs.
    Refuse {
        /// The reply code.
        code: &'static str,
        /// The agent-visible sentence, one constant per message.
        message: String,
    },
}

impl LabelOutcome {
    /// Whether the launch may continue.
    pub fn proceeds(&self) -> bool {
        matches!(self, LabelOutcome::Proceed)
    }

    /// The reply code the call ends with, if it ends.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            LabelOutcome::Proceed => None,
            LabelOutcome::FileConsent { .. } => Some(SECRET_ACCESS_PENDING),
            LabelOutcome::Refuse { code, .. } => Some(code),
        }
    }
}

/// Refuse a label, naming it and the code.
pub fn refuse(code: &'static str, label: &str, detail: &str) -> LabelOutcome {
    let message = match code {
        SECRET_REFUSED => format!(
            "Secret label {label} was refused: this call's scope may not use secrets ({detail})."
        ),
        SECRETS_UNAVAILABLE => format!(
            "Secret label {label} could not be resolved: the vault is not available ({detail}). No command was started."
        ),
        _ => format!("Secret label {label} was refused ({code}): {detail}."),
    };
    LabelOutcome::Refuse { code, message }
}

/// Refuse a call whose scope is not eligible (V2): `secret_refused`, naming the
/// label, with no `resolve` call.
pub fn refuse_ineligible(label: &str, reason: Ineligibility) -> LabelOutcome {
    let detail = match reason {
        Ineligibility::NoScope => "the call carries no daemon stamp",
        Ineligibility::Standalone => "the call is not on a stamped route",
        Ineligibility::PluginOnlyRoute => "the call is on a plugin-only route",
        Ineligibility::OtherOwner => "the call's scope is not a prefrontal-core scope",
        Ineligibility::FlowRead => "the call is a read-only or flow-read call",
        Ineligibility::EphemeralOutsideCodemode => "the call's stamp is not a command lifetime",
        Ineligibility::WorkerRunSession => "the call's run belongs to a worker, not a head",
        Ineligibility::NotACommandScope => "the call's stamp kind is not a command's",
        Ineligibility::RunNotBound => "the codemode run is not bound to a live head route",
        Ineligibility::HeadScopeNotAlfonso => "the run's head scope is not a prefrontal-core head",
    };
    refuse(SECRET_REFUSED, label, detail)
}

/// The outcome for one `resolve` answer (V3).
pub fn outcome_for(label: &str, resolution: &Resolution) -> LabelOutcome {
    match resolution {
        Resolution::Granted { .. } => LabelOutcome::Proceed,
        Resolution::ApprovalRequired(request) => LabelOutcome::FileConsent {
            request_id: request.request_id.clone(),
            statement_b64url: request.statement_b64url.clone(),
        },
        Resolution::Denied { reason } => refuse(SECRET_REFUSED, label, reason),
        Resolution::Unavailable => refuse(
            SECRETS_UNAVAILABLE,
            label,
            "no vault is connected in this build",
        ),
    }
}

/// The outcome after Cingulate answered but the vault still has not granted
/// (V3). Cingulate's answer alone never grants: the caller resolves again, and
/// this is what a second pending or refused answer leaves.
pub fn outcome_for_second_resolve(label: &str, resolution: &Resolution) -> LabelOutcome {
    match resolution {
        // The vault answered at resolution time, which a re-resolve turns into
        // either a grant or a refusal, never a second pending ask: a second
        // `ApprovalRequired` means the vault is still waiting and nothing may
        // be substituted this run.
        Resolution::ApprovalRequired(_) => refuse(
            SECRETS_UNAVAILABLE,
            label,
            "the vault is still waiting after the consent answer",
        ),
        other => outcome_for(label, other),
    }
}

/// The outcome for a `material` call that failed at request time (V3).
///
/// A lapsed grant is a refusal with no consent filed, because the proxy is
/// already live and a fresh consent would not make this request legal.
pub fn outcome_for_material(label: &str, error: &MaterialError) -> LabelOutcome {
    match error {
        MaterialError::GrantLapsed => refuse(
            SECRET_REFUSED,
            label,
            "the grant expired or its scope ended",
        ),
        MaterialError::Unavailable { detail } => refuse(SECRETS_UNAVAILABLE, label, detail),
        MaterialError::Refused { reason } => refuse(SECRET_REFUSED, label, reason),
    }
}

/// The first failing outcome across every scanned label, in scan order (V3).
///
/// `None` means every label may proceed. A pending label is a failure like any
/// other: the call is held, and no child starts.
pub fn first_failure(outcomes: &[LabelOutcome]) -> Option<&LabelOutcome> {
    outcomes.iter().find(|outcome| !outcome.proceeds())
}
