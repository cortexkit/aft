use super::*;
use crate::harness::Harness;
use crate::subc_config::{ApprovedUser, PreparedBindConfig};

struct ConfigFiles {
    root: tempfile::TempDir,
    project: PathBuf,
    user: PathBuf,
}

impl ConfigFiles {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join(".cortexkit/aft.jsonc");
        let user = root.path().join("user.jsonc");
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        Self {
            root,
            project,
            user,
        }
    }

    fn bind(
        &self,
        cache: &mut BindConfigCache,
        approved: Option<&ApprovedUser>,
    ) -> Result<Arc<PreparedBindConfig>, ConfigObservationError> {
        PreparedBindConfig::read_cached(
            cache,
            Some(&self.user),
            approved,
            self.root.path(),
            self.root.path(),
            &Harness::Opencode,
        )
    }
}

fn write_at(path: &Path, text: &str, modified: SystemTime) {
    use std::io::Write;
    let mut file = std::fs::File::create(path).unwrap();
    file.write_all(text.as_bytes()).unwrap();
    file.set_modified(modified).unwrap();
}

fn old_time() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000)
}

#[test]
fn unchanged_binds_parse_each_file_once() {
    let files = ConfigFiles::new();
    write_at(
        &files.user,
        r#"{"url_fetch_allow_private":true}"#,
        old_time(),
    );
    write_at(
        &files.project,
        r#"{"idle":{"checkout_hours":7}}"#,
        old_time(),
    );
    let mut cached = BindConfigCache::default();
    let mut uncached_parses = 0;
    for _ in 0..100 {
        let mut empty = BindConfigCache::default();
        files.bind(&mut empty, None).unwrap();
        uncached_parses += empty.file_parses();
        let bind = files.bind(&mut cached, None).unwrap();
        assert!(bind.resolved().url_fetch_allow_private);
        assert_eq!(bind.resolved().idle.checkout_hours, 7);
    }
    assert_eq!(uncached_parses, 200);
    assert_eq!(cached.file_parses(), 2);
}

