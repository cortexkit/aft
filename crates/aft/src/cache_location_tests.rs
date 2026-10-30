use super::clean_old_caches_for_test as clean_old_caches;
use super::prepare_cache_root_for_test as prepare_cache_root;

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires AFT_TEST_CACHE_VOLUME on a real mounted second filesystem"]
fn macos_real_cache_volume_uses_kernel_devices_and_refuses_descendant_redirects() {
    use std::os::unix::fs::MetadataExt;
    assert!(std::env::var_os("AFT_STORAGE_DIR").is_none());
    let volume = fs::canonicalize(
        std::env::var_os("AFT_TEST_CACHE_VOLUME").expect("set the mounted test volume"),
    )
    .unwrap();
    let cache = tempfile::tempdir_in(volume).unwrap();
    let primary = tempfile::tempdir().unwrap();
    let storage = primary.path().join("storage");
    let mut config = Config {
        storage_dir: Some(storage.clone()),
        cache_dir: Some(cache.path().to_path_buf()),
        ..Default::default()
    };
    assert!(super::prepare_cache_root(&mut config).is_none());
    let root = cache_root(&config);
    assert_ne!(
        fs::metadata(&root).unwrap().dev(),
        fs::metadata(&storage).unwrap().dev()
    );
    for domain in ["views", "blobs"] {
        let directory = root.join(domain).join("scope");
        crate::private_storage::create_dir_all(&directory).unwrap();
        crate::private_storage::write(directory.join("payload"), b"cache").unwrap();
        assert_eq!(
            fs::metadata(&directory).unwrap().dev(),
            fs::metadata(&root).unwrap().dev()
        );
        assert!(!storage.join(domain).exists());
    }
    let blobs = crate::blob_store::BlobStore::open(
        &root,
        "0123456789abcdef",
        crate::blob_store::BlobPlane::Semantic,
    )
    .unwrap();
    let database = root.join("blobs/0123456789abcdef/semantic.sqlite");
    assert!(
        database.is_file(),
        "BlobStore::open must initialize its database on the cache volume"
    );
    assert_eq!(
        fs::metadata(&database).unwrap().dev(),
        fs::metadata(&root).unwrap().dev()
    );
    assert!(!storage.join("blobs").exists());
    drop(blobs);
    let durable = storage.join("checkpoints");
    fs::create_dir(&durable).unwrap();
    std::os::unix::fs::symlink(&durable, root.join("index")).unwrap();
    assert!(crate::private_storage::create_dir_all(root.join("index/new-cache")).is_err());
    assert!(!durable.join("new-cache").exists());
}

#[test]
fn legacy_cleanup_refuses_a_non_resolved_storage_root() {
    let (_temp, storage, artifacts, config) = fixture();
    fs::write(storage.join("aft.db"), "AFT database").unwrap();
    let old = canary(&storage, "index/old");
    let parent = artifacts.parent().unwrap().to_path_buf();
    drop(config);
    let tiers = [crate::config_resolve::ConfigTier {
        tier: "user".into(),
        source: "test".into(),
        doc: serde_json::json!({"storage":{"cache_dir":parent}}).to_string(),
    }];
    let report = super::clean_old_caches(&tiers, &storage, |_| true);
    assert_eq!(
        report.reason,
        Some("old_cache_storage_root_mismatch"),
        "{report:?}"
    );
    assert!(old.is_file());
}

#[test]
fn legacy_cleanup_excludes_every_harness_directory() {
    let (_temp, storage, artifacts, config) = fixture();
    let legacy = canary(&storage, "pi/callgraph/old");
    let current = canary(&storage, "index/old");
    drop(config);
    let report = remove_legacy_cache_roots(&storage, &[artifacts]);
    assert!(report.error.is_none(), "{report:?}");
    assert!(
        legacy.is_file(),
        "the seven-directory exception reached a harness folder"
    );
    assert!(!current.exists());
}

#[test]
fn legacy_cleanup_requires_an_existing_aft_storage_proof() {
    let (_temp, storage, artifacts, config) = fixture();
    fs::remove_file(storage.join("aft.db")).unwrap();
    let old = canary(&storage, "index/old");
    let parent = artifacts.parent().unwrap().to_path_buf();
    drop(config);
    let tiers = [crate::config_resolve::ConfigTier {
        tier: "user".into(),
        source: "test".into(),
        doc: serde_json::json!({"storage":{"cache_dir":parent}}).to_string(),
    }];
    let report = clean_old_caches(&tiers, &storage, |_| {
        panic!("ownership must be proved before confirmation")
    });
    assert_eq!(
        report.reason,
        Some("old_cache_storage_root_unproven"),
        "{report:?}"
    );
    assert!(old.is_file());
}

#[cfg(unix)]
#[test]
fn legacy_cleanup_requires_the_active_namespace_on_its_own_device() {
    let (_temp, storage, artifacts, config) = fixture();
    let old = canary(&storage, "index/old");
    let parent = artifacts.parent().unwrap().to_path_buf();
    drop(config);
    let tiers = [crate::config_resolve::ConfigTier {
        tier: "user".into(),
        source: "test".into(),
        doc: serde_json::json!({"storage":{"cache_dir":parent}}).to_string(),
    }];
    // No simulated volume here: this deliberately models an old enrollment on
    // the primary device, which is not sufficient to authorize legacy cleanup.
    let report = super::clean_old_caches_resolved(&tiers, &storage, &storage, |_| {
        panic!("same-device namespace must be refused before confirmation")
    });
    assert_eq!(
        report.reason,
        Some("old_cache_external_root_required"),
        "{report:?}"
    );
    assert!(old.is_file());
}

#[test]
fn enrollment_device_comparison_uses_the_pinned_creation_parent() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("cache-parent");
    fs::create_dir(&parent).unwrap();
    let redirected_probe = Arc::new(Mutex::new(None));
    let probe_guard = redirected_probe.clone();
    let probe_path = parent.clone();
    BEFORE_ENROLLMENT_DEVICE.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            *probe_guard.lock().unwrap() = Some(crate::walk_boundary::simulated_volumes::simulate(
                &probe_path,
                u64::MAX - 71,
            ));
        }))
    });
    let mut config = Config {
        storage_dir: Some(temp.path().join("storage")),
        cache_dir: Some(parent.clone()),
        ..Default::default()
    };
    let warning = super::prepare_cache_root(&mut config)
        .expect("a different pathname probe must not change the pinned creation device");
    assert_eq!(warning.code, CACHE_DIR_ON_STORAGE_DEVICE);
    assert!(!parent.join("aft").exists());
    assert!(!config.storage_dir.unwrap().exists());
}

/// The test helper simulates the cache folder as its own volume. That must hold
/// however the configured path is spelled: GitHub's Windows runners keep TEMP
/// under the 8.3 short name `C:\Users\RUNNER~1`, enrollment opens the folder
/// under its resolved long spelling, and a simulation keyed only by the given
/// spelling left enrollment comparing the real, shared volume.
#[test]
fn test_cache_volume_is_simulated_under_a_non_canonical_spelling() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("detour")).unwrap();
    fs::create_dir(temp.path().join("cache-volume")).unwrap();
    let mut config = Config {
        storage_dir: Some(temp.path().join("storage")),
        cache_dir: Some(temp.path().join("detour").join("..").join("cache-volume")),
        ..Default::default()
    };
    let warning = prepare_cache_root(&mut config);
    assert!(warning.is_none(), "cache root not enrolled: {warning:?}");
    assert!(config.cache_lease.is_some());
}

fn unenrolled_cache_config(temp: &Path) -> Config {
    let parent = temp.join("cache-volume");
    fs::create_dir(&parent).unwrap();
    Config {
        storage_dir: Some(temp.join("storage")),
        cache_dir: Some(parent),
        ..Default::default()
    }
}

#[cfg(unix)]
#[test]
fn cache_location_crosstalk_ancestor_probe_does_not_register_a_root() {
    use crate::bash_background::persistence::TRUSTED_TEST_SYMLINKS;
    let temp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let real = base.join("real");
    let alias = base.join("system-alias");
    let second = base.join("second-root");
    fs::create_dir_all(real.join("second")).unwrap();
    fs::create_dir(&second).unwrap();
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let link = real.join("second/root");
    std::os::unix::fs::symlink(&second, &link).unwrap();
    struct Reset(Vec<PathBuf>);
    impl Drop for Reset {
        fn drop(&mut self) {
            TRUSTED_TEST_SYMLINKS.with(|links| {
                for path in &self.0 {
                    links.borrow_mut().remove(path);
                }
            });
        }
    }
    // Model root-owned system links, including macOS /var, on Linux too.
    TRUSTED_TEST_SYMLINKS.with(|links| links.borrow_mut().extend([alias.clone(), link.clone()]));
    let _reset = Reset(vec![alias.clone(), link]);
    let other = alias.join("second/root");
    let before = PinnedDir::repair_open_path(&other).unwrap();
    let mut config = unenrolled_cache_config(&base);
    config.storage_dir = Some(alias.join("first/storage"));
    let warning = prepare_cache_root(&mut config);
    assert!(warning.is_none(), "first prepare failed: {warning:?}");
    let after = PinnedDir::repair_open_path(&other)
        .expect("probing another storage root's ancestor changed this walk");
    assert_eq!(
        pinned_identity(&before).unwrap(),
        pinned_identity(&after).unwrap()
    );
    drop(config);
    let replacement = base.join("replacement");
    fs::create_dir_all(replacement.join("second/root")).unwrap();
    fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&replacement, &alias).unwrap();
    let reopened = PinnedDir::repair_open_path(&other)
        .expect("a stale ancestor probe blocked an independent root's later walk");
    assert_eq!(
        pinned_identity(&reopened).unwrap(),
        identity(&replacement.join("second/root")).unwrap()
    );
}

#[test]
fn cache_location_crosstalk_retired_root_does_not_disable_another_prepare() {
    let (_first_temp, _storage, first_root, first_config) = fixture();
    drop(first_config);
    let retired = first_root.clone();
    BEFORE_OTHER_ROOT_OVERLAP.with(|hook| {
        *hook.borrow_mut() = Some((
            first_root,
            Box::new(move || {
                fs::remove_dir_all(&retired).unwrap();
            }),
        ));
    });
    let temp = tempfile::tempdir().unwrap();
    let mut config = unenrolled_cache_config(temp.path());
    let warning = prepare_cache_root(&mut config);
    let hook_ran = BEFORE_OTHER_ROOT_OVERLAP.with(|hook| hook.borrow_mut().take().is_none());
    assert!(
        hook_ran,
        "the registered root must disappear during the overlap check"
    );
    assert!(
        warning.is_none(),
        "a retired root disabled an unrelated cache: {warning:?}"
    );
    assert!(cache_unavailable(&config).is_none());
    assert!(cache_root(&config).join(OWNER).is_file());
}

/// The configured cache folder is resolved once, and every cache path is then
/// built from that resolved path. On Windows it must be the plain spelling,
/// not the verbatim `\\?\C:\...` form `fs::canonicalize` returns: the rest of
/// AFT spells paths without that prefix, and Win32 takes `/` literally under
/// it.
#[test]
fn cache_location_resolved_cache_root_is_a_plain_path() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = unenrolled_cache_config(temp.path());
    let warning = prepare_cache_root(&mut config);
    assert!(warning.is_none(), "prepare failed: {warning:?}");
    let root = cache_root(&config);
    assert!(
        !root.to_string_lossy().starts_with(r"\\?\"),
        "cache root kept a verbatim prefix: {}",
        root.display()
    );
    assert_eq!(
        root,
        crate::bash_background::persistence::canonical_root_path(&temp.path().join("cache-volume"))
            .unwrap()
            .join("aft")
    );
    let models = config.semantic.local_cache_dir.clone().unwrap();
    crate::private_storage::create_dir_all(&models).unwrap();
    assert!(models.is_dir(), "{} is not reachable", models.display());
}

#[cfg(unix)]
fn symlinked_storage_config(temp: &Path) -> Config {
    let mut config = unenrolled_cache_config(temp);
    let resolved = temp.join("real-storage");
    fs::create_dir(&resolved).unwrap();
    std::os::unix::fs::symlink(&resolved, config.storage_dir.as_ref().unwrap()).unwrap();
    config.cache_dir = Some(fs::canonicalize(config.cache_dir.unwrap()).unwrap());
    config
}

#[cfg(unix)]
#[test]
fn cache_location_symlinked_storage_enrolls() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = symlinked_storage_config(temp.path());
    let warning = prepare_cache_root(&mut config);
    assert!(warning.is_none(), "first enrollment failed: {warning:?}");
    let storage = config.storage_dir.as_ref().unwrap();
    let history = read_root_history(storage).unwrap();
    assert_eq!(history.roots.len(), 1);
    assert!(history.pending.is_empty());
    assert!(cache_unavailable(&config).is_none());
    register_durable_owner(&StoragePaths::for_config(&config)).unwrap();
    let durable = canary(storage, "checkpoints/keep");
    let protected = cache_deletion_protections(&cache_root(&config).join("index")).unwrap();
    let id = identity(durable.parent().unwrap()).unwrap();
    assert!(protected.contains(&(id.device, id.inode)));
}

