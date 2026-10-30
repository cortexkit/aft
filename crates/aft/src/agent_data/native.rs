//! Native logical-store adapter. The participant's host barrier must be held
//! across these calls and across the corresponding ordinary mutation paths.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::participant::{ResolvedSession, SessionStores};
use super::{DropCounts, ProjectRegistry, SessionSnapshot, SnapshotGroup};
use crate::backup::BackupStore;
use crate::checkpoint::CheckpointStore;

pub struct NativeStores {
    pub storage_root: PathBuf,
    pub registry: Arc<dyn ProjectRegistry>,
    /// Supplied by the process that owns aft.db; this adapter never opens it.
    pub database: Option<Arc<Mutex<crate::db::TrackedConnection>>>,
}

impl NativeStores {
    fn stores(&self, session: &ResolvedSession) -> (BackupStore, CheckpointStore) {
        let mut backups = BackupStore::new();
        backups.set_storage_dir_for_harness(
            self.storage_root.clone(),
            session.storage_harness.clone(),
            0,
        );
        backups.set_portable_registry(self.registry.clone());
        if let Some(database) = &self.database {
            backups.set_db_pool(database.clone());
            backups.set_db_harness(session.storage_harness.clone());
            backups.set_db_project_key(crate::path_identity::project_scope_key(
                &session.project_root,
            ));
        }
        let mut checkpoints = CheckpointStore::new();
        checkpoints.set_storage_dir_for_harness(
            self.storage_root.clone(),
            session.storage_harness.clone(),
        );
        checkpoints.set_portable_registry(self.registry.clone());
        (backups, checkpoints)
    }
}

impl SessionStores for NativeStores {
    fn generation(&self, session: &ResolvedSession) -> Result<String, String> {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let mut hash = Sha256::new();
        let harness = self
            .storage_root
            .join(session.storage_harness.storage_segment());
        let session_hash = crate::backup::hash_session(&session.session);
        let mut files = Vec::new();
        let mut examined = 0usize;
        let mut size = 0u64;
        for domain in ["backups", "checkpoints"] {
            let root = harness.join(domain).join(&session_hash);
            let mut directories = vec![root.clone()];
            while let Some(dir) = directories.pop() {
                let entries = match std::fs::read_dir(dir) {
                    Ok(entries) => entries,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.to_string()),
                };
                for item in entries {
                    examined += 1;
                    if examined > super::snapshot::ENTRY_LIMIT * 4 + 64 {
                        return Err("entry_limit".into());
                    }
                    let item = item.map_err(|error| error.to_string())?;
                    let kind = item.file_type().map_err(|error| error.to_string())?;
                    if kind.is_symlink() {
                        return Err("unsafe history storage link".into());
                    }
                    if kind.is_dir() {
                        directories.push(item.path());
                        continue;
                    }
                    if !kind.is_file() {
                        return Err("unsafe history storage kind".into());
                    }
                    let name = item.file_name();
                    let name = name.to_string_lossy();
                    if name == "session.json" || name.ends_with(".lock") {
                        continue;
                    }
                    size = size
                        .checked_add(item.metadata().map_err(|error| error.to_string())?.len())
                        .ok_or("session_limit")?;
                    if size > super::snapshot::SESSION_LIMIT {
                        return Err("session_limit".into());
                    }
                    files.push(item.path());
                }
            }
        }
        files.sort();
        for path in files {
            let relative = path
                .strip_prefix(&harness)
                .map_err(|error| error.to_string())?;
            let name = relative.to_str().ok_or("non-UTF8 history name")?.as_bytes();
            hash.update((name.len() as u64).to_le_bytes());
            hash.update(name);
            let mut file = std::fs::File::open(&path).map_err(|error| error.to_string())?;
            hash.update(
                file.metadata()
                    .map_err(|error| error.to_string())?
                    .len()
                    .to_le_bytes(),
            );
            let mut chunk = [0u8; 65536];
            loop {
                let count = file.read(&mut chunk).map_err(|error| error.to_string())?;
                if count == 0 {
                    break;
                }
                hash.update(&chunk[..count]);
            }
        }
        Ok(format!("{:x}", hash.finalize()))
    }

    fn snapshot(
        &self,
        session: &ResolvedSession,
        drops: &mut DropCounts,
    ) -> Result<SessionSnapshot, String> {
        if matches!(session.session.as_str(), "" | "__default__" | "unbound") {
            return Err("session ownership is not bound".into());
        }
        let (mut backups, mut checkpoints) = self.stores(session);
        let mut groups =
            backups.export_portable_undo(&session.session, self.registry.as_ref(), drops)?;
        groups.extend(checkpoints.export_portable_checkpoints(
            &session.session,
            self.registry.as_ref(),
            drops,
        )?);
        Ok(SessionSnapshot { groups })
    }

    fn publish(
        &self,
        session: &ResolvedSession,
        projection: &[(SnapshotGroup, Vec<PathBuf>)],
    ) -> Result<(), String> {
        let (mut backups, mut checkpoints) = self.stores(session);
        backups.install_portable_undo(&session.session, projection)?;
        checkpoints.install_portable_checkpoints(&session.session, projection)?;
        Ok(())
    }
}
