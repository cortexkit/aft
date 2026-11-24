use super::*;
use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
use crate::context::AppContext;
use crate::db::access_grants::{self as grants, ScopeKey};
use aft_shell::test_bridge::sandbox_spawn::relaxed_profile_for_test;
use std::{path::Path, sync::Arc};

#[test]
fn granted_checkout_controls_join_relaxed_denies() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("own");
    let granted = base.join("granted");
    let home = base.join("home");
    let store = base.join("store");
    let task_io = base.join("tasks/session/task/io");
    let temp = base.join("temp");
    for path in [&root, &granted, &home, &store, &task_io, &temp] {
        std::fs::create_dir_all(path).unwrap();
    }
    assert!(std::process::Command::new("git")
        .args(["init", "-q"])
        .arg(&granted)
        .status()
        .unwrap()
        .success());
    let ctx = AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.clone()),
            storage_dir: Some(store.clone()),
            ..Default::default()
        },
    );
    let scope = ScopeKey {
        owner: "reserved:prefrontal-core".into(),
        scope_ref: "checkout-controls".into(),
        epoch: 1,
    };
    let conn = crate::db::open(&store.join("aft.db")).unwrap();
    grants::register_scope(&conn, &scope, None, &ctx.config()).unwrap();
    grants::register_worker(&conn, &scope).unwrap();
    grants::insert(
        &conn,
        &scope,
        &AccessGrant {
            path: granted.clone(),
            operation: AccessOperation::Write,
            kind: GrantKind::Subtree,
        },
    )
    .unwrap();
    ctx.set_db(Arc::new(std::sync::Mutex::new(conn)));
    crate::access_policy::with_scope(Some(scope), || {
        let profile =
            relaxed_profile_for_test(&ctx, &home, &[root.clone()], &root, &store, &task_io, &temp)
                .unwrap();
        for path in [
            ".git/hooks",
            ".git/config",
            ".cortexkit/aft.jsonc",
            ".cortexkit/aft/filters",
        ] {
            assert!(
                profile.write_deny.contains(&granted.join(path)),
                "missing deny: {path}"
            );
        }
    });
}