#[cfg(unix)]
#[test]
fn cache_location_symlinked_storage_without_split_keeps_owned_cache_access() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = symlinked_storage_config(temp.path());
    config.cache_dir = None;
    let warning = prepare_cache_root(&mut config);
    assert!(warning.is_none(), "same-root prepare failed: {warning:?}");
    let storage = config.storage_dir.as_ref().unwrap();
    register_durable_owner(&StoragePaths::same(storage)).unwrap();
    let cache = canary(storage, "index/keep");
    let access = pin_owned_cache_path(cache.parent().unwrap()).unwrap();
    let directory = access.directory(false).unwrap();
    assert_eq!(
        pinned_identity(&directory).unwrap(),
        identity(cache.parent().unwrap()).unwrap()
    );
    ensure_config_storage_lease(&mut config).unwrap();
    let lease = config.storage_lease.as_ref().unwrap().clone();
    ensure_config_storage_lease(&mut config).unwrap();
    assert!(Arc::ptr_eq(&lease, config.storage_lease.as_ref().unwrap()));
}

#[cfg(unix)]
#[test]
fn cache_location_symlinked_storage_survives_retry() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = symlinked_storage_config(temp.path());
    crate::bash_background::persistence::resolve_configured_root(
        config.storage_dir.as_ref().unwrap(),
    )
    .unwrap();
    AFTER_CACHE_ROOT_RENAME.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|| panic!("simulated publication crash")));
    });
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        prepare_cache_root(&mut config)
    }))
    .is_err());
    let root = cache_root(&config);
    let before = identity(&root).unwrap();
    assert!(root.join(OWNER).is_file());
    let history = read_root_history(config.storage_dir.as_ref().unwrap()).unwrap();
    assert!(history.roots.is_empty());
    assert_eq!(history.pending.len(), 1);
    let warning = prepare_cache_root(&mut config);
    assert!(warning.is_none(), "retry failed: {warning:?}");
    assert_eq!(identity(&root).unwrap(), before);
    assert!(cache_unavailable(&config).is_none());
    let history = read_root_history(config.storage_dir.as_ref().unwrap()).unwrap();
    assert_eq!(history.roots.len(), 1);
    assert!(history.pending.is_empty());
}

#[cfg(unix)]
#[test]
fn cache_location_symlinked_storage_doctor_cleanup_works() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = symlinked_storage_config(temp.path());
    crate::bash_background::persistence::resolve_configured_root(
        config.storage_dir.as_ref().unwrap(),
    )
    .unwrap();
    let _ = prepare_cache_root(&mut config);
    let storage = config.storage_dir.as_ref().unwrap().clone();
    let root = cache_root(&config);
    assert!(root.join(OWNER).is_file());
    let old = canary(&root, "index/old");
    let durable = canary(&storage, "checkpoints/keep");
    drop(config);
    let report = remove_inactive_cache_roots(&storage, &[]);
    assert!(report.error.is_none(), "doctor cleanup failed: {report:?}");
    assert_eq!(report.removed, vec![root.join("index")]);
    assert!(!old.exists());
    assert!(durable.is_file());
    assert!(root.join(OWNER).is_file());
}

#[cfg(unix)]
#[test]
fn cache_location_symlinked_storage_legacy_cleanup_works() {
    let temp = tempfile::tempdir().unwrap();
    let config = symlinked_storage_config(temp.path());
    let storage = config.storage_dir.as_ref().unwrap();
    let old = canary(storage, "index/old");
    let durable = canary(storage, "checkpoints/keep");
    canary(storage, "aft.db");
    let tiers = [user_tier(
        &serde_json::json!({"storage":{"cache_dir":config.cache_dir}}).to_string(),
    )];
    let report = clean_old_caches(&tiers, storage, |inventory| {
        assert_eq!(inventory.len(), 1);
        assert_eq!(inventory[0].path, storage.join("index"));
        true
    });
    assert!(report.error.is_none(), "legacy cleanup failed: {report:?}");
    assert!(!old.exists());
    assert!(durable.is_file());
}

#[cfg(target_os = "macos")]
#[test]
fn cache_location_macos_var_storage_enrolls_and_retries() {
    assert!(fs::symlink_metadata("/var")
        .unwrap()
        .file_type()
        .is_symlink());
    let temp = tempfile::tempdir_in("/private/var/tmp").unwrap();
    let mut config = unenrolled_cache_config(temp.path());
    config.storage_dir = Some(
        Path::new("/var/tmp")
            .join(temp.path().file_name().unwrap())
            .join("storage"),
    );
    assert_ne!(
        config.storage_dir.as_ref().unwrap(),
        &temp.path().join("storage")
    );
    for attempt in ["first enrollment", "retry"] {
        let warning = prepare_cache_root(&mut config);
        assert!(warning.is_none(), "{attempt} failed: {warning:?}");
        assert!(cache_unavailable(&config).is_none());
        register_durable_owner(&StoragePaths::for_config(&config)).unwrap();
    }
    let storage = config.storage_dir.as_ref().unwrap().clone();
    let root = cache_root(&config);
    let old = canary(&root, "index/old");
    drop(config);
    let report = remove_inactive_cache_roots(&storage, &[]);
    assert!(report.error.is_none(), "{report:?}");
    assert!(!old.exists());
}

#[cfg(unix)]
#[test]
fn cache_location_symlinked_cache_side_ancestor_is_refused() {
    let (temp, storage, root, config) = fixture();
    let precious = canary(&root, "index/keep");
    drop(config);
    let parent = root.parent().unwrap();
    let parked = temp.path().join("parked-cache-volume");
    fs::rename(parent, &parked).unwrap();
    std::os::unix::fs::symlink(&parked, parent).unwrap();
    let report = remove_inactive_cache_roots(&storage, &[]);
    assert!(
        report.error.is_some(),
        "cache ancestor was followed: {report:?}"
    );
    assert!(report.removed.is_empty(), "{report:?}");
    assert!(precious.is_file());
}

#[test]
fn cache_location_enrollment_recovers_after_create_interruption() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = unenrolled_cache_config(temp.path());
    AFTER_CACHE_ROOT_CREATE.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|| panic!("simulated enrollment crash")));
    });
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        prepare_cache_root(&mut config)
    }))
    .is_err());
    assert!(!history_path(config.storage_dir.as_ref().unwrap()).exists());
    let warning = prepare_cache_root(&mut config);
    assert!(warning.is_none(), "retry after create failed: {warning:?}");
    assert!(cache_root(&config).join(OWNER).is_file());
}

#[test]
fn cache_location_enrollment_recovers_after_marker_interruption() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = unenrolled_cache_config(temp.path());
    BEFORE_ROOT_HISTORY_WRITE.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|| panic!("simulated registration crash")));
    });
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        prepare_cache_root(&mut config)
    }))
    .is_err());
    assert!(!history_path(config.storage_dir.as_ref().unwrap()).exists());
    let warning = prepare_cache_root(&mut config);
    assert!(warning.is_none(), "retry after marker failed: {warning:?}");
    let root = PinnedDir::open(&cache_root(&config)).unwrap();
    let storage = config.storage_dir.as_ref().unwrap();
    marker_matches_pinned(&root, &pin_storage_root(storage).unwrap()).unwrap();
}

#[test]
fn cache_location_enrollment_recovers_after_registration_disk_full() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = unenrolled_cache_config(temp.path());
    BEFORE_ROOT_HISTORY_WRITE.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|| {
            #[cfg(unix)]
            let code = libc::ENOSPC;
            #[cfg(windows)]
            let code = 112; // ERROR_DISK_FULL
            Err(io::Error::from_raw_os_error(code))
        }));
    });
    let warning = prepare_cache_root(&mut config).expect("registration must report disk full");
    assert!(
        warning.message.contains("space") || warning.message.contains("full"),
        "{warning:?}"
    );
    assert!(config.cache_lease.is_none());
    assert!(!history_path(config.storage_dir.as_ref().unwrap()).exists());
    let warning = prepare_cache_root(&mut config);
    assert!(
        warning.is_none(),
        "retry after disk full failed: {warning:?}"
    );
    assert!(config.cache_lease.is_some());
    assert!(prepare_cache_root(&mut config).is_none());
}

#[test]
fn cache_location_enrollment_recovers_after_registration_interruption() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = unenrolled_cache_config(temp.path());
    crate::bash_background::persistence::BEFORE_REPAIR_RENAME.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|| panic!("simulated publication crash")));
    });
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        prepare_cache_root(&mut config)
    }))
    .is_err());
    assert!(history_path(config.storage_dir.as_ref().unwrap()).is_file());
    assert!(!cache_root(&config).exists());
    let before = read_root_history(config.storage_dir.as_ref().unwrap()).unwrap();
    assert!(before.roots.is_empty());
    assert_eq!(before.pending.len(), 1);
    let staging = before.pending[0].staging.clone();
    let warning = prepare_cache_root(&mut config);
    assert!(
        warning.is_none(),
        "retry after registration failed: {warning:?}"
    );
    assert!(config.cache_lease.is_some());
    let report =
        remove_inactive_cache_roots(config.storage_dir.as_ref().unwrap(), &[cache_root(&config)]);
    assert!(
        report.error.is_none(),
        "doctor cleanup after retry: {report:?}"
    );
    assert!(
        staging.is_dir(),
        "cleanup must preserve the unfinished folder"
    );
    let after = read_root_history(config.storage_dir.as_ref().unwrap()).unwrap();
    assert_eq!(after.roots.len(), 1);
    assert_eq!(after.pending.len(), 1);
}

#[test]
fn cache_location_enrollment_recovers_after_rename_before_history_promotion() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = unenrolled_cache_config(temp.path());
    AFTER_CACHE_ROOT_RENAME.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|| panic!("simulated post-rename crash")));
    });
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        prepare_cache_root(&mut config)
    }))
    .is_err());
    let storage = config.storage_dir.clone().unwrap();
    let before = read_root_history(&storage).unwrap();
    assert!(before.roots.is_empty());
    assert_eq!(before.pending.len(), 1);
    let published = identity(&cache_root(&config)).unwrap();
    assert_eq!(before.pending[0].root.identity, Some(published));
    let precious = canary(&cache_root(&config), "index/active");
    assert!(prepare_cache_root(&mut config).is_none());
    let after = read_root_history(&storage).unwrap();
    assert!(after.pending.is_empty());
    assert_eq!(after.roots.len(), 1);
    assert_eq!(after.roots[0].identity, Some(published));
    let report = remove_inactive_cache_roots(&storage, &[cache_root(&config)]);
    assert!(report.error.is_none(), "{report:?}");
    assert!(precious.is_file());
}

#[test]
fn cache_location_cleanup_drops_pending_with_missing_staging_without_adopting_foreign_root() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = unenrolled_cache_config(temp.path());
    crate::bash_background::persistence::BEFORE_REPAIR_RENAME.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|| panic!("simulated publication crash")));
    });
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        prepare_cache_root(&mut config)
    }))
    .is_err());
    let storage = config.storage_dir.clone().unwrap();
    let before = read_root_history(&storage).unwrap();
    fs::remove_dir_all(&before.pending[0].staging).unwrap();
    let precious = canary(&cache_root(&config), "index/foreign");
    let report = remove_inactive_cache_roots(&storage, &[]);
    assert!(report.error.is_none(), "{report:?}");
    let after = read_root_history(&storage).unwrap();
    assert!(after.pending.is_empty());
    assert!(after.roots.is_empty());
    assert!(prepare_cache_root(&mut config).is_some());
    assert!(precious.is_file());
}

/// Same crash as above, but the foreign folder created at the publication path
/// gets the deleted staging folder's inode, as ext4 and tmpfs readily do for a
/// directory created right after another is removed. Path and identity then
/// match the pending record, yet the folder has no ownership marker, so it is
/// not our publication: cleanup must drop the record instead of failing with
/// NotFound on every later doctor run.
#[test]
fn cache_location_cleanup_drops_pending_when_a_foreign_root_reuses_the_staging_inode() {
    use crate::bash_background::persistence::REPAIR_IDENTITIES;
    let temp = tempfile::tempdir().unwrap();
    let mut config = unenrolled_cache_config(temp.path());
    crate::bash_background::persistence::BEFORE_REPAIR_RENAME.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|| panic!("simulated publication crash")));
    });
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        prepare_cache_root(&mut config)
    }))
    .is_err());
    let storage = config.storage_dir.clone().unwrap();
    let before = read_root_history(&storage).unwrap();
    let pending = &before.pending[0];
    fs::remove_dir_all(&pending.staging).unwrap();
    let precious = canary(&pending.root.path, "index/foreign");
    let reused = pending.root.identity.unwrap();
    REPAIR_IDENTITIES.with(|ids| {
        ids.borrow_mut()
            .insert(pending.root.path.clone(), (reused.device, reused.inode));
    });
    assert_eq!(identity(&pending.root.path).unwrap(), reused);
    let report = remove_inactive_cache_roots(&storage, &[]);
    REPAIR_IDENTITIES.with(|ids| ids.borrow_mut().clear());
    assert!(report.error.is_none(), "{report:?}");
    let after = read_root_history(&storage).unwrap();
    assert!(after.pending.is_empty());
    assert!(after.roots.is_empty());
    assert!(precious.is_file());
}

