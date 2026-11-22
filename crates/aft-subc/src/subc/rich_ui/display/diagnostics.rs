//! Only fixed error labels cross into health or logs; SDK messages can contain
//! caller identities and request data.
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use subc_client_rs::CallError;

const CODES: &[&str] = &[
    "daemon_too_old",
    "not_sent",
    "outcome_unknown",
    "module_error",
    "subscription_backpressure",
    "stale_route_handle",
    "capability_unprovided",
    "capability_ambiguous",
    "invalid_capability_identifier",
    "invalid_request",
    "unknown_module",
    "module_removed",
    "module_reloading",
    "module_warming",
    "target_unavailable",
    "target_flow_unsupported",
    "target_agent_run_unsupported",
    "module_timeout",
    "module_no_protocol",
    "route_busy",
    "bind_busy",
    "scope_not_synced",
    "scope_changed",
    "scope_epoch_required",
    "scope_not_live",
    "scope_ended",
    "scope_not_carrier",
    "scope_bind_proof_required",
    "scope_bind_proof_mismatch",
    "scope_unsupported",
    "delegation_not_registered",
    "unlisted",
];
static LAST: AtomicUsize = AtomicUsize::new(usize::MAX);
static WARNED: AtomicU64 = AtomicU64::new(0);

fn error_code(error: &CallError) -> &'static str {
    if let Some(code) = error
        .route_open_refusal()
        .map(|body| body.code.as_str())
        .or(error.code())
    {
        return CODES
            .iter()
            .copied()
            .find(|known| *known == code)
            .unwrap_or("unlisted");
    }
    match error {
        CallError::DaemonTooOld { .. } => "daemon_too_old",
        CallError::NotSent(_) => "not_sent",
        CallError::OutcomeUnknown(_) => "outcome_unknown",
        CallError::Module(_) => "module_error",
        CallError::SubscriptionBackpressure(_) => "subscription_backpressure",
        CallError::StaleRouteHandle(_) => "stale_route_handle",
        CallError::CapabilityUnprovided { .. } => "capability_unprovided",
        CallError::CapabilityAmbiguous { .. } => "capability_ambiguous",
        CallError::InvalidCapabilityIdentifier { .. } => "invalid_capability_identifier",
    }
}

pub(super) fn route_open(error: &CallError) {
    let code = error_code(error);
    let index = CODES
        .iter()
        .position(|known| *known == code)
        .expect("fixed display error code");
    LAST.store(index, Ordering::Relaxed);
    // Each fixed code gets at most one warning per process, even if retries
    // keep failing. No peer-provided text is formatted into the warning.
    if WARNED.fetch_or(1u64 << index, Ordering::Relaxed) & (1u64 << index) == 0 {
        log::warn!("tool display failed stage=route_open code={code}");
    }
}

pub(super) fn health() -> serde_json::Value {
    CODES.get(LAST.load(Ordering::Relaxed)).map_or(
        serde_json::Value::Null,
        |code| serde_json::json!({"stage":"route_open", "code":code}),
    )
}

#[cfg(test)]
#[test]
fn display_route_error_diagnostics_never_retain_peer_text() {
    let error = CallError::Module(subc_protocol::ErrorBody::new(
        "private-token",
        "private-payload",
    ));
    assert_eq!(error_code(&error), "unlisted");
    assert!(CODES.len() <= 64);
}
