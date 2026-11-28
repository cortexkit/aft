use super::*;

#[test]
fn lsp_lifecycle_cache_bounds_many_created_paths_and_evicts_oldest() {
    let mut cache = WatchedLifecycleCache::default();
    let now = Instant::now();
    for index in 0..WATCHED_LIFECYCLE_CAP * 2 {
        cache.remember(
            PathBuf::from(format!("generated/{index}.ts")),
            FileChangeType::CREATED,
            None,
            now,
        );
        assert!(cache.entries.len() <= WATCHED_LIFECYCLE_CAP);
        assert_eq!(cache.oldest.len(), cache.entries.len());
    }
    assert_eq!(cache.entries.len(), WATCHED_LIFECYCLE_CAP);
    assert!(!cache.entries.contains_key(Path::new("generated/0.ts")));
    assert!(cache.entries.contains_key(&PathBuf::from(format!(
        "generated/{}.ts",
        WATCHED_LIFECYCLE_CAP
    ))));

    let path = PathBuf::from("repeated.ts");
    for _ in 0..WATCHED_LIFECYCLE_CAP * 2 {
        cache.remember(path.clone(), FileChangeType::DELETED, None, now);
        assert_eq!(cache.entries.len(), WATCHED_LIFECYCLE_CAP);
        assert_eq!(cache.oldest.len(), WATCHED_LIFECYCLE_CAP);
    }
}

#[test]
fn lsp_lifecycle_late_echo_within_ttl_is_absorbed_once_and_releases_entry() {
    let mut cache = WatchedLifecycleCache::default();
    let now = Instant::now();
    let path = PathBuf::from("new.ts");
    let stamp = Some((42, Some(std::time::UNIX_EPOCH)));
    cache.remember(path.clone(), FileChangeType::CREATED, stamp, now);
    let late = now + WATCHED_LIFECYCLE_TTL - Duration::from_millis(1);
    assert!(cache.absorb_echo(&path, FileChangeType::CHANGED, stamp, late));
    assert!(cache.entries.is_empty());
    assert!(cache.oldest.is_empty());
    // Once consumed, additional echoes may be forwarded instead of retained.
    assert!(!cache.absorb_echo(&path, FileChangeType::CREATED, stamp, late));
}

#[test]
fn lsp_lifecycle_expired_entries_no_longer_suppress_notifications() {
    let mut cache = WatchedLifecycleCache::default();
    let now = Instant::now();
    let path = PathBuf::from("deleted.ts");
    cache.remember(path.clone(), FileChangeType::DELETED, None, now);
    assert!(!cache.absorb_echo(
        &path,
        FileChangeType::DELETED,
        None,
        now + WATCHED_LIFECYCLE_TTL
    ));
    assert!(cache.entries.is_empty());
    assert!(cache.oldest.is_empty());
}

#[test]
fn lsp_lifecycle_prepared_stamps_distinguish_later_content_edits() {
    let mut cache = WatchedLifecycleCache::default();
    let now = Instant::now();
    let path = PathBuf::from("new.ts");
    let original = Some((42, Some(std::time::UNIX_EPOCH)));
    cache.remember(path.clone(), FileChangeType::CREATED, original, now);
    assert!(!cache.absorb_echo(
        &path,
        FileChangeType::CHANGED,
        Some((43, Some(std::time::UNIX_EPOCH))),
        now
    ));
    assert_eq!(cache.entries.len(), 1);
    assert!(cache.absorb_echo(&path, FileChangeType::CREATED, original, now));
}