#[test]
fn cache_location_concurrent_first_enrollments_do_not_poison_doctor_cleanup() {
    let temp = tempfile::tempdir().unwrap();
    let config = unenrolled_cache_config(temp.path());
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let handles = (0..2)
        .map(|_| {
            let mut config = config.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                AFTER_CACHE_ROOT_CREATE.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move || {
                        barrier.wait();
                    }));
                });
                let warning = prepare_cache_root(&mut config);
                (config, warning)
            })
        })
        .collect::<Vec<_>>();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .filter(|(_, warning)| warning.is_none())
            .count(),
        1
    );
    let mut retry = config.clone();
    assert!(prepare_cache_root(&mut retry).is_none());
    let report =
        remove_inactive_cache_roots(config.storage_dir.as_ref().unwrap(), &[cache_root(&retry)]);
    assert!(
        report.error.is_none(),
        "doctor cleanup after race: {report:?}"
    );
    let history = read_root_history(config.storage_dir.as_ref().unwrap()).unwrap();
    assert_eq!(history.roots.len(), 1);
    assert_eq!(history.pending.len(), 1);
    assert!(history.pending[0].staging.is_dir());
}

#[test]
fn cache_location_doctor_cleanup_protects_existing_pending_folder() {
    let (_temp, storage, artifacts, config) = fixture();
    let staging = artifacts.join("index/.aft-enrolling-unfinished");
    let precious = canary(&staging, "views/precious");
    let mut history: serde_json::Value =
        serde_json::from_slice(&fs::read(history_path(&storage)).unwrap()).unwrap();
    history["pending"] = serde_json::json!([{
        "root": {
            "path": artifacts.join("index/aft"),
            "identity": identity(&staging).unwrap(),
            "registration": "unfinished"
        },
        "staging": staging
    }]);
    fs::write(
        history_path(&storage),
        serde_json::to_vec(&history).unwrap(),
    )
    .unwrap();
    drop(config);
    let report = remove_inactive_cache_roots(&storage, &[]);
    assert!(precious.is_file(), "pending folder was deleted: {report:?}");
    assert!(
        report.error.is_none(),
        "pending protection should not poison cleanup: {report:?}"
    );
}

#[test]
fn cache_location_enrollment_publication_does_not_replace_a_foreign_folder() {
    for populated in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let mut config = unenrolled_cache_config(temp.path());
        let root = cache_root(&config);
        let foreign = root.clone();
        crate::bash_background::persistence::BEFORE_REPAIR_RENAME.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::create_dir(&foreign).unwrap();
                if populated {
                    canary(&foreign, "index/foreign");
                }
            }));
        });
        assert!(prepare_cache_root(&mut config).is_some());
        assert!(!root.join(OWNER).exists());
        assert_eq!(fs::read_dir(root).unwrap().count(), usize::from(populated));
        assert!(prepare_cache_root(&mut config).is_some());
    }
}

#[cfg(unix)]
#[test]
fn cache_location_enrollment_publication_refuses_a_symlink() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = unenrolled_cache_config(temp.path());
    let root = cache_root(&config);
    let outside = temp.path().join("outside");
    let precious = canary(&outside, "index/foreign");
    let destination = root.clone();
    crate::bash_background::persistence::BEFORE_REPAIR_RENAME.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            std::os::unix::fs::symlink(outside, destination).unwrap();
        }));
    });
    assert!(prepare_cache_root(&mut config).is_some());
    assert!(root.is_symlink());
    assert!(!root.join(OWNER).exists());
    assert!(precious.is_file());
    assert!(prepare_cache_root(&mut config).is_some());
}

#[test]
fn cache_location_enrollment_refuses_an_existing_unmarked_folder() {
    for populated in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let mut config = unenrolled_cache_config(temp.path());
        let root = cache_root(&config);
        fs::create_dir(&root).unwrap();
        if populated {
            canary(&root, "index/foreign");
        }
        assert!(prepare_cache_root(&mut config).is_some());
        assert!(!root.join(OWNER).exists());
        assert_eq!(fs::read_dir(root).unwrap().count(), usize::from(populated));
        assert!(!history_path(config.storage_dir.as_ref().unwrap()).exists());
    }
}

#[test]
fn split_durable_legacy_sqlite_refuses_writable_opens() {
    let (_temp, storage, artifacts, _config) = fixture();
    let family = "0123456789abcdef";
    let path = storage.join(format!("blobs/{family}/semantic.sqlite"));
    // Model a cache left on primary storage before cache_dir was enabled.
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let seed = rusqlite::Connection::open(&path).unwrap();
    seed.execute_batch(
        "CREATE TABLE precious(value TEXT); INSERT INTO precious VALUES ('legacy');",
    )
    .unwrap();
    drop(seed);
    let before = fs::read(&path).unwrap();
    for flags in [
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
    ] {
        assert!(
            crate::private_storage::prepare_sqlite(&path, flags).is_err(),
            "writable legacy open was admitted: {flags:?}"
        );
    }
    assert!(crate::blob_store::BlobStore::open(
        &storage,
        family,
        crate::blob_store::BlobPlane::Semantic
    )
    .is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(!path.with_extension("sqlite-wal").exists());
    assert!(!path.with_extension("sqlite-shm").exists());
    crate::private_storage::prepare_sqlite(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap();
    drop(
        crate::blob_store::BlobStore::open(
            &artifacts,
            family,
            crate::blob_store::BlobPlane::Semantic,
        )
        .unwrap(),
    );
}

#[test]
fn blob_store_open_cannot_create_caches_on_a_split_durable_root() {
    let (_temp, storage, artifacts, mut config) = fixture();
    // Low-level creation guards complement the configure routing test: even
    // a misrouted writer may not create caches on durable storage.
    let refused = crate::blob_store::BlobStore::open(
        &storage,
        "0123456789abcdef",
        crate::blob_store::BlobPlane::Semantic,
    );
    assert!(
        refused.is_err(),
        "a cache database was opened on durable storage"
    );
    assert!(
        !storage.join("blobs").exists(),
        "BlobStore::open left a cache folder on durable storage"
    );
    assert!(crate::private_storage::create_dir_all(storage.join("views/scope")).is_err());
    assert!(!storage.join("views").exists());
    // Durable state beside the caches is still writable.
    crate::private_storage::create_dir_all(storage.join("checkpoints/session")).unwrap();
    // The cache root itself keeps working.
    drop(
        crate::blob_store::BlobStore::open(
            &artifacts,
            "0123456789abcdef",
            crate::blob_store::BlobPlane::Semantic,
        )
        .unwrap(),
    );
    assert!(artifacts
        .join("blobs/0123456789abcdef/semantic.sqlite")
        .is_file());
    // Setting cache_dir to null restores the single-root layout for this
    // storage (merely removing it keeps caches off; see the tests below).
    config.cache_dir = None;
    config.cache_dir_opt_out = true;
    assert!(prepare_cache_root(&mut config).is_none());
    drop(
        crate::blob_store::BlobStore::open(
            &storage,
            "0123456789abcdef",
            crate::blob_store::BlobPlane::Semantic,
        )
        .unwrap(),
    );
    assert!(storage
        .join("blobs/0123456789abcdef/semantic.sqlite")
        .is_file());
}
use super::*;
use crate::config::Config;

#[test]
fn first_enrollment_refuses_the_durable_storage_device() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("unmounted-cache");
    fs::create_dir(&parent).unwrap();
    let mut config = Config {
        storage_dir: Some(temp.path().join("storage")),
        cache_dir: Some(parent.clone()),
        ..Default::default()
    };
    let warning =
        super::prepare_cache_root(&mut config).expect("a same-device mount stub must be refused");
    assert_eq!(warning.code, CACHE_DIR_ON_STORAGE_DEVICE);
    assert!(warning.message.contains(CACHE_DIR_ON_STORAGE_DEVICE));
    assert!(cache_unavailable(&config)
        .unwrap()
        .contains(CACHE_DIR_ON_STORAGE_DEVICE));
    assert!(!parent.join("aft").exists());
    assert!(!config.storage_dir.as_ref().unwrap().exists());
    let ctx =
        crate::context::AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let request = serde_json::from_value(
        serde_json::json!({"id":"device-warning","command":"memory.census"}),
    )
    .unwrap();
    let census = crate::commands::memory_census::handle_memory_census(&request, &ctx);
    assert_eq!(
        census.data["cache_unavailable_reason"],
        CACHE_DIR_ON_STORAGE_DEVICE
    );
}

#[test]
fn deletion_protections_refuse_an_unknown_durable_owner() {
    let temp = tempfile::tempdir().unwrap();
    let candidate = temp.path().join("unregistered/index/key");
    fs::create_dir_all(&candidate).unwrap();
    let error = cache_deletion_protections(&candidate)
        .expect_err("an unknown owner cannot supply a protection census");
    assert!(
        error.to_string().contains("durable owner is unknown"),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn cleanup_refuses_readers_symlink_before_touching_outside_markers() {
    let (_temp, storage, artifacts, config) = fixture();
    let precious = canary(&storage, "backups/generation/precious");
    fs::create_dir_all(artifacts.join("index")).unwrap();
    std::os::unix::fs::symlink(storage.join("backups"), artifacts.join("index/readers")).unwrap();
    drop(config);
    let report = remove_inactive_cache_roots(&storage, &[]);
    assert!(
        precious.is_file(),
        "preflight followed readers into durable data: {report:?}"
    );
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|reason| reason.contains("symlink")),
        "{report:?}"
    );
}

#[cfg(unix)]
#[test]
fn cleanup_refuses_linked_coordination_files_without_changing_durable_bytes() {
    for symlink in [true, false] {
        let (_temp, storage, artifacts, config) = fixture();
        let precious = canary(&storage, "backups/precious");
        fs::create_dir_all(artifacts.join("index")).unwrap();
        let lock = artifacts.join("index/cache.lock");
        if symlink {
            std::os::unix::fs::symlink(&precious, &lock).unwrap();
        } else {
            fs::hard_link(&precious, &lock).unwrap();
        }
        drop(config);
        let report = remove_inactive_cache_roots(&storage, &[]);
        assert_eq!(
            fs::read_to_string(&precious).unwrap(),
            "keep me",
            "{report:?}"
        );
        assert!(
            report
                .error
                .as_deref()
                .is_some_and(|reason| reason.contains("symlink")
                    || reason.contains("coordination")
                    || reason.contains("hard links")),
            "{report:?}"
        );
    }
}

#[test]
fn cleanup_keeps_hardlinked_durable_payload_canary() {
    let (_temp, storage, artifacts, config) = fixture();
    let precious = canary(&storage, "backups/precious");
    fs::create_dir_all(artifacts.join("index")).unwrap();
    fs::hard_link(&precious, artifacts.join("index/payload")).unwrap();
    drop(config);
    let report = remove_inactive_cache_roots(&storage, &[]);
    assert_eq!(fs::read_to_string(&precious).unwrap(), "keep me");
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|reason| reason.contains("hard links")),
        "{report:?}"
    );
}

fn deep_tree(root: &Path) -> PathBuf {
    let mut leaf = root.to_path_buf();
    for _ in 0..80 {
        leaf.push("d");
    }
    canary(&leaf, "keep")
}

#[test]
fn cache_location_measurement_refuses_excessive_depth() {
    let temp = tempfile::tempdir().unwrap();
    let canary = deep_tree(&temp.path().join("index"));
    let parent = PinnedDir::open(temp.path()).unwrap();
    let error = parent
        .repair_child_tree_bytes(std::ffi::OsStr::new("index"), &[])
        .expect_err("cache preflight must refuse an incomplete traversal");
    assert!(error.to_string().contains("traversal limit"), "{error}");
    assert!(canary.is_file());
}

#[test]
fn cache_location_durable_scan_refuses_excessive_depth() {
    let temp = tempfile::tempdir().unwrap();
    let canary = deep_tree(&temp.path().join("backups"));
    let error = deletion_protections(&[temp.path().join("backups")])
        .expect_err("an incomplete durable identity census must refuse deletion");
    assert!(error.to_string().contains("traversal limit"), "{error}");
    assert!(canary.is_file());
}

#[test]
fn cache_location_recursive_removal_refuses_late_excessive_depth() {
    let temp = tempfile::tempdir().unwrap();
    let candidate = temp.path().join("index");
    let canary = deep_tree(&candidate);
    let expected = PinnedDir::open(&candidate)
        .unwrap()
        .directory_identity()
        .unwrap();
    let parent = PinnedDir::open(temp.path()).unwrap();
    let mut removed = 0;
    let error = parent
        .repair_remove_child(std::ffi::OsStr::new("index"), expected, &[], &mut removed)
        .expect_err("removal must remain bounded even if the tree grows after preflight");
    assert!(error.to_string().contains("traversal limit"), "{error}");
    assert_eq!(removed, 0);
    assert!(canary.is_file());
}

#[test]
fn cache_location_same_device_mount_is_refused_during_every_walk() {
    use crate::bash_background::persistence::REPAIR_MOUNTS;
    let temp = tempfile::tempdir().unwrap();
    // The simulated mount is keyed by path, and the census walks resolved
    // paths (macOS temp directories sit behind the /var system symlink).
    let base = fs::canonicalize(temp.path()).unwrap();
    let candidate = base.join("index");
    let outside = canary(&candidate, "mounted/outside-canary");
    let mounted = candidate.join("mounted");
    REPAIR_MOUNTS.with(|ids| ids.borrow_mut().insert(mounted.clone(), u64::MAX));
    let parent = PinnedDir::open(&base).unwrap();
    let measured = parent.repair_child_tree_bytes(std::ffi::OsStr::new("index"), &[]);
    let scanned = deletion_protections(std::slice::from_ref(&candidate));
    let idle = ensure_cache_idle(&candidate, &[]);
    let expected = PinnedDir::open(&candidate)
        .unwrap()
        .directory_identity()
        .unwrap();
    let removed = parent.repair_remove_child(std::ffi::OsStr::new("index"), expected, &[], &mut 0);
    REPAIR_MOUNTS.with(|ids| ids.borrow_mut().remove(&mounted));
    assert!(measured.is_err(), "measurement crossed a same-device mount");
    assert!(
        scanned.is_err(),
        "durable census crossed a same-device mount"
    );
    assert!(idle.is_err(), "lock census crossed a same-device mount");
    assert!(removed.is_err(), "removal crossed a same-device mount");
    assert!(outside.is_file());
    assert!(!mounted.join("cache.lock").exists());
}

