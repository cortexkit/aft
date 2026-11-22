//! Shared recovery reporting for mutating commands.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

pub(super) struct RollbackOutcome {
    restored: bool,
    deleted: HashSet<PathBuf>,
    failed_deletions: Vec<PathBuf>,
    errors: Vec<String>,
}

impl RollbackOutcome {
    pub(super) fn from_restore(result: Result<(), String>) -> Self {
        match result {
            Ok(()) => Self {
                restored: true,
                deleted: HashSet::new(),
                failed_deletions: Vec::new(),
                errors: Vec::new(),
            },
            Err(error) => Self {
                restored: false,
                deleted: HashSet::new(),
                failed_deletions: Vec::new(),
                errors: vec![error],
            },
        }
    }

    pub(super) fn record_deletion(&mut self, path: &Path, result: std::io::Result<()>) {
        match result {
            Ok(()) => {
                self.deleted.insert(path.to_path_buf());
            }
            Err(error) => {
                self.failed_deletions.push(path.to_path_buf());
                self.errors
                    .push(format!("failed to delete {}: {error}", path.display()));
            }
        }
    }

    pub(super) fn restored(&self) -> bool {
        self.restored
    }
    pub(super) fn deleted(&self, path: &Path) -> bool {
        self.deleted.contains(path)
    }
    pub(super) fn complete(&self) -> bool {
        self.restored && self.errors.is_empty()
    }

    pub(super) fn append_to(&self, data: &mut Value, checkpoint: &str) {
        data["rollback_complete"] = self.complete().into();
        if !self.complete() {
            data["rollback_error"] = self.errors.join("; ").into();
            data["recovery_checkpoint"] = checkpoint.into();
            if !self.failed_deletions.is_empty() {
                data["recovery_remove_files"] = self
                    .failed_deletions
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .into();
            }
        }
    }

    pub(super) fn failure_message(&self, message: &str, checkpoint: &str) -> String {
        if self.complete() {
            return message.to_string();
        }
        let mut message = format!(
            "{message}; rollback incomplete: {}; recovery checkpoint: {checkpoint}",
            self.errors.join("; ")
        );
        // Checkpoints restore pre-existing files but cannot remove a newly
        // created destination. Name the cleanup still required beside it.
        if !self.failed_deletions.is_empty() {
            let paths = self
                .failed_deletions
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            message.push_str(&format!("; remove created files: {paths}"));
        }
        message
    }
}
