#[test]
fn siblings_read_gate_requires_checkout_marker_and_canonicalizes_gitfile_root() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let child = root.path().join("child");
    std::fs::create_dir(&child).unwrap();
    let ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.path().into()),
            storage_dir: Some(storage.path().into()),
            ..Default::default()
        },
    );
    let given = child.join("..");
    let mut params = FrozenParams {
        siblings: vec![given.to_str().unwrap().into()],
        ..Default::default()
    };
    assert_eq!(
        params
            .validate_siblings(&ctx, "siblings", "session", &[])
            .map_err(refused_sibling_message),
        Err(not_checkout_root(&given))
    );
    std::fs::write(root.path().join(".git"), "gitdir: /checkout/metadata\n").unwrap();
    params
        .validate_siblings(&ctx, "siblings", "session", &[])
        .unwrap();
    assert_eq!(
        params.siblings,
        [root.path().canonicalize().unwrap().to_str().unwrap()]
    );
}

/// The exact agent-visible refusal for a sibling that is not absolute, does not resolve,
/// or has no `.git` at its top, naming the path as the plan gave it. Spelled out
/// here so a wording change cannot pass unnoticed.
fn not_checkout_root(given: &std::path::Path) -> String {
    format!(
        "sibling {} is not the root of a git checkout; the command was not run",
        given.display()
    )
}

/// The exact agent-visible refusal for a checkout root the session may not read, naming
/// the resolved path.
fn not_readable(resolved: &std::path::Path) -> String {
    format!(
        "this session's file tools may not read sibling {}; grant read access to it in allow_paths; the command was not run",
        resolved.display()
    )
}

fn refused_sibling_message(error: SiblingValidationError) -> String {
    match error {
        SiblingValidationError::Refused(message) => message,
        SiblingValidationError::Unavailable(response) => {
            panic!("unexpected unavailable policy: {response:?}")
        }
    }
}

#[test]
fn siblings_busy_grant_store_preserves_read_error_instead_of_missing_grant_refusal() {
    use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};

    let root = tempfile::tempdir().unwrap();
    let sibling = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    std::fs::create_dir(sibling.path().join(".git")).unwrap();
    let sibling = sibling.path().canonicalize().unwrap();
    let ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.path().into()),
            storage_dir: Some(storage.path().into()),
            ..Default::default()
        },
    );
    let db = Arc::new(Mutex::new(
        crate::db::open(&storage.path().join("aft.db")).unwrap(),
    ));
    ctx.set_db(db.clone());
    let scope = crate::db::access_grants::ScopeKey {
        owner: "worker-owner".into(),
        scope_ref: "worker-scope".into(),
        epoch: 1,
    };
    let grant = AccessGrant {
        path: crate::policy_path::policy_path(&sibling),
        kind: GrantKind::Subtree,
        operation: AccessOperation::Read,
    };
    crate::db::access_grants::register_scope(&db.lock().unwrap(), &scope, None, &ctx.config())
        .unwrap();
    crate::db::access_grants::insert(&db.lock().unwrap(), &scope, &grant).unwrap();
    let _scope = crate::access_policy::scope_guard(Some(scope));
    let _ask =
        crate::access_policy::ask_route_guard(Some(crate::access_policy::AskRoute::Unavailable));
    ctx.validate_read_path_without_approval("read", "worker", &sibling)
        .unwrap();

    let mut params = FrozenParams {
        siblings: vec![sibling.to_str().unwrap().into()],
        ..Default::default()
    };
    params
        .validate_siblings(&ctx, "runon", "worker", &[])
        .unwrap();
    for preflight in [vec![], vec![grant]] {
        let budget = crate::bounded_io::Budget::after(Duration::from_secs(1));
        let release_at = budget.deadline + Duration::from_millis(20);
        let (locked_tx, locked_rx) = mpsc::channel();
        let held_db = db.clone();
        let holder = std::thread::spawn(move || {
            let _held = held_db.lock().unwrap();
            locked_tx.send(()).unwrap();
            std::thread::sleep(release_at.saturating_duration_since(Instant::now()));
        });
        locked_rx.recv().unwrap();
        let result = {
            let _budget = crate::bounded_io::enter(Some(budget));
            params.validate_siblings(&ctx, "runon", "worker", &preflight)
        };
        holder.join().unwrap();
        let Err(SiblingValidationError::Unavailable(response)) = result else {
            panic!("busy grant store was reported as a missing grant or bypassed: {result:?}");
        };
        assert_eq!(
            response.data["code"], "read_deadline_exceeded",
            "{response:?}"
        );
        assert!(!response.data["message"]
            .as_str()
            .unwrap()
            .contains("allow_paths"));
    }
    params
        .validate_siblings(&ctx, "runon", "worker", &[])
        .unwrap();
    db.lock()
        .unwrap()
        .execute_batch("DROP TABLE access_grants")
        .unwrap();
    let read_error = ctx
        .validate_read_path_without_approval("runon", "worker", &sibling)
        .unwrap_err();
    assert_eq!(read_error.data["code"], "grants_unavailable");
    for preflight in [
        vec![],
        vec![AccessGrant {
            path: crate::policy_path::policy_path(&sibling),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        }],
    ] {
        let error = params
            .validate_siblings(&ctx, "runon", "worker", &preflight)
            .unwrap_err();
        let SiblingValidationError::Unavailable(response) = error else {
            panic!(
                "unavailable grant store was bypassed or reported as a missing grant: {error:?}"
            );
        };
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            serde_json::to_value(&read_error).unwrap()
        );
    }
}