#[test]
fn old_cache_cleanup_keeps_durable_names_when_a_candidate_is_their_bind_alias() {
    use crate::bash_background::persistence::REPAIR_IDENTITIES;
    let (_temp, storage, artifacts, config) = fixture();
    let durable = canary(&storage, "backups/precious");
    let candidate = storage.join("index");
    let outside = canary(&candidate, "outside-canary");
    let id = identity(durable.parent().unwrap()).unwrap();
    REPAIR_IDENTITIES.with(|ids| {
        ids.borrow_mut()
            .insert(candidate.clone(), (id.device, id.inode));
    });
    let (_, durable_paths) = candidates_and_durable(&storage).unwrap();
    assert!(
        durable_paths.contains(&storage.join("backups")),
        "a cache alias cannot make the durable name cease to be protected"
    );
    let parent = artifacts.parent().unwrap().to_path_buf();
    drop(config);
    let tiers = [crate::config_resolve::ConfigTier {
        tier: "user".into(),
        source: "user".into(),
        doc: serde_json::json!({"storage":{"cache_dir":parent}}).to_string(),
    }];
    let report = clean_old_caches(&tiers, &storage, |_| true);
    REPAIR_IDENTITIES.with(|ids| {
        ids.borrow_mut().remove(&candidate);
    });
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|reason| reason.contains("mounted or protected")
                || reason.contains("aliases active or durable")),
        "{report:?}"
    );
    assert!(durable.is_file());
    assert!(outside.is_file());
}

#[test]
fn process_storage_lease_reuses_live_ownership_and_releases_the_last_handle() {
    let temp = tempfile::tempdir().unwrap();
    let first = acquire_daemon_storage_lease(temp.path()).unwrap();
    let second = acquire_daemon_storage_lease(temp.path()).unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    drop(first);
    assert_eq!(
        acquire_cache_cleanup_lease(temp.path()).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(second);
    acquire_cache_cleanup_lease(temp.path()).unwrap();
}

/// Hold the exclusive side of a root's use lock, as cache cleanup does.
fn hold_use_lock_exclusively(root: &Path) -> File {
    fs::create_dir_all(root).unwrap();
    let holder = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(IN_USE))
        .unwrap();
    holder.lock().unwrap();
    holder
}

#[cfg(unix)]
#[test]
fn cache_location_concurrent_coordination_create_keeps_cache_available() {
    use std::os::unix::fs::MetadataExt;

    // A process-wide storage override outranks the explicit fixture path.
    // Preparation and the injected sibling must use this same temporary root.
    let _storage_override = crate::test_env::without_storage_override();
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("storage");
    let sibling = Arc::new(Mutex::new(None));
    let held = Arc::clone(&sibling);
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&attempts);
    let sibling_path = storage.join(IN_USE);
    let mut first = true;
    let mut config = Config {
        storage_dir: Some(storage.clone()),
        ..Default::default()
    };
    let warning = crate::bash_background::persistence::with_openat_file_hook(
        Box::new(move |dirfd, name, flags| {
            if name != std::ffi::OsStr::new(IN_USE) {
                return None;
            }
            observed.lock().unwrap().push((dirfd, flags));
            if !first {
                return None;
            }
            first = false;
            // Darwin can report ENOENT to a losing openat(O_CREAT) even though
            // the winner has already created the entry in this same directory.
            let file = std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        let file = File::options()
                            .read(true)
                            .write(true)
                            .create(true)
                            .truncate(false)
                            .open(&sibling_path)
                            .unwrap();
                        file.lock_shared().unwrap();
                        file
                    })
                    .join()
                    .unwrap()
            });
            *held.lock().unwrap() = Some(file);
            Some(Err(io::Error::from_raw_os_error(libc::ENOENT)))
        }),
        || prepare_cache_root(&mut config),
    );
    assert!(
        warning.is_none(),
        "concurrent creator disabled caches: {warning:?}"
    );
    assert!(cache_unavailable(&config).is_none());
    let lease = config.cache_lease.as_ref().unwrap();
    let actual = lease._file.metadata().unwrap();
    let sibling = sibling.lock().unwrap();
    let expected = sibling.as_ref().unwrap().metadata().unwrap();
    assert_eq!(
        (actual.dev(), actual.ino()),
        (expected.dev(), expected.ino())
    );
    let attempts = attempts.lock().unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(
        attempts[0].0, attempts[1].0,
        "reopen must use the same pinned parent"
    );
    assert_ne!(attempts[0].1 & libc::O_CREAT, 0);
    assert_eq!(attempts[1].1, attempts[0].1 & !libc::O_CREAT);
    assert_ne!(attempts[1].1 & libc::O_NOFOLLOW, 0);
}

#[cfg(unix)]
#[test]
fn cache_location_coordination_create_exhaustion_is_typed_busy() {
    let temp = tempfile::tempdir().unwrap();
    let root = PinnedDir::repair_open_path(temp.path()).unwrap();
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&attempts);
    let error = crate::bash_background::persistence::with_openat_file_hook(
        Box::new(move |_, name, flags| {
            assert_eq!(name, std::ffi::OsStr::new(IN_USE));
            observed.lock().unwrap().push(flags);
            Some(Err(io::Error::from_raw_os_error(libc::ENOENT)))
        }),
        || {
            root.repair_coordination_file(std::ffi::OsStr::new(IN_USE))
                .unwrap_err()
        },
    );
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert!(error
        .get_ref()
        .unwrap()
        .is::<crate::bash_background::persistence::CoordinationCreateContended>());
    let contended = error
        .get_ref()
        .unwrap()
        .downcast_ref::<crate::bash_background::persistence::CoordinationCreateContended>()
        .unwrap();
    assert_eq!(contended.path(), root.path().join(IN_USE));
    assert_eq!(
        error.to_string(),
        format!("{CACHE_DIR_BUSY}: {CACHE_DIR_BUSY_MESSAGE}")
    );
    let warning = unavailable_warning(root.path(), &error.to_string());
    assert_eq!(warning.code, CACHE_DIR_BUSY);
    assert_eq!(warning.message, CACHE_DIR_BUSY_MESSAGE);
    assert!(!warning.message.contains("is unavailable"));
    let attempts = attempts.lock().unwrap();
    assert_eq!(attempts.len(), 6);
    for (attempt, flags) in attempts.iter().enumerate() {
        assert_eq!(flags & libc::O_CREAT != 0, attempt % 2 == 0);
        assert_ne!(flags & libc::O_NOFOLLOW, 0);
    }
    assert!(!temp.path().join(IN_USE).exists());
}

#[cfg(unix)]
#[test]
fn cache_location_coordination_reopen_refuses_a_sibling_symlink() {
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("storage");
    let outside = temp.path().join("outside");
    fs::write(&outside, b"canary").unwrap();
    fs::create_dir(&storage).unwrap();
    let root = PinnedDir::repair_open_path(&storage).unwrap();
    let destination = outside.clone();
    let link = storage.join(IN_USE);
    let mut first = true;
    let error = crate::bash_background::persistence::with_openat_file_hook(
        Box::new(move |_, _, _| {
            if !first {
                return None;
            }
            first = false;
            std::os::unix::fs::symlink(&destination, &link).unwrap();
            Some(Err(io::Error::from_raw_os_error(libc::ENOENT)))
        }),
        || {
            root.repair_coordination_file(std::ffi::OsStr::new(IN_USE))
                .unwrap_err()
        },
    );
    assert_ne!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(fs::read(&outside).unwrap(), b"canary");
}

#[test]
fn cache_location_busy_use_lock_without_cache_dir_never_blocks_durable_writes() {
    // Without `storage.cache_dir` the use lock sits on durable storage. A
    // conflict there must report busy and leave durable writes working; once
    // the lock frees, the next prepare turns caches on, with no restart.
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("storage");
    let holder = hold_use_lock_exclusively(&storage);
    let mut config = Config {
        storage_dir: Some(storage.clone()),
        ..Default::default()
    };
    let warning = prepare_cache_root(&mut config).expect("a held use lock reports busy");
    assert!(
        warning.message.contains("storage lease contention"),
        "{warning:?}"
    );
    assert!(cache_unavailable(&config).is_some());
    drop(holder);

    crate::private_storage::open_root(&storage).expect("durable root refused after busy");
    crate::private_storage::create_dir_all(storage.join("opencode/backups/session"))
        .expect("durable backup folder refused after busy");
    crate::private_storage::write(storage.join("opencode/backups/session/entry"), b"kept")
        .expect("durable write refused after busy");
    assert!(prepare_cache_root(&mut config).is_none());
    assert!(cache_unavailable(&config).is_none());
}

#[test]
fn cache_location_use_lock_freed_within_the_bounded_wait_prepares_normally() {
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("storage");
    let holder = hold_use_lock_exclusively(&storage);
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(60));
        drop(holder);
    });
    let mut config = Config {
        storage_dir: Some(storage.clone()),
        ..Default::default()
    };
    let warning = prepare_cache_root(&mut config);
    release.join().unwrap();
    assert!(warning.is_none(), "{warning:?}");
    assert!(cache_unavailable(&config).is_none());
}

#[test]
fn cache_location_busy_cache_root_recovers_on_the_next_prepare() {
    let (_temp, storage, artifacts, config) = fixture();
    let parent = config.cache_dir.clone().unwrap();
    drop(config);
    let holder = hold_use_lock_exclusively(&artifacts);
    let fresh = || Config {
        storage_dir: Some(storage.clone()),
        cache_dir: Some(parent.clone()),
        ..Default::default()
    };
    let mut busy = fresh();
    let warning = prepare_cache_root(&mut busy).expect("a held use lock reports busy");
    assert!(
        warning.message.contains("storage lease contention"),
        "{warning:?}"
    );
    assert!(cache_unavailable(&busy).is_some());
    drop(holder);

    let mut next = fresh();
    assert!(prepare_cache_root(&mut next).is_none());
    assert!(cache_unavailable(&next).is_none());
    crate::private_storage::create_dir_all(artifacts.join("index/after-busy"))
        .expect("cache write refused after the root recovered");
    crate::private_storage::create_dir_all(storage.join("opencode/backups/session"))
        .expect("durable write refused");
}

#[cfg(unix)]
#[test]
fn cache_location_split_root_recovers_when_its_drive_returns() {
    // An unmounted cache volume is modeled by moving the cache parent away.
    let (temp, storage, artifacts, config) = fixture();
    let parent = config.cache_dir.clone().unwrap();
    drop(config);
    let away = temp.path().join("unmounted");
    fs::rename(&parent, &away).unwrap();
    let fresh = || Config {
        storage_dir: Some(storage.clone()),
        cache_dir: Some(parent.clone()),
        ..Default::default()
    };
    let mut missing = fresh();
    assert!(prepare_cache_root(&mut missing).is_some());
    assert!(cache_unavailable(&missing).is_some());
    assert!(crate::private_storage::create_dir_all(artifacts.join("index/while-away")).is_err());
    assert!(!parent.exists(), "the missing volume was recreated");
    crate::private_storage::create_dir_all(storage.join("opencode/backups/session"))
        .expect("durable write refused while the cache volume is away");

    fs::rename(&away, &parent).unwrap();
    let mut back = fresh();
    let warning = prepare_cache_root(&mut back);
    assert!(warning.is_none(), "{warning:?}");
    assert!(cache_unavailable(&back).is_none());
    crate::private_storage::create_dir_all(artifacts.join("index/after-return"))
        .expect("cache write refused after the volume returned");
}

#[test]
fn cache_cleanup_upgrade_window_refuses_a_prelease_daemon() {
    let (_temp, storage, artifacts, config) = fixture();
    let tiers = [user_tier(
        &serde_json::json!({"storage":{"cache_dir":artifacts.parent().unwrap()}}).to_string(),
    )];
    let old = canary(&storage, "index/old");
    canary(&storage, "aft.db");
    drop(config);
    // The scan lists this process too; one that does not cannot tell whether
    // a daemon is running, which would refuse for a different reason.
    let processes = format!(
        "{}\t/opt/test-runner\t/opt/test-runner\n{}\t/opt/aft\t/opt/aft --subc /run/connection.json\n",
        std::process::id(),
        std::process::id() ^ 1
    );
    LEGACY_DAEMON_PROCESSES.with(|text| *text.borrow_mut() = processes);
    let doctor = reclaim_configured_inactive_caches(&tiers, &storage);
    let mut confirmed = false;
    let report = clean_old_caches(&tiers, &storage, |_| {
        confirmed = true;
        true
    });
    LEGACY_DAEMON_PROCESSES.with(|text| text.borrow_mut().clear());
    assert!(
        doctor
            .error
            .as_deref()
            .is_some_and(|error| error
                .starts_with("legacy_daemon_upgrade_window: stop the running subc daemon")),
        "{doctor:?}"
    );
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|error| error
                .starts_with("legacy_daemon_upgrade_window: stop the running subc daemon")),
        "{report:?}"
    );
    assert!(!confirmed);
    assert!(old.is_file());
}

