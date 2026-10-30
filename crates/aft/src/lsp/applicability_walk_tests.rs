use super::*;
use crate::lsp::applicability_cache::{ApplicabilityCache, MAX_CACHED_SNAPSHOTS};
use crate::lsp::registry::servers_with_root_marker;
use crate::lsp::roots::RootPathMode;

fn config(root: &Path) -> Config {
    Config {
        project_root: Some(root.to_path_buf()),
        ..Config::default()
    }
}

fn scan(root: &Path, config: &Config) -> ApplicabilityWalk {
    walk_applicable_area_uncached(
        root,
        None,
        config,
        Instant::now() + Duration::from_secs(30),
        200_000,
        RootPathMode::WalkEntry,
    )
    .unwrap()
}

fn signature(walk: &ApplicabilityWalk) -> Vec<String> {
    let mut result: Vec<_> = walk
        .candidates
        .iter()
        .map(|candidate| {
            format!(
                "{:?} {} {}",
                candidate.key.kind,
                candidate.key.root.display(),
                candidate.source_file.display()
            )
        })
        .chain(walk.markers.iter().map(|(kind, (_, root, marker))| {
            format!("marker {kind:?} {} {marker}", root.display())
        }))
        .collect();
    result.sort();
    result
}

/// The parts of a walk that do not depend on directory listing order: the
/// selected server keys with their definitions, and the server kinds with a
/// root marker. Which file is recorded for a key or marker follows listing
/// order, so only its existence is checked.
fn canonical(walk: &ApplicabilityWalk) -> (Vec<String>, Vec<String>) {
    for candidate in &walk.candidates {
        assert!(
            candidate.source_file.is_file(),
            "recorded file {} for {:?} no longer exists",
            candidate.source_file.display(),
            candidate.key
        );
    }
    for (kind, (_, dir, name)) in &walk.markers {
        assert!(
            dir.join(name).is_file(),
            "recorded {kind:?} marker {} no longer exists",
            dir.join(name).display()
        );
    }
    let mut keys: Vec<_> = walk
        .candidates
        .iter()
        .map(|candidate| {
            format!(
                "{:?} {} {:?}",
                candidate.key.kind,
                candidate.key.root.display(),
                candidate.definition
            )
        })
        .collect();
    keys.sort();
    let mut markers: Vec<_> = walk
        .markers
        .keys()
        .map(|kind| format!("{kind:?}"))
        .collect();
    markers.sort();
    (keys, markers)
}

fn measured<T>(f: impl FnOnce() -> T) -> (T, Vec<PathBuf>, Duration) {
    crate::inspect::job::CANONICALIZE_CALLS.with(|calls| *calls.borrow_mut() = Some(Vec::new()));
    let start = Instant::now();
    let result = f();
    let elapsed = start.elapsed();
    let calls =
        crate::inspect::job::CANONICALIZE_CALLS.with(|calls| calls.borrow_mut().take().unwrap());
    (result, calls, elapsed)
}

#[test]
fn applicability_walk_plain_files_have_no_per_file_realpath() {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
    std::fs::write(root.join("package.json"), "{}").unwrap();
    std::fs::write(root.join("biome.json"), r#"{"files":{"includes":["**"]}}"#).unwrap();
    for extension in ["rs", "ts", "js", "py", "go", "sh", "yaml", "json"] {
        for n in 0..20 {
            std::fs::write(root.join(format!("file{n}.{extension}")), "").unwrap();
        }
    }
    let config = config(&root);
    let (walk, calls, _) = measured(|| walk_applicable_area(&root, None, &config, None).unwrap());
    assert!(!walk.candidates.is_empty());
    assert_eq!(calls, vec![root], "only the walk root may need realpath");
}

#[test]
fn applicability_walk_concurrent_binds_share_one_scan() {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    std::fs::write(root.join("main.py"), "").unwrap();
    let config = config(&root);
    let cache = ApplicabilityCache::default();
    let scans = std::sync::atomic::AtomicUsize::new(0);
    let start = std::sync::Barrier::new(3);
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for _ in 0..2 {
            workers.push(scope.spawn(|| {
                start.wait();
                cache
                    .walk(
                        &root,
                        &config,
                        Instant::now() + Duration::from_secs(10),
                        || {
                            scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            Ok(scan(&root, &config))
                        },
                    )
                    .unwrap()
            }));
        }
        start.wait();
        let first = workers.remove(0).join().unwrap();
        let second = workers.remove(0).join().unwrap();
        assert_eq!(signature(&first), signature(&second));
    });
    assert_eq!(scans.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn applicability_walk_rechecks_ancestor_marker_without_watcher_event() {
    let temp = tempfile::tempdir().unwrap();
    let parent = crate::inspect::job::canonicalize_normalized(temp.path());
    let root = parent.join("subdir");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("main.go"), "package main").unwrap();
    let config = config(&root);
    let before = walk_applicable_area(&root, None, &config, None).unwrap();
    assert!(!before
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Go));

    // This marker is outside the watched project, so no watcher event arrives.
    std::fs::write(parent.join("go.mod"), "module example.test/app\n").unwrap();
    let after = walk_applicable_area(&root, None, &config, None).unwrap();
    assert!(after
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Go));
    std::fs::remove_file(parent.join("go.mod")).unwrap();
    let removed = walk_applicable_area(&root, None, &config, None).unwrap();
    assert!(!removed
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Go));
}

#[test]
fn applicability_walk_equivalent_config_sets_share_one_scan() {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    let configs: Vec<_> = (0..8)
        .map(|rotation| {
            let mut config = config(&root);
            for offset in 0..16 {
                config
                    .disabled_lsp
                    .insert(format!("custom-{}", (offset + rotation) % 16));
                config
                    .lsp_auto_install_binaries
                    .insert(format!("binary-{}", (offset + rotation) % 16));
                config
                    .lsp_inflight_installs
                    .insert(format!("install-{}", (offset + rotation) % 16));
            }
            config
        })
        .collect();
    let first = serde_json::to_vec(&configs[0]).unwrap();
    assert!(
        configs
            .iter()
            .skip(1)
            .any(|config| serde_json::to_vec(config).unwrap() != first),
        "fixture must exercise different HashSet serialization orders"
    );
    let cache = ApplicabilityCache::default();
    let mut scans = 0;
    for config in &configs {
        cache
            .walk(
                &root,
                config,
                Instant::now() + Duration::from_secs(5),
                || {
                    scans += 1;
                    Ok(scan(&root, config))
                },
            )
            .unwrap();
    }
    assert_eq!(scans, 1, "equivalent set values must share a cache entry");
}

#[test]
fn applicability_walk_keeps_ordered_server_arguments_in_key() {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    let mut first = config(&root);
    first.lsp_servers.push(crate::config::UserServerDef {
        id: "custom".into(),
        args: vec!["first".into(), "second".into()],
        ..Default::default()
    });
    let mut second = first.clone();
    second.lsp_servers[0].args.reverse();
    let cache = ApplicabilityCache::default();
    let mut scans = 0;
    for config in [&first, &second] {
        cache
            .walk(
                &root,
                config,
                Instant::now() + Duration::from_secs(5),
                || {
                    scans += 1;
                    Ok(scan(&root, config))
                },
            )
            .unwrap();
    }
    assert_eq!(scans, 2, "server argument order changes the snapshot");
}