/// A plan preflight may count grants its own allow_paths validated but did not
/// save. They admit a sibling only when one of them covers the resolved
/// checkout root, and the sibling must still be a checkout root.
#[test]
fn siblings_read_gate_counts_preflight_grants_only_where_they_cover_a_checkout_root() {
    use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
    use crate::policy_path::policy_path;
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let granted = dir.path().join("granted");
    let commons = granted.join("commons");
    let plain = granted.join("plain");
    let other = dir.path().join("other");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::create_dir_all(commons.join(".git")).unwrap();
    std::fs::create_dir_all(other.join(".git")).unwrap();
    let ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(project.clone()),
            storage_dir: Some(dir.path().join("store")),
            ..Default::default()
        },
    );
    let grant = |path: &std::path::Path, operation| AccessGrant {
        path: policy_path(path),
        kind: GrantKind::Subtree,
        operation,
    };
    let check = |sibling: &std::path::Path, grants: &[AccessGrant]| {
        let mut params = FrozenParams {
            siblings: vec![sibling.to_str().unwrap().into()],
            ..Default::default()
        };
        params
            .validate_siblings(&ctx, "siblings", "session", grants)
            .map_err(refused_sibling_message)
            .map(|()| params.siblings)
    };
    let folder_grant = [grant(&granted, AccessOperation::Read)];
    // The session's own file tools may not read outside its project.
    assert_eq!(
        check(&commons, &[]),
        Err(not_readable(&policy_path(&commons)))
    );
    let canonical = commons.canonicalize().unwrap().to_str().unwrap().to_owned();
    assert_eq!(
        check(&commons, &[grant(&commons, AccessOperation::Read)]).unwrap(),
        [canonical.clone()]
    );
    // A write grant on a folder containing the checkout covers reading it.
    assert_eq!(
        check(&commons, &[grant(&granted, AccessOperation::Write)]).unwrap(),
        [canonical]
    );
    // A grant that does not cover the checkout admits nothing.
    assert_eq!(
        check(&commons, &[grant(&other, AccessOperation::Read)]),
        Err(not_readable(&policy_path(&commons)))
    );
    // A covered folder that is not a checkout root is still refused, as are
    // a missing path and a relative one.
    assert_eq!(check(&plain, &folder_grant), Err(not_checkout_root(&plain)));
    let missing = granted.join("missing");
    assert_eq!(
        check(&missing, &folder_grant),
        Err(not_checkout_root(&missing))
    );
    let relative = std::path::Path::new("granted/commons");
    assert_eq!(
        check(relative, &folder_grant),
        Err(not_checkout_root(relative))
    );
    // A link inside the granted folder resolves to a checkout outside it,
    // which is named as resolved.
    #[cfg(unix)]
    {
        let link = granted.join("link");
        std::os::unix::fs::symlink(&other, &link).unwrap();
        assert_eq!(
            check(&link, &folder_grant),
            Err(not_readable(&policy_path(&other)))
        );
    }
}
