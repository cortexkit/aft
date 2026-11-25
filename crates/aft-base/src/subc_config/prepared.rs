//! Trusted-local config observations retained across a bind's configure attempts.

use super::{bind_cache::ObservedFile, *};
use crate::config_resolve::resolve_parsed_config_for_harness;
use std::cell::RefCell;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static NEXT_SNAPSHOT_ID: AtomicU64 = AtomicU64::new(1);

pub(super) const CONFIG_FILE_LIMIT: usize = 1024 * 1024;

#[derive(Debug)]
pub struct ConfigObservationError {
    pub code: &'static str,
    detail: String,
}

impl std::fmt::Display for ConfigObservationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

#[derive(Clone, Debug)]
pub struct PreparedBindConfig {
    snapshot_id: u64,
    root: PathBuf,
    harness: Harness,
    user_path: Option<PathBuf>,
    approved_user: Option<ApprovedUser>,
    project_path: PathBuf,
    tiers: Vec<ConfigTier>,
    migration: Option<UserConfigMigration>,
    resolved: crate::config::Config,
}

impl PreparedBindConfig {
    #[cfg(any(test, feature = "test-support"))]
    pub fn read(
        user_path: Option<&Path>,
        root: &Path,
        harness: &Harness,
    ) -> Result<Arc<Self>, ConfigObservationError> {
        Self::read_with_approved(user_path, None, root, harness)
    }

    pub fn read_with_approved(
        user_path: Option<&Path>,
        approved_user: Option<&ApprovedUser>,
        root: &Path,
        harness: &Harness,
    ) -> Result<Arc<Self>, ConfigObservationError> {
        Self::read_cached(
            &mut BindConfigCache::default(),
            user_path,
            approved_user,
            root,
            root,
            harness,
        )
    }

    pub fn read_cached(
        cache: &mut BindConfigCache,
        user_path: Option<&Path>,
        approved_user: Option<&ApprovedUser>,
        root: &Path,
        root_key: &Path,
        harness: &Harness,
    ) -> Result<Arc<Self>, ConfigObservationError> {
        let (user, migration) = if let Some(user) = approved_user {
            (Some(ObservedFile::approved(user.snapshot())), None)
        } else if let Some(path) = user_path {
            cache.observe(path, None)?
        } else {
            (None, None)
        };
        let mut project_path = cortexkit_project_config_path(root);
        let mut project = None;
        if approved_user.is_none() {
            for candidate in super::project_config_candidates(root, Some(harness)) {
                if let (Some(file), _) = cache.observe(&candidate, Some(root_key))? {
                    project_path = candidate;
                    project = Some(file);
                    break;
                }
            }
        }
        let files = user.iter().chain(&project).map(|file| &file.parsed);
        let resolution = resolve_parsed_config_for_harness(
            files.clone().map(|file| (&file.tier, &file.document)),
            Some(harness),
        );
        if !resolution.errors.is_empty() {
            return Err(ConfigObservationError {
                code: "config_rejected",
                detail: format!("configuration rejected: {}", resolution.errors.join(", ")),
            });
        }
        let tiers = files.map(|file| file.tier.clone()).collect();
        if let Some(user) = user {
            cache.remember(user, None);
        }
        if let Some(project) = project {
            cache.remember(project, Some(root_key));
        }
        Ok(Arc::new(Self {
            snapshot_id: NEXT_SNAPSHOT_ID.fetch_add(1, Ordering::Relaxed),
            root: root.into(),
            harness: harness.clone(),
            user_path: user_path.map(Path::to_path_buf),
            approved_user: approved_user.cloned(),
            project_path,
            tiers,
            migration,
            resolved: resolution.config,
        }))
    }

    pub fn matches(&self, root: &Path, harness: &Harness) -> bool {
        self.root == root && &self.harness == harness
    }
    pub fn id(&self) -> u64 {
        self.snapshot_id
    }

    pub fn tiers(&self) -> &[ConfigTier] {
        &self.tiers
    }
    pub fn migration(&self) -> Option<UserConfigMigration> {
        self.migration.clone()
    }
    pub fn resolved(&self) -> &crate::config::Config {
        &self.resolved
    }
    pub fn sources(&self, config: &crate::config::Config) -> crate::config_sources::ConfigSources {
        let mut sources = crate::config_sources::ConfigSources::from_observed_paths(
            self.user_path.clone(),
            self.project_path.clone(),
            &self.tiers,
            config,
        );
        sources.prepared_bind_snapshot_id = Some(self.snapshot_id);
        if let Some(user) = &self.approved_user {
            sources.approved_user = Some(user.clone());
            sources.owns_user_watch = false;
            sources.user.path = None;
            sources.user.from_file = false;
            sources.project.path = None;
            sources.project.from_file = false;
        }
        sources
    }
    pub fn legacy_notice(&self) -> Option<String> {
        self.tiers
            .iter()
            .any(|tier| tier.tier == "project")
            .then(|| legacy_project_config_location_notice(&self.root, &self.project_path))
            .flatten()
    }
}

pub(super) fn unknown(path: &Path, error: &std::io::Error) -> ConfigObservationError {
    ConfigObservationError {
        code: "config_unknown",
        detail: format!(
            "could not observe configuration {}: {}",
            path.display(),
            error
        ),
    }
}