#[test]
fn applicability_walk_rechecks_ancestor_biome_contents_and_identity() {
    let temp = tempfile::tempdir().unwrap();
    let parent = crate::inspect::job::canonicalize_normalized(temp.path());
    let root = parent.join("subdir");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("main.json"), "{}").unwrap();
    let marker = parent.join("biome.json");
    std::fs::write(&marker, r#"{"files":{"includes":["**"]}}"#).unwrap();
    let config = config(&root);
    let before = walk_applicable_area(&root, None, &config, None).unwrap();
    assert!(before
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Biome));

    // Preserve mtime across replacement: identity must still invalidate both
    // the walk result and the mtime-based Biome parse memo.
    let modified =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&marker).unwrap());
    std::fs::rename(&marker, parent.join("old-biome.json")).unwrap();
    std::fs::write(&marker, r#"{"files":{"includes":["**","!**"]}}"#).unwrap();
    filetime::set_file_mtime(&marker, modified).unwrap();
    let after = walk_applicable_area(&root, None, &config, None).unwrap();
    assert!(!after
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Biome));
}

#[test]
fn applicability_walk_manifest_change_invalidates_from_watcher() {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    std::fs::write(root.join("main.go"), "package main").unwrap();
    let config = config(&root);
    let before = walk_applicable_area(&root, None, &config, None).unwrap();
    assert!(!before
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Go));
    std::fs::write(root.join("go.mod"), "module example.test/app\n").unwrap();
    let ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config.clone(),
    );
    ctx.lsp_forward_watcher_file_events(&[root.join("go.mod")]);
    let after = walk_applicable_area(&root, None, &config, None).unwrap();
    assert!(after
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Go));
}

#[test]
fn applicability_walk_fifty_content_batches_do_zero_full_walks() {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    let source = root.join("main.py");
    std::fs::write(&source, "print(0)").unwrap();
    let config = config(&root);
    let cache = ApplicabilityCache::default();
    let walk = || {
        cache
            .walk(
                &root,
                &config,
                Instant::now() + Duration::from_secs(10),
                || Ok(scan(&root, &config)),
            )
            .unwrap()
    };
    let before = signature(&walk());
    let baseline = cache.metrics(&root).full_walks;
    for n in 1..=50 {
        std::fs::write(&source, format!("print({n})")).unwrap();
        cache.invalidate_batch(
            &root,
            std::slice::from_ref(&source),
            crate::watcher_filter::watcher_event_is_content_only(&notify::EventKind::Modify(
                notify::event::ModifyKind::Data(notify::event::DataChange::Content),
            )),
        );
        assert_eq!(signature(&walk()), before);
    }
    let metrics = cache.metrics(&root);
    assert_eq!(
        metrics.full_walks - baseline,
        0,
        "content edits must not walk"
    );
    assert_eq!(metrics.hits, 50);
    assert_eq!(metrics.retained_content_batches, 50);
    assert_eq!(metrics.warm_validations, 50);
    assert!(metrics.invalidations.is_empty());
}

#[test]
fn applicability_walk_cache_cap_keeps_recently_used_snapshots() {
    // The product hands the cache and the walk roots in
    // `canonicalize_normalized` form (see `walk_applicable_area`); a bare
    // `fs::canonicalize` gives a verbatim `\\?\C:\...` root on Windows,
    // which no production caller passes.
    let warm = tempfile::tempdir().unwrap();
    let warm_root = crate::inspect::job::canonicalize_normalized(warm.path());
    std::fs::write(warm_root.join("main.py"), "").unwrap();
    let warm_config = config(&warm_root);
    let other = tempfile::tempdir().unwrap();
    let other_root = crate::inspect::job::canonicalize_normalized(other.path());
    // Each configuration is its own cache key for the same root.
    let other_config = |n: usize| {
        let mut config = config(&other_root);
        config.disabled_lsp.insert(format!("custom-{n}"));
        config
    };
    let cache = ApplicabilityCache::default();
    let walk = |root: &Path, config: &Config| {
        cache
            .walk(
                root,
                config,
                Instant::now() + Duration::from_secs(10),
                || Ok(scan(root, config)),
            )
            .unwrap();
    };
    walk(&warm_root, &warm_config);
    // Fill the cache to capacity with idle snapshots of other keys.
    for n in 0..MAX_CACHED_SNAPSHOTS - 1 {
        walk(&other_root, &other_config(n));
    }
    // At capacity, a lookup must still find its own warm snapshot.
    walk(&warm_root, &warm_config);
    // A new key at capacity makes room by dropping the least recently used
    // idle snapshot, not one a caller has just used.
    walk(&other_root, &other_config(MAX_CACHED_SNAPSHOTS - 1));
    walk(&warm_root, &warm_config);
    let warm_metrics = cache.metrics(&warm_root);
    assert_eq!(
        warm_metrics.full_walks, 1,
        "a recently used snapshot must survive the cache cap: {warm_metrics:?}"
    );
    assert_eq!(warm_metrics.hits, 2);

    // The cap still bounds the cache: the oldest key was the one dropped,
    // and a key used since then is still warm.
    let other_walks = cache.metrics(&other_root).full_walks;
    walk(&other_root, &other_config(0));
    assert_eq!(cache.metrics(&other_root).full_walks, other_walks + 1);
    walk(&other_root, &other_config(MAX_CACHED_SNAPSHOTS - 2));
    assert_eq!(cache.metrics(&other_root).full_walks, other_walks + 1);
}

#[test]
fn applicability_walk_config_change_keeps_active_content_cache() {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    let source = root.join("main.py");
    std::fs::write(&source, "print(0)").unwrap();
    let base_config = config(&root);
    let mut unused_config = base_config.clone();
    unused_config
        .disabled_lsp
        .insert("unused-server".to_owned());
    let mut active_config = base_config;
    active_config
        .disabled_lsp
        .insert("active-server".to_owned());
    let cache = ApplicabilityCache::default();
    let walk = |config: &Config| {
        cache
            .walk(
                &root,
                config,
                Instant::now() + Duration::from_secs(10),
                || Ok(scan(&root, config)),
            )
            .unwrap()
    };

    walk(&unused_config);
    walk(&active_config);
    let before_config_invalidation = cache.metrics(&root).full_walks;
    cache.invalidate(
        &root,
        crate::lsp::applicability_cache::InvalidationCause::SelectionInput,
    );
    walk(&active_config);
    let baseline = cache.metrics(&root).full_walks;
    assert_eq!(
        baseline,
        before_config_invalidation + 1,
        "the active config must rebuild"
    );

    for n in 1..=20 {
        std::fs::write(&source, format!("print({n})")).unwrap();
        cache.invalidate_batch(
            &root,
            std::slice::from_ref(&source),
            crate::watcher_filter::watcher_event_is_content_only(&notify::EventKind::Modify(
                notify::event::ModifyKind::Data(notify::event::DataChange::Content),
            )),
        );
        walk(&active_config);
    }

    let metrics = cache.metrics(&root);
    assert_eq!(
        metrics.full_walks - baseline,
        0,
        "content edits must not walk"
    );
    assert_eq!(metrics.retained_content_batches, 20);
    assert_eq!(metrics.warm_validations, 20);
}

struct WatcherWalkFixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    config: Config,
    cache: ApplicabilityCache,
}

