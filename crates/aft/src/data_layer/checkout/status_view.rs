//! Reading the published checkout and the status of its index planes.

use super::{Checkout, CheckoutStatus, CheckoutView, Failure, Live, PlaneStatus, SliceSource};
use std::sync::Arc;

impl Checkout {
    /// The checkout's data as queries see it now: the last publication with
    /// the changes made since laid over it. `None` until the first build
    /// publishes.
    #[must_use]
    pub fn view(&self) -> Option<CheckoutView> {
        let live = self.live.read().expect("no panics while holding it");
        self.view_from_live(&live)
    }

    fn view_from_live(&self, live: &Live) -> Option<CheckoutView> {
        let published = Arc::clone(live.published.as_ref()?);
        let graph = live.graph.clone().unwrap_or_else(|| {
            let graph: Arc<dyn SliceSource + Send + Sync> = published.graph.clone();
            graph
        });
        Some(CheckoutView {
            memory_epoch: Arc::clone(&self.memory_epoch),
            published,
            planes: self.planes,
            text: Arc::clone(&live.text),
            text_members: Arc::clone(&live.text_members),
            callgraph_pending: Arc::clone(&live.callgraph_pending),
            rescanning: live.rescanning,
            graph,
            refreshed: live.refreshed.clone(),
            sequence: live.sequence,
            unparsed: Arc::clone(&live.unparsed),
            current: live.state.clone(),
        })
    }

    /// Commit work for a pinned meaning view only while its publication and
    /// live checkout state still match. A parser can record a failure in that
    /// state without changing any file contents or advancing the text sequence.
    pub(crate) fn with_current_meaning_view<T>(
        &self,
        view: &CheckoutView,
        commit: impl FnOnce() -> T,
    ) -> Option<T> {
        let live = self.live.read().expect("no panics while holding it");
        let same_overlay = match (&live.state, &view.current) {
            (None, None) => true,
            (Some(current), Some(pinned)) => Arc::ptr_eq(current, pinned),
            _ => false,
        };
        (Arc::ptr_eq(live.published.as_ref()?, &view.published)
            && live.sequence == view.sequence
            && same_overlay)
            .then(commit)
    }

    /// How each plane stands, for status and for replies that disclose
    /// what they could not cover.
    #[must_use]
    pub fn status(&self) -> CheckoutStatus {
        self.status_view().0
    }

    /// Read readiness and the corresponding query view under one lock.
    pub(crate) fn status_view(&self) -> (CheckoutStatus, Option<CheckoutView>) {
        let live = self.live.read().expect("no panics while holding it");
        let view = self.view_from_live(&live);
        let status = self.status_from_view(view.as_ref(), live.building, live.failure.is_some());
        (status, view)
    }

    /// Read status and its view together, without waiting for index publication.
    pub(crate) fn try_status_view(
        &self,
    ) -> (CheckoutStatus, Option<CheckoutView>, Option<Failure>) {
        let Ok(live) = self.live.try_read() else {
            return (self.status_from_view(None, true, false), None, None);
        };
        let view = self.view_from_live(&live);
        let status = self.status_from_view(view.as_ref(), live.building, live.failure.is_some());
        (status, view, live.failure.clone())
    }

    #[cfg(test)]
    pub(crate) fn with_live_write_lock_for_test(&self, run: impl FnOnce()) {
        let _owner = self.write_live();
        run();
    }

    fn status_from_view(
        &self,
        view: Option<&CheckoutView>,
        building: bool,
        failed: bool,
    ) -> CheckoutStatus {
        if let Some(view) = view {
            return CheckoutStatus {
                building,
                failed,
                ..view.status()
            };
        }
        let plane = |on: bool| {
            if building && on {
                PlaneStatus::Building {
                    done: 0,
                    total: None,
                }
            } else {
                PlaneStatus::Off
            }
        };
        CheckoutStatus {
            text: plane(self.planes.text),
            callgraph: plane(self.planes.callgraph),
            callgraph_published: false,
            planes: self.planes,
            callgraph_pending: 0,
            building,
            unparsed: 0,
            failed,
        }
    }

    /// Why the last build or call-graph refresh failed, until one succeeds.
    #[must_use]
    pub fn failure(&self) -> Option<String> {
        self.last_failure().map(|failure| failure.error)
    }

    /// The last failed build or call-graph refresh, until one succeeds.
    #[must_use]
    pub fn last_failure(&self) -> Option<Failure> {
        self.live
            .read()
            .expect("no panics while holding it")
            .failure
            .clone()
    }

    pub(crate) fn try_last_failure(&self) -> Option<Failure> {
        self.live.try_read().ok()?.failure.clone()
    }
}
