//! Checkpoints carry their historical bytes and a separate cut-time baseline.

use super::*;
use crate::agent_data::{
    DropCounts, ExpectedState, FileRecord, ImportedGuard, ProjectRegistry, SnapshotGroup,
};

impl CheckpointStore {
    pub fn set_portable_registry(&mut self, registry: Arc<dyn ProjectRegistry>) {
        self.portable_registry = Some(registry);
    }

    pub fn export_portable_checkpoints(
        &mut self,
        session: &str,
        registry: &dyn ProjectRegistry,
        drops: &mut DropCounts,
    ) -> Result<Vec<SnapshotGroup>, String> {
        let _lock = self
            .acquire_mutation_lock()
            .map_err(|error| error.to_string())?;
        // Check stored lengths before hydration, which otherwise reads every blob.
        if let Some(dir) = self.durable_session_dir(session) {
            let mut count = 0usize;
            let mut bytes = 0u64;
            let mut names = 0usize;
            match fs::read_dir(dir) {
                Ok(items) => {
                    for item in items {
                        names += 1;
                        if names > MAX_NAMED_CHECKPOINTS_PER_SESSION + 4 {
                            return Err("checkpoint count limit".into());
                        }
                        let item = item.map_err(|error| error.to_string())?;
                        if !item
                            .file_type()
                            .map_err(|error| error.to_string())?
                            .is_dir()
                        {
                            continue;
                        }
                        let meta_path = item.path().join("meta.json");
                        if fs::metadata(&meta_path)
                            .map_err(|error| error.to_string())?
                            .len()
                            > crate::agent_data::snapshot::SESSION_LIMIT
                        {
                            return Err("session_limit".into());
                        }
                        let meta: DiskCheckpointMeta = serde_json::from_slice(
                            &fs::read(&meta_path).map_err(|error| error.to_string())?,
                        )
                        .map_err(|error| error.to_string())?;
                        for file in &meta.files {
                            count += 1;
                            if count > crate::agent_data::snapshot::ENTRY_LIMIT {
                                return Err("entry_limit".into());
                            }
                            if !is_safe_blob_name(&file.blob) {
                                return Err("invalid checkpoint blob".into());
                            }
                            bytes = bytes
                                .checked_add(
                                    fs::symlink_metadata(item.path().join(&file.blob))
                                        .map_err(|error| error.to_string())?
                                        .len(),
                                )
                                .ok_or("session_limit")?;
                            if bytes > crate::agent_data::snapshot::SESSION_LIMIT {
                                return Err("session_limit".into());
                            }
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        self.hydrate_session_locked(session)
            .map_err(|error| error.to_string())?;
        let mut groups = Vec::new();
        let mut bytes = 0u64;
        let mut entries = 0usize;
        if let Some(checkpoints) = self.checkpoints.get(session) {
            for checkpoint in checkpoints.values() {
                if checkpoint
                    .created_at
                    .saturating_add(NAMED_CHECKPOINT_RETENTION_SECS)
                    <= current_timestamp()
                {
                    continue;
                }
                let mut files = Vec::new();
                let mut reason = None;
                for (path, file) in &checkpoint.file_contents {
                    entries += 1;
                    if entries > crate::agent_data::snapshot::ENTRY_LIMIT {
                        return Err("entry_limit".into());
                    }
                    let CheckpointFileKind::Regular { bytes: content } = &file.kind else {
                        reason = Some("unsupported_kind");
                        break;
                    };
                    bytes = bytes
                        .checked_add(content.len() as u64)
                        .ok_or("session_limit")?;
                    if bytes > crate::agent_data::snapshot::SESSION_LIMIT {
                        return Err("session_limit".into());
                    }
                    let expected = match ExpectedState::capture(path) {
                        Ok(state) => state,
                        Err(_) => {
                            reason = Some("unsupported_kind");
                            break;
                        }
                    };
                    let reply = registry.resolve(path)?;
                    let key = match crate::agent_data::registry::export_path(path, &reply) {
                        Ok(key) => key,
                        Err(drop) => {
                            reason = Some(drop.reason());
                            break;
                        }
                    };
                    files.push(FileRecord {
                        path: key,
                        id: format!("{}:{}", checkpoint.name, files.len()),
                        order: checkpoint.created_order as u128,
                        timestamp: checkpoint.created_at,
                        mode: file.mode,
                        before: Some(content.to_vec()),
                        expected,
                    });
                }
                if let Some(reason) = reason {
                    drops.add(reason, checkpoint.file_contents.len() as u64);
                } else {
                    groups.push(SnapshotGroup::Checkpoint {
                        name: checkpoint.name.clone(),
                        created_at: checkpoint.created_at,
                        created_order: checkpoint.created_order,
                        files,
                    });
                }
            }
        }
        groups.sort_by_key(|group| match group {
            SnapshotGroup::Checkpoint { created_order, .. } => *created_order,
            _ => 0,
        });
        Ok(groups)
    }

    pub fn install_portable_checkpoints(
        &mut self,
        session: &str,
        groups: &[(SnapshotGroup, Vec<PathBuf>)],
    ) -> Result<(), String> {
        let _lock = self
            .acquire_mutation_lock()
            .map_err(|error| error.to_string())?;
        let mut imported = HashMap::new();
        for (group, paths) in groups {
            let SnapshotGroup::Checkpoint {
                name,
                created_at,
                created_order,
                files,
            } = group
            else {
                continue;
            };
            validate_checkpoint_name(name).map_err(|error| error.to_string())?;
            if created_at.saturating_add(NAMED_CHECKPOINT_RETENTION_SECS) <= current_timestamp() {
                continue;
            }
            if files.len() != paths.len() {
                return Err("invalid path projection".into());
            }
            let mut file_contents = HashMap::new();
            let mut imported_guards = HashMap::new();
            for (file, path) in files.iter().zip(paths) {
                let bytes = file.before.clone().ok_or("checkpoint missing content")?;
                file_contents.insert(
                    path.clone(),
                    CheckpointFile {
                        metadata: None,
                        mode: file.mode,
                        kind: CheckpointFileKind::Regular {
                            bytes: bytes.into(),
                        },
                    },
                );
                imported_guards.insert(
                    path.clone(),
                    ImportedGuard {
                        path: file.path.clone(),
                        expected: file.expected.clone(),
                    },
                );
            }
            let checkpoint = Checkpoint {
                name: name.clone(),
                file_contents,
                imported_guards,
                created_at: *created_at,
                created_order: *created_order,
                pending_purge: Vec::new(),
            };
            if imported.insert(name.clone(), checkpoint).is_some() {
                return Err("duplicate checkpoint".into());
            }
        }
        if imported.len() > MAX_NAMED_CHECKPOINTS_PER_SESSION {
            return Err("checkpoint count limit".into());
        }
        for checkpoint in imported.values() {
            self.persist_checkpoint_locked(session, checkpoint)
                .map_err(|error| error.to_string())?;
        }
        self.checkpoints.insert(session.to_owned(), imported);
        Ok(())
    }
}