impl WatcherWalkFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::inspect::job::canonicalize_normalized(temp.path());
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join("main.py"), "print(0)").unwrap();
        std::fs::write(root.join("go.mod"), "module example.test/app\n").unwrap();
        std::fs::write(root.join("package.json"), "{}").unwrap();
        std::fs::write(root.join(".gitignore"), "# initial rules\n").unwrap();
        let mut config = config(&root);
        // Biome may probe ancestor directories outside this checkout. Other
        // parallel tempfile fixtures change those directories' metadata; this
        // fixture tests watcher invalidation, not ancestor revalidation.
        config.disabled_lsp.insert("biome".into());
        Self {
            _temp: temp,
            root,
            config,
            cache: ApplicabilityCache::default(),
        }
    }

    fn walk(&self) -> ApplicabilityWalk {
        self.cache
            .walk(
                &self.root,
                &self.config,
                Instant::now() + Duration::from_secs(10),
                || Ok(scan(&self.root, &self.config)),
            )
            .unwrap()
    }

    fn batch(&self, paths: &[PathBuf], kind: notify::EventKind) {
        self.cache.invalidate_batch(
            &self.root,
            paths,
            crate::watcher_filter::watcher_event_is_content_only(&kind),
        );
    }

    fn assert_one_new_walk(&self) -> ApplicabilityWalk {
        let result = self.walk();
        assert_eq!(self.cache.metrics(&self.root).full_walks, 2);
        result
    }
}

fn data_modify() -> notify::EventKind {
    notify::EventKind::Modify(notify::event::ModifyKind::Data(
        notify::event::DataChange::Content,
    ))
}

#[test]
fn applicability_walk_new_source_in_fresh_directory_invalidates() {
    let fixture = WatcherWalkFixture::new();
    assert!(!fixture
        .walk()
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Go));
    let source = fixture.root.join("fresh/deep/main.go");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, "package main").unwrap();
    fixture.batch(
        &[source],
        notify::EventKind::Create(notify::event::CreateKind::File),
    );
    assert!(fixture
        .assert_one_new_walk()
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Go));
    assert_eq!(
        fixture.cache.metrics(&fixture.root).invalidations["create_remove_rename_or_directory"],
        1
    );
}

#[test]
fn applicability_walk_deleted_source_invalidates() {
    let fixture = WatcherWalkFixture::new();
    assert!(!fixture.walk().candidates.is_empty());
    let source = fixture.root.join("main.py");
    std::fs::remove_file(&source).unwrap();
    fixture.batch(
        &[source],
        notify::EventKind::Remove(notify::event::RemoveKind::File),
    );
    assert!(!fixture
        .assert_one_new_walk()
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Python));
}

#[test]
fn applicability_walk_new_tsconfig_invalidates() {
    let fixture = WatcherWalkFixture::new();
    std::fs::write(fixture.root.join("main.ts"), "export const a = 1;").unwrap();
    fixture.walk();
    let config = fixture.root.join("tsconfig.json");
    std::fs::write(&config, "{}").unwrap();
    fixture.batch(
        &[config],
        notify::EventKind::Create(notify::event::CreateKind::File),
    );
    fixture.assert_one_new_walk();
}

#[test]
fn applicability_walk_package_json_content_invalidates() {
    let fixture = WatcherWalkFixture::new();
    fixture.walk();
    let manifest = fixture.root.join("package.json");
    std::fs::write(&manifest, r#"{"type":"module"}"#).unwrap();
    fixture.batch(&[manifest], data_modify());
    fixture.assert_one_new_walk();
    assert_eq!(
        fixture.cache.metrics(&fixture.root).invalidations["marker_ignore_config_or_ts_install"],
        1
    );
}

#[test]
fn applicability_walk_gitignore_content_invalidates() {
    let fixture = WatcherWalkFixture::new();
    fixture.walk();
    let ignore = fixture.root.join(".gitignore");
    std::fs::write(&ignore, "main.py\n").unwrap();
    fixture.batch(&[ignore], data_modify());
    let result = fixture.assert_one_new_walk();
    assert!(!result
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Python));
}

#[test]
fn applicability_walk_overflow_invalidates() {
    let fixture = WatcherWalkFixture::new();
    fixture.walk();
    fixture
        .cache
        .invalidate(&fixture.root, InvalidationCause::OverflowLostEvents);
    fixture.assert_one_new_walk();
    assert_eq!(
        fixture.cache.metrics(&fixture.root).invalidations["overflow_or_lost_events"],
        1
    );
}

#[test]
fn applicability_walk_rename_invalidates() {
    let fixture = WatcherWalkFixture::new();
    fixture.walk();
    let before = fixture.root.join("main.py");
    let after = fixture.root.join("renamed.py");
    std::fs::rename(&before, &after).unwrap();
    fixture.batch(
        &[before, after.clone()],
        notify::EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::Both,
        )),
    );
    let result = fixture.assert_one_new_walk();
    assert!(result.candidates.iter().any(|c| c.source_file == after));
}

#[test]
fn applicability_walk_custom_marker_and_captured_ignore_content_invalidate() {
    let mut fixture = WatcherWalkFixture::new();
    fixture
        .config
        .lsp_servers
        .push(crate::config::UserServerDef {
            id: "custom".into(),
            root_markers: vec!["project.anchor".into()],
            ..Default::default()
        });
    let marker = fixture.root.join("project.anchor");
    let ignore = fixture.root.join(".ignore");
    std::fs::write(&marker, "before").unwrap();
    std::fs::write(&ignore, "# before").unwrap();
    fixture.walk();
    std::fs::write(&marker, "after").unwrap();
    fixture.batch(&[marker], data_modify());
    fixture.assert_one_new_walk();
    std::fs::write(&ignore, "# after").unwrap();
    fixture.batch(&[ignore], data_modify());
    fixture.walk();
    let metrics = fixture.cache.metrics(&fixture.root);
    assert_eq!(metrics.full_walks, 3);
    assert_eq!(
        metrics.invalidations["marker_ignore_config_or_ts_install"],
        2
    );
    assert!(metrics.dependency_count > 0);
}

/// The cache may be handed a root spelled differently from the normalized
/// paths its probes record: on Windows, `fs::canonicalize` returns a `\\?\`
/// verbatim path. Watcher paths built from that root must still be recognised
/// as selection inputs. A `..` component reproduces the mismatch everywhere.
#[test]
fn applicability_walk_selection_inputs_match_across_root_spellings() {
    let mut fixture = WatcherWalkFixture::new();
    std::fs::create_dir(fixture.root.join("nested")).unwrap();
    fixture.root = fixture.root.join("nested").join("..");
    let ignore = fixture.root.join(".ignore");
    std::fs::write(&ignore, "# before").unwrap();
    fixture.walk();
    std::fs::write(&ignore, "# after").unwrap();
    fixture.batch(&[ignore], data_modify());
    fixture.assert_one_new_walk();
    assert_eq!(
        fixture.cache.metrics(&fixture.root).invalidations["marker_ignore_config_or_ts_install"],
        1
    );
}

#[test]
fn applicability_walk_directory_event_invalidates() {
    let fixture = WatcherWalkFixture::new();
    fixture.walk();
    let path = fixture.root.join("folder");
    std::fs::create_dir(&path).unwrap();
    fixture.batch(&[path], data_modify());
    fixture.assert_one_new_walk();
}