#[test]
fn cache_location_process_scan_names_windows_executables_from_win32_process() {
    // `Name` names the executable; a quoted `CommandLine` path with spaces
    // must not be split into `"C:\Program`.
    let me = 4000;
    let listing =
        |rows: &[&str]| format!("{me}\tcargo-test.exe\tcargo test\r\n{}", rows.join("\r\n"));
    for row in [
        "4100\tsubc.exe\t\"C:\\Program Files\\AFT\\subc.exe\" --connection C:\\run\\c.json",
        "4101\tsubc.exe\t\"C:\\AFT\\subc.exe\" --connection C:\\run\\c.json",
        "4102\tSUBC.EXE\tC:\\AFT\\SUBC.EXE",
        "4103\taft.exe\t\"C:\\Program Files\\AFT\\aft.exe\" --subc C:\\run\\c.json",
        "4104\taft.exe\t\"C:\\AFT\\aft.exe\" --subc=C:\\run\\c.json",
        // A subc daemon whose command line Windows did not disclose.
        "4105\tsubc.exe",
    ] {
        assert!(
            legacy_daemon_present(&listing(&[row]), me).unwrap(),
            "daemon not recognised: {row}"
        );
    }
    for row in [
        // An aft process that is not a daemon.
        "4200\taft.exe\t\"C:\\Program Files\\AFT\\aft.exe\" index",
        // `--subc` inside the quoted program path is not an argument.
        "4201\taft.exe\t\"C:\\x --subc\\aft.exe\" index",
        // Unrelated processes, with and without a disclosed command line.
        "4202\tnotepad.exe\t",
        "4203\tSystem",
    ] {
        assert!(
            !legacy_daemon_present(&listing(&[row]), me).unwrap(),
            "not a daemon: {row}"
        );
    }
    // This process is never its own blocker.
    assert!(!legacy_daemon_present(
        &format!("{me}\tsubc.exe\t\"C:\\Program Files\\AFT\\subc.exe\""),
        me
    )
    .unwrap());
}

#[test]
fn cache_location_process_scan_reads_unix_executables_with_spaces_whole() {
    // `ps -o comm=` gives the executable as the rest of its line, so a home
    // or app folder with a space keeps the whole path; splitting the command
    // line on whitespace would read `/Users/Jane` and miss the daemon.
    let me = 4000;
    let executables = format!(
        "{me} /usr/bin/cargo-test\n\
         4100 /Users/Jane Doe/.local/bin/subc\n\
         4101 /Applications/AFT Tools.app/Contents/MacOS/aft\n\
         4102 /Applications/AFT Tools.app/Contents/MacOS/aft\n\
         4103 /usr/sbin/sshd\n"
    );
    let command_lines = format!(
        "{me} /usr/bin/cargo-test --lib\n\
         4100 /Users/Jane Doe/.local/bin/subc --connection /run/c.json\n\
         4101 /Applications/AFT Tools.app/Contents/MacOS/aft --subc /run/c.json\n\
         4102 /Applications/AFT Tools.app/Contents/MacOS/aft index\n\
         4103 sshd: listener\n"
    );
    let snapshot = compose_ps_snapshot(&executables, &command_lines);
    let only = |pids: &[u32]| {
        snapshot
            .lines()
            .filter(|line| {
                listed_process(line)
                    .is_some_and(|process| process.pid == me || pids.contains(&process.pid))
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        legacy_daemon_present(&only(&[4100]), me).unwrap(),
        "{snapshot}"
    );
    assert!(
        legacy_daemon_present(&only(&[4101]), me).unwrap(),
        "{snapshot}"
    );
    assert!(
        !legacy_daemon_present(&only(&[4102, 4103]), me).unwrap(),
        "{snapshot}"
    );
}

#[test]
fn cache_location_process_scan_that_cannot_tell_refuses() {
    // An empty or partial scan, or a process whose executable or (for aft)
    // arguments are unknown, is no evidence that no daemon runs: cleanup must
    // hold rather than read it as "no daemon".
    let me = 4000;
    for processes in [
        String::new(),
        "4300\tsvchost.exe\tC:\\Windows\\svchost.exe".to_string(),
        compose_ps_snapshot("4300 /usr/bin/sshd\n", "4300 sshd\n"),
        compose_ps_snapshot("ps: permission denied\n", ""),
        // The executable query named no executable for one process.
        compose_ps_snapshot(&format!("{me} /opt/test-runner\n4300\n"), ""),
        // An aft process that exited before the arguments query.
        compose_ps_snapshot(
            &format!("{me} /opt/test-runner\n4300 /Users/Jane Doe/bin/aft\n"),
            &format!("{me} /opt/test-runner\n"),
        ),
        // An aft process whose command line Windows did not disclose.
        format!("{me}\tcargo-test.exe\tcargo test\n4300\taft.exe"),
    ] {
        let error = legacy_daemon_present(&processes, me)
            .expect_err("an undecidable scan must not read as no daemon");
        assert!(
            error
                .to_string()
                .starts_with("legacy_daemon_upgrade_window: cannot tell"),
            "{processes:?}: {error}"
        );
    }
    assert!(!legacy_daemon_present(
        &compose_ps_snapshot(
            &format!("{me} /opt/test-runner\n4300 /usr/bin/sshd\n"),
            &format!("{me} /opt/test-runner\n"),
        ),
        me
    )
    .unwrap());
}

/// Runs the real process query on this machine: the scan must list this
/// test process by its executable's name.
#[test]
fn cache_location_process_scan_lists_this_process() {
    let processes =
        legacy_process_query("test process scan", Duration::from_secs(60)).expect("process scan");
    let me = std::process::id();
    let listed = processes
        .lines()
        .filter_map(listed_process)
        .find(|process| process.pid == me)
        .map(|process| process.executable.to_string())
        .unwrap_or_else(|| {
            panic!(
                "scan did not list pid {me} among {} lines",
                processes.lines().count()
            )
        });
    let listed = Path::new(&listed)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let exe = std::env::current_exe().unwrap();
    let name = exe
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_ascii_lowercase();
    // Linux reports at most the first 15 bytes of a process name.
    assert!(
        !listed.is_empty() && name.starts_with(&listed),
        "{listed} is not {name}"
    );
    // A daemon may really be running here; the scan only has to be usable.
    let verdict = legacy_daemon_present(&processes, me);
    assert!(
        verdict.is_ok()
            || verdict
                .as_ref()
                .is_err_and(|error| error.to_string().contains("arguments of")),
        "{verdict:?}"
    );
}

#[cfg(unix)]
#[test]
fn cache_location_process_scan_stalls_have_named_deadlines() {
    for (script, waiting_for) in [
        ("exec sleep 1", "process snapshot stdout EOF"),
        ("exec 1>&-; exec sleep 1", "process snapshot exit"),
        ("sleep 1 & exit 0", "process snapshot stdout EOF"),
    ] {
        let started = std::time::Instant::now();
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", script]);
        let error =
            legacy_process_snapshot(&mut command, "test process scan", Duration::from_millis(50))
                .expect_err("a stalled process scan must refuse cleanup");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("test process scan"), "{error}");
        assert!(error.to_string().contains(waiting_for), "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}

#[cfg(unix)]
#[test]
fn cache_location_process_scan_collects_a_completed_snapshot() {
    let mut command = std::process::Command::new("/bin/sh");
    command.args(["-c", "printf '42 /bin/not-a-daemon\\n'"]);
    assert_eq!(
        legacy_process_snapshot(&mut command, "test process scan", Duration::from_secs(3)).unwrap(),
        "42 /bin/not-a-daemon\n"
    );
}

#[test]
fn cache_cleanup_refuses_a_daemon_using_the_same_storage_root() {
    let (_temp, storage, artifacts, config) = fixture();
    let tiers = [user_tier(
        &serde_json::json!({"storage":{"cache_dir":artifacts.parent().unwrap()}}).to_string(),
    )];
    let old = canary(&storage, "index/old");
    canary(&storage, "aft.db");
    drop(config);
    // No cache lease or bound checkout is needed: the module holds this lease
    // from startup, even while it is waiting for its first route.
    let daemon = acquire_daemon_storage_lease(&storage).unwrap();
    let mut confirmed = false;
    let report = clean_old_caches(&tiers, &storage, |_| {
        confirmed = true;
        true
    });
    assert!(
        report.error.as_deref().is_some_and(|error| error
            .strip_prefix(&format!(
                "stop the running daemon using storage root {} before cache cleanup:",
                storage.display()
            ))
            .is_some_and(|detail| !detail.trim().is_empty())),
        "{report:?}"
    );
    assert!(!confirmed);
    assert!(old.is_file());
    drop(daemon);
    let report = clean_old_caches(&tiers, &storage, |_| true);
    assert!(report.error.is_none(), "{report:?}");
    assert!(!old.exists());
}

#[test]
fn cache_cleanup_allows_a_daemon_using_a_different_storage_root() {
    let (temp, storage, artifacts, config) = fixture();
    let tiers = [user_tier(
        &serde_json::json!({"storage":{"cache_dir":artifacts.parent().unwrap()}}).to_string(),
    )];
    let old = canary(&storage, "index/old");
    canary(&storage, "aft.db");
    drop(config);
    let other = temp.path().join("other-storage");
    let other_canary = canary(&other, "logs/keep");
    let _daemon = acquire_daemon_storage_lease(&other).unwrap();
    assert_eq!(
        acquire_cache_cleanup_lease(&other).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    let report = clean_old_caches(&tiers, &storage, |inventory| {
        assert_eq!(inventory.len(), 1);
        assert_eq!(inventory[0].path, storage.join("index"));
        // Cleanup holds its exclusive lease through the confirmation prompt,
        // so a module cannot start against this root between check and delete.
        assert_eq!(
            acquire_daemon_storage_lease(&storage).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        true
    });
    assert!(report.error.is_none(), "{report:?}");
    assert!(!old.exists());
    assert!(other_canary.is_file());
}

#[test]
fn cache_location_recursive_removal_rejects_mount_and_durable_bind_alias() {
    use crate::bash_background::persistence::REPAIR_IDENTITIES;
    for mounted in [true, false] {
        let (temp, storage, artifacts, config) = fixture();
        let candidate = artifacts.join("index");
        let outside = canary(&candidate, "nested/outside-canary");
        let backups = temp.path().join("backups");
        fs::create_dir(&backups).unwrap();
        let durable = canary(&storage, "backups/real-canary");
        let id = identity(durable.parent().unwrap()).unwrap();
        let simulated = if mounted {
            (id.device.wrapping_add(1), id.inode)
        } else {
            (id.device, id.inode)
        };
        let nested = candidate.join("nested");
        let late_child = nested.clone();
        BEFORE_REPAIR_REMOVE.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                REPAIR_IDENTITIES.with(|ids| {
                    ids.borrow_mut().insert(late_child, simulated);
                });
            }));
        });
        drop(config);
        let report = remove_inactive_cache_roots(&storage, &[]);
        REPAIR_IDENTITIES.with(|ids| {
            ids.borrow_mut().remove(&nested);
        });
        assert!(
            report
                .error
                .as_deref()
                .is_some_and(|reason| reason.contains("mounted or protected")),
            "{report:?}"
        );
        assert!(outside.is_file(), "nested mount/bind contents must survive");
        assert!(durable.is_file());
    }
}

#[cfg(unix)]
#[test]
fn cache_location_repair_refuses_symlinks_without_following_targets() {
    use crate::bash_background::persistence::REPAIR_SYMLINK_ELOOP;
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            REPAIR_SYMLINK_ELOOP.with(|enabled| enabled.set(false));
        }
    }
    REPAIR_SYMLINK_ELOOP.with(|enabled| enabled.set(true));
    let _reset = Reset;
    let (temp, storage, artifacts, config) = fixture();
    let outside = canary(temp.path(), "foreign/precious");
    canary(&artifacts, "index/own");
    std::os::unix::fs::symlink(outside.parent().unwrap(), artifacts.join("index/link")).unwrap();
    drop(config);
    let report = remove_inactive_cache_roots(&storage, &[]);
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|reason| reason.contains("symlink")),
        "{report:?}"
    );
    assert!(artifacts.join("index").exists());
    assert!(outside.is_file());
}

#[test]
fn cache_location_real_directory_with_copied_marker_cannot_replace_historical_root() {
    let (_temp, storage, artifacts, config) = fixture();
    drop(config);
    let parked = artifacts.with_file_name("real-old-root");
    fs::rename(&artifacts, &parked).unwrap();
    let precious = canary(&artifacts, "index/foreign");
    fs::copy(parked.join(OWNER), artifacts.join(OWNER)).unwrap();
    let report = remove_inactive_cache_roots(&storage, &[]);
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|reason| reason.contains("preflight refused")),
        "{report:?}"
    );
    assert!(precious.is_file());
}

