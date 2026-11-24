use super::location::remove_inactive_cache_roots;
use super::prepare_cache_root_for_test as prepare_cache_root;
use super::*;
use crate::config::Config;
use crate::context::AppContext;
use crate::parser::TreeSitterProvider;


#[test]
fn cache_root_unavailable_refuses_inspect_lane_before_open() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    fs::create_dir(&project).unwrap();
    let storage = temp.path().join("storage");
    let missing = temp.path().join("missing");
    let mut config = Config {
        storage_dir: Some(storage.clone()),
        cache_dir: Some(missing.clone()),
        ..Default::default()
    };
    assert!(prepare_cache_root(&mut config).is_some());
    let key = crate::path_identity::project_scope_key(&project);
    configure_artifact_access(
        &project,
        &crate::root_identity::key::artifact_cache_key(&project),
        false,
    );
    let result = crate::inspect::cache::InspectCache::open(
        cache_root(&config).join("inspect").join(key),
        project,
    );
    assert!(
        result.is_err(),
        "an unavailable cache lane must not open or build an empty healthy cache"
    );
    assert!(result
        .err()
        .unwrap()
        .to_string()
        .contains(CACHE_DIR_UNAVAILABLE));
    assert!(!missing.exists());
    assert!(!storage.exists());
}


fn fixture(custom: bool) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf, AppContext) {
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("aft");
    let cache = if custom {
        temp.path().join("cache")
    } else {
        storage.clone()
    };
    if custom {
        fs::create_dir(&cache).unwrap();
    }
    let project = temp.path().join("project");
    fs::create_dir(&project).unwrap();
    fs::write(
        project.join("main.rs"),
        "fn main() { leaf(); }\nfn leaf() {}\n",
    )
    .unwrap();
    let project = fs::canonicalize(project).unwrap();
    let mut config = Config {
        storage_dir: Some(storage.clone()),
        cache_dir: custom.then(|| cache.clone()),
        project_root: Some(project.clone()),
        ..Config::default()
    };
    assert!(prepare_cache_root(&mut config).is_none());
    let cache = cache_root(&config);
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);
    ctx.set_canonical_cache_root(project.clone());
    let key = ctx.memoized_artifact_cache_key(&project);
    configure_artifact_access(&project, &key, false);
    (temp, storage, cache, project, ctx)
}


#[test]
fn cache_root_change_is_fresh_and_doctor_reclaims_only_marked_inactive_domains() {
    let (_temp, storage, old, _project, ctx) = fixture(true);
    for root in [&storage, &old] {
        for domain in CACHE_DOMAINS {
            fs::create_dir_all(root.join(domain)).unwrap();
            fs::write(root.join(domain).join("canary"), "rebuildable").unwrap();
        }
    }
    fs::write(old.join("index/old-artifact"), "before root change").unwrap();
    let new_parent = old.parent().unwrap().with_file_name("new-cache");
    fs::create_dir(&new_parent).unwrap();
    let mut config = Config {
        storage_dir: Some(storage.clone()),
        cache_dir: Some(new_parent),
        ..Default::default()
    };
    let warning = prepare_cache_root(&mut config);
    assert!(warning.is_none(), "{warning:?}");
    let new = cache_root(&config);
    for root in [&new] {
        for domain in CACHE_DOMAINS {
            fs::create_dir_all(root.join(domain)).unwrap();
            fs::write(root.join(domain).join("canary"), "rebuildable").unwrap();
        }
    }
    for name in [
        "aft.db",
        "aft.db-wal",
        "aft.db-shm",
        "engram-catalog.json",
        "trusted-filter-projects.json",
    ] {
        fs::write(storage.join(name), "durable").unwrap();
    }
    for domain in [
        "opencode/backups",
        "opencode/checkpoints",
        "opencode/bash-tasks",
        "shims",
        "artifact-owners",
        "dsym",
        "logs",
    ] {
        fs::create_dir_all(storage.join(domain)).unwrap();
        fs::write(storage.join(domain).join("canary"), "durable").unwrap();
    }
    assert!(
        old.join("index/canary").is_file(),
        "startup must not delete old caches"
    );
    assert!(
        !new.join("index/old-artifact").exists(),
        "startup must not migrate caches"
    );
    drop(ctx);
    let report = remove_inactive_cache_roots(&storage, &[new.clone()]);
    assert!(report.error.is_none(), "{report:?}");
    assert_eq!(report.removed.len(), CACHE_DOMAINS.len());
    for domain in CACHE_DOMAINS {
        assert!(!old.join(domain).exists());
        assert!(storage.join(domain).join("canary").is_file());
        assert!(new.join(domain).join("canary").is_file());
    }
    assert_eq!(
        fs::read_to_string(storage.join("aft.db")).unwrap(),
        "durable"
    );
    assert!(storage.join("opencode/backups/canary").is_file());
    assert!(storage.join("artifact-owners/canary").is_file());
    assert!(old.is_dir());
    assert!(storage.is_dir());
}

#[test]
fn cache_root_doctor_keeps_in_use_root_and_active_alias() {
    let (_temp, storage, cache, _project, ctx) = fixture(true);
    fs::create_dir_all(cache.join("views")).unwrap();
    fs::write(cache.join("views/canary"), "live").unwrap();
    let in_use = remove_inactive_cache_roots(&storage, &[]);
    assert!(
        in_use
            .error
            .as_deref()
            .is_some_and(|error| error.contains("in use")),
        "{in_use:?}"
    );
    assert!(cache.join("views/canary").is_file());
    drop(ctx);
    let alias = cache.join("..").join(cache.file_name().unwrap());
    let report = remove_inactive_cache_roots(&storage, &[alias]);
    assert!(report.error.is_none(), "{report:?}");
    assert!(cache.join("views/canary").is_file());
}


#[test]
fn cache_root_doctor_ignores_harness_redirects_and_keeps_user_base() {
    // Cache-root preparation resolves the storage root through the process
    // environment, where AFT_STORAGE_DIR overrides the configured root. A test
    // that sets it holds this lock; reclaiming meanwhile would prepare and
    // clean another test's live storage.
    let _env = crate::test_env::process_env_lock();
    let (_temp, storage, cache, _project, ctx) = fixture(true);
    fs::create_dir_all(cache.join("index")).unwrap();
    fs::create_dir_all(storage.join("index")).unwrap();
    let parent = cache.parent().unwrap();
    let tiers = [crate::config_resolve::ConfigTier { tier: "user".into(), source: "user".into(), doc: serde_json::json!({
        "storage": {"cache_dir": parent}, "harnesses": {"pi": {"storage": {"cache_dir": "/unavailable-harness-root"}}}
    }).to_string() }];
    // Cleanup excludes live storage owners, including this fixture's context.
    drop(ctx);
    let report = reclaim_configured_inactive_caches(&tiers, &storage);
    assert!(report.error.is_none(), "{report:?}");
    assert!(cache.join("index").is_dir());
    assert!(
        storage.join("index").is_dir(),
        "doctor leaves legacy cleanup to the explicit command"
    );
}