/// A modify event with no more specific subtype is classified from disk: an existing
/// source file whose server is already selected cannot change the result,
/// whatever the event was.
#[test]
fn applicability_walk_ambiguous_modify_of_covered_source_keeps_result() {
    let fixture = WatcherWalkFixture::new();
    fixture.walk();
    fixture.batch(
        &[fixture.root.join("main.py")],
        notify::EventKind::Modify(notify::event::ModifyKind::Any),
    );
    let result = fixture.walk();
    let metrics = fixture.cache.metrics(&fixture.root);
    assert_eq!(metrics.full_walks, 1);
    assert_eq!(metrics.retained_topology_batches, 1);
    assert_eq!(
        canonical(&result),
        canonical(&scan(&fixture.root, &fixture.config))
    );
}

#[cfg(unix)]
#[test]
fn applicability_walk_symlinked_selection_input_content_invalidates() {
    let fixture = WatcherWalkFixture::new();
    let target = fixture.root.join("workspace.settings");
    std::fs::write(&target, "[workspace]\nmembers = []\n").unwrap();
    std::os::unix::fs::symlink(&target, fixture.root.join("Cargo.toml")).unwrap();
    std::fs::write(fixture.root.join("lib.rs"), "pub fn example() {}\n").unwrap();
    fixture.walk();
    std::fs::write(&target, "[workspace]\nmembers = []\nresolver = '2'\n").unwrap();
    // The watcher resolves the symlink, so its event names the target rather
    // than Cargo.toml. The captured input's identity must still invalidate it.
    fixture.batch(&[target], data_modify());
    fixture.assert_one_new_walk();
    assert_eq!(
        fixture.cache.metrics(&fixture.root).invalidations["marker_ignore_config_or_ts_install"],
        1
    );
}

#[cfg(unix)]
#[test]
fn applicability_walk_replaced_symlinked_input_target_invalidates() {
    let fixture = WatcherWalkFixture::new();
    let target = fixture.root.join("workspace.settings");
    std::fs::write(&target, "[workspace]\nmembers = []\n").unwrap();
    std::os::unix::fs::symlink(&target, fixture.root.join("Cargo.toml")).unwrap();
    std::fs::write(fixture.root.join("lib.rs"), "pub fn example() {}\n").unwrap();
    fixture.walk();
    // An atomic save writes a new file and renames it over the target, so
    // the target's identity changes and only its path can match the input.
    let staged = fixture.root.join("workspace.settings.new");
    std::fs::write(&staged, "[package]\nname = 'app'\nversion = '0.1.0'\n").unwrap();
    std::fs::rename(&staged, &target).unwrap();
    fixture.batch(
        &[staged, target],
        notify::EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::Both,
        )),
    );
    fixture.assert_one_new_walk();
    assert_eq!(
        fixture.cache.metrics(&fixture.root).invalidations["marker_ignore_config_or_ts_install"],
        1
    );
}

#[test]
fn applicability_walk_invalidation_returns_uncached_result_to_parked_callers() {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    let config = config(&root);
    let cache = ApplicabilityCache::default();
    let scans = std::sync::atomic::AtomicUsize::new(0);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = std::sync::Mutex::new(release_rx);
    let (caller, waiter) = std::thread::scope(|scope| {
        let caller = scope.spawn(|| {
            cache.walk(
                &root,
                &config,
                Instant::now() + Duration::from_secs(5),
                || {
                    let attempt = scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let result = scan(&root, &config);
                    if attempt == 0 {
                        started_tx.send(()).unwrap();
                        release_rx
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(5))
                            .unwrap();
                    }
                    Ok(result)
                },
            )
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let waiter = scope.spawn(|| {
            cache.walk(
                &root,
                &config,
                Instant::now() + Duration::from_secs(5),
                || {
                    scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(scan(&root, &config))
                },
            )
        });
        assert!(cache.wait_for_waiter_for_test(Instant::now() + Duration::from_secs(5)));
        std::fs::write(root.join("new.py"), "").unwrap();
        cache.invalidate(&root, InvalidationCause::CreateRemoveRename);
        release_tx.send(()).unwrap();
        (
            caller.join().unwrap().unwrap(),
            waiter.join().unwrap().unwrap(),
        )
    });
    assert_eq!(scans.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(caller.candidates.is_empty());
    assert_eq!(signature(&caller), signature(&waiter));
    let next = cache
        .walk(
            &root,
            &config,
            Instant::now() + Duration::from_secs(5),
            || {
                scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(scan(&root, &config))
            },
        )
        .unwrap();
    assert_eq!(scans.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(next
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Python));
}

#[test]
fn applicability_walk_panic_releases_flight_for_next_caller() {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    std::fs::write(root.join("main.py"), "").unwrap();
    let config = config(&root);
    let cache = ApplicabilityCache::default();
    let scans = std::sync::atomic::AtomicUsize::new(0);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = std::sync::Mutex::new(release_rx);
    let recovered = std::thread::scope(|scope| {
        let caller = scope.spawn(|| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                cache.walk(
                    &root,
                    &config,
                    Instant::now() + Duration::from_secs(5),
                    || {
                        scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        started_tx.send(()).unwrap();
                        release_rx
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(5))
                            .unwrap();
                        panic!("injected scan panic");
                    },
                )
            }))
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let waiter = scope.spawn(|| {
            cache.walk(
                &root,
                &config,
                Instant::now() + Duration::from_secs(2),
                || {
                    scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(scan(&root, &config))
                },
            )
        });
        assert!(cache.wait_for_waiter_for_test(Instant::now() + Duration::from_secs(5)));
        release_tx.send(()).unwrap();
        assert!(caller.join().unwrap().is_err());
        waiter
            .join()
            .unwrap()
            .expect("panic must release the parked caller, not leave it waiting for its deadline")
    });
    let next = cache
        .walk(
            &root,
            &config,
            Instant::now() + Duration::from_secs(2),
            || panic!("the successful recovery scan should be cached"),
        )
        .unwrap();
    assert_eq!(signature(&next), signature(&recovered));
    assert_eq!(scans.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[test]
fn applicability_walk_entry_budget_reports_incomplete() {
    let temp = tempfile::tempdir().unwrap();
    for n in 0..10 {
        std::fs::write(temp.path().join(format!("{n}.txt")), "").unwrap();
    }
    let error = walk_applicable_area_uncached(
        temp.path(),
        None,
        &config(temp.path()),
        Instant::now() + Duration::from_secs(10),
        3,
        RootPathMode::WalkEntry,
    )
    .unwrap_err();
    assert!(
        matches!(error, ApplicabilityResolutionError::RootUnreadable { reason, .. } if reason.contains("incomplete: entry limit 3"))
    );
}

#[test]
fn applicability_walk_deadline_is_checked_inside_iterator() {
    let temp = tempfile::tempdir().unwrap();
    let result = walk_applicable_area_uncached(
        temp.path(),
        None,
        &config(temp.path()),
        Instant::now() - Duration::from_secs(1),
        200_000,
        RootPathMode::WalkEntry,
    );
    assert!(matches!(
        result,
        Err(ApplicabilityResolutionError::RequestDeadline { .. })
    ));
}

#[cfg(unix)]
#[test]
fn applicability_walk_matches_legacy_nested_workspaces_and_outside_symlinks() {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.join("crates/member/src")).unwrap();
    std::fs::create_dir_all(root.join("nested/src")).unwrap();
    std::fs::create_dir_all(root.join("python/deep")).unwrap();
    for (file, contents) in [
        (
            "Cargo.toml",
            "[workspace]\nmembers = ['crates/member']\nexclude = ['nested']\n",
        ),
        (
            "crates/member/Cargo.toml",
            "[package]\nname = 'member'\nversion = '0.1.0'",
        ),
        ("crates/member/src/lib.rs", ""),
        ("nested/Cargo.toml", "[workspace]\nmembers = []"),
        ("nested/src/lib.rs", ""),
        ("package.json", "{}"),
        ("main.ts", ""),
        ("python/pyproject.toml", "[tool.pyright]"),
        ("python/deep/requirements.txt", ""),
        ("python/deep/main.py", ""),
    ] {
        std::fs::write(root.join(file), contents).unwrap();
    }
    std::fs::write(outside.path().join("go.mod"), "module outside").unwrap();
    std::fs::write(outside.path().join("outside.go"), "").unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("outside")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("outside.go"), root.join("outside.go")).unwrap();
    let config = config(&root);
    let old = legacy_walk(&root, &config);
    let new = scan(&root, &config);
    assert_eq!(signature(&old), signature(&new));
    assert!(new
        .candidates
        .iter()
        .any(|c| c.key.root == root && c.key.kind == ServerKind::Rust));
    assert!(!new.candidates.iter().any(|c| c.key.kind == ServerKind::Go));
}

