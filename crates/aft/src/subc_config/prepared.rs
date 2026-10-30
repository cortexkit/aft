//! Trusted-local config observations retained across a bind's configure attempts.

use super::*;
use std::cell::RefCell;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static NEXT_SNAPSHOT_ID: AtomicU64 = AtomicU64::new(1);

const CONFIG_FILE_LIMIT: usize = 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) enum ConfigObservation {
    Present(ConfigTier),
    Missing,
}

#[derive(Debug)]
pub(crate) struct ConfigObservationError {
    pub(crate) code: &'static str,
    detail: String,
}

impl std::fmt::Display for ConfigObservationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedBindConfig {
    snapshot_id: u64,
    root: PathBuf,
    harness: Harness,
    user_path: Option<PathBuf>,
    project_path: PathBuf,
    tiers: Vec<ConfigTier>,
    migration: Option<UserConfigMigration>,
    resolved: crate::config::Config,
}

impl PreparedBindConfig {
    pub(crate) fn read(
        user_path: Option<&Path>,
        root: &Path,
        harness: &Harness,
    ) -> Result<Arc<Self>, ConfigObservationError> {
        let mut observations = Vec::new();
        let mut migration = None;
        if let Some(path) = user_path {
            // Distinguish a missing user file from other read errors before
            // migration. Migration's own rereads use the same byte limit.
            let first = observe(path, "user")?;
            if matches!(first, ConfigObservation::Present(_)) {
                migration =
                    crate::config_fix::auto_migrate_user_config_bounded(path, CONFIG_FILE_LIMIT)
                        .map_err(|error| unknown(path, &error))?;
                observations.push(observe(path, "user")?);
            } else {
                observations.push(first);
            }
        }
        let mut project_path = cortexkit_project_config_path(root);
        for candidate in super::project_config_candidates(root, Some(harness)) {
            let observation = observe(&candidate, "project")?;
            let present = matches!(observation, ConfigObservation::Present(_));
            if present {
                project_path = candidate;
                observations.push(observation);
                break;
            }
            observations.push(observation);
        }
        let tiers: Vec<_> = observations
            .iter()
            .filter_map(|observation| match observation {
                ConfigObservation::Present(tier) => Some(tier.clone()),
                ConfigObservation::Missing => None,
            })
            .collect();
        let resolution = crate::config_resolve::resolve_config_for_harness(&tiers, Some(harness));
        if !resolution.errors.is_empty() {
            return Err(ConfigObservationError {
                code: "config_rejected",
                detail: format!("configuration rejected: {}", resolution.errors.join(", ")),
            });
        }
        Ok(Arc::new(Self {
            snapshot_id: NEXT_SNAPSHOT_ID.fetch_add(1, Ordering::Relaxed),
            root: root.into(),
            harness: harness.clone(),
            user_path: user_path.map(Path::to_path_buf),
            project_path,
            tiers,
            migration,
            resolved: resolution.config,
        }))
    }

    pub(crate) fn matches(&self, root: &Path, harness: &Harness) -> bool {
        self.root == root && &self.harness == harness
    }
    pub(crate) fn id(&self) -> u64 {
        self.snapshot_id
    }

    pub(crate) fn tiers(&self) -> &[ConfigTier] {
        &self.tiers
    }
    pub(crate) fn migration(&self) -> Option<UserConfigMigration> {
        self.migration.clone()
    }
    pub(crate) fn resolved(&self) -> &crate::config::Config {
        &self.resolved
    }
    pub(crate) fn sources(
        &self,
        config: &crate::config::Config,
    ) -> crate::config_live::ConfigSources {
        let mut sources = crate::config_live::ConfigSources::from_observed_paths(
            self.user_path.clone(),
            self.project_path.clone(),
            &self.tiers,
            config,
        );
        sources.prepared_bind_snapshot_id = Some(self.snapshot_id);
        sources
    }
    pub(crate) fn legacy_notice(&self) -> Option<String> {
        self.tiers
            .iter()
            .any(|tier| tier.tier == "project")
            .then(|| legacy_project_config_location_notice(&self.root, &self.project_path))
            .flatten()
    }
}

fn unknown(path: &Path, error: &std::io::Error) -> ConfigObservationError {
    ConfigObservationError {
        code: "config_unknown",
        detail: format!(
            "could not observe configuration {}: {}",
            path.display(),
            error
        ),
    }
}

fn observe(path: &Path, tier: &str) -> Result<ConfigObservation, ConfigObservationError> {
    // Shared open: on Windows this handle shares delete, so a tool may replace
    // the file while the bind reads it instead of being refused access.
    let file = match crate::config::open_config_file(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ConfigObservation::Missing)
        }
        Err(error) => return Err(unknown(path, &error)),
    };
    let doc =
        read_bounded_config(file, CONFIG_FILE_LIMIT).map_err(|error| unknown(path, &error))?;
    Ok(ConfigObservation::Present(ConfigTier {
        tier: tier.into(),
        source: path.to_string_lossy().into_owned(),
        doc,
    }))
}

/// Bound I/O itself, including migration's locked reread, rather than reading
/// the entire file and checking its size after allocation.
pub(crate) fn read_bounded_config(reader: impl Read, limit: usize) -> std::io::Result<String> {
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

pub(crate) fn current_prepared_bind_config() -> Option<Arc<PreparedBindConfig>> {
    PREPARED.with(|slot| slot.borrow().clone())
}

pub(crate) fn with_prepared_bind_config<T>(
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
            .split_once("pub(crate) fn auto_migrate_user_config_bounded(")
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
            r#"{"callgraph_chunk_size":7}"#,
        );
        put(
            &dir.path().join(".opencode/aft.jsonc"),
            r#"{"callgraph_chunk_size":8}"#,
        );
        assert_eq!(
            PreparedBindConfig::read(None, dir.path(), &Harness::Pi)
                .unwrap()
                .resolved()
                .callgraph_chunk_size,
            7
        );
        assert_eq!(
            PreparedBindConfig::read(None, dir.path(), &Harness::Opencode)
                .unwrap()
                .resolved()
                .callgraph_chunk_size,
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
