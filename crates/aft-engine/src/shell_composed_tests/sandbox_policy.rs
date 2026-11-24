
#[cfg(unix)]
fn assert_scoped_profile_grants(profile: &SandboxProfile, write: &Path, read: &Path) {
    assert!(
        profile
            .writable_roots
            .contains(&write.canonicalize().unwrap()),
        "{:?}",
        profile.writable_roots
    );
    // Seatbelt uses these read exceptions directly; Landlock also lowers
    // them into its top-level allowlist during Linux profile construction.
    assert!(
        profile
            .data_policy
            .read_allow
            .contains(&read.canonicalize().unwrap()),
        "{:?}",
        profile.data_policy.read_allow
    );
}

#[cfg(unix)]
fn assert_scoped_tool_grant_fixture(root: &Path, persist_aliases: bool) {
    use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
    use crate::db::access_grants::{self, ScopeKey};
    let project = root.join("project");
    let store = root.join("store");
    let write = root.join("approved-write");
    let read = root.join("approved-read");
    let common_temp = root.join("common-temp");
    for path in [&project, &store, &write, &read, &common_temp] {
        std::fs::create_dir(path).unwrap();
    }
    let ctx = context(project);
    ctx.update_config(|config| config.storage_dir = Some(store.clone()));
    let scope = ScopeKey {
        owner: "reserved:prefrontal-core".into(),
        scope_ref: "tool-policy".into(),
        epoch: 1,
    };
    let conn = crate::db::open(&store.join("aft.db")).unwrap();
    access_grants::register_scope(&conn, &scope, None, &ctx.config()).unwrap();
    for (path, operation) in [
        (&write, AccessOperation::Write),
        (&read, AccessOperation::Read),
    ] {
        access_grants::insert(
            &conn,
            &scope,
            &AccessGrant {
                path: path.clone(),
                operation,
                kind: GrantKind::Subtree,
            },
        )
        .unwrap();
        if persist_aliases {
            // Keep the alias in storage so replay, not just insertion,
            // must resolve the persisted path to the same directory.
            assert_ne!(*path, path.canonicalize().unwrap());
            assert_eq!(conn.execute(
                    "UPDATE access_grants SET path=?1 WHERE owner=?2 AND scope_ref=?3 AND epoch=?4 AND path=?5",
                    rusqlite::params![path.to_string_lossy(), scope.owner, scope.scope_ref, scope.epoch.to_string(), path.canonicalize().unwrap().to_string_lossy()],
                ).unwrap(), 1);
        }
    }
    if persist_aliases {
        let stored = access_grants::list(&conn, &scope).unwrap();
        assert!(stored.iter().any(|grant| grant.path == write));
        assert!(stored.iter().any(|grant| grant.path == read));
    }
    ctx.set_db(Arc::new(std::sync::Mutex::new(conn)));
    let policy =
        crate::access_policy::with_scope(Some(scope.clone()), || ToolLaunchPolicy::resolve(&ctx));
    let SpawnPlan::Launcher { profile, .. } = policy.plan_for_test() else {
        panic!(
            "a scoped strict tool must get its shell profile: {:?}",
            policy.plan_for_test()
        );
    };
    assert_scoped_profile_grants(profile, &write, &read);
    #[cfg(target_os = "linux")]
    assert!(
        profile.read_allow.contains(&read.canonicalize().unwrap()),
        "{:?}",
        profile.read_allow
    );
    if persist_aliases {
        // Seatbelt consumes the common profile without Landlock's lowering.
        let common = crate::access_policy::with_scope(Some(scope), || {
            ctx.access_policy("")
                .unwrap()
                .compile_shell(common_temp, Vec::new(), Vec::new())
                .unwrap()
                .canonicalize_for_launch()
                .unwrap()
        });
        assert!(common.read_allow.is_empty());
        let transferred: SandboxProfile =
            serde_json::from_slice(&serde_json::to_vec(&common).unwrap()).unwrap();
        assert_scoped_profile_grants(
            &transferred.canonicalize_for_launch().unwrap(),
            &write,
            &read,
        );
    }
}

