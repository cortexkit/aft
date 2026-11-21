/// A delegated worker's ask and the parent session's approval both use the
/// parent's filesystem policy. Path resolution must leave aft.db unlocked so
/// a slow filesystem cannot keep other routes from the database.
#[tokio::test]
async fn consent_head_policy_resolves_with_database_unlocked() {
    use crate::policy_path::path_probe;
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let db = f.ctx.db().unwrap();
    let probes = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let hook = {
        let (probes, db) = (probes.clone(), db.clone());
        access_grants::unlocked_hook::install(move |_| {
            probes.borrow_mut().push(db.try_lock().is_ok())
        })
    };
    let target = f.head_root.join("shared.txt");
    let grant = AccessGrant {
        path: target.clone(),
        kind: GrantKind::File,
        operation: AccessOperation::Read,
    };
    let Prep::Ask(prep) = prepare(
        &db,
        &f.ctx,
        &f.worker,
        "read",
        &json!({"path":target}),
        &grant,
        &grant,
        None,
    )
    .unwrap() else {
        panic!("expected a fresh ask");
    };
    assert_eq!(prep.request["head_policy"], "allow", "{}", prep.request);
    let asked = probes.borrow().len();
    assert!(asked > 0, "preparing the ask resolved the head's policy");

    asks::set_elicitation(
        &db.lock().unwrap(),
        &prep.key,
        &prep.call_key,
        "acc_log_granted",
    )
    .unwrap();
    let path_probes = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let (applied, lines) = path_probe::capture(
        {
            let (path_probes, db, target) = (path_probes.clone(), db.clone(), target.clone());
            let registry = f.ctx.bash_background().clone();
            move |kind, path| {
                if path == target {
                    path_probes.borrow_mut().push((
                        kind,
                        db.try_lock().is_ok(),
                        registry.scope_fence_unlocked_for_test(),
                    ));
                }
            }
        },
        || {
            crate::logging::capture_log_lines(|| {
                apply_allow(
                    &db,
                    &f.ctx,
                    &f.worker,
                    &prep.key,
                    &prep.call_key,
                    &asks::new_holder(),
                    &prep.grant,
                    None,
                    true,
                    false,
                )
            })
        },
    );
    let applied = applied.unwrap();
    assert!(
        matches!(applied, Applied::Run(_)),
        "the head's answer applies"
    );
    let inserted: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("consent grant inserted"))
        .collect();
    assert_eq!(inserted.len(), 1, "{lines:?}");
    let line = inserted[0];
    assert!(
        line.starts_with("INFO ") && line.contains("[ses_worker]"),
        "{line}"
    );
    assert!(
        line.contains("ask=acc_log_granted kind=file scope_epoch=7"),
        "{line}"
    );
    let hash = line.split_once("path_hash=").unwrap().1;
    assert_eq!(hash.len(), 12, "{line}");
    assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()), "{line}");
    assert!(
        !line.contains(target.to_str().unwrap()) && !line.contains("shared.txt"),
        "{line}"
    );
    assert_eq!(f.worker_grants(), vec![grant]);
    let path_probes = path_probes.borrow();
    for expected in [path_probe::Kind::SqliteFloor, path_probe::Kind::Resolve] {
        assert!(
            path_probes.iter().any(|(kind, _, _)| *kind == expected),
            "missing {expected:?}: {path_probes:?}"
        );
    }
    assert!(
        path_probes.iter().all(|(_, db, scope)| *db && *scope),
        "a path probe held aft.db or the scope fence: {path_probes:?}"
    );
    drop(hook);
    let probes = probes.borrow();
    assert!(
        probes.len() > asked,
        "applying the head's answer resolved the head's policy"
    );
    assert!(
        probes.iter().all(|unlocked| *unlocked),
        "aft.db was locked during resolution: {probes:?}"
    );
}
