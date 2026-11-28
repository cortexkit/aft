/// Points in pending-change capture where a test can move work downstream or
/// release a concurrent metadata reader before the capture continues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingCaptureBoundary {
    /// After the flush-window set, before the dispatch channel and drain.
    AfterFlushWindow,
    /// After a contended intake-counter probe, before reading the snapshot.
    AfterIntakeProbe,
    /// After a contended cleanup-state probe, before reading the snapshot.
    AfterCleanupProbe,
    /// After a contended receiver-slot probe, before reading the snapshot.
    AfterReceiverProbe,
    /// After probing the continuation mutex, before waiting for its metadata.
    AfterContinuationProbe,
}

type PendingCaptureHook = Box<dyn FnMut(PendingCaptureBoundary)>;

thread_local! {
    static PENDING_CAPTURE_HOOK: std::cell::RefCell<Option<PendingCaptureHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `hook` at each capture boundary reached on this thread until the
/// returned guard is dropped.
pub(crate) fn install_pending_capture_hook_for_test(
    hook: impl FnMut(PendingCaptureBoundary) + 'static,
) -> PendingCaptureHookGuard {
    PENDING_CAPTURE_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    PendingCaptureHookGuard
}

pub(crate) struct PendingCaptureHookGuard;

impl Drop for PendingCaptureHookGuard {
    fn drop(&mut self) {
        PENDING_CAPTURE_HOOK.with(|slot| *slot.borrow_mut() = None);
    }
}

pub(super) fn pending_capture_boundary_for_test(boundary: PendingCaptureBoundary) {
    let hook = PENDING_CAPTURE_HOOK.with(|slot| slot.borrow_mut().take());
    if let Some(mut hook) = hook {
        hook(boundary);
        PENDING_CAPTURE_HOOK.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_none() {
                *slot = Some(hook);
            }
        });
    }
}
