//! Same-host cooperative exclusion for one bounded chunk-vector batch.
//!
//! This is not provider idempotency: an accepted request lost before durable
//! persistence can be billed again. All writers must participate, and network
//! filesystems / foreign-host heartbeat takeover do not provide this guarantee.

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::fs_lock::{self, AcquireError, LockGuard};

// Placeholder reason for pending semantic work, not a backend failure.
pub(crate) const PENDING_REASON: &str = "SEMANTIC_BATCH_PENDING_APPROVAL";

pub(crate) fn path(storage: &Path, family: &str, producer: &str) -> PathBuf {
    let mut hash = blake3::Hasher::new();
    hash.update(b"aft-semantic-batch-lease-v1\0");
    for field in [family, producer] {
        hash.update(&(field.len() as u64).to_le_bytes());
        hash.update(field.as_bytes());
    }
    // A sibling of blobs/, never a foreign payload in a v3 plane namespace.
    // Filesystem path aliases of the same directory address the same lease.
    storage
        .join("semantic-batch-leases-v1")
        .join(format!("{}.lock", hash.finalize().to_hex()))
}

pub(crate) struct BatchLease {
    guard: LockGuard,
    started: Instant,
}

pub(crate) fn try_acquire(
    storage: &Path,
    family: &str,
    producer: &str,
) -> Result<Option<BatchLease>, String> {
    let path = path(storage, family, producer);
    std::fs::create_dir_all(path.parent().expect("lease directory"))
        .map_err(|error| format!("semantic batch lease directory: {error}"))?;
    // Recover only this requested, unowned/stale lease via the existing
    // incarnation-aware protocol. Never sweep or expire a live owner's lease.
    match fs_lock::try_acquire_once_with_policy(&path, fs_lock::MalformedOwnerPolicy::FailClosed) {
        Ok(guard) => Ok(Some(BatchLease {
            guard,
            started: Instant::now(),
        })),
        Err(AcquireError::Timeout) => {
            let metadata = fs_lock::read_lease_settled(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
            let pid = metadata.as_ref().and_then(|m| m["pid"].as_u64());
            let created = metadata.as_ref().and_then(|m| m["created_at_ms"].as_u64());
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            crate::slog_debug!(
                "semantic batch contended owner_pid={pid:?} owner_age_ms={:?}",
                created.map(|created| now.saturating_sub(created))
            );
            Ok(None)
        }
        Err(error) => Err(format!("semantic batch lease: {error}")),
    }
}

impl BatchLease {
    pub(crate) fn verify(&self) -> Result<(), String> {
        match self.guard.verify_writer_epoch() {
            Ok(true) => Ok(()),
            Ok(false) => {
                Err("semantic batch lease ownership lost; provider outcome may be uncertain".into())
            }
            Err(error) => Err(format!("semantic batch lease verification: {error}")),
        }
    }
}

impl Drop for BatchLease {
    fn drop(&mut self) {
        crate::slog_debug!(
            "semantic batch finished batch_ms={}",
            self.started.elapsed().as_millis()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_batch_lease_malformed_live_owner_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let mut owner = try_acquire(root.path(), "family", "producer")
            .unwrap()
            .unwrap();
        owner.verify().unwrap();
        // Keep the owner alive, but stop automatic repair so the deliberately
        // damaged bytes remain unchanged throughout the contender's grace read.
        owner.guard.stop_heartbeat_for_removal();
        let path = path(root.path(), "family", "producer");
        let damaged = b"deliberately damaged owner metadata";
        std::fs::write(&path, damaged).unwrap();
        let result = try_acquire(root.path(), "family", "producer");
        assert!(result.is_err(), "malformed live-owner metadata must be unusable, never a new owner or ordinary contention");
        assert_eq!(std::fs::read(&path).unwrap(), damaged);
        assert!(crate::fs_lock::process_alive(std::process::id()));
        drop(owner);
    }

    #[test]
    fn semantic_batch_lease_child_owner() {
        let Some(storage) = std::env::var_os("AFT_SEMANTIC_LEASE_CHILD") else {
            return;
        };
        let storage = Path::new(&storage);
        let _owner = try_acquire(storage, "family", "producer").unwrap().unwrap();
        std::fs::write(storage.join("owner-ready"), b"ready").unwrap();
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn semantic_batch_lease_dead_process_recovery() {
        let root = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "views::semantic_batch_lease::tests::semantic_batch_lease_child_owner",
                "--nocapture",
            ])
            .env("AFT_SEMANTIC_LEASE_CHILD", root.path())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        while !root.path().join("owner-ready").exists() {
            if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
                let _ = child.kill();
                let _ = child.wait();
                panic!("child did not acquire its lease");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(try_acquire(root.path(), "family", "producer")
            .unwrap()
            .is_none());
        // Child::kill uses TerminateProcess on Windows, not a Unix-only signal.
        child.kill().unwrap();
        child.wait().unwrap();
        let recovered = try_acquire(root.path(), "family", "producer")
            .unwrap()
            .unwrap();
        recovered.verify().unwrap();
    }

    #[test]
    fn semantic_batch_lease_identity_and_live_contention() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let owner = try_acquire(root.path(), "family", "producer")
            .unwrap()
            .unwrap();
        owner.verify().unwrap();
        assert!(try_acquire(root.path(), "family", "producer")
            .unwrap()
            .is_none());
        for (storage, family, producer) in [
            (other.path(), "family", "producer"),
            (root.path(), "other", "producer"),
            (root.path(), "family", "other"),
        ] {
            assert!(try_acquire(storage, family, producer).unwrap().is_some());
        }
        assert_ne!(path(root.path(), "ab", "c"), path(root.path(), "a", "bc"));
        drop(owner);
        assert!(try_acquire(root.path(), "family", "producer")
            .unwrap()
            .is_some());
    }

    #[test]
    fn semantic_batch_lease_loss_fails_closed_and_preserves_successor() {
        let root = tempfile::tempdir().unwrap();
        let owner = try_acquire(root.path(), "family", "producer")
            .unwrap()
            .unwrap();
        std::fs::remove_file(path(root.path(), "family", "producer")).unwrap();
        let successor = try_acquire(root.path(), "family", "producer")
            .unwrap()
            .unwrap();
        assert!(owner.verify().is_err());
        drop(owner);
        successor.verify().unwrap();
    }
}
