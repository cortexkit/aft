use super::*;
use crate::gh_shim_speech::{Code, Refusal};

pub(super) fn run_captured_protected(
    args: &[OsString],
    paths: &StatePaths,
    manifest_guard: &manifest_guard::ManifestGuard<'_>,
    target: &TargetRepository,
    cwd: &Path,
    now: u64,
) -> i32 {
    // Background writes retain a signed policy snapshot from command launch,
    // but still require the current manifest. Report its absence before dispatch.
    if matches!(
        resolve_manifest(paths, now),
        ManifestResolution::Invalid(_) | ManifestResolution::Dormant
    ) {
        if let Some(status) = manifest_guard.unavailable_refusal(args) {
            return status;
        }
    }
    let (manifest, classification, binding, rung) = match captured_plan(
        args, paths, target, cwd, now,
    ) {
        Ok(plan) => plan,
        Err(PlanError::Undeclared) => {
            return refuse(
                RefusalCode::SeamRefusal,
                &format!("{}: this GitHub command is not declared in the signed manifest (not_governed); ask the operator to update the manifest and start the command again.", Code::PolicyDenied.as_str()),
            );
        }
        Err(PlanError::Refusal(error)) => {
            let mut text = error.agent_text();
            // run_with already refuses worker writes. Keep operator-bypass
            // advice here instead of attaching it to shared refusal text.
            if error.code == Code::PolicyDenied && error.reason == "not_governed" {
                text.push_str(" This GitHub command is not a bot operation. If it should run as the operator, run it again with GH_SHIM_BYPASS=operator; the shim records that use.");
            }
            return refuse(RefusalCode::SeamRefusal, &text);
        }
    };
    // Protected invocations never hand native gh the operator's credentials.
    dispatch_r3(
        args,
        classification,
        &manifest,
        paths,
        &rung,
        &binding,
        now,
        |_| {
            refuse(
                RefusalCode::SeamRefusal,
                &captured_rung_refusal(&rung).agent_text(),
            )
        },
    )
}

pub(super) fn captured_rung_refusal(rung: &RungRecord) -> Refusal {
    // Discovery records survive module restarts. A missing daemon or relay is
    // retryable; missing policy configuration or ambient credentials are not.
    let temporary = match rung.rung {
        Rung::R1 => [R1Reason::Unreachable, R1Reason::DiscoveryBudgetExhausted]
            .into_iter()
            .any(|reason| {
                rung.inputs.get("connection_file").map(String::as_str) == Some(reason.diagnostic())
            }),
        Rung::R2 => [
            R2Reason::DaemonUnreachable,
            R2Reason::CatalogGhRouteAbsent,
            R2Reason::GhRouteHolderUnbound,
        ]
        .into_iter()
        .any(|reason| rung.inputs.get(reason.diagnostic()).map(String::as_str) == Some("failed")),
        Rung::R3 => false,
    };
    if temporary {
        Refusal::new(Code::AuthorityUnavailable, "relay_unavailable")
    } else {
        Refusal::new(Code::PolicyDenied, "not_r3")
    }
}

pub(super) enum PlanError {
    Undeclared,
    Refusal(Refusal),
}

impl From<Refusal> for PlanError {
    fn from(error: Refusal) -> Self {
        Self::Refusal(error)
    }
}

impl PlanError {
    #[cfg(test)]
    pub(super) fn refusal(self) -> Refusal {
        match self {
            Self::Undeclared => Refusal::new(Code::PolicyDenied, "not_governed"),
            Self::Refusal(error) => error,
        }
    }
}

pub(super) fn captured_plan(
    args: &[OsString],
    paths: &StatePaths,
    target: &TargetRepository,
    cwd: &Path,
    now: u64,
) -> Result<(Manifest, Classification, AgentBinding, RungRecord), PlanError> {
    if shim_disabled_by_operator() {
        return Err(Refusal::new(Code::PolicyDenied, "not_r3").into());
    }
    if operator_bypass_requested() || operator_credential_use(args).is_some() {
        return Err(Refusal::new(Code::PolicyDenied, "not_governed").into());
    }
    load_manifest(paths, now)
        .map_err(|_| Refusal::new(Code::AuthorityUnavailable, "current_manifest_unverifiable"))?;
    let (snapshot, _, _) = capture_speech_policy()?
        .ok_or_else(|| Refusal::new(Code::AuthorityUnavailable, "current_manifest_unverifiable"))?;
    let manifest = verified_speech_manifest(&snapshot.signed_envelope)
        .map_err(|_| Refusal::new(Code::AuthorityUnavailable, "current_manifest_unverifiable"))?;
    let classification = classify(args, &manifest, current_platform());
    match classification {
        Classification::Governed { .. } => {}
        Classification::Unclassified => return Err(PlanError::Undeclared),
        _ => return Err(Refusal::new(Code::PolicyDenied, "not_governed").into()),
    }
    let binding = governing_binding(&classification, &manifest, target, cwd)
        .ok_or_else(|| Refusal::new(Code::PolicyDenied, "repository_denied"))?;
    let determination = determine_rung(paths, target, cwd, now);
    if determination.record.rung != Rung::R3 {
        return Err(captured_rung_refusal(&determination.record).into());
    }
    Ok((manifest, classification, binding, determination.record))
}
