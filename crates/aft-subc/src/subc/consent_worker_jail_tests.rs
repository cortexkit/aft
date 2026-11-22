use crate::sandbox_spawn::{
    resolve_sandbox_spawn, with_worker_jail, RequestedSandboxTier, SandboxTaskKind, SpawnPlan,
};
#[cfg(test)]
use aft_engine::test_bridge::bash_remote::SpawnPlanTestExt as _;

fn jail_context(root: &Path, sandbox: bool) -> AppContext {
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.to_path_buf());
    config.storage_dir = Some(root.join(".store"));
    config.sandbox.profile.worker = crate::config::SandboxMode::Strict;
    config.sandbox.profile.head = if sandbox {
        crate::config::SandboxMode::Strict
    } else {
        crate::config::SandboxMode::Off
    };
    crate::test_context(Box::new(crate::parser::TreeSitterProvider::new()), config)
}

#[test]
fn consent_worker_jail_never_runs_a_shell_unconfined() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("worktree");
    let task = dir.path().join("task").join("io");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&task).unwrap();
    let ctx = jail_context(&root, false);
    let principal = AuthenticatedPrincipal::RouteBind {
        trust: PrincipalTrust::FirstParty,
        route_channel: 41,
        route_epoch: 1,
        project_root: root.clone(),
        harness: "runner".into(),
        session_id: "worker".into(),
        principal_id: Some("reserved:broca".into()),
    };
    let resolve = |tier| {
        resolve_sandbox_spawn(
            &ctx,
            &principal,
            tier,
            SandboxTaskKind::BashForeground,
            &task,
            None,
        )
    };
    // A head (no jail) keeps the user's choice: sandbox off runs unsandboxed.
    assert!(matches!(
        resolve(RequestedSandboxTier::Disabled),
        SpawnPlan::Unsandboxed
    ));
    for mode in [
        crate::config::SandboxMode::Credentials,
        crate::config::SandboxMode::Standard,
        crate::config::SandboxMode::Strict,
    ] {
        ctx.update_config(|config| config.sandbox.profile.worker = mode);
        with_worker_jail(true, || {
            // The worker profile is independent of the head profile. Windows
            // Credentials and Standard are explicitly unenforced; Strict must still fail closed.
            let native = resolve(RequestedSandboxTier::Native);
            #[cfg(windows)]
            {
                assert!(!native.is_native_launcher(), "{native:?}");
                match mode {
                    crate::config::SandboxMode::Standard
                    | crate::config::SandboxMode::Credentials => {
                        assert_eq!(
                            native,
                            SpawnPlan::Unenforced {
                                mode,
                                worker_gh_fence: true
                            }
                        );
                    }
                    crate::config::SandboxMode::Strict => {
                        assert_eq!(native.refusal_code(), Some("sandbox_unavailable"));
                    }
                    crate::config::SandboxMode::Off => unreachable!(),
                }
            }
            // macOS/Linux workers must use confinement or refuse the shell.
            #[cfg(not(windows))]
            assert!(
                native.is_native_launcher() || native.refusal_code() == Some("sandbox_unavailable"),
                "{native:?}"
            );
            if let SpawnPlan::Launcher { profile, .. } = &native {
                assert_eq!(profile.mode, mode);
                if mode == crate::config::SandboxMode::Strict {
                    // Writes are confined to the worktree: the directory holding it
                    // (and the sibling worktrees in it) is not writable.
                    let worktree = root.canonicalize().unwrap();
                    let parent = worktree.parent().unwrap();
                    assert!(profile.writable_roots.contains(&worktree), "{profile:?}");
                    assert!(
                        !profile
                            .writable_roots
                            .iter()
                            .any(|writable| parent.starts_with(writable)),
                        "{profile:?}"
                    );
                }
            }
            for tier in [RequestedSandboxTier::Disabled, RequestedSandboxTier::Host] {
                assert_eq!(resolve(tier).refusal_code(), Some("sandbox_unavailable"));
            }
            let untrusted = AuthenticatedPrincipal::RouteBind {
                trust: PrincipalTrust::Untrusted,
                route_channel: 41,
                route_epoch: 1,
                project_root: root.clone(),
                harness: "runner".into(),
                session_id: "worker".into(),
                principal_id: None,
            };
            // Untrusted routes now run under strict confinement instead of being
            // rejected solely for being untrusted. They must never inherit relaxed
            // or off from the role profile.
            let untrusted_plan = resolve_sandbox_spawn(
                &ctx,
                &untrusted,
                RequestedSandboxTier::Native,
                SandboxTaskKind::BashForeground,
                &task,
                None,
            );
            match untrusted_plan {
                SpawnPlan::Launcher { profile, .. } => {
                    assert_eq!(profile.mode, crate::config::SandboxMode::Strict);
                }
                plan => assert_eq!(plan.refusal_code(), Some("sandbox_unavailable")),
            }
        });
    }
    assert!(!crate::sandbox_spawn::worker_jail_active());
}
