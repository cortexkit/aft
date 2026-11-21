use super::{BgTaskRegistry, HashSet};
use std::sync::Arc;

/// Shared by every registry attached to this daemon connection. Only the
/// message loop publishes; readers hold an immutable snapshot.
#[derive(Default)]
#[doc(hidden)]
pub struct RouteDeliverySessions(parking_lot::RwLock<Arc<HashSet<String>>>);

impl RouteDeliverySessions {
    #[doc(hidden)]
    pub fn publish(&self, sessions: HashSet<&str>) {
        let current = self.snapshot();
        if current.len() == sessions.len() && sessions.iter().all(|id| current.contains(*id)) {
            return;
        }
        *self.0.write() = Arc::new(sessions.into_iter().map(str::to_owned).collect());
    }

    fn snapshot(&self) -> Arc<HashSet<String>> {
        Arc::clone(&self.0.read())
    }
}

pub(super) enum LiveDelivery {
    Local(HashSet<String>),
    Routes(Arc<RouteDeliverySessions>),
}

impl Default for LiveDelivery {
    fn default() -> Self {
        Self::Local(HashSet::new())
    }
}

impl BgTaskRegistry {
    pub(crate) fn record_live_delivery_session(&self, session_id: &str) {
        if let Ok(mut live_sessions) = self.inner.live_delivery_sessions.lock() {
            let _hold = self.inner.live_delivery_lock_diagnostics.hold();
            if let LiveDelivery::Local(sessions) = &mut *live_sessions {
                sessions.insert(session_id.to_string());
            }
        }
    }

    #[doc(hidden)]
    pub fn use_route_delivery_sessions(&self, routes: &Arc<RouteDeliverySessions>) {
        if let Ok(mut live_sessions) = self.inner.live_delivery_sessions.lock() {
            let _hold = self.inner.live_delivery_lock_diagnostics.hold();
            if !matches!(&*live_sessions, LiveDelivery::Routes(current) if Arc::ptr_eq(current, routes))
            {
                *live_sessions = LiveDelivery::Routes(Arc::clone(routes));
            }
        }
    }

    #[doc(hidden)]
    pub fn clear_live_delivery_sessions(&self) {
        if let Ok(mut live_sessions) = self.inner.live_delivery_sessions.lock() {
            let _hold = self.inner.live_delivery_lock_diagnostics.hold();
            *live_sessions = LiveDelivery::default();
        }
    }

    #[doc(hidden)]
    pub fn live_delivery_lock_diagnostics(&self) -> Arc<crate::lock_diagnostics::LockDiagnostics> {
        Arc::clone(&self.inner.live_delivery_lock_diagnostics)
    }

    pub(super) fn originating_session_has_live_route(&self, session_id: &str) -> bool {
        self.inner
            .live_delivery_sessions
            .lock()
            .map(|sessions| {
                let _hold = self.inner.live_delivery_lock_diagnostics.hold();
                match &*sessions {
                    LiveDelivery::Local(local) => local.contains(session_id),
                    LiveDelivery::Routes(routes) => routes.snapshot().contains(session_id),
                }
            })
            .unwrap_or(true)
    }

    pub(super) fn should_retire_foreign_delivery(
        &self,
        originating_session_id: &str,
        binding_session_id: &str,
    ) -> bool {
        originating_session_id != binding_session_id
            && !self.originating_session_has_live_route(originating_session_id)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn route_delivery_snapshot(&self) -> Arc<HashSet<String>> {
        match &*self.inner.live_delivery_sessions.lock().unwrap() {
            LiveDelivery::Routes(routes) => routes.snapshot(),
            LiveDelivery::Local(_) => panic!("registry is not attached to the route table"),
        }
    }
}