#[test]
fn edits_are_observed_on_next_bind() {
    let files = ConfigFiles::new();
    let mtime = old_time();
    write_at(&files.project, r#"{"idle":{"checkout_hours":7}}"#, mtime);
    write_at(&files.user, r#"{"url_fetch_allow_private":true }"#, mtime);
    let mut cache = BindConfigCache::default();
    let first = files.bind(&mut cache, None).unwrap();
    assert_eq!(first.resolved().idle.checkout_hours, 7);
    assert!(first.resolved().url_fetch_allow_private);

    // Keeping both size and whole seconds unchanged isolates nanoseconds.
    #[cfg(unix)]
    let edited = mtime + Duration::from_nanos(123_456_789);
    #[cfg(not(unix))]
    let edited = mtime + Duration::from_secs(4);
    write_at(&files.project, r#"{"idle":{"checkout_hours":8}}"#, edited);
    write_at(&files.user, r#"{"url_fetch_allow_private":false}"#, edited);
    let next = files.bind(&mut cache, None).unwrap();
    assert_eq!(next.resolved().idle.checkout_hours, 8);
    assert!(!next.resolved().url_fetch_allow_private);
    assert_eq!(cache.file_parses(), 4);
}

#[test]
fn same_size_same_second_rewrite_cannot_confirm_old_bytes() {
    let files = ConfigFiles::new();
    let mtime = old_time();
    let mut cache = BindConfigCache {
        now: Some(mtime + Duration::from_millis(250)),
        ..Default::default()
    };
    write_at(&files.project, r#"{"idle":{"checkout_hours":7}}"#, mtime);
    assert_eq!(
        files
            .bind(&mut cache, None)
            .unwrap()
            .resolved()
            .idle
            .checkout_hours,
        7
    );
    let (before, identity) = file_identity::metadata(&files.project).unwrap();
    write_at(&files.project, r#"{"idle":{"checkout_hours":8}}"#, mtime);
    let (after, new_identity) = file_identity::metadata(&files.project).unwrap();
    assert_eq!(
        (after.len(), after.modified().unwrap(), new_identity),
        (before.len(), before.modified().unwrap(), identity)
    );
    cache.now = Some(mtime + Duration::from_millis(500));
    assert_eq!(
        files
            .bind(&mut cache, None)
            .unwrap()
            .resolved()
            .idle
            .checkout_hours,
        8
    );

    write_at(&files.project, r#"{"idle":{"checkout_hours":9}}"#, mtime);
    cache.now = Some(mtime + Duration::from_secs(3));
    for _ in 0..2 {
        assert_eq!(
            files
                .bind(&mut cache, None)
                .unwrap()
                .resolved()
                .idle
                .checkout_hours,
            9
        );
    }
    assert_eq!(cache.file_parses(), 3);
}

#[test]
fn atomic_replacement_invalidates_even_with_unchanged_size_and_mtime() {
    let files = ConfigFiles::new();
    let mut cache = BindConfigCache::default();
    write_at(
        &files.project,
        r#"{"idle":{"checkout_hours":7}}"#,
        old_time(),
    );
    files.bind(&mut cache, None).unwrap();
    crate::file_identity::replace_keeping_times(
        &files.project,
        br#"{"idle":{"checkout_hours":8}}"#,
    );
    assert_eq!(
        files
            .bind(&mut cache, None)
            .unwrap()
            .resolved()
            .idle
            .checkout_hours,
        8
    );
    assert_eq!(cache.file_parses(), 2);
}

#[test]
fn bad_file_is_reparsed_and_refused_until_fixed() {
    let files = ConfigFiles::new();
    let mut cache = BindConfigCache::default();
    write_at(&files.project, "{}", old_time());
    files.bind(&mut cache, None).unwrap();
    write_at(&files.project, "{ broken", old_time());
    for _ in 0..2 {
        let error = files.bind(&mut cache, None).unwrap_err();
        assert_eq!(error.code, "config_rejected");
        assert_eq!(
            error.to_string(),
            "configuration rejected: invalid_config_document"
        );
    }
    assert_eq!(cache.file_parses(), 3);
    write_at(&files.project, "{}", old_time());
    files.bind(&mut cache, None).unwrap();
    assert_eq!(cache.file_parses(), 4);
}

#[test]
fn next_bind_uses_changed_approved_settings_without_reading_either_local_file() {
    let files = ConfigFiles::new();
    write_at(&files.user, "{ ignored and invalid", old_time());
    write_at(&files.project, "{ also ignored and invalid", old_time());
    let tier = |doc: &str| ConfigTier {
        tier: "user".into(),
        source: "approved".into(),
        doc: doc.into(),
    };
    let view = ApprovedUser::new(tier(r#"{"url_fetch_allow_private":false}"#));
    let mut cache = BindConfigCache::default();
    assert!(
        !files
            .bind(&mut cache, Some(&view))
            .unwrap()
            .resolved()
            .url_fetch_allow_private
    );
    view.replace(tier(r#"{"url_fetch_allow_private":true}"#));
    assert!(
        files
            .bind(&mut cache, Some(&view))
            .unwrap()
            .resolved()
            .url_fetch_allow_private
    );
    assert_eq!(cache.file_parses(), 0);
}

#[test]
fn candidate_appearance_disappearance_and_root_release_expire_project_entries() {
    let files = ConfigFiles::new();
    write_at(&files.user, "{}", old_time());
    let legacy = files.root.path().join(".opencode/aft.jsonc");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    write_at(&legacy, r#"{"idle":{"checkout_hours":7}}"#, old_time());
    let mut cache = BindConfigCache::default();
    assert_eq!(
        files
            .bind(&mut cache, None)
            .unwrap()
            .resolved()
            .idle
            .checkout_hours,
        7
    );
    write_at(
        &files.project,
        r#"{"idle":{"checkout_hours":8}}"#,
        old_time(),
    );
    assert_eq!(
        files
            .bind(&mut cache, None)
            .unwrap()
            .resolved()
            .idle
            .checkout_hours,
        8
    );
    std::fs::remove_file(&files.project).unwrap();
    assert_eq!(
        files
            .bind(&mut cache, None)
            .unwrap()
            .resolved()
            .idle
            .checkout_hours,
        7
    );
    assert!(!cache.files.contains_key(&files.project));
    assert_eq!(cache.file_parses(), 3);
    cache.release_project(files.root.path());
    assert_eq!(
        cache.files.len(),
        1,
        "the connection keeps only the user entry"
    );
    files.bind(&mut cache, None).unwrap();
    assert_eq!(cache.file_parses(), 4);
}
