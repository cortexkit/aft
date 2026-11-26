use super::{query_embed_health_registry, root_backend_probes};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, TryLockError};

pub(crate) fn try_lock_root_state<T>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match mutex.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(error)) => Some(error.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// Forget a retired root without making the daemon frame loop wait on a probe.
pub(crate) fn try_retire_root_embedding_state(root: &Path) -> bool {
    let (Some(mut probes), Some(mut health)) = (
        try_lock_root_state(root_backend_probes()),
        try_lock_root_state(query_embed_health_registry()),
    ) else {
        return false;
    };
    if !crate::synapse_embed::try_retire_root_state(root) {
        return false;
    }
    probes.remove(root);
    health.remove(root);
    true
}