// Frozen pre-optimization algorithm: keep the independent traversal and public
// root-resolution APIs so parity checks do not compare the fast path to itself.
fn legacy_walk(project_root: &Path, config: &Config) -> ApplicabilityWalk {
    legacy_scoped_walk(project_root, config, None)
}

fn legacy_scoped_walk(
    project_root: &Path,
    config: &Config,
    scopes: Option<&[PathBuf]>,
) -> ApplicabilityWalk {
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    let mut markers = HashMap::new();
    let roots = scopes.unwrap_or_default();
    let mut builder =
        ignore::WalkBuilder::new(roots.first().map_or(project_root, PathBuf::as_path));
    for root in roots.iter().skip(1) {
        builder.add(root);
    }
    builder.same_file_system(true).standard_filters(true);
    let walker = crate::context::apply_project_ignore_rules(&mut builder, project_root)
        .filter_entry(|entry| {
            !crate::lsp::roots::skip_in_server_walk(
                entry.file_name().to_string_lossy().as_ref(),
                entry.depth(),
            )
        })
        .build();
    for entry in walker {
        let entry = entry.unwrap();
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let file = entry.path();
        for definition in servers_with_root_marker(file, config) {
            if markers.contains_key(&definition.kind) {
                continue;
            }
            let directory = file.parent().unwrap_or(project_root).to_path_buf();
            let name = file.file_name().unwrap().to_string_lossy().into_owned();
            markers.insert(definition.kind.clone(), (definition, directory, name));
        }
        for definition in servers_for_file(file, config) {
            let Some(key) = server_key_for_definition(&definition, file, config) else {
                continue;
            };
            if !seen.insert(key.clone()) {
                continue;
            }
            candidates.push(ApplicableServerCandidate {
                key,
                definition,
                source_file: file.into(),
            });
        }
    }
    ApplicabilityWalk {
        candidates,
        markers,
        entries: 0,
    }
}

fn write_mixed_registry_fixture(root: &Path) {
    for (name, contents) in [
        (
            "Cargo.toml",
            "[workspace]\nmembers=['crates/member']\nexclude=['nested']",
        ),
        (
            "crates/member/Cargo.toml",
            "[package]\nname='member'\nversion='0.1.0'",
        ),
        ("crates/member/src/lib.rs", ""),
        ("nested/Cargo.toml", "[workspace]\nmembers=[]"),
        ("nested/src/lib.rs", ""),
        ("package.json", "{}"),
        (
            "biome.json",
            r#"{"files":{"includes":["**/*.ts","!**/skip.ts"]}}"#,
        ),
        ("src/keep.ts", ""),
        ("src/skip.ts", ""),
        ("src/data.json", "{}"),
        ("src/UPPER.TS", ""),
        ("next/package.json", "{}"),
        (
            "next/node_modules/typescript/package.json",
            r#"{"name":"typescript","version":"7.0.2"}"#,
        ),
        ("next/src/index.ts", ""),
        ("old/package.json", "{}"),
        (
            "old/node_modules/typescript/package.json",
            r#"{"name":"typescript","version":"5.9.3"}"#,
        ),
        ("old/src/index.ts", ""),
        ("python/pyproject.toml", "[tool.pyright]"),
        ("python/deep/requirements.txt", ""),
        ("python/deep/helper.py", ""),
        ("scripts/helper.py", ""),
        ("next/scripts/run.sh", ""),
        ("next/data.yaml", ""),
        ("custom/marker.root", ""),
        ("custom/main.xyz", ""),
        ("custom/other.XYZ", ""),
    ] {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
}

fn mixed_registry_config(root: &Path, overrides: bool) -> Config {
    let mut config = config(root);
    config.experimental_lsp_ty = true;
    if overrides {
        use crate::config::UserServerDef;
        config.lsp_servers = vec![
            UserServerDef {
                id: "custom".into(),
                binary: "custom-lsp".into(),
                extensions: vec!["xyz".into(), "XYZ".into()],
                root_markers: vec!["marker.root".into()],
                ..Default::default()
            },
            UserServerDef {
                id: "typescript".into(),
                args: vec!["--stdio".into()],
                initialization_options: Some(
                    serde_json::json!({"preferences":{"quotePreference":"single"}}),
                ),
                ..Default::default()
            },
            UserServerDef {
                id: "biome".into(),
                root_markers: vec!["package.json".into()],
                ..Default::default()
            },
            UserServerDef {
                id: "go".into(),
                disabled: true,
                ..Default::default()
            },
        ];
    }
    config
}

#[test]
fn applicability_walk_mixed_registry_matches_legacy_byte_for_byte() {
    let temp = tempfile::tempdir().unwrap();
    // The uncached walk keeps its root's spelling, so hand it the normalized
    // root `walk_applicable_area` would. A bare `canonicalize` is `\\?\` on
    // Windows; the entry-level test below covers that spelling.
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    write_mixed_registry_fixture(&root);
    for overrides in [false, true] {
        let config = mixed_registry_config(&root, overrides);
        for scopes in [
            None,
            Some(vec![root.join("src/skip.ts")]),
            Some(vec![root.join("src/keep.ts")]),
            Some(vec![root.join("next"), root.join("custom")]),
        ] {
            let old = legacy_scoped_walk(&root, &config, scopes.as_deref());
            let new = walk_applicable_area_uncached(
                &root,
                scopes.as_deref(),
                &config,
                Instant::now() + Duration::from_secs(30),
                200_000,
                RootPathMode::WalkEntry,
            )
            .unwrap();
            let candidates = |walk: ApplicabilityWalk| {
                walk.candidates
                    .into_iter()
                    .map(|c| (c.key, c.definition, c.source_file))
                    .collect::<Vec<_>>()
            };
            assert_eq!(new.markers, old.markers);
            assert_eq!(candidates(new), candidates(old));
        }
    }
}

/// Language servers are keyed and started by the roots the production entry
/// returns. Hand the entry the spelling a bare `fs::canonicalize` gives (`\\?\C:\...`
/// on Windows) and require the server keys legacy per-file resolution derives
/// from the same root, none of them verbatim. Source files and marker
/// directories keep each walk's own spelling, so only keys are compared.
#[test]
fn applicability_walk_entry_keys_match_legacy_for_canonicalized_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    write_mixed_registry_fixture(&root);
    for overrides in [false, true] {
        let config = mixed_registry_config(&root, overrides);
        for scopes in [None, Some(vec![root.join("next"), root.join("custom")])] {
            let old = legacy_scoped_walk(&root, &config, scopes.as_deref());
            let new = walk_applicable_area(
                &root,
                scopes.as_deref(),
                &config,
                Some(Instant::now() + Duration::from_secs(30)),
            )
            .unwrap();
            let keys = |walk: &ApplicabilityWalk| {
                walk.candidates
                    .iter()
                    .map(|c| (c.key.clone(), c.definition.clone()))
                    .collect::<Vec<_>>()
            };
            assert!(!new.candidates.is_empty());
            assert_eq!(keys(&new), keys(&old));
            for candidate in &new.candidates {
                assert!(
                    !is_windows_verbatim(&candidate.key.root),
                    "verbatim server root {}",
                    candidate.key.root.display()
                );
            }
        }
    }
}

