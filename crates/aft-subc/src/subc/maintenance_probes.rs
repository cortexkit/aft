use crate::lsp::child_registry::LspChildRegistry;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::time::Instant;

pub(super) struct OrphanedChildrenProbe(Instant);

impl OrphanedChildrenProbe {
    pub(super) fn new() -> Self {
        Self(Instant::now())
    }

    pub(super) fn request(&mut self, registry: LspChildRegistry) -> bool {
        static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
        let now = Instant::now();
        if now < self.0
            || !registry.has_children_without_waiting()
            || IN_FLIGHT.swap(true, Ordering::AcqRel)
        {
            return false;
        }
        self.0 = now + Duration::from_secs(5);
        let spawned = std::thread::Builder::new()
            .name("aft-lsp-child-reap".to_owned())
            .spawn(move || {
                let reaped = registry.reap_children_with_gone_cwd_or_reclaimed_root();
                if reaped > 0 {
                    log::warn!("subc attach: reaped {reaped} orphaned LSP child process group(s)");
                }
                IN_FLIGHT.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            IN_FLIGHT.store(false, Ordering::Release);
        }
        spawned.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn idle_maintenance_skips_empty_lsp_child_registry() {
        let mut probe = OrphanedChildrenProbe::new();
        let registry = LspChildRegistry::default();
        for _ in 0..40 {
            assert!(!probe.request(registry.clone()));
            tokio::time::advance(Duration::from_millis(250)).await;
        }
    }
}
