use super::prepare_cache_root_for_test as prepare_cache_root;
use super::*;
use crate::config::Config;

fn cache_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("aft");
    let cache = temp.path().join("cache");
    fs::create_dir(&cache).unwrap();
    let mut config = Config {
        storage_dir: Some(storage.clone()),
        cache_dir: Some(cache),
        ..Config::default()
    };
    assert!(prepare_cache_root(&mut config).is_none());
    let cache = cache_root(&config);
    (temp, storage, cache)
}

#[cfg(unix)]
#[test]
fn cache_file_creation_keeps_the_open_parent_after_ancestor_retarget() {
    use std::io::Write;
    let (temp, storage, cache) = cache_fixture();
    let directory = cache.join("views/scope");
    crate::private_storage::create_dir_all(&directory).unwrap();
    let parent = crate::root_cache::pin_cache_path(&directory.join("payload"))
        .unwrap()
        .unwrap()
        .parent()
        .unwrap()
        .0;
    let durable = storage.join("checkpoints");
    fs::create_dir_all(durable.join("scope")).unwrap();
    let parked = temp.path().join("parked-views");
    fs::rename(cache.join("views"), &parked).unwrap();
    std::os::unix::fs::symlink(&durable, cache.join("views")).unwrap();
    parent
        .repair_create_file(std::ffi::OsStr::new("payload"))
        .unwrap()
        .write_all(b"cache")
        .unwrap();
    assert!(
        !durable.join("scope/payload").exists(),
        "cache creation followed an ancestor after its parent was pinned"
    );
    assert_eq!(fs::read(parked.join("scope/payload")).unwrap(), b"cache");
}

#[test]
fn cache_root_missing_is_unavailable_without_creating_anything() {
    let temp = tempfile::tempdir().unwrap();
    let missing = temp.path().join("offline-volume");
    let storage = temp.path().join("aft");
    let mut config = Config {
        storage_dir: Some(storage.clone()),
        cache_dir: Some(missing.clone()),
        ..Default::default()
    };
    let warning = prepare_cache_root(&mut config);
    assert!(
        !missing.exists(),
        "validation must never create the configured cache root"
    );
    assert!(
        !storage.exists(),
        "unavailable cache validation must not write durable state"
    );
    assert_eq!(warning.unwrap().code, "cache_dir_unavailable");
    assert_eq!(config.cache_dir, Some(missing));
}

#[cfg(unix)]
#[test]
fn cache_root_unavailable_rejects_dangling_and_retargeting_symlinks() {
    // A configured cache_dir may be a symlink the user chose. It is resolved
    // once when first used and pinned by identity: a dangling link is
    // refused, and so is the same spelling pointing somewhere else later.
    let temp = tempfile::tempdir().unwrap();
    let link = temp.path().join("link");
    let target = temp.path().join("target");
    let other = temp.path().join("other");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let config_for = || Config {
        storage_dir: Some(temp.path().join("storage")),
        cache_dir: Some(link.clone()),
        ..Default::default()
    };
    let mut config = config_for();
    assert!(prepare_cache_root(&mut config).is_some());
    assert!(cache_unavailable(&config).is_some());
    assert!(!target.join("aft").exists());

    fs::create_dir(&target).unwrap();
    let mut config = config_for();
    let warning = prepare_cache_root(&mut config);
    assert!(warning.is_none(), "{warning:?}");
    assert!(target.join("aft").is_dir());
    // Paths built from the configured spelling are admitted too.
    crate::private_storage::create_dir_all(link.join("aft/index/through-link")).unwrap();
    assert!(target.join("aft/index/through-link").is_dir());

    fs::create_dir(&other).unwrap();
    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&other, &link).unwrap();
    let mut config = config_for();
    let warning = prepare_cache_root(&mut config).expect("a retargeted link must be refused");
    assert!(
        warning.message.contains("not the volume it was"),
        "{warning:?}"
    );
    assert!(cache_unavailable(&config).is_some());
    assert!(!other.join("aft").exists());
}

#[cfg(unix)]
#[test]
fn cache_root_recheck_notices_a_repointed_link_while_the_old_target_remains() {
    // The volume probe re-prepares the running config, whose `cache_dir` is
    // the directory the link resolved to. It must resolve the configured link
    // again: if the link now names another directory, caches stop, even
    // though the old target is still there and healthy.
    let temp = tempfile::tempdir().unwrap();
    let link = temp.path().join("cache-link");
    let target = temp.path().join("target");
    let other = temp.path().join("other");
    fs::create_dir(&target).unwrap();
    fs::create_dir(&other).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let mut config = Config {
        storage_dir: Some(temp.path().join("storage")),
        cache_dir: Some(link.clone()),
        ..Default::default()
    };
    let warning = prepare_cache_root(&mut config);
    assert!(warning.is_none(), "{warning:?}");
    assert!(cache_unavailable(&config).is_none());

    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&other, &link).unwrap();
    let mut rechecked = config.clone();
    let warning = prepare_cache_root(&mut rechecked).expect("a repointed link must be refused");
    assert_eq!(warning.code, CACHE_DIR_UNAVAILABLE, "{warning:?}");
    assert!(cache_unavailable(&rechecked).is_some());
    assert!(target.join("aft").is_dir(), "the old target is untouched");
    assert!(!other.join("aft").exists());

    // Pointing the link back at the enrolled directory restores caches.
    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let mut restored = config.clone();
    let warning = prepare_cache_root(&mut restored);
    assert!(warning.is_none(), "{warning:?}");
    assert!(cache_unavailable(&restored).is_none());
}

#[test]
fn cache_root_unavailable_preserves_requested_location_without_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("file");
    fs::write(&file, "not a directory").unwrap();
    for requested in [file, PathBuf::from("relative")] {
        let storage = temp.path().join("aft");
        let mut config = Config {
            storage_dir: Some(storage.clone()),
            cache_dir: Some(requested.clone()),
            ..Default::default()
        };
        let warning = prepare_cache_root(&mut config).expect("typed warning");
        assert_eq!(warning.code, CACHE_DIR_UNAVAILABLE);
        assert_eq!(warning.key, "storage.cache_dir");
        assert!(warning
            .message
            .contains("AFT is running without its caches until it is back."));
        assert_eq!(cache_root(&config), requested.join("aft"));
        assert!(cache_unavailable(&config).is_some());
        assert!(!storage.exists());
    }
}

#[test]
fn cache_root_reader_floor_refusals_cover_external_artifacts() {
    let (_temp, storage, cache) = cache_fixture();
    use crate::persisted_format::{self, PersistedStore, UnsupportedPersistedFormat};
    let refusal = UnsupportedPersistedFormat::floor(
        PersistedStore::SearchIndex,
        &storage.join("reader-floor.json"),
        999,
    );
    persisted_format::record(&refusal, &storage);
    crate::reader_floor::prepare_artifact_root(&storage, &cache);
    assert!(persisted_format::refusal_covering(
        PersistedStore::SearchIndex,
        &cache.join("index/key/cache.bin")
    )
    .is_some());
    assert!(persisted_format::gate(
        PersistedStore::SearchIndex,
        &cache.join("index/key/cache.bin"),
        &cache.join("index/key/cache.bin"),
        None
    )
    .is_err());
}