fn cost_fixture(files_per_directory: usize, directories: usize) -> (tempfile::TempDir, Config) {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    std::fs::write(root.join("package.json"), "{}").unwrap();
    std::fs::write(root.join("biome.json"), r#"{"files":{"includes":["**"]}}"#).unwrap();
    for directory in 0..directories {
        let dir = root.join(format!("packages/pkg{directory}/src"));
        std::fs::create_dir_all(&dir).unwrap();
        for file in 0..files_per_directory {
            let extension = ["ts", "json", "py", "sh", "yaml", "rs", "go", "txt"][file % 8];
            std::fs::write(dir.join(format!("file{file}.{extension}")), "").unwrap();
        }
    }
    let config = config(&root);
    (temp, config)
}

#[test]
fn applicability_walk_sibling_cost_does_not_rebuild_definitions_or_roots() {
    let (temp, config) = cost_fixture(512, 1);
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    let (walk, allocations) = crate::test_allocations::count(|| scan(&root, &config));
    println!("sibling_files=512 allocations={allocations}");
    assert!(!walk.candidates.is_empty());
    assert!(
        allocations < 40_000,
        "per-file server discovery allocated {allocations} times"
    );
}

#[test]
#[ignore = "copies the repository for a before/after measurement"]
fn applicability_walk_repository_measurement() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    let mut copied = 0;
    for entry in ignore::WalkBuilder::new(source).build() {
        let entry = entry.unwrap();
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let dest = root.join(entry.path().strip_prefix(source).unwrap());
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(entry.path(), dest).unwrap();
        copied += 1;
    }
    let config = config(&root);
    super::super::typescript_project::clear_typescript_selection();
    let (old, old_calls, old_time) = measured(|| legacy_walk(&root, &config));
    super::super::typescript_project::clear_typescript_selection();
    let (new, new_calls, new_time) =
        measured(|| walk_applicable_area(&root, None, &config, None).unwrap());
    let (_, cached_calls, cached_time) =
        measured(|| walk_applicable_area(&root, None, &config, None).unwrap());
    println!("copied_files={copied} legacy: canonicalize={} elapsed={old_time:?}; lexical: canonicalize={} elapsed={new_time:?}; cached: canonicalize={} elapsed={cached_time:?}", old_calls.len(), new_calls.len(), cached_calls.len());
    assert_eq!(signature(&old), signature(&new));
    assert_eq!(new_calls, vec![root]);
}

/// Mirror the TypeScript-selection half of `invalidate_applicability_batch`,
/// which these tests bypass to use their own cache.
fn forget_typescript_selection_for(paths: &[PathBuf]) {
    if paths.iter().any(|path| !path.is_file()) {
        super::super::typescript_project::clear_typescript_selection();
    } else {
        super::super::typescript_project::invalidate_typescript_selection(
            paths.iter().map(PathBuf::as_path),
        );
    }
}

/// Creating source files of languages the walk already selected, in
/// directories it visited, keeps the cached result without a walk. So does a
/// file in a hidden directory the walk never enters.
#[test]
fn applicability_walk_plain_source_creates_cause_no_full_walk() {
    let fixture = WatcherWalkFixture::new();
    std::fs::write(fixture.root.join("main.go"), "package main").unwrap();
    std::fs::create_dir(fixture.root.join("lib")).unwrap();
    std::fs::write(fixture.root.join("lib/util.py"), "").unwrap();
    std::fs::write(fixture.root.join("lib/util.go"), "package lib").unwrap();
    let before = fixture.walk();
    std::fs::create_dir(fixture.root.join(".cache")).unwrap();
    let create = notify::EventKind::Create(notify::event::CreateKind::File);
    for n in 0..20 {
        let paths = [
            fixture.root.join(format!("extra{n}.py")),
            fixture.root.join(format!("lib/extra{n}.go")),
            fixture.root.join(format!("notes{n}.txt")),
            // A shell script would select the Bash server, which no walked file
            // selects; the walk never enters the hidden directory.
            fixture.root.join(format!(".cache/scratch{n}.sh")),
        ];
        for path in &paths {
            std::fs::write(path, "").unwrap();
        }
        fixture.batch(&paths, create);
        assert_eq!(canonical(&fixture.walk()), canonical(&before), "batch {n}");
    }
    let metrics = fixture.cache.metrics(&fixture.root);
    assert_eq!(metrics.full_walks, 1, "plain source creates must not walk");
    assert_eq!(metrics.retained_topology_batches, 20);
    assert_eq!(
        canonical(&before),
        canonical(&scan(&fixture.root, &fixture.config))
    );
}

/// Deterministic xorshift generator, so a failing sequence can be replayed.
struct Sequence(u64);

impl Sequence {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % bound as u64) as usize
    }
}

/// Random create, remove and rename batches over a mixed-language tree: after
/// every batch, the cached result (kept or walked again) must equal a fresh
/// walk of the tree.
#[test]
fn applicability_walk_incremental_batches_match_fresh_walks() {
    let mut retained = 0;
    let mut walks = 0;
    for seed in [
        0x9e37_79b9_7f4a_7c15,
        7,
        42,
        1234,
        0xdead_beef,
        31,
        99,
        2024,
        0x5eed,
        65_537,
        777,
        13,
        0xc0ffee,
        4096,
        123_456_789,
        8,
    ] {
        let (kept, walked) = differential_sequence(seed, 80);
        retained += kept;
        walks += walked;
    }
    // Both the kept and the walked paths must have run for the comparison to
    // cover the incremental decision.
    println!("kept={retained} walked={walks}");
    assert!(
        retained > 40,
        "only {retained} batches kept the cached result"
    );
    assert!(walks > 20, "only {walks} batches walked again");
}