#[test]
fn cache_location_doctor_never_removes_unmarked_storage_domains() {
    let (_temp, storage, artifacts, config) = fixture();
    let canary = canary(&storage, "index/legacy");
    add_record(&storage, &storage);
    drop(config);
    let report = remove_inactive_cache_roots(&storage, &[artifacts]);
    assert!(report.error.is_none(), "{report:?}");
    assert!(report.removed.is_empty());
    assert!(canary.is_file());
}

#[test]
fn cache_location_variant_spelling_uses_filesystem_identity() {
    let (temp, storage, artifacts, config) = fixture();
    let canary = canary(&artifacts, "index/new-cache/precious");
    let variant = temp.path().join("CACHE-VOLUME/AFT/INDEX/new-cache");
    #[cfg(target_os = "macos")]
    assert_eq!(
        identity(&variant).ok(),
        identity(canary.parent().unwrap()).ok(),
        "native alias coverage requires a case-insensitive test volume"
    );
    if identity(&variant).ok() != identity(canary.parent().unwrap()).ok() {
        // Case-sensitive filesystems have distinct names, not a case alias.
        assert!(!variant.exists());
        return;
    }
    drop(config);
    let report = remove_inactive_cache_roots(&storage, &[variant]);
    assert!(report.error.is_none(), "{report:?}");
    assert!(canary.is_file());
}

#[test]
fn copied_marker_cannot_enroll_a_foreign_nonempty_namespace() {
    let (temp, storage, artifacts, _owner) = fixture();
    let parent = temp.path().join("foreign-parent");
    let foreign = parent.join("aft");
    let precious = canary(&foreign, "index/other-tool-data");
    fs::copy(artifacts.join(OWNER), foreign.join(OWNER)).unwrap();
    let mut config = Config {
        storage_dir: Some(storage),
        cache_dir: Some(parent),
        ..Default::default()
    };
    let warning = prepare_cache_root(&mut config)
        .expect("a copied marker must not enroll a foreign namespace");
    assert!(warning.message.contains("enroll"), "{warning:?}");
    assert!(precious.is_file());
    assert!(cache_unavailable(&config).is_some());
}

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, Config) {
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("storage");
    let parent = temp.path().join("cache-volume");
    fs::create_dir(&parent).unwrap();
    let mut config = Config {
        storage_dir: Some(storage.clone()),
        cache_dir: Some(parent),
        ..Default::default()
    };
    let warning = prepare_cache_root(&mut config);
    assert!(warning.is_none(), "{warning:?}");
    let artifacts = cache_root(&config);
    fs::write(storage.join("aft.db"), "AFT fixture database").unwrap();
    (temp, storage, artifacts, config)
}
fn canary(root: &Path, relative: &str) -> PathBuf {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "keep me").unwrap();
    path
}

#[test]
fn cache_location_unprepared_config_is_unavailable_even_at_epoch_zero() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("volume");
    fs::create_dir(&parent).unwrap();
    let config = Config {
        cache_dir: Some(parent.clone()),
        storage_dir: Some(temp.path().join("storage")),
        ..Default::default()
    };
    let root = cache_root(&config);
    assert!(cache_unavailable(&config).is_some());
    assert!(check_cache_access(&root.join("index/new")).is_err());
    assert!(!parent.join("aft").exists());
}

#[cfg(unix)]
#[test]
fn cache_location_identity_gate_denies_quarantined_root_through_alias() {
    let (temp, _storage, artifacts, _config) = fixture();
    let alias = temp.path().join("root-alias");
    std::os::unix::fs::symlink(&artifacts, &alias).unwrap();
    set_status(
        &artifacts,
        Some("volume unavailable".into()),
        Some(identity(&artifacts).unwrap()),
    );
    assert!(check_cache_access(&alias.join("index/not-created")).is_err());
    assert!(!artifacts.join("index").exists());
}

#[test]
fn cache_location_saved_volume_uuid_mismatch_refuses_admission() {
    let (_temp, storage, artifacts, config) = fixture();
    let mut saved = read_parent_state(&storage).unwrap();
    saved.parents[0].volume_uuid = Some("a-different-volume".into());
    fs::write(
        parent_state_path(&storage),
        serde_json::to_vec(&saved).unwrap(),
    )
    .unwrap();
    let mut changed = config.clone();
    let warning = prepare_cache_root(&mut changed).unwrap();
    assert!(warning.message.contains("not the volume it was"));
    assert!(cache_unavailable(&changed).is_some());
    assert!(artifacts.is_dir());
}

#[test]
fn cache_location_refuses_the_volumes_container_itself() {
    let temp = tempfile::tempdir().unwrap();
    let volumes = temp.path().join("Volumes");
    fs::create_dir(&volumes).unwrap();
    let actual = identity(&volumes).unwrap();
    let error =
        verify_mounted_parent_under(&volumes, actual, &volumes, &[actual.device]).unwrap_err();
    assert!(error.to_string().contains("cannot be /Volumes"));
}

#[test]
fn cache_location_invalid_user_config_never_unsets_into_default_cache_writes() {
    let temp = tempfile::tempdir().unwrap();
    let user = temp.path().join("unreadable-user");
    fs::create_dir(&user).unwrap();
    let mut tiers = crate::subc_config::read_local_cortexkit_config_tiers(Some(&user), temp.path());
    for doc in [None, Some("{"), Some(r#"{"storage":{"cache_dir":42}}"#)] {
        if let Some(doc) = doc {
            tiers = vec![crate::config_resolve::ConfigTier {
                tier: "user".into(),
                source: "user file".into(),
                doc: doc.into(),
            }];
        }
        let result = crate::config_resolve::resolve_config(&tiers);
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.code == "cache_dir_config_unknown"),
            "{result:?}"
        );
        let mut config = result.config;
        let storage = temp.path().join("storage");
        config.storage_dir = Some(storage.clone());
        let warning = prepare_cache_root(&mut config).unwrap();
        assert_eq!(warning.code, "cache_dir_config_unknown");
        assert!(cache_unavailable(&config).is_some());
        assert!(crate::private_storage::create_dir_all(storage.join("index/new")).is_err());
        assert!(!storage.exists());
    }
}

fn resolve_user_doc(doc: &str) -> crate::config_resolve::ResolveResult {
    crate::config_resolve::resolve_config(&[crate::config_resolve::ConfigTier {
        tier: "user".into(),
        source: "user file".into(),
        doc: doc.into(),
    }])
}

fn split_then_resolve(
    tiers: &[crate::config_resolve::ConfigTier],
) -> (tempfile::TempDir, PathBuf, Config) {
    let (temp, storage, _artifacts, config) = fixture();
    drop(config);
    let mut config = crate::config_resolve::resolve_config(tiers).config;
    config.storage_dir = Some(storage.clone());
    (temp, storage, config)
}

fn user_tier(doc: &str) -> crate::config_resolve::ConfigTier {
    crate::config_resolve::ConfigTier {
        tier: "user".into(),
        source: "user file".into(),
        doc: doc.into(),
    }
}

