//! Command features may tighten the role's sandbox policy, never relax it.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SandboxRequirementReason {
    SecretPlaceholder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SandboxRequirement {
    pub(crate) mode: SandboxMode,
    pub(crate) reason: SandboxRequirementReason,
}

impl SandboxRequirement {
    pub(crate) fn for_command(command: &str) -> Option<Self> {
        (!secret_rules::placeholder::scan(command).is_empty()).then_some(Self {
            mode: SandboxMode::Strict,
            reason: SandboxRequirementReason::SecretPlaceholder,
        })
    }

    pub(crate) fn raise(self, mode: SandboxMode) -> SandboxMode {
        match (mode, self.mode) {
            (SandboxMode::Strict, _) | (_, SandboxMode::Strict) => SandboxMode::Strict,
            (SandboxMode::Relaxed, _) | (_, SandboxMode::Relaxed) => SandboxMode::Relaxed,
            _ => SandboxMode::Off,
        }
    }

    pub(crate) fn refusal_code(self) -> &'static str {
        match self.reason {
            SandboxRequirementReason::SecretPlaceholder => "secrets_sandbox_required",
        }
    }

    pub(crate) fn refusal_message(self, cause: impl std::fmt::Display) -> String {
        match self.reason {
            SandboxRequirementReason::SecretPlaceholder => format!(
                "This command needs the strict sandbox because it uses a vault key, but {cause}. No command was started."
            ),
        }
    }

    pub(crate) fn check_request(
        self,
        tier: RequestedSandboxTier,
        supported_platform: bool,
    ) -> Result<(), String> {
        if tier == RequestedSandboxTier::Host {
            return Err(
                self.refusal_message("a host-escalation request would bypass the strict sandbox")
            );
        }
        if !supported_platform {
            return Err(self.refusal_message(
                "this platform has no strict native sandbox (Windows is unsupported)",
            ));
        }
        Ok(())
    }

    #[cfg(unix)]
    pub(super) fn refused(self, cause: impl std::fmt::Display) -> SpawnPlan {
        SpawnPlan::Refused {
            code: self.refusal_code(),
            message: self.refusal_message(cause),
            mismatch_class: None,
        }
    }

    #[cfg(unix)]
    fn check_plan(self, plan: SpawnPlan) -> SpawnPlan {
        let cause = match self.reason {
            SandboxRequirementReason::SecretPlaceholder if plan.protects_secret_memory() => None,
            SandboxRequirementReason::SecretPlaceholder => plan.secret_memory_unavailable(),
        };
        if let Some(cause) = cause {
            plan.cleanup_unspawned();
            self.refused(cause)
        } else {
            plan
        }
    }
}

pub(crate) fn resolve_command_sandbox_spawn(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    requested_tier: RequestedSandboxTier,
    task_kind: SandboxTaskKind,
    task_bundle_dir: &Path,
    host_escalation: Option<&HostEscalationAttempt>,
    requirement: Option<SandboxRequirement>,
) -> SpawnPlan {
    if let Some(requirement) = requirement {
        if let Err(message) = requirement.check_request(
            requested_tier,
            cfg!(any(target_os = "macos", target_os = "linux")),
        ) {
            return SpawnPlan::Refused {
                code: requirement.refusal_code(),
                message,
                mismatch_class: None,
            };
        }
    }
    let mode = effective_sandbox_mode(ctx, principal);
    let mode = requirement.map_or(mode, |requirement| requirement.raise(mode));
    let plan = resolve_sandbox_spawn_with_mode(
        ctx,
        principal,
        requested_tier,
        task_kind,
        task_bundle_dir,
        host_escalation,
        mode,
        requirement,
    );
    #[cfg(unix)]
    if let Some(requirement) = requirement {
        if let Some(message) = plan.refusal_message() {
            if plan.refusal_code() != Some(requirement.refusal_code()) {
                return requirement.refused(format!(
                    "the strict sandbox could not be prepared: {message}"
                ));
            }
        }
        if plan.refusal_code().is_none() {
            return requirement.check_plan(plan);
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_bash_sandbox_raise_requirement_never_lowers_and_names_request_causes() {
        assert!(SandboxRequirement::for_command("echo ordinary").is_none());
        let requirement = SandboxRequirement::for_command("echo '{{ck-secret:test}}'").unwrap();
        assert_eq!(
            requirement.reason,
            SandboxRequirementReason::SecretPlaceholder
        );
        for mode in [SandboxMode::Off, SandboxMode::Relaxed, SandboxMode::Strict] {
            assert_eq!(requirement.raise(mode), SandboxMode::Strict);
        }
        assert_eq!(requirement.check_request(RequestedSandboxTier::Native, false).unwrap_err(),
            "This command needs the strict sandbox because it uses a vault key, but this platform has no strict native sandbox (Windows is unsupported). No command was started.");
        assert_eq!(requirement.check_request(RequestedSandboxTier::Host, true).unwrap_err(),
            "This command needs the strict sandbox because it uses a vault key, but a host-escalation request would bypass the strict sandbox. No command was started.");
    }
}