/// Bound I/O itself, including migration's locked reread, rather than reading
/// the entire file and checking its size after allocation.
pub fn read_bounded_config(reader: impl Read, limit: usize) -> std::io::Result<String> {
    let detection_limit = limit
        .checked_add(1)
        .ok_or_else(|| std::io::Error::other("invalid config byte limit"))?;
    let mut bytes = Vec::new();
    reader
        .take(detection_limit as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(std::io::Error::other("configuration exceeds byte limit"));
    }
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

thread_local! {
    static PREPARED: RefCell<Option<Arc<PreparedBindConfig>>> = const { RefCell::new(None) };
}

pub fn current_prepared_bind_config() -> Option<Arc<PreparedBindConfig>> {
    PREPARED.with(|slot| slot.borrow().clone())
}

pub fn with_prepared_bind_config<T>(
    prepared: Arc<PreparedBindConfig>,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<Arc<PreparedBindConfig>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            PREPARED.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(PREPARED.with(|slot| slot.replace(Some(prepared))));
    run()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn prepared_read_never_requests_bytes_beyond_limit_plus_one() {
        struct ReadFence {
            bytes: usize,
        }
        impl Read for &mut ReadFence {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                if out.is_empty() {
                    return Ok(0);
                }
                assert!(self.bytes < 17, "read beyond config limit+1");
                let count = out.len().min(17 - self.bytes);
                out[..count].fill(b' ');
                self.bytes += count;
                Ok(count)
            }
        }
        let mut reader = ReadFence { bytes: 0 };
        assert!(read_bounded_config(&mut reader, 16).is_err());
        assert_eq!(reader.bytes, 17);
    }

    #[test]
    fn prepared_migration_rereads_use_the_same_read_time_bound() {
        let source = include_str!("../config_fix.rs");
        let body = source
            .split_once("pub(crate) fn migrate_user_config_to(")
            .unwrap()
            .1
            .split_once("fn auto_migrate_user_config_with_reader(")
            .unwrap()
            .0;
        assert!(body.contains("read_bounded_config(file, limit)"));
        assert!(!body.contains("read_to_string"));
        assert!(!body.contains("read_to_end"));
    }

    #[test]
    fn prepared_config_uses_active_harness_and_stops_on_unknown_candidate() {
        let dir = tempfile::tempdir().unwrap();
        put(
            &dir.path().join(".pi/aft.jsonc"),
            r#"{"idle": {"checkout_hours": 7}}"#,
        );
        put(
            &dir.path().join(".opencode/aft.jsonc"),
            r#"{"idle": {"checkout_hours": 8}}"#,
        );
        assert_eq!(
            PreparedBindConfig::read(None, dir.path(), &Harness::Pi)
                .unwrap()
                .resolved()
                .idle
                .checkout_hours,
            7
        );
        assert_eq!(
            PreparedBindConfig::read(None, dir.path(), &Harness::Opencode)
                .unwrap()
                .resolved()
                .idle
                .checkout_hours,
            8
        );
        std::fs::create_dir_all(dir.path().join(".cortexkit/aft.jsonc")).unwrap();
        let error = PreparedBindConfig::read(None, dir.path(), &Harness::Pi).unwrap_err();
        assert_eq!(
            error.code, "config_unknown",
            "an unreadable canonical candidate must not select legacy config"
        );
    }

    #[test]
    fn prepared_config_bounds_reads_and_distinguishes_missing_from_invalid() {
        let dir = tempfile::tempdir().unwrap();
        assert!(PreparedBindConfig::read(None, dir.path(), &Harness::Pi)
            .unwrap()
            .tiers()
            .is_empty());
        let path = dir.path().join(".cortexkit/aft.jsonc");
        put(&path, &" ".repeat(CONFIG_FILE_LIMIT + 1));
        assert_eq!(
            PreparedBindConfig::read(None, dir.path(), &Harness::Pi)
                .unwrap_err()
                .code,
            "config_unknown"
        );
        put(&path, "{ invalid config");
        assert_eq!(
            PreparedBindConfig::read(None, dir.path(), &Harness::Pi)
                .unwrap_err()
                .code,
            "config_rejected"
        );
    }

    #[test]
    fn prepared_config_scope_restores_on_unwind() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = PreparedBindConfig::read(None, dir.path(), &Harness::Pi).unwrap();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_prepared_bind_config(snapshot, || panic!("scope unwind"))
        }));
        assert!(current_prepared_bind_config().is_none());
    }

    #[test]
    fn prepared_nested_scope_restores_the_previous_snapshot_after_unwind() {
        let dir = tempfile::tempdir().unwrap();
        let outer = PreparedBindConfig::read(None, dir.path(), &Harness::Pi).unwrap();
        let inner = PreparedBindConfig::read(None, dir.path(), &Harness::Opencode).unwrap();
        let expected = outer.id();
        with_prepared_bind_config(outer, || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                with_prepared_bind_config(inner, || panic!("nested scope unwind"))
            }));
            assert_eq!(current_prepared_bind_config().unwrap().id(), expected);
        });
        assert!(current_prepared_bind_config().is_none());
    }
}