#[test]
fn split_storage_keeps_caches_off_when_the_setting_disappears() {
    // After caches move to another volume, omitting storage.cache_dir or
    // deleting the user file must not put them back under the storage root.
    for tiers in [
        vec![user_tier(r#"{}"#)],
        vec![user_tier(r#"{"storage":null}"#)],
        vec![user_tier(r#"{"storage":{}}"#)],
        vec![user_tier(
            r#"{"harnesses":{"pi":{"storage":{"cache_dir":null}}}}"#,
        )],
        vec![],
    ] {
        let (_temp, storage, mut config) = split_then_resolve(&tiers);
        assert!(config.cache_dir.is_none());
        let warning = prepare_cache_root(&mut config).expect("caches must stay off");
        assert_eq!(warning.code, CACHE_DIR_UNAVAILABLE);
        assert!(
            warning.message.contains("caches live on")
                && warning.message.contains("\"cache_dir\": null"),
            "{warning:?}"
        );
        assert!(cache_unavailable(&config).is_some());
        assert!(crate::private_storage::create_dir_all(storage.join("index/new")).is_err());
        assert!(!storage.join("index").exists());
    }
}

#[test]
fn unreadable_user_config_after_split_keeps_caches_off_with_a_named_notice() {
    let (temp, storage, _artifacts, config) = fixture();
    drop(config);
    let user = temp.path().join("user-config.jsonc");
    fs::create_dir(&user).unwrap();
    let read_error = fs::read_to_string(&user).unwrap_err();
    let tiers = crate::subc_config::read_local_cortexkit_config_tiers(Some(&user), temp.path());
    assert_eq!(
        tiers[0].doc,
        format!("unreadable user configuration: {read_error}")
    );
    let mut config = crate::config_resolve::resolve_config(&tiers).config;
    config.storage_dir = Some(storage.clone());
    let warning = prepare_cache_root(&mut config).expect("unreadable cache location must stay off");
    assert_eq!(warning.code, "cache_dir_config_unknown");
    assert!(
        config.cache_config_unknown.as_deref().is_some_and(
            |reason| reason.starts_with(&format!("{} does not parse:", user.display()))
        ),
        "{warning:?}"
    );
    assert!(cache_unavailable(&config).is_some());
    assert!(crate::private_storage::create_dir_all(storage.join("index/new")).is_err());
    assert!(!storage.join("index").exists());
}

#[test]
fn rejected_config_without_a_cache_path_fences_context_cache_writes() {
    let temp = tempfile::tempdir().unwrap();
    let result = resolve_user_doc(
        r#"{"storage":{"cache_dir":123},"opencode":{"server_url":"http://localhost"}}"#,
    );
    assert!(!result.errors.is_empty());
    let mut config = result.config;
    assert!(config.cache_dir.is_none());
    config.storage_dir = Some(temp.path().join("storage"));
    let ctx = crate::context::AppContext::from_app(crate::context::App::default_shared(), config);
    assert!(ctx.cache_unavailable_reason().is_some());
    assert!(
        crate::private_storage::create_dir_all(ctx.storage_dir().join("blobs/new")).is_err(),
        "context admitted primary-volume caches after config rejection"
    );
}

#[test]
fn rejected_config_preserves_requested_cache_location_and_disables_writes() {
    for extra in [
        serde_json::json!({"opencode":{"server_url":"http://localhost"}}),
        serde_json::json!({"harnesses":{"pi":{"opencode":{"server_url":"http://localhost"}}}}),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("requested-cache");
        let mut doc = extra;
        doc["storage"] = serde_json::json!({"cache_dir":parent});
        let result = resolve_user_doc(&doc.to_string());
        assert!(!result.errors.is_empty(), "{result:?}");
        let mut config = result.config;
        assert_eq!(config.cache_dir.as_ref(), Some(&parent));
        assert!(config.cache_config_unknown.is_some());
        let storage = temp.path().join("storage");
        config.storage_dir = Some(storage.clone());
        assert_eq!(
            prepare_cache_root(&mut config).unwrap().code,
            "cache_dir_config_unknown"
        );
        assert!(crate::private_storage::create_dir_all(storage.join("index/new")).is_err());
    }
}

#[test]
fn cache_reclaim_takes_storage_lease_before_preparing_cache_root() {
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("storage");
    let parent = temp.path().join("cache-volume");
    fs::create_dir(&parent).unwrap();
    let _volume = crate::walk_boundary::simulated_volumes::simulate(&parent, u64::MAX - 17);
    let _daemon = acquire_daemon_storage_lease(&storage).unwrap();
    let tiers = [user_tier(
        &serde_json::json!({"storage":{"cache_dir":parent}}).to_string(),
    )];
    let report = reclaim_configured_inactive_caches(&tiers, &storage);
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("storage lease contention")),
        "{report:?}"
    );
    assert!(
        !parent.join("aft").exists(),
        "reclaim prepared a cache namespace before excluding its owner"
    );
}

#[test]
fn cache_dir_misspelled_storage_keys_are_errors() {
    for (doc, key) in [
        (r#"{"storge":{"cache_dir":"/elsewhere"}}"#, "storge"),
        (r#"{"storage":{"cachedir":"/elsewhere"}}"#, "cachedir"),
        (
            r#"{"harnesses":{"pi":{"storge":{"cache_dir":"/elsewhere"}}}}"#,
            "storge",
        ),
        (
            r#"{"harnesses":{"pi":{"storage":{"cachedir":"/elsewhere"}}}}"#,
            "cachedir",
        ),
    ] {
        let result = resolve_user_doc(doc);
        assert!(
            result.errors.iter().any(|error| error.contains(key)),
            "misspelled {key} silently resolved to defaults: {result:?}"
        );
        assert!(result.config.cache_config_unknown.is_some(), "{result:?}");
    }
}

#[test]
fn misspelled_key_inside_storage_turns_caches_off() {
    let (_temp, storage, mut config) =
        split_then_resolve(&[user_tier(r#"{"storage":{"cachedir":"/elsewhere"}}"#)]);
    let warning = prepare_cache_root(&mut config).expect("caches must stay off");
    assert_eq!(warning.code, "cache_dir_config_unknown");
    assert!(
        warning.message.contains("invalid storage section") && warning.message.contains("cachedir"),
        "{warning:?}"
    );
    assert!(cache_unavailable(&config).is_some());
    assert!(!storage.join("index").exists());
}

#[test]
fn explicit_null_cache_dir_moves_caches_back_to_the_storage_root() {
    let (_temp, storage, mut config) =
        split_then_resolve(&[user_tier(r#"{"storage":{"cache_dir":null}}"#)]);
    assert!(config.cache_dir_opt_out);
    let warning = prepare_cache_root(&mut config);
    assert!(warning.is_none(), "{warning:?}");
    assert!(cache_unavailable(&config).is_none());
    crate::private_storage::create_dir_all(storage.join("index/new")).unwrap();
}

#[test]
fn never_split_storage_without_cache_dir_keeps_the_single_root_layout() {
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("storage");
    let mut config = crate::config_resolve::resolve_config(&[user_tier(r#"{}"#)]).config;
    config.storage_dir = Some(storage.clone());
    let warning = prepare_cache_root(&mut config);
    assert!(warning.is_none(), "{warning:?}");
    assert!(cache_unavailable(&config).is_none());
    crate::private_storage::create_dir_all(storage.join("index/new")).unwrap();
}

#[test]
fn cache_location_unrelated_invalid_user_setting_keeps_caches_on() {
    // A typo outside storage does not hide where caches belong. The invalid
    // section is a non-security one, which is discarded while the rest loads;
    // an invalid security section rejects the whole candidate instead.
    let result = resolve_user_doc(
        r#"{"inspect":{"categories":{"diagnostics":"off"}},"format_on_edit":true}"#,
    );
    assert!(result.errors.is_empty(), "{result:?}");
    assert!(
        !result
            .warnings
            .iter()
            .any(|warning| warning.code == "cache_dir_config_unknown"),
        "{result:?}"
    );
    let temp = tempfile::tempdir().unwrap();
    let mut config = result.config;
    assert!(config.cache_config_unknown.is_none());
    config.storage_dir = Some(temp.path().join("storage"));
    assert!(prepare_cache_root(&mut config).is_none());
    assert!(cache_unavailable(&config).is_none());
}

#[test]
fn cache_location_invalid_storage_sections_turn_caches_off_and_name_the_section() {
    for (doc, section) in [
        (r#"{"storage":{"cache_dir":42}}"#, "invalid storage section"),
        (r#"{"storage":"elsewhere"}"#, "invalid storage section"),
        (
            r#"{"harnesses":{"opencode":{"storage":{"cache_dir":["x"]}}}}"#,
            "invalid harnesses.opencode.storage section",
        ),
    ] {
        let result = resolve_user_doc(doc);
        let warning = result
            .warnings
            .iter()
            .find(|warning| warning.code == "cache_dir_config_unknown")
            .unwrap_or_else(|| panic!("{doc}: {result:?}"));
        assert!(
            warning.message.contains(section),
            "{doc}: {}",
            warning.message
        );
        let temp = tempfile::tempdir().unwrap();
        let mut config = result.config;
        config.storage_dir = Some(temp.path().join("storage"));
        assert_eq!(
            prepare_cache_root(&mut config).unwrap().code,
            "cache_dir_config_unknown"
        );
        let reason = cache_unavailable(&config).unwrap();
        assert!(reason.contains(section), "{doc}: {reason}");
    }
}

#[test]
fn old_cache_cleanup_refuses_without_external_root_and_without_confirmation() {
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("storage");
    let precious = canary(&storage, "index/old");
    let report = clean_old_caches(&[], &storage, |_| panic!("must refuse before confirmation"));
    assert_eq!(report.reason, Some("old_cache_external_root_required"));
    assert!(precious.is_file());
    let parent = temp.path().join("volume");
    fs::create_dir(&parent).unwrap();
    let mut config = Config {
        storage_dir: Some(storage.clone()),
        cache_dir: Some(parent.clone()),
        ..Default::default()
    };
    assert!(prepare_cache_root(&mut config).is_none());
    drop(config);
    fs::write(storage.join("aft.db"), "AFT fixture database").unwrap();
    let tiers = [crate::config_resolve::ConfigTier {
        tier: "user".into(),
        source: "user file".into(),
        doc: serde_json::json!({"storage":{"cache_dir":parent}}).to_string(),
    }];
    let report = clean_old_caches(&tiers, &storage, |inventory| {
        assert_eq!(inventory.len(), 1);
        assert_eq!(inventory[0].path, storage.join("index"));
        assert_eq!(inventory[0].bytes, 7);
        false
    });
    assert_eq!(report.reason, Some("old_cache_confirmation_required"));
    assert!(precious.is_file());
}

#[cfg(unix)]
#[test]
fn old_cache_cleanup_resolves_system_symlink_ancestors_before_preflight() {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real");
    fs::create_dir(&real).unwrap();
    let alias = temp.path().join("system-alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    // Model macOS /var -> /private/var without requiring a root-owned fixture.
    crate::bash_background::persistence::TRUSTED_TEST_SYMLINKS
        .with(|links| links.borrow_mut().insert(alias.clone()));
    let storage = alias.join("storage");
    let old = canary(&storage, "index/old");
    let durable = canary(&storage, "logs/keep");
    canary(&storage, "aft.db");
    let parent = temp.path().join("volume");
    fs::create_dir(&parent).unwrap();
    let tiers = [user_tier(
        &serde_json::json!({"storage":{"cache_dir":parent}}).to_string(),
    )];
    let report = clean_old_caches(&tiers, &storage, |inventory| {
        assert_eq!(inventory.len(), 1);
        assert_eq!(inventory[0].path, storage.join("index"));
        assert_eq!(inventory[0].bytes, 7);
        true
    });
    crate::bash_background::persistence::TRUSTED_TEST_SYMLINKS
        .with(|links| links.borrow_mut().remove(&alias));
    assert!(report.error.is_none(), "{report:?}");
    assert!(!old.exists());
    assert!(durable.is_file());
    assert!(storage.join("aft.db").is_file());
}

#[test]
fn old_cache_cleanup_refuses_nested_mounts_and_reports_partial_removal() {
    use crate::bash_background::persistence::{
        with_repair_failure_after_for_test, REPAIR_IDENTITIES,
    };
    for kind in [0, 1, 2] {
        let (temp, storage, artifacts, config) = fixture();
        let first = canary(&storage, "index/000-first");
        let last = canary(&storage, "index/nested/last");
        let durable = canary(&storage, "backups/precious");
        let parent = artifacts.parent().unwrap().to_path_buf();
        drop(config);
        let tiers = [crate::config_resolve::ConfigTier {
            tier: "user".into(),
            source: "user file".into(),
            doc: serde_json::json!({"storage":{"cache_dir":parent}}).to_string(),
        }];
        let nested = storage.join("index/nested");
        if kind < 2 {
            let id = identity(durable.parent().unwrap()).unwrap();
            REPAIR_IDENTITIES.with(|ids| {
                ids.borrow_mut()
                    .insert(nested.clone(), (id.device + u64::from(kind == 0), id.inode));
            });
        }
        let report =
            with_repair_failure_after_for_test(1, || clean_old_caches(&tiers, &storage, |_| true));
        REPAIR_IDENTITIES.with(|ids| {
            ids.borrow_mut().remove(&nested);
        });
        assert!(
            report.error.as_deref().is_some_and(|reason| if kind < 2 {
                reason.contains("mounted or protected")
            } else {
                reason.contains("injected repair failure")
            }),
            "{report:?}"
        );
        if kind < 2 {
            assert!(
                first.is_file(),
                "preflight must finish before deleting anything"
            );
        } else {
            assert!(!first.exists());
            assert_eq!(report.partial.len(), 1);
            assert_eq!(report.partial[0].removed_entries, 1);
        }
        assert!(last.is_file());
        assert!(durable.is_file());
        drop(temp);
    }
}
fn add_record(storage: &Path, root: &Path) {
    let path = history_path(storage);
    let mut history: History = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    history.roots.push(RecordedRoot {
        path: root.to_path_buf(),
        identity: Some(real_directory(root).unwrap()),
        registration: None,
    });
    fs::write(path, serde_json::to_vec(&history).unwrap()).unwrap();
}

#[test]
fn cache_location_owns_only_its_namespace_and_skips_unmarked_roots() {
    let (temp, storage, artifacts, config) = fixture();
    assert!(artifacts.ends_with("cache-volume/aft"));
    let owner: OwnerMarker =
        serde_json::from_slice(&fs::read(artifacts.join(OWNER)).unwrap()).unwrap();
    assert_eq!(owner.owner, identity(&storage).unwrap());
    let parent_data = canary(artifacts.parent().unwrap(), "index/another-tool");
    let foreign = temp.path().join("another-tool");
    let foreign_data = canary(&foreign, "index/durable");
    add_record(&storage, &foreign);
    let disposable = canary(&artifacts, "index/rebuildable");
    drop(config);
    let report = remove_inactive_cache_roots(&storage, &[]);
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|reason| reason.contains("preflight refused")),
        "{report:?}"
    );
    assert!(
        disposable.exists(),
        "all roots must pass preflight before any removal"
    );
    assert!(parent_data.is_file());
    assert!(foreign_data.is_file());
    assert!(
        report
            .skipped
            .iter()
            .any(|entry| entry.contains("unowned root")),
        "{report:?}"
    );
}

#[cfg(unix)]
#[test]
fn cache_location_refuses_retargeted_historical_root_identity() {
    let (temp, storage, artifacts, config) = fixture();
    drop(config);
    let parked = artifacts.with_file_name("parked");
    fs::rename(&artifacts, &parked).unwrap();
    let target = temp.path().join("durable-other-root");
    let protected = canary(&target, "index/precious");
    fs::copy(parked.join(OWNER), target.join(OWNER)).unwrap();
    std::os::unix::fs::symlink(&target, &artifacts).unwrap();
    let report = remove_inactive_cache_roots(&storage, &[]);
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|reason| reason.contains("preflight refused")),
        "{report:?}"
    );
    assert!(
        protected.is_file(),
        "a retargeted historical path must never redirect deletion"
    );
    assert!(
        report
            .skipped
            .iter()
            .any(|entry| entry.contains("identity/symlink refusal")),
        "{report:?}"
    );
}

#[cfg(unix)]
#[test]
fn cache_location_protects_active_ancestor_identity_through_distinct_symlink() {
    let (_temp, storage, artifacts, config) = fixture();
    let protected = canary(&artifacts, "index/new-cache/precious");
    std::os::unix::fs::symlink(artifacts.join("index"), artifacts.join("other-name")).unwrap();
    let active = artifacts.join("other-name/new-cache");
    drop(config);
    let report = remove_inactive_cache_roots(&storage, &[active]);
    assert!(report.error.is_none(), "{report:?}");
    assert!(
        protected.is_file(),
        "filesystem identity, not case-sensitive spelling, must protect active descendants"
    );
    assert!(report
        .skipped
        .iter()
        .any(|entry| entry.contains("active artifact or durable subtree")));
}

#[test]
fn cache_location_shared_lease_excludes_doctor_until_last_snapshot_drops() {
    let (_temp, storage, artifacts, config) = fixture();
    let protected = canary(&artifacts, "index/live");
    let worker_snapshot = config.clone();
    drop(config);
    let report = remove_inactive_cache_roots(&storage, &[]);
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("in use")),
        "{report:?}"
    );
    assert!(protected.is_file());
    drop(worker_snapshot);
    let report = remove_inactive_cache_roots(&storage, &[]);
    assert!(report.error.is_none(), "{report:?}");
    assert!(!protected.exists());
}

#[test]
fn cache_location_preflight_failure_preserves_all_inventory() {
    let (_temp, storage, artifacts, _config) = fixture();
    let old = canary(&storage, "index/old");
    let busy = canary(&artifacts, "index/live");
    let report = remove_inactive_cache_roots(&storage, &[]);
    assert!(
        old.exists(),
        "doctor never removes unmarked storage-root folders"
    );
    assert!(busy.is_file());
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|reason| reason.contains("in use")),
        "{report:?}"
    );
    assert!(report.removed.is_empty());
}

#[test]
fn cache_location_overlapping_cache_dir_is_refused_without_blocking_durable_writes() {
    // `storage.cache_dir` that contains durable storage (cache /data, storage
    // /data/aft: the cache root would be the storage root itself), or sits
    // inside it, is a configuration error. Durable writes keep working, and
    // setting cache_dir back to null recovers with no restart.
    let temp = tempfile::tempdir().unwrap();
    let data = fs::canonicalize(temp.path()).unwrap().join("data");
    let storage = data.join("aft");
    fs::create_dir_all(&storage).unwrap();
    for parent in [data.clone(), storage.join("cache")] {
        fs::create_dir_all(&parent).unwrap();
        let mut config = Config {
            storage_dir: Some(storage.clone()),
            cache_dir: Some(parent.clone()),
            ..Default::default()
        };
        let warning = prepare_cache_root(&mut config).expect("overlap must be refused");
        assert_eq!(warning.code, CACHE_DIR_OVERLAPS_STORAGE, "{warning:?}");
        assert!(
            warning.message.contains(&parent.display().to_string())
                && warning.message.contains(&storage.display().to_string()),
            "{warning:?}"
        );
        let reason = cache_unavailable(&config).expect("caches stay off");
        assert_eq!(unavailable_code(&reason), CACHE_DIR_OVERLAPS_STORAGE);
        // A refused cache setting still yields the cache root path; using it
        // must not register durable storage either.
        let _ = cache_root(&config);

        crate::private_storage::open_root(&storage).expect("durable root refused");
        for durable in [
            "opencode/backups/session/entry",
            "opencode/bash-tasks/task.json",
        ] {
            let path = storage.join(durable);
            crate::private_storage::create_dir_all(path.parent().unwrap())
                .unwrap_or_else(|error| panic!("{durable}: {error}"));
            crate::private_storage::write(&path, b"kept")
                .unwrap_or_else(|error| panic!("{durable}: {error}"));
        }

        let mut back = Config {
            storage_dir: Some(storage.clone()),
            cache_dir_opt_out: true,
            ..Default::default()
        };
        let warning = prepare_cache_root(&mut back);
        assert!(warning.is_none(), "{warning:?}");
        assert!(cache_unavailable(&back).is_none());
        crate::private_storage::write(storage.join("opencode/backups/session/after"), b"kept")
            .expect("durable write refused after cache_dir was set back to null");
    }
}

#[test]
fn cache_location_rejects_storage_overlap_and_nested_artifact_roots() {
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("storage");
    fs::create_dir_all(storage.join("nested")).unwrap();
    for parent in [&storage, temp.path(), &storage.join("nested")] {
        let mut config = Config {
            storage_dir: Some(storage.clone()),
            cache_dir: Some(parent.to_path_buf()),
            ..Default::default()
        };
        let warning = prepare_cache_root(&mut config).unwrap();
        assert!(
            warning.message.contains("overlaps the storage root"),
            "{warning:?}"
        );
        assert!(!parent.join("aft").exists());
    }
    let (_temp, storage, artifacts, _owner) = fixture();
    let parent = artifacts.join("nested");
    fs::create_dir(&parent).unwrap();
    let mut config = Config {
        storage_dir: Some(storage),
        cache_dir: Some(parent.clone()),
        ..Default::default()
    };
    assert!(prepare_cache_root(&mut config)
        .unwrap()
        .message
        .contains("another configured artifact root"));
    assert!(!parent.join("aft").exists());
}

#[test]
fn cache_location_rejects_another_storage_owner_without_redirecting_metadata() {
    let (temp, storage, artifacts, owner) = fixture();
    let history = fs::read(history_path(&storage)).unwrap();
    let other = temp.path().join("other-storage");
    let mut config = Config {
        storage_dir: Some(other.clone()),
        cache_dir: Some(artifacts.parent().unwrap().to_path_buf()),
        ..Default::default()
    };
    let warning = prepare_cache_root(&mut config).unwrap();
    assert!(
        warning.message.contains("another storage root")
            || warning.message.contains("different storage root"),
        "{warning:?}"
    );
    assert!(
        cache_unavailable(&config).is_some_and(|reason| reason.contains("another storage root"))
    );
    assert_eq!(fs::read(history_path(&storage)).unwrap(), history);
    assert!(!other.join("retention").exists());
    assert!(!artifacts.join("retention").exists());
    assert!(cache_unavailable(&owner).is_none());
}

#[test]
fn cache_location_flat_deserialization_cannot_redirect_cache_root() {
    let config: Config = serde_json::from_value(
        serde_json::json!({"cache_dir":"/evil", "cache_unavailable":null, "cache_epoch":999}),
    )
    .unwrap();
    assert!(config.cache_dir.is_none());
    assert_eq!(config.cache_epoch, 0);
    let result = crate::config_resolve::resolve_config(&[crate::config_resolve::ConfigTier {
        tier: "user".into(),
        source: "file".into(),
        doc: r#"{"cache_dir":"/evil"}"#.into(),
    }]);
    assert!(result.config.cache_dir.is_none());
}

#[test]
fn cache_location_legacy_exception_keeps_all_harness_children() {
    let (_temp, storage, artifacts, _owner) = fixture();
    for name in [
        "opencode/callgraph/old",
        "opencode/inspect/old",
        "pi/callgraph/old",
        "runner/inspect/old",
    ] {
        canary(&storage, name);
    }
    let protected = [
        "opencode/backups/history",
        "opencode/checkpoints/history",
        "opencode/bash-tasks/output",
        "opencode/unknown/precious",
        "other-tool/callgraph/precious",
        "aft.db",
        "aft.db-wal",
        "aft.db-shm",
        "logs/precious",
    ];
    for name in protected {
        canary(&storage, name);
    }
    // Exercise the storage-root selection directly; the public cleanup command
    // adds external-volume validation, daemon checks and exact confirmation.
    add_record(&storage, &storage);
    let report = remove_legacy_cache_roots(&storage, &[artifacts]);
    assert!(report.error.is_none(), "{report:?}");
    assert!(report.removed.is_empty());
    for name in [
        "opencode/callgraph/old",
        "opencode/inspect/old",
        "pi/callgraph/old",
        "runner/inspect/old",
    ] {
        assert!(
            storage.join(name).is_file(),
            "legacy cleanup reached harness data: {name}"
        );
    }
    for name in protected {
        assert!(
            storage.join(name).is_file(),
            "durable or unknown path removed: {name}"
        );
    }
}

#[test]
fn cache_location_honors_retention_locks_and_pins() {
    let wait_for_forked_handles = |root: &Path| {
        // Parallel spawn tests can briefly inherit a shared lock until exec
        // closes CLOEXEC descriptors. Drain those copies before testing the
        // independent writer-lock and pin protections.
        let root = PinnedDir::open(root).unwrap();
        let file = open_use_lock(&root).unwrap();
        file.lock().unwrap();
    };
    for lock_name in [
        "cache.lock",
        "symbols.lock",
        "semantic.lock",
        "writer.lease",
    ] {
        let (_temp, storage, artifacts, config) = fixture();
        let data = canary(&artifacts, "index/key/data");
        let lock = crate::fs_lock::try_acquire(
            &artifacts.join("index/key").join(lock_name),
            Duration::ZERO,
        )
        .unwrap();
        drop(config);
        wait_for_forked_handles(&artifacts);
        let report = remove_inactive_cache_roots(&storage, &[]);
        assert!(
            report
                .error
                .as_deref()
                .is_some_and(|reason| reason.contains("coordination lock")),
            "{lock_name} was ignored: {report:?}"
        );
        assert!(data.is_file());
        drop(lock);
    }
    let (_temp, storage, artifacts, config) = fixture();
    let pin = canary(&artifacts, "views/key/pins/live.json");
    drop(config);
    wait_for_forked_handles(&artifacts);
    let report = remove_inactive_cache_roots(&storage, &[]);
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("pins")),
        "{report:?}"
    );
    assert!(pin.is_file());
}

#[test]
fn cache_location_unavailable_root_refuses_all_cleanup() {
    let (_temp, storage, artifacts, config) = fixture();
    let data = canary(&storage, "index/keep");
    let parent = artifacts.parent().unwrap().to_path_buf();
    drop(config);
    fs::rename(&parent, parent.with_file_name("unmounted")).unwrap();
    let tiers = [crate::config_resolve::ConfigTier {
        tier: "user".into(),
        source: "file".into(),
        doc: serde_json::json!({"storage":{"cache_dir":parent}}).to_string(),
    }];
    let report = reclaim_configured_inactive_caches(&tiers, &storage);
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("cleanup refused")
                && error.contains("running without its caches")),
        "{report:?}"
    );
    assert!(report.removed.is_empty());
    assert!(data.is_file());
    assert!(!parent.exists());
}

#[cfg(unix)]
#[test]
fn cache_location_replaced_namespace_requires_new_epoch_before_reads() {
    let (_temp, _storage, artifacts, config) = fixture();
    fs::rename(&artifacts, artifacts.with_file_name("retired-namespace")).unwrap();
    fs::create_dir(&artifacts).unwrap();
    assert!(check_cache_access(&artifacts.join("index"))
        .unwrap_err()
        .to_string()
        .contains("identity changed"));
    fs::remove_dir(&artifacts).unwrap();
    let mut recovered = config.clone();
    assert!(prepare_cache_root(&mut recovered).is_none());
    assert_ne!(config.cache_epoch, recovered.cache_epoch);
    assert!(
        cache_unavailable(&config).is_some(),
        "old snapshots must wait for their configure/cold path"
    );
    assert!(cache_unavailable(&recovered).is_none());
}

#[test]
fn cache_location_context_keeps_retired_root_lease_across_reconfigure() {
    let (_temp, storage, artifacts, config) = fixture();
    let protected = canary(&artifacts, "index/old-reader");
    let parent = artifacts.parent().unwrap().with_file_name("new-volume");
    fs::create_dir(&parent).unwrap();
    let mut next = Config {
        storage_dir: Some(storage.clone()),
        cache_dir: Some(parent),
        ..Default::default()
    };
    assert!(prepare_cache_root(&mut next).is_none());
    let active = cache_root(&next);
    let ctx =
        crate::context::AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    ctx.set_config(next);
    let report = remove_inactive_cache_roots(&storage, &[active]);
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("in use")),
        "{report:?}"
    );
    assert!(protected.is_file());
}

#[test]
fn cache_location_remembered_parent_identity_mismatch_disables_caches() {
    let (temp, storage, artifacts, config) = fixture();
    let parent = artifacts.parent().unwrap();
    let other = temp.path().join("different-directory");
    fs::create_dir(&other).unwrap();
    let mut saved = read_parent_state(&storage).unwrap();
    saved.parents[0].identity = identity(&other).unwrap();
    fs::write(
        parent_state_path(&storage),
        serde_json::to_vec(&saved).unwrap(),
    )
    .unwrap();
    // A copy inside the cache volume is not authoritative.
    fs::write(
        parent.join("cache-parents.json"),
        serde_json::to_vec(&ParentState {
            version: 1,
            parents: vec![ParentIdentity {
                path: parent.to_path_buf(),
                identity: identity(parent).unwrap(),
                configured: None,
                volume_uuid: crate::storage_retention::cache_volume_uuid(parent),
            }],
        })
        .unwrap(),
    )
    .unwrap();
    let mut candidate = config.clone();
    let warning =
        prepare_cache_root(&mut candidate).expect("changed parent identity must be unavailable");
    assert!(warning
        .message
        .contains("cache_dir is not the volume it was"));
    assert!(cache_unavailable(&candidate).is_some());
    let new_parent = temp.path().join("new-selected-path");
    fs::create_dir(&new_parent).unwrap();
    candidate.cache_dir = Some(new_parent.clone());
    assert!(
        prepare_cache_root(&mut candidate).is_none(),
        "selecting a new path starts fresh"
    );
    assert!(new_parent.join("aft").is_dir());
}

#[test]
fn cache_location_first_use_mount_stub_does_not_create_namespace() {
    struct Restore(Option<(PathBuf, Vec<u64>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            MOUNT_TEST_SEAM.with(|seam| *seam.borrow_mut() = self.0.take());
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let volumes = temp.path().join("Volumes");
    let parent = volumes.join("offline-volume").join("folder");
    fs::create_dir_all(&parent).unwrap();
    let devices = vec![identity(temp.path()).unwrap().device];
    let _restore = Restore(MOUNT_TEST_SEAM.with(|seam| seam.replace(Some((volumes, devices)))));
    let storage = temp.path().join("storage");
    let mut config = Config {
        storage_dir: Some(storage.clone()),
        cache_dir: Some(parent.clone()),
        ..Default::default()
    };
    let warning =
        prepare_cache_root(&mut config).expect("a system-volume mount stub must be unavailable");
    assert!(
        warning
            .message
            .contains("volume offline-volume is not mounted"),
        "{warning:?}"
    );
    assert!(!parent.join("aft").exists());
    assert!(!storage.exists());
    let ordinary = temp.path().join("ordinary-folder");
    fs::create_dir(&ordinary).unwrap();
    config.cache_dir = Some(ordinary.clone());
    assert!(
        prepare_cache_root(&mut config).is_none(),
        "the fixture simulates an available separate volume for this ordinary path"
    );
    assert!(ordinary.join("aft").is_dir());
}

#[cfg(unix)]
#[test]
fn cache_location_pinned_delete_cannot_follow_late_root_retarget() {
    let (temp, storage, artifacts, config) = fixture();
    let own = canary(&artifacts, "index/rebuildable");
    let target = temp.path().join("foreign-target");
    let foreign = canary(&target, "index/precious");
    let parked = artifacts.with_file_name("parked-after-pin");
    let source = artifacts.clone();
    let moved = parked.clone();
    BEFORE_REPAIR_REMOVE.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            fs::rename(&source, &moved).unwrap();
            std::os::unix::fs::symlink(&target, &source).unwrap();
        }))
    });
    drop(config);
    let report = remove_inactive_cache_roots(&storage, &[]);
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|reason| reason.contains("symlink")),
        "retargeting must stop the remaining repair: {report:?}"
    );
    assert!(
        foreign.is_file(),
        "deletion must remain anchored to the opened root"
    );
    assert!(!parked.join("index/rebuildable").exists());
    let _ = own;
}

#[test]
fn cache_location_partial_subtree_failure_reports_progress() {
    let (_temp, storage, artifacts, config) = fixture();
    let first = canary(&artifacts, "index/000-first");
    let last = canary(&artifacts, "index/zzz-last");
    drop(config);
    let report = crate::bash_background::persistence::with_repair_failure_after_for_test(1, || {
        remove_inactive_cache_roots(&storage, &[])
    });
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|reason| reason.contains("injected repair failure")),
        "{report:?}"
    );
    assert!(!first.exists());
    assert!(last.is_file());
    assert!(report.removed.is_empty());
    assert_eq!(report.partial.len(), 1);
    assert_eq!(report.partial[0].path, artifacts.join("index"));
    assert_eq!(report.partial[0].removed_entries, 1);
}