#[cfg(unix)]
#[test]
fn tool_policy_keeps_session_grants_that_config_alone_cannot_recover() {
    let root = tempfile::tempdir().unwrap();
    assert_scoped_tool_grant_fixture(root.path(), false);
}

#[cfg(unix)]
#[test]
fn tool_policy_keeps_session_grants_stored_under_var_folders_alias() {
    // Reproduce /var -> /private/var beneath a fixture root on every Unix host.
    let root = tempfile::tempdir().unwrap();
    let canonical_root = root.path().canonicalize().unwrap();
    let real = canonical_root.join("private/var/folders");
    std::fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(
        canonical_root.join("private/var"),
        canonical_root.join("var"),
    )
    .unwrap();
    let alias = canonical_root.join("var/folders");
    assert_eq!(alias.canonicalize().unwrap(), real);
    assert_scoped_tool_grant_fixture(&alias, true);
}

#[cfg(unix)]
#[test]
fn native_sandbox_predicate_controls_spawn_and_rewrite() {
    let project = tempfile::tempdir().unwrap();
    let file = project.path().join("rewrite-probe.txt");
    std::fs::write(&file, "sandboxed\n").unwrap();
    let ctx = context(project.path().to_path_buf());
    ctx.update_config(|config| config.experimental_bash_rewrite = true);
    // Capture files belong to the private task store, not the project. Using
    // the project as task IO would also deny project access on Landlock.
    let task = crate::bash_background::persistence::allocate_task_layout(
        &ctx.storage_dir(),
        "sandbox-predicate-test",
    )
    .unwrap();
    let principal = AuthenticatedPrincipal::FirstParty;
    let command = format!("cat {}", file.display());

    assert!(native_sandbox_enforced(&ctx, &principal));
    assert!(crate::bash_rewrite::try_rewrite(&command, None, &ctx, &principal).is_none());
    let sandboxed = resolve_sandbox_spawn(
        &ctx,
        &principal,
        RequestedSandboxTier::Native,
        SandboxTaskKind::BashForeground,
        &task.paths.io_dir,
        None,
    );
    assert!(
        matches!(&sandboxed, SpawnPlan::Launcher { .. }),
        "native sandbox must use the launcher: {sandboxed:?}"
    );
    sandboxed.cleanup_unspawned();

    ctx.update_config(|config| config.sandbox.profile.head = crate::config::SandboxMode::Off);
    assert!(!native_sandbox_enforced(&ctx, &principal));
    assert!(crate::bash_rewrite::try_rewrite(&command, None, &ctx, &principal).is_some());
    assert_eq!(
        resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Native,
            SandboxTaskKind::BashForeground,
            &task.paths.io_dir,
            None,
        ),
        SpawnPlan::Unsandboxed
    );
}
use crate::context::AppContext;
use crate::sandbox_profile::SandboxProfile;
use crate::sandbox_spawn::*;
use aft_shell::test_bridge::{SpawnPlanTestExt as _, ToolLaunchPolicyTestExt as _};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

fn context(project_root: PathBuf) -> AppContext {
    use crate::config::{Config, SandboxConfig, SandboxMode, SandboxRoles};
    AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        Config {
            project_root: Some(project_root),
            sandbox: SandboxConfig {
                profile: SandboxRoles {
                    head: SandboxMode::Strict,
                    worker: SandboxMode::Strict,
                },
                ..Default::default()
            },
            ..Default::default()
        },
    )
}

#[test]
fn pending_provider_grant_does_not_keep_its_context_alive() {
    let project = tempfile::tempdir().unwrap();
    let ctx = context(project.path().to_owned());
    let (_, scratch) = aft_shell::test_bridge::sandbox_spawn::provider_layout_grant_for_test(
        &ctx,
        Duration::from_secs(30),
    );
    assert!(scratch.exists());
    let weak = Arc::downgrade(ctx.escalation_grants());
    drop(ctx);

    let deadline = Instant::now() + Duration::from_secs(2);
    while weak.upgrade().is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        weak.upgrade().is_none(),
        "the pending expiry timer kept its owner alive"
    );
    assert!(
        !scratch.exists(),
        "dropping the owner must release provider scratch"
    );
}
