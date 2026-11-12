//! Durable undo-stack discovery, and the tracked-file count that status pushes
//! read without holding the `BackupStore` mutex.
//!
//! A session's tracked files are the union of the stacks this process holds in
//! memory and every loadable, non-empty stack whose `meta.json` sits in the
//! session's backup directory. [`discover_session_stacks`] is the one
//! implementation of that directory pass. Restore points and explicit status
//! run it with a fresh read of every `meta.json`.
//!
//! A status push runs often (about once a second per busy root) and only needs
//! the count. Reading and parsing every `meta.json` on each push costs
//! milliseconds per hundred stacks, and the push used to do it while holding the
//! store mutex that every edit needs. [`StatusInventory`] copies the resident
//! keys under that mutex and runs the pass after the caller drops it. The pass
//! keeps what it parsed from each `meta.json` and re-reads a file only when its
//! stamp (file identity, size and modification time) differs from the stamp the
//! parse was taken from. Writers always publish `meta.json` by renaming a fresh
//! temporary file over it, so every rewrite changes the file identity even when
//! the size and modification time stay the same. Every other check that decides
//! whether a stack counts (live-database aliasing, format and reader-floor
//! gates, path identity, entry count) still runs on every pass, so the count is
//! the one the full discovery would report.

use super::{
    check_backup_meta_version, is_loadable_backup_path, meta_entry_count, refuse_live_history_file,
    AftError, BackupIoBudget, DiskMeta,
};
use crate::file_identity::FileIdentity;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
#[cfg(test)]
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::SystemTime;

/// How far one discovery pass may go before it gives up. A pass that stops
/// early fails instead of returning the stacks it saw so far.
pub(super) struct DiscoveryLimits {
    pub(super) budget: BackupIoBudget,
    pub(super) entry_limit: usize,
    #[cfg(test)]
    pub(super) examined: Arc<AtomicU64>,
}

/// The fields of one `meta.json` that discovery decides with.
#[derive(Debug, Clone)]
struct ParsedStackMeta {
    schema_version: Option<u64>,
    path: Option<String>,
    count: Option<usize>,
}

impl ParsedStackMeta {
    /// The caller has already refused live-database aliases for `meta_path`.
    fn read(meta_path: &Path) -> Result<Self, AftError> {
        let io_error = |message: String| AftError::IoError {
            path: meta_path.display().to_string(),
            message,
        };
        let content =
            std::fs::read_to_string(meta_path).map_err(|error| io_error(error.to_string()))?;
        let meta = serde_json::from_str::<serde_json::Value>(&content)
            .map_err(|error| io_error(error.to_string()))?;
        Ok(Self {
            schema_version: meta
                .get("schema_version")
                .and_then(serde_json::Value::as_u64),
            path: meta
                .get("path")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            count: meta_entry_count(&meta),
        })
    }
}

/// Identifies one published version of a `meta.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MetaStamp {
    identity: FileIdentity,
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Debug)]
struct CachedStackMeta {
    stamp: MetaStamp,
    meta: ParsedStackMeta,
}

/// Parsed `meta.json` fields for one session directory, keyed by stack
/// directory. It holds only entries seen by the latest pass, so it never grows
/// past the directory's own size.
#[derive(Debug, Default)]
pub(super) struct SessionMetaCache {
    dir: PathBuf,
    stacks: HashMap<PathBuf, CachedStackMeta>,
    #[cfg(test)]
    parses: u64,
}

#[derive(Debug, Default)]
struct CacheState {
    /// Bumped whenever memory is released, so a pass that started before the
    /// release does not put its cache back afterwards.
    epoch: u64,
    sessions: HashMap<String, SessionMetaCache>,
}

/// Status-push metadata caches for every session of one store. It has its own
/// lock, which is held only to take a session's cache out or put it back, never
/// during a directory pass.
#[derive(Debug, Default)]
pub(super) struct StatusMetaCaches {
    state: Mutex<CacheState>,
    #[cfg(test)]
    parses: AtomicU64,
}

