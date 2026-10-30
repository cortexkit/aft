//! Logical undo-store access; never copy the path-hashed storage directories.

use super::*;
use crate::agent_data::registry::export_path;
use crate::agent_data::{
    DropCounts, ExpectedState, FileRecord, ImportedGuard, ProjectRegistry, SnapshotGroup,
};

impl BackupStore {
    pub fn set_portable_registry(&mut self, registry: Arc<dyn ProjectRegistry>) {
        self.portable_registry = Some(registry);
    }

    pub fn export_portable_undo(
        &mut self,
        session: &str,
        registry: &dyn ProjectRegistry,
        drops: &mut DropCounts,
    ) -> Result<Vec<SnapshotGroup>, String> {
        let mut keys = HashSet::new();
        for key in self
            .entries
            .get(session)
            .into_iter()
            .flat_map(|files| files.keys())
            .chain(
                self.disk_index
                    .get(session)
                    .into_iter()
                    .flat_map(|files| files.keys()),
            )
        {
            keys.insert(key.clone());
            if keys.len() > crate::agent_data::snapshot::ENTRY_LIMIT {
                return Err("entry_limit".into());
            }
        }
        let mut disk_entries = 0usize;
        if let Some(dir) = self.session_dir(session) {
            match std::fs::read_dir(dir) {
                Ok(entries) => {
                    let mut examined = 0;
                    for item in entries {
                        examined += 1;
                        if examined > crate::agent_data::snapshot::ENTRY_LIMIT + 4 {
                            return Err("entry_limit".into());
                        }
                        let item = item.map_err(|error| error.to_string())?;
                        if item.file_name() == ".locks" {
                            continue;
                        }
                        if !item
                            .file_type()
                            .map_err(|error| error.to_string())?
                            .is_dir()
                        {
                            continue;
                        }
                        let meta_path = item.path().join("meta.json");
                        let size = std::fs::metadata(&meta_path)
                            .map_err(|error| error.to_string())?
                            .len();
                        if size > crate::agent_data::snapshot::SESSION_LIMIT {
                            return Err("session_limit".into());
                        }
                        let meta: serde_json::Value = serde_json::from_slice(
                            &std::fs::read(&meta_path).map_err(|error| error.to_string())?,
                        )
                        .map_err(|error| error.to_string())?;
                        disk_entries = disk_entries
                            .checked_add(
                                meta_entry_count(&meta).ok_or("invalid backup entry count")?,
                            )
                            .ok_or("entry_limit")?;
                        if disk_entries > crate::agent_data::snapshot::ENTRY_LIMIT {
                            return Err("entry_limit".into());
                        }
                        let path = meta
                            .get("path")
                            .and_then(|value| value.as_str())
                            .ok_or("invalid backup path")?;
                        let key = canonicalize_key(Path::new(path));
                        if Self::path_hash(&key) != item.file_name().to_string_lossy() {
                            return Err("invalid backup path hash".into());
                        }
                        keys.insert(key);
                        if keys.len() > crate::agent_data::snapshot::ENTRY_LIMIT {
                            return Err("entry_limit".into());
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        let mut keys: Vec<_> = keys.into_iter().collect();
        keys.sort();
        let _locks = self
            .acquire_stack_disk_locks(session, &keys)
            .map_err(|error| error.to_string())?;
        let mut groups: std::collections::BTreeMap<String, Vec<(PathBuf, BackupEntry)>> =
            std::collections::BTreeMap::new();
        let mut count = 0usize;
        let mut bytes = 0u64;
        for key in keys {
            self.ensure_stack_hydrated_locked(session, &key)
                .map_err(|error| error.to_string())?;
            if let Some(stack) = self.entries.get(session).and_then(|files| files.get(&key)) {
                for entry in stack {
                    count += 1;
                    if count > crate::agent_data::snapshot::ENTRY_LIMIT {
                        return Err("entry_limit".into());
                    }
                    bytes = bytes
                        .checked_add(entry.content_size())
                        .ok_or("session_limit")?;
                    if bytes > crate::agent_data::snapshot::SESSION_LIMIT {
                        return Err("session_limit".into());
                    }
                    let id = entry
                        .op_id
                        .clone()
                        .unwrap_or_else(|| entry.backup_id.clone());
                    groups
                        .entry(id)
                        .or_default()
                        .push((key.clone(), entry.clone()));
                }
            }
        }
        let mut portable = Vec::new();
        for (id, entries) in groups {
            let count = entries.len() as u64;
            let mut files = Vec::new();
            let mut reason = None;
            for (path, entry) in entries {
                if !matches!(
                    entry.kind,
                    BackupEntryKind::Content | BackupEntryKind::Tombstone
                ) || entry.hardlink_detached
                {
                    reason = Some("unsupported_kind");
                    break;
                }
                let expected = match &entry.post_state {
                    Some(PathFingerprint::Absent) => ExpectedState::Absent,
                    Some(PathFingerprint::Content(hash)) => ExpectedState::File(hash.clone()),
                    _ => {
                        reason = Some("missing_postimage");
                        break;
                    }
                };
                let reply = registry.resolve(&path)?;
                let key = match export_path(&path, &reply) {
                    Ok(key) => key,
                    Err(drop) => {
                        reason = Some(drop.reason());
                        break;
                    }
                };
                let before = if entry.kind == BackupEntryKind::Content {
                    Some(
                        entry
                            .content_bytes()
                            .map_err(|error| error.to_string())?
                            .to_vec(),
                    )
                } else {
                    None
                };
                files.push(FileRecord {
                    path: key,
                    id: entry.backup_id,
                    order: entry.order,
                    timestamp: entry.timestamp,
                    mode: entry.mode,
                    before,
                    expected,
                });
            }
            if let Some(reason) = reason {
                drops.add(reason, count);
            } else {
                portable.push(SnapshotGroup::Undo { id, files });
            }
        }
        portable.sort_by_key(|group| {
            group
                .files()
                .iter()
                .map(|file| file.order)
                .min()
                .unwrap_or(0)
        });
        Ok(portable)
    }

    pub fn install_portable_undo(
        &mut self,
        session: &str,
        groups: &[(SnapshotGroup, Vec<PathBuf>)],
    ) -> Result<(), String> {
        let mut stacks: HashMap<PathBuf, Vec<BackupEntry>> = HashMap::new();
        for (group, paths) in groups {
            let SnapshotGroup::Undo { id, files } = group else {
                continue;
            };
            if files.len() != paths.len() {
                return Err("invalid path projection".into());
            }
            for (file, path) in files.iter().zip(paths) {
                refuse_storage_path(path).map_err(|error| error.to_string())?;
                let guard = ImportedGuard {
                    path: file.path.clone(),
                    expected: file.expected.clone(),
                };
                let post_state = match &file.expected {
                    ExpectedState::Absent => PathFingerprint::Absent,
                    ExpectedState::File(hash) => PathFingerprint::Content(hash.clone()),
                };
                // Share snapshot/lookup identity: a projected spelling can name
                // a symlinked parent or omit Windows's canonical path prefix.
                stacks
                    .entry(canonicalize_key(path))
                    .or_default()
                    .push(BackupEntry {
                        backup_id: file.id.clone(),
                        content: BackupContent::Captured(
                            file.before.clone().unwrap_or_default().into(),
                        ),
                        timestamp: file.timestamp,
                        order: file.order,
                        description: "imported undo".into(),
                        op_id: Some(id.clone()),
                        kind: if file.before.is_some() {
                            BackupEntryKind::Content
                        } else {
                            BackupEntryKind::Tombstone
                        },
                        mode: file.mode,
                        link_target: None,
                        created_dirs: Vec::new(),
                        post_state: Some(post_state),
                        external_change_before: false,
                        external_change_checkpoint: None,
                        link_to: None,
                        hardlink_detached: false,
                        imported_guard: Some(guard),
                    });
            }
        }
        for stack in stacks.values_mut() {
            stack.sort_by_key(|entry| entry.order);
            if stack.len() > self.policy.max_depth {
                return Err("imported stack exceeds undo depth".into());
            }
        }
        let mut keys: Vec<_> = stacks.keys().cloned().collect();
        keys.sort();
        let _locks = self
            .acquire_stack_disk_locks(session, &keys)
            .map_err(|error| error.to_string())?;
        for (path, stack) in &stacks {
            self.write_snapshot_to_disk_locked(session, path, stack)
                .map_err(|error| error.to_string())?;
        }
        self.entries.insert(session.to_string(), stacks);
        self.touch_session(session);
        Ok(())
    }
}