fn differential_sequence(seed: u64, batches: usize) -> (u64, u64) {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    std::fs::create_dir(root.join(".git")).unwrap();
    for (name, contents) in [
        (".gitignore", "ignored/\n"),
        ("Cargo.toml", "[workspace]\nmembers = ['crates/*']\n"),
        (
            "crates/core/Cargo.toml",
            "[package]\nname = 'core'\nversion = '0.1.0'\n",
        ),
        ("crates/core/src/lib.rs", ""),
        ("go/go.mod", "module example.test/go\n"),
        ("go/main.go", "package main"),
        ("web/package.json", "{}"),
        ("web/src/index.ts", ""),
        ("web/src/app.js", ""),
        (
            "web/node_modules/typescript/package.json",
            r#"{"name":"typescript","version":"7.0.2"}"#,
        ),
        ("pkg/package.json", "{}"),
        ("py/pyproject.toml", "[project]\nname = 'py'\n"),
        ("py/app.py", ""),
        ("scripts/run.sh", ""),
        ("docs/readme.md", ""),
        ("ignored/skip.py", ""),
        ("node_modules/pkg/index.js", ""),
        (".cache/tmp.py", ""),
    ] {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    let mut config = config(&root);
    // Biome probes directories above the checkout, which parallel tests change.
    config.disabled_lsp.insert("biome".into());
    let mut dirs: Vec<PathBuf> = [
        "",
        "crates/core/src",
        "go",
        "web/src",
        "pkg",
        "py",
        "scripts",
        "docs",
        "ignored",
        "node_modules/pkg",
        ".cache",
    ]
    .iter()
    .map(|dir| root.join(dir))
    .collect();
    let mut files: Vec<PathBuf> = [
        "crates/core/src/lib.rs",
        "go/main.go",
        "web/src/index.ts",
        "web/src/app.js",
        "py/app.py",
        "scripts/run.sh",
        "docs/readme.md",
        "ignored/skip.py",
        "node_modules/pkg/index.js",
        ".cache/tmp.py",
    ]
    .iter()
    .map(|file| root.join(file))
    .collect();
    let extensions = [
        "py", "go", "rs", "ts", "js", "sh", "yaml", "txt", "md", "c", "lua", "tmp",
    ];
    let markers = [
        (
            "Cargo.toml",
            "[package]\nname = 'extra'\nversion = '0.1.0'\n",
        ),
        ("go.mod", "module example.test/extra\n"),
        ("package.json", "{}"),
        ("pyproject.toml", "[project]\nname = 'extra'\n"),
    ];
    let cache = ApplicabilityCache::default();
    let walk = || {
        cache
            .walk(
                &root,
                &config,
                Instant::now() + Duration::from_secs(30),
                || Ok(scan(&root, &config)),
            )
            .unwrap()
    };
    walk();
    let mut sequence = Sequence(seed);
    let mut counter = 0;
    for batch in 0..batches {
        let mut paths = Vec::new();
        if batch == 0 {
            // Removing an installed TypeScript reported as its enclosing
            // `node_modules` alone: no recorded file or marker is below it,
            // but the walk read the install to choose the TypeScript server.
            let modules = root.join("web/node_modules");
            std::fs::remove_dir_all(&modules).unwrap();
            paths.push(modules);
        }
        let operations = if batch == 0 { 0 } else { 1 + sequence.below(3) };
        for _ in 0..operations {
            counter += 1;
            match sequence.below(100) {
                30..=34 => {
                    // Install or remove one package's TypeScript, reported as
                    // the topmost directory that appeared or disappeared.
                    let package = root.join(["", "web", "pkg"][sequence.below(3)]);
                    if !package.is_dir() {
                        continue;
                    }
                    let modules = package.join("node_modules");
                    if modules.exists() && sequence.below(2) == 0 {
                        std::fs::remove_dir_all(&modules).unwrap();
                        files.retain(|file| !file.starts_with(&modules));
                        dirs.retain(|dir| !dir.starts_with(&modules));
                        paths.push(modules);
                    } else {
                        let version = ["5.4.5", "5.9.3", "7.0.2"][sequence.below(3)];
                        let install = modules.join("typescript");
                        let reported = if modules.exists() {
                            install.clone()
                        } else {
                            modules
                        };
                        std::fs::create_dir_all(&install).unwrap();
                        std::fs::write(
                            install.join("package.json"),
                            format!(r#"{{"name":"typescript","version":"{version}"}}"#),
                        )
                        .unwrap();
                        paths.push(reported);
                    }
                }
                0..=29 => {
                    let dir = &dirs[sequence.below(dirs.len())];
                    let ext = extensions[sequence.below(extensions.len())];
                    let file = dir.join(format!("f{counter}.{ext}"));
                    std::fs::write(&file, "").unwrap();
                    paths.push(file.clone());
                    files.push(file);
                }
                35..=59 if !files.is_empty() => {
                    let file = files.swap_remove(sequence.below(files.len()));
                    std::fs::remove_file(&file).unwrap();
                    paths.push(file);
                }
                60..=74 if !files.is_empty() => {
                    let index = sequence.below(files.len());
                    let dir = &dirs[sequence.below(dirs.len())];
                    let ext = extensions[sequence.below(extensions.len())];
                    let to = dir.join(format!("f{counter}.{ext}"));
                    std::fs::rename(&files[index], &to).unwrap();
                    paths.push(std::mem::replace(&mut files[index], to.clone()));
                    paths.push(to);
                }
                75..=82 => {
                    let dir = &dirs[sequence.below(dirs.len())];
                    let (name, contents) = markers[sequence.below(markers.len())];
                    let marker = dir.join(name);
                    if !marker.exists() {
                        std::fs::write(&marker, contents).unwrap();
                        files.push(marker.clone());
                    }
                    paths.push(marker);
                }
                83..=94 => {
                    let parent = &dirs[sequence.below(dirs.len())];
                    let dir = parent.join(format!("d{counter}"));
                    std::fs::create_dir(&dir).unwrap();
                    // Some watchers report a new directory's files without the
                    // directory itself.
                    if sequence.below(2) == 0 {
                        paths.push(dir.clone());
                    }
                    if sequence.below(2) == 0 {
                        let ext = extensions[sequence.below(extensions.len())];
                        let file = dir.join(format!("f{counter}.{ext}"));
                        std::fs::write(&file, "").unwrap();
                        paths.push(file.clone());
                        files.push(file);
                    }
                    dirs.push(dir);
                }
                95..=97 if dirs.len() > 1 => {
                    let dir = dirs[1 + sequence.below(dirs.len() - 1)].clone();
                    let to = root.join(format!("r{counter}"));
                    std::fs::rename(&dir, &to).unwrap();
                    let moved = |path: &mut PathBuf| {
                        if let Ok(relative) = path.strip_prefix(&dir) {
                            *path = to.join(relative);
                        }
                    };
                    files.iter_mut().for_each(moved);
                    dirs.iter_mut().for_each(moved);
                    paths.push(dir);
                    paths.push(to);
                }
                98.. if dirs.len() > 1 => {
                    let dir = dirs[1 + sequence.below(dirs.len() - 1)].clone();
                    std::fs::remove_dir_all(&dir).unwrap();
                    // A recursive removal may be reported as the directory
                    // alone, or with the files it held.
                    let report_files = sequence.below(2) == 0;
                    files.retain(|file| {
                        let inside = file.starts_with(&dir);
                        if inside && report_files {
                            paths.push(file.clone());
                        }
                        !inside
                    });
                    dirs.retain(|other| !other.starts_with(&dir));
                    paths.push(dir);
                }
                _ => {}
            }
        }
        if paths.is_empty() {
            continue;
        }
        forget_typescript_selection_for(&paths);
        cache.invalidate_batch(&root, &paths, false);
        let cached = walk();
        let fresh = scan(&root, &config);
        assert_eq!(
            canonical(&cached),
            canonical(&fresh),
            "seed {seed} batch {batch} paths {paths:?}"
        );
    }
    let metrics = cache.metrics(&root);
    (metrics.retained_topology_batches, metrics.full_walks - 1)
}

#[cfg(unix)]
fn thread_cpu_time() -> Duration {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `time` is a valid, writable timespec for the call to fill.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) },
        0
    );
    Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
}

