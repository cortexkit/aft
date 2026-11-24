use super::prepared::{unknown, ConfigObservationError, CONFIG_FILE_LIMIT};
use super::{read_bounded_config, ConfigTier, UserConfigMigration};
use crate::config_resolve::ParsedConfigDocument;
use crate::file_identity::{self, FileIdentity};
use std::collections::HashMap;
use std::fs::Permissions;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

#[derive(PartialEq, Eq)]
struct FileStamp {
    modified: SystemTime,
    size: u64,
    identity: FileIdentity,
    permissions: Permissions,
}

impl FileStamp {
    fn read(path: &Path, now: SystemTime) -> std::io::Result<Option<Self>> {
        let (meta, identity) = file_identity::metadata(path)?;
        if !meta.is_file() || identity == FileIdentity::Unavailable {
            return Ok(None);
        }
        let modified = meta.modified()?;
        // Neither reuse nor retain a recent read: a second write in the same
        // filesystem clock tick can change bytes without changing the stamp.
        if !now
            .duration_since(modified)
            .is_ok_and(|age| age > Duration::from_secs(2))
        {
            return Ok(None);
        }
        Ok(Some(Self {
            modified,
            size: meta.len(),
            identity,
            permissions: meta.permissions(),
        }))
    }
}

pub(super) struct ParsedFile {
    pub(super) tier: ConfigTier,
    pub(super) document: ParsedConfigDocument,
}

impl ParsedFile {
    pub(super) fn new(tier: ConfigTier) -> Arc<Self> {
        let document = ParsedConfigDocument::new(&tier);
        Arc::new(Self { tier, document })
    }
}

pub(super) struct ObservedFile {
    pub(super) parsed: Arc<ParsedFile>,
    stamp: Option<(PathBuf, FileStamp)>,
}

impl ObservedFile {
    pub(super) fn approved(tier: ConfigTier) -> Self {
        Self {
            parsed: ParsedFile::new(tier),
            stamp: None,
        }
    }
}

struct CachedFile {
    stamp: FileStamp,
    root: Option<PathBuf>,
    parsed: Arc<ParsedFile>,
}

/// Parsed config files observed by one daemon connection's bind loop. The
/// user file is connection-wide; project files are released with their root.
#[derive(Default)]
pub struct BindConfigCache {
    files: HashMap<PathBuf, CachedFile>,
    #[cfg(any(test, feature = "test-support"))]
    file_parses: usize,
    #[cfg(test)]
    now: Option<SystemTime>,
}

impl BindConfigCache {
    pub fn release_project(&mut self, root: &Path) {
        self.files
            .retain(|_, entry| entry.root.as_deref() != Some(root));
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn file_parses(&self) -> usize {
        self.file_parses
    }

    fn now(&self) -> SystemTime {
        #[cfg(test)]
        if let Some(now) = self.now {
            return now;
        }
        SystemTime::now()
    }

    pub(super) fn observe(
        &mut self,
        path: &Path,
        root: Option<&Path>,
    ) -> Result<(Option<ObservedFile>, Option<UserConfigMigration>), ConfigObservationError> {
        let now = self.now();
        let stamp = match FileStamp::read(path, now) {
            Ok(stamp) => stamp,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                self.files.remove(path);
                return Ok((None, None));
            }
            // Fall back to the existing bounded read for the refusal text if
            // metadata is not available or is less accessible than the file.
            Err(_) => None,
        };
        if let Some(entry) = self
            .files
            .get(path)
            .filter(|entry| Some(&entry.stamp) == stamp.as_ref() && entry.root.as_deref() == root)
        {
            return Ok((
                Some(ObservedFile {
                    parsed: entry.parsed.clone(),
                    stamp: None,
                }),
                None,
            ));
        }
        self.files.remove(path);
        let tier = if root.is_some() { "project" } else { "user" };
        let Some(mut doc) = read_file(path)? else {
            return Ok((None, None));
        };
        let migration = if tier == "user" {
            let migration =
                crate::config_fix::auto_migrate_user_config_bounded(path, CONFIG_FILE_LIMIT)
                    .map_err(|error| unknown(path, &error))?;
            let Some(after) = read_file(path)? else {
                return Ok((None, migration));
            };
            doc = after;
            migration
        } else {
            None
        };
        // Verify a miss against the path again before associating the loaded
        // bytes with the earlier stamp. An editor can rename over the file
        // while the open handle still refers to the original.
        let stamp = stamp.filter(|stamp| {
            migration.is_none() && FileStamp::read(path, now).ok().flatten().as_ref() == Some(stamp)
        });
        #[cfg(any(test, feature = "test-support"))]
        {
            self.file_parses += 1;
        }
        let parsed = ParsedFile::new(ConfigTier {
            tier: tier.into(),
            source: path.to_string_lossy().into_owned(),
            doc,
        });
        Ok((
            Some(ObservedFile {
                parsed,
                stamp: stamp.map(|stamp| (path.to_owned(), stamp)),
            }),
            migration,
        ))
    }

    // Publish only observations that passed bind config resolution. An invalid
    // document must never leave the last good parse in use under its path.
    pub(super) fn remember(&mut self, observation: ObservedFile, root: Option<&Path>) {
        if let Some((path, stamp)) = observation.stamp {
            self.files.insert(
                path,
                CachedFile {
                    stamp,
                    root: root.map(Path::to_owned),
                    parsed: observation.parsed,
                },
            );
        }
    }
}

fn read_file(path: &Path) -> Result<Option<String>, ConfigObservationError> {
    // Windows shares delete so editors can atomically replace an open file.
    let file = match crate::config::open_config_file(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unknown(path, &error)),
    };
    read_bounded_config(file, CONFIG_FILE_LIMIT)
        .map(Some)
        .map_err(|error| unknown(path, &error))
}

#[cfg(test)]
mod tests;