#[test]
fn worker_scope_grants_reach_both_shell_profiles_without_widening() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let home = base.join("home");
    let worktrees = home.join(".local/share/cortexkit/alfonso/worktrees");
    let root = worktrees.join("hash-a/own");
    let granted = home.join("Work/shared");
    let sibling = home.join("Work/sibling");
    let read_only = home.join("Work/reference");
    let credential = home.join(".ssh");
    let other_scope = home.join("Work/other-scope");
    let store = base.join("store");
    let temp = base.join("temp");
    let task_io = base.join("tasks/session/task/io");
    let private = home.join(".local/share/cortexkit/another-session");
    for path in [
        &root,
        &granted,
        &sibling,
        &read_only,
        &credential,
        &other_scope,
        &store,
        &temp,
        &task_io,
        &private,
    ] {
        std::fs::create_dir_all(path).unwrap();
    }
    let file = sibling.join("one-file");
    std::fs::write(&file, "file").unwrap();
    let cargo_config = root.parent().unwrap().join(".cargo/config.toml");
    let target = home.join("Work/configured-target");
    std::fs::create_dir_all(cargo_config.parent().unwrap()).unwrap();
    std::fs::write(&cargo_config, format!("[build]\ntarget-dir = {target:?}\n")).unwrap();
    let ctx = AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.clone()),
            storage_dir: Some(store.clone()),
            sandbox: crate::config::SandboxConfig {
                read_deny: vec![credential.clone(), private.clone()],
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let scope = ScopeKey {
        owner: "reserved:prefrontal-core".into(),
        scope_ref: "worker".into(),
        epoch: 1,
    };
    let other = ScopeKey {
        scope_ref: "other".into(),
        ..scope.clone()
    };
    let conn = crate::db::open(&store.join("aft.db")).unwrap();
    for key in [&scope, &other] {
        grants::register_scope(&conn, key, None, &ctx.config()).unwrap();
        grants::register_worker(&conn, key).unwrap();
    }
    for (path, operation, kind) in [
        (&granted, AccessOperation::Write, GrantKind::Subtree),
        (&read_only, AccessOperation::Read, GrantKind::Subtree),
        (&credential, AccessOperation::Write, GrantKind::Subtree),
        (&private, AccessOperation::Write, GrantKind::Subtree),
        (&file, AccessOperation::Write, GrantKind::File),
        (&sibling, AccessOperation::Write, GrantKind::File),
    ] {
        grants::insert(
            &conn,
            &scope,
            &AccessGrant {
                path: path.clone(),
                operation,
                kind,
            },
        )
        .unwrap();
    }
    grants::insert(
        &conn,
        &other,
        &AccessGrant {
            path: other_scope.clone(),
            operation: AccessOperation::Write,
            kind: GrantKind::Subtree,
        },
    )
    .unwrap();
    ctx.set_db(Arc::new(std::sync::Mutex::new(conn)));

    crate::access_policy::with_scope(Some(scope), || {
        let policy = ctx.access_policy("").unwrap();
        let strict = policy.compile_shell(temp.clone(), vec![], vec![]).unwrap();
        let relaxed_profile = |jailed: bool| {
            crate::worker_jail_canary::with_test_preparation_home(
                &home,
                usize::from(!jailed || cfg!(target_os = "macos")),
                || {
                    with_worker_jail(jailed, || {
                        relaxed_profile_for_test(
                            &ctx,
                            &home,
                            &[root.clone()],
                            &root,
                            &store,
                            &task_io,
                            &temp,
                        )
                        .unwrap()
                    })
                },
            )
        };
        let relaxed = relaxed_profile(true);
        let head = relaxed_profile(false);
        assert!(head.data_policy.read_allow.contains(&worktrees));
        assert_eq!(
            relaxed.data_policy.read_allow.contains(&worktrees),
            !cfg!(target_os = "macos"),
            "only macOS worker profiles narrow the pool read exception"
        );
        assert!(!strict.data_policy.read_allow.contains(&worktrees));
        assert!(head.data_policy.read_files.is_empty());
        if cfg!(target_os = "macos") {
            assert!(relaxed.data_policy.read_files.contains(&cargo_config));
            assert!(relaxed.data_policy.read_allow.contains(&target));
            assert!(relaxed.data_policy.write_allow.contains(&target));
            assert!(relaxed.writable_roots.contains(&target));
        }
        for profile in [&strict, &relaxed] {
            assert!(profile.data_policy.read_allow.contains(&root));
            assert!(profile.data_policy.read_allow.contains(&temp));
        }
        for profile in [&strict, &relaxed] {
            assert!(
                profile.writable_roots.contains(&granted),
                "{:?}",
                profile.mode
            );
            assert!(
                profile.data_policy.write_allow.contains(&granted),
                "{:?}",
                profile.mode
            );
            assert!(
                profile.data_policy.read_allow.contains(&granted),
                "{:?}",
                profile.mode
            );
            assert!(
                profile.data_policy.read_allow.contains(&read_only),
                "{:?}",
                profile.mode
            );
            for excluded in [
                &sibling,
                &other_scope,
                &credential,
                &private,
                &read_only,
                &file,
            ] {
                assert!(
                    !profile.writable_roots.contains(excluded),
                    "{:?}: {}",
                    profile.mode,
                    excluded.display()
                );
                assert!(
                    !profile.data_policy.write_allow.contains(excluded),
                    "{:?}: {}",
                    profile.mode,
                    excluded.display()
                );
            }
            assert!(!profile.data_policy.read_allow.contains(&credential));
            assert!(!profile.data_policy.read_allow.contains(&private));
            assert!(!profile.data_policy.read_allow.contains(&other_scope));
            assert!(!profile.data_policy.read_allow.contains(&sibling));
        }
        assert!(relaxed.write_deny_nested.contains(&home.join("Work")));
        #[cfg(target_os = "linux")]
        {
            let linux =
                compute_linux_profile(strict, &home, &[], &[], &mut SecureReadDirectoryLister)
                    .unwrap();
            assert!(linux.writable_roots.contains(&granted));
            assert!(linux.read_allow.contains(&granted));
            assert!(linux.read_allow.contains(&read_only));
            assert!(!linux
                .read_allow
                .iter()
                .any(|path| credential.starts_with(path)));
        }
    });
    assert!(ctx.access_policy("").unwrap().grants.is_empty());
}