/// Replays 1,000 create and remove events on a 20,000-file tree, each
/// followed by a lookup as an inspection would make. "before" handles each
/// batch as the cache did before topology batches were checked, invalidating
/// the result; "after" uses the batch classification.
#[cfg(unix)]
#[test]
#[ignore = "builds a 20,000-file tree for a before/after measurement"]
fn applicability_walk_create_remove_burst_measurement() {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::write(root.join("package.json"), "{}").unwrap();
    std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
    std::fs::write(root.join("go.mod"), "module example.test/app\n").unwrap();
    std::fs::write(root.join("pyproject.toml"), "[project]\nname = 'app'\n").unwrap();
    let extensions = ["ts", "py", "rs", "go", "sh", "yaml", "txt", "md"];
    for directory in 0..200 {
        let dir = root.join(format!("packages/pkg{directory}/src"));
        std::fs::create_dir_all(&dir).unwrap();
        for file in 0..100 {
            let extension = extensions[file % extensions.len()];
            std::fs::write(dir.join(format!("file{file}.{extension}")), "").unwrap();
        }
    }
    let mut config = config(&root);
    config.disabled_lsp.insert("biome".into());
    for incremental in [false, true] {
        let cache = ApplicabilityCache::default();
        let walk = || {
            cache
                .walk(
                    &root,
                    &config,
                    Instant::now() + Duration::from_secs(60),
                    || Ok(scan(&root, &config)),
                )
                .unwrap()
        };
        walk();
        let walks_before = cache.metrics(&root).full_walks;
        let walk_micros_before = cache.metrics(&root).full_walk_micros;
        let cpu = thread_cpu_time();
        let wall = Instant::now();
        for n in 0..500 {
            let file = root.join(format!(
                "packages/pkg{}/src/burst{n}.{}",
                n % 200,
                extensions[n % extensions.len()]
            ));
            for create in [true, false] {
                if create {
                    std::fs::write(&file, "").unwrap();
                } else {
                    std::fs::remove_file(&file).unwrap();
                }
                let paths = std::slice::from_ref(&file);
                forget_typescript_selection_for(paths);
                if incremental {
                    cache.invalidate_batch(&root, paths, false);
                } else {
                    cache.invalidate(&root, InvalidationCause::CreateRemoveRename);
                }
                walk();
            }
        }
        let cpu = thread_cpu_time() - cpu;
        let metrics = cache.metrics(&root);
        println!(
            "{} events=1000 files=20000 full_walks={} cpu={cpu:?} wall={:?} full_walk_time={:?}",
            if incremental { "after" } else { "before" },
            metrics.full_walks - walks_before,
            wall.elapsed(),
            Duration::from_micros(metrics.full_walk_micros - walk_micros_before),
        );
        if incremental {
            assert_eq!(metrics.full_walks, walks_before);
        }
    }
}

/// A fresh walk counts every directory it enters as well as every file, and
/// reports an incomplete result past [`APPLICABILITY_WALK_ENTRY_LIMIT`]
/// entries. A covered file kept without a walk must count its new parent
/// directories too, even when the watcher reported only the file.
#[test]
fn applicability_walk_entry_estimate_counts_new_parent_directories_at_the_limit() {
    let fixture = WatcherWalkFixture::new();
    let real_entries = scan(&fixture.root, &fixture.config).entries;
    let create = notify::EventKind::Create(notify::event::CreateKind::File);
    for (walked_entries, file, walks_again) in [
        // One past the limit: a fresh walk reports an incomplete result.
        (
            APPLICABILITY_WALK_ENTRY_LIMIT - 2,
            "fresh/deeper/extra.py",
            true,
        ),
        (APPLICABILITY_WALK_ENTRY_LIMIT - 1, "fresh/extra.py", true),
        // Exactly at the limit: a fresh walk still completes.
        (
            APPLICABILITY_WALK_ENTRY_LIMIT - 3,
            "fresh/deeper/extra.py",
            false,
        ),
        (APPLICABILITY_WALK_ENTRY_LIMIT - 1, "extra.py", false),
    ] {
        let cache = ApplicabilityCache::default();
        // Pretend the tree already held all but a few entries of the limit.
        let walk = || {
            cache
                .walk(
                    &fixture.root,
                    &fixture.config,
                    Instant::now() + Duration::from_secs(10),
                    || {
                        let mut walk = scan(&fixture.root, &fixture.config);
                        walk.entries = walk.entries - real_entries + walked_entries;
                        Ok(walk)
                    },
                )
                .unwrap()
        };
        walk();
        let path = fixture.root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        let added = Path::new(file).components().count();
        assert_eq!(
            scan(&fixture.root, &fixture.config).entries,
            real_entries + added,
            "a fresh walk counts the file and each new directory"
        );
        cache.invalidate_batch(
            &fixture.root,
            std::slice::from_ref(&path),
            crate::watcher_filter::watcher_event_is_content_only(&create),
        );
        walk();
        assert_eq!(
            cache.metrics(&fixture.root).full_walks,
            1 + u64::from(walks_again),
            "{file} with {walked_entries} walked entries"
        );
        std::fs::remove_dir_all(fixture.root.join("fresh")).ok();
        std::fs::remove_file(fixture.root.join("extra.py")).ok();
    }
}

/// The root and a child package each install the same TypeScript, and only
/// the root has TypeScript sources, so the walk never reads the child's
/// install. The child's first TypeScript file shares the root's server and is
/// kept without a walk, but deciding that read the child's install. Removing
/// the child's `node_modules`, reported alone, must then walk again.
#[test]
fn applicability_walk_kept_create_records_the_inputs_it_read() {
    let mut fixture = WatcherWalkFixture::new();
    // Oxlint looks for its configuration above the checkout, in directories
    // parallel tests change; this test counts walks.
    fixture.config.disabled_lsp.insert("oxlint".into());
    let typescript = r#"{"name":"typescript","version":"5.4.5"}"#;
    for (name, contents) in [
        ("node_modules/typescript/package.json", typescript),
        ("src/index.ts", ""),
        ("pkg/package.json", "{}"),
        ("pkg/node_modules/typescript/package.json", typescript),
    ] {
        let path = fixture.root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    super::super::typescript_project::clear_typescript_selection();
    fixture.walk();
    let source = fixture.root.join("pkg/a.ts");
    std::fs::write(&source, "").unwrap();
    let created = [source];
    forget_typescript_selection_for(&created);
    fixture.batch(
        &created,
        notify::EventKind::Create(notify::event::CreateKind::File),
    );
    fixture.walk();
    assert_eq!(
        fixture.cache.metrics(&fixture.root).full_walks,
        1,
        "the child's file shares the root's TypeScript server: {:?}",
        fixture.cache.metrics(&fixture.root)
    );
    let modules = fixture.root.join("pkg/node_modules");
    std::fs::remove_dir_all(&modules).unwrap();
    let removed = [modules];
    forget_typescript_selection_for(&removed);
    fixture.batch(
        &removed,
        notify::EventKind::Remove(notify::event::RemoveKind::Folder),
    );
    let result = fixture.walk();
    assert_eq!(
        canonical(&result),
        canonical(&scan(&fixture.root, &fixture.config))
    );
    assert_eq!(
        fixture.cache.metrics(&fixture.root).full_walks,
        2,
        "removing an install the kept create read must walk again"
    );
}