impl StatusMetaCaches {
    pub(super) fn take(&self, session: &str, dir: &Path) -> (u64, SessionMetaCache) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cache = state
            .sessions
            .remove(session)
            .filter(|cache| cache.dir == dir)
            .unwrap_or_else(|| SessionMetaCache {
                dir: dir.to_path_buf(),
                ..SessionMetaCache::default()
            });
        (state.epoch, cache)
    }

    pub(super) fn put_back(&self, session: &str, epoch: u64, cache: SessionMetaCache) {
        #[cfg(test)]
        let cache = {
            let mut cache = cache;
            self.parses
                .fetch_add(std::mem::take(&mut cache.parses), Ordering::Relaxed);
            cache
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.epoch == epoch {
            state.sessions.insert(session.to_string(), cache);
        }
    }

    pub(super) fn release_session(&self, session: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.epoch += 1;
        state.sessions.remove(session);
    }

    pub(super) fn release_all(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.epoch += 1;
        state.sessions.clear();
    }

    #[cfg(test)]
    pub(super) fn parses(&self) -> u64 {
        self.parses.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(super) fn cached_stacks(&self, session: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .sessions
            .get(session)
            .map_or(0, |cache| cache.stacks.len())
    }
}

/// Everything a status push needs to count one session's tracked files,
/// copied out of the store so the directory pass runs without its mutex.
pub(crate) struct StatusInventory {
    pub(super) session: String,
    pub(super) resident: HashSet<PathBuf>,
    pub(super) session_dir: Option<PathBuf>,
    pub(super) limits: DiscoveryLimits,
    pub(super) caches: std::sync::Arc<StatusMetaCaches>,
}

impl StatusInventory {
    /// Equal to `BackupStore::tracked_files(session).len()` at the moment the
    /// inventory was taken, except that a stack written or removed by this
    /// process while the pass runs may already be reflected in the directory
    /// half: a stack removed after the copy still counts once through the
    /// resident keys, and the next push no longer sees it. As with
    /// `tracked_files`, a failed pass is logged and only the resident keys
    /// count.
    pub(crate) fn tracked_file_count(self) -> usize {
        let mut files = self.resident;
        let discovered = match &self.session_dir {
            None => Ok(HashMap::new()),
            Some(dir) => {
                let (epoch, mut cache) = self.caches.take(&self.session, dir);
                let result = discover_session_stacks(dir, &self.limits, Some(&mut cache));
                self.caches.put_back(&self.session, epoch, cache);
                result
            }
        };
        match discovered {
            Ok(stacks) => files.extend(stacks.into_keys()),
            Err(error) => crate::slog_warn!(
                "backup path discovery failed for session {}: {error}",
                self.session
            ),
        }
        files.len()
    }
}

/// Discover every loadable, non-empty stack in `session_dir`. Any entry that
/// cannot be read or understood fails the whole pass: an incomplete scan must
/// never certify that history is absent. With `cache`, unchanged `meta.json`
/// files are not read again, and the cache afterwards holds exactly the stacks
/// this pass reached.
pub(super) fn discover_session_stacks(
    session_dir: &Path,
    limits: &DiscoveryLimits,
    mut cache: Option<&mut SessionMetaCache>,
) -> Result<HashMap<PathBuf, DiskMeta>, AftError> {
    let mut previous = cache
        .as_deref_mut()
        .map(|cache| std::mem::take(&mut cache.stacks))
        .unwrap_or_default();
    let mut reached = HashMap::new();
    let result = discover_into(session_dir, limits, &mut previous, &mut reached, &mut cache);
    if let Some(cache) = cache {
        cache.stacks = reached;
    }
    result
}

fn discover_into(
    session_dir: &Path,
    limits: &DiscoveryLimits,
    previous: &mut HashMap<PathBuf, CachedStackMeta>,
    reached: &mut HashMap<PathBuf, CachedStackMeta>,
    cache: &mut Option<&mut SessionMetaCache>,
) -> Result<HashMap<PathBuf, DiskMeta>, AftError> {
    if !session_dir.exists() {
        return Ok(HashMap::new());
    }
    let session_error = |message: String| AftError::IoError {
        path: session_dir.display().to_string(),
        message,
    };
    let mut path_dirs =
        std::fs::read_dir(session_dir).map_err(|error| session_error(error.to_string()))?;
    let mut per_session = HashMap::new();
    let mut examined = 0;
    loop {
        // Check before advancing ReadDir, including entries that will be
        // ignored. An incomplete scan must never certify absent history.
        limits.budget.check(session_dir)?;
        if examined >= limits.entry_limit {
            return Err(session_error(format!(
                "backup stack discovery reached its {}-entry limit; use per-file undo or retry",
                limits.entry_limit
            )));
        }
        let Some(path_entry) = path_dirs.next() else {
            break;
        };
        examined += 1;
        #[cfg(test)]
        limits.examined.fetch_add(1, Ordering::Relaxed);
        let path_entry = path_entry.map_err(|error| session_error(error.to_string()))?;
        let path_dir = path_entry.path();
        let meta_path = path_dir.join("meta.json");
        // A stack is a directory holding `meta.json`. The stat fails for any
        // other entry, including files such as `session.json` and directories
        // without metadata such as `.locks`, which therefore hold no stack.
        let Ok((stat, identity)) = crate::file_identity::metadata(&meta_path) else {
            continue;
        };
        let meta_error = |message: String| AftError::IoError {
            path: meta_path.display().to_string(),
            message,
        };
        refuse_live_history_file(&meta_path).map_err(|error| meta_error(error.to_string()))?;
        let meta = match cache.as_deref_mut() {
            None => ParsedStackMeta::read(&meta_path)?,
            Some(cache) => {
                let stamp = MetaStamp {
                    identity,
                    len: stat.len(),
                    modified: stat.modified().ok(),
                };
                let cached = previous
                    .remove(&path_dir)
                    .filter(|cached| cached.stamp == stamp);
                let cached = match cached {
                    Some(cached) => cached,
                    None => {
                        #[cfg(test)]
                        {
                            cache.parses += 1;
                        }
                        #[cfg(not(test))]
                        let _ = cache;
                        CachedStackMeta {
                            stamp,
                            meta: ParsedStackMeta::read(&meta_path)?,
                        }
                    }
                };
                let meta = cached.meta.clone();
                reached.insert(path_dir.clone(), cached);
                meta
            }
        };
        check_backup_meta_version(&meta_path, meta.schema_version)
            .map_err(|refusal| meta_error(refusal.to_string()))?;
        let path_str = meta
            .path
            .ok_or_else(|| meta_error("backup meta missing path".to_string()))?;
        let key = PathBuf::from(path_str);
        if !is_loadable_backup_path(&key, &path_dir) {
            continue;
        }
        let count = meta
            .count
            .ok_or_else(|| meta_error("backup meta missing entry count".to_string()))?;
        if count > 0 {
            per_session.insert(
                key,
                DiskMeta {
                    dir: path_dir,
                    count,
                },
            );
        }
    }
    Ok(per_session)
}
