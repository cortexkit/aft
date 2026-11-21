//! Session-keyed hashline bindings: registration, effective mode, and lifetime.
//!
//! Registration computes edit-slot eligibility independently of schema selection,
//! derives `effective = configured_enabled AND edit_slot_survives`, and installs
//! the binding for `(canonical project root, session id)`. Request handlers capture
//! a binding guard for the duration of the call so concurrent sessions under one
//! root never share tags, stores, or schemas. Effective-value changes drain
//! in-flight guards before clearing stores.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use crate::hashline::apply::RegisterStore;
use crate::hashline::snapshot::SnapshotStore;

/// Stable identity for one session under one project root.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionKey {
    pub root: PathBuf,
    pub session_id: String,
}

impl SessionKey {
    pub fn new(root: impl Into<PathBuf>, session_id: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            session_id: session_id.into(),
        }
    }
}

/// Configure-channel warning emitted when hashline is configured on but a slot
/// the hashline surface depends on did not survive final surface selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DowngradeWarning {
    pub code: &'static str,
    pub reason: &'static str,
}

impl DowngradeWarning {
    pub const EDIT_NOT_REGISTERED: Self = Self {
        code: "hashline_downgraded",
        reason: "edit_not_registered",
    };

    /// The `edit` slot survived but the tagged `read` slot did not, so nothing
    /// in the session could mint the `[path#TAG]` snapshots a patch addresses.
    pub const TAGGED_READ_UNAVAILABLE: Self = Self {
        code: "hashline_downgraded",
        reason: "tagged_read_unavailable",
    };

    /// JSON object for the configure-warnings channel.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "code": self.code,
            "reason": self.reason,
        })
    }
}

/// Inputs the host supplies when registering (or re-registering) a session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegistrationRequest {
    /// Resolved `hashline.enabled` from Rust config_resolve.
    pub configured_enabled: bool,
    /// Host-computed flag: `edit` survived final surface selection, pruning,
    /// hoisting, and `disabled_tools`. Sessions without a host pruning layer
    /// (MCP, daemon-direct) default this to `true`.
    pub edit_slot_survives: bool,
    /// Whether the tagged `read` slot survived the same filtering. Only a tagged
    /// read mints the snapshots a hashline patch addresses, so an edit slot on
    /// its own is not a usable hashline surface.
    pub read_slot_survives: bool,
}

impl RegistrationRequest {
    pub const fn effective(self) -> bool {
        self.configured_enabled && self.edit_slot_survives && self.read_slot_survives
    }

    pub const fn should_downgrade(self) -> bool {
        self.configured_enabled && !(self.edit_slot_survives && self.read_slot_survives)
    }

    /// Which missing slot to report.
    ///
    /// The read slot is reported first because it is the harder failure to
    /// diagnose: a host can leave a perfectly good `edit` tool registered next
    /// to its own untagged read, and the resulting "I never got a hashline"
    /// symptom points at the edit tool, which is not the missing piece.
    pub const fn downgrade_warning(self) -> Option<DowngradeWarning> {
        if !self.should_downgrade() {
            return None;
        }
        if self.read_slot_survives {
            Some(DowngradeWarning::EDIT_NOT_REGISTERED)
        } else {
            Some(DowngradeWarning::TAGGED_READ_UNAVAILABLE)
        }
    }
}

/// Outcome of a completed registration attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistrationOutcome {
    pub configured_enabled: bool,
    pub edit_slot_survives: bool,
    pub read_slot_survives: bool,
    pub effective: bool,
    /// Present when configured on but edit was not registered.
    pub downgrade: Option<DowngradeWarning>,
    /// True when stores were cleared because the effective value changed.
    pub stores_cleared: bool,
    /// True when same-effective re-registration preserved snapshot/register state.
    pub stores_preserved: bool,
}

/// Session-owned hashline state installed atomically at registration.
#[derive(Debug)]
pub struct HashlineBinding {
    key: SessionKey,
    configured_enabled: bool,
    edit_slot_survives: bool,
    read_slot_survives: bool,
    effective: bool,
    snapshots: SnapshotStore,
    registers: RegisterStore,
    /// In-flight request guards holding this binding.
    in_flight: usize,
}

impl HashlineBinding {
    fn new(key: SessionKey, request: RegistrationRequest) -> Self {
        Self {
            key,
            configured_enabled: request.configured_enabled,
            edit_slot_survives: request.edit_slot_survives,
            read_slot_survives: request.read_slot_survives,
            effective: request.effective(),
            snapshots: SnapshotStore::new(),
            registers: RegisterStore::new(),
            in_flight: 0,
        }
    }

    pub fn key(&self) -> &SessionKey {
        &self.key
    }

    pub fn configured_enabled(&self) -> bool {
        self.configured_enabled
    }

    pub fn edit_slot_survives(&self) -> bool {
        self.edit_slot_survives
    }

    pub fn read_slot_survives(&self) -> bool {
        self.read_slot_survives
    }

    pub fn effective(&self) -> bool {
        self.effective
    }

    pub fn snapshots(&self) -> &SnapshotStore {
        &self.snapshots
    }

    pub fn snapshots_mut(&mut self) -> &mut SnapshotStore {
        &mut self.snapshots
    }

    pub fn registers(&self) -> &RegisterStore {
        &self.registers
    }

    pub fn registers_mut(&mut self) -> &mut RegisterStore {
        &mut self.registers
    }

    /// Borrow both session stores for one atomic request pipeline.
    pub fn stores_mut(&mut self) -> (&mut SnapshotStore, &mut RegisterStore) {
        (&mut self.snapshots, &mut self.registers)
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn clear_stores(&mut self) {
        self.snapshots.clear();
        *self.registers_mut() = RegisterStore::new();
    }
}

/// Keep each condition variable beside the only mutex it may ever wait on.
/// A registry-wide condition variable cannot drain multiple session mutexes:
/// `std::sync::Condvar` permanently binds to the first mutex it observes.
#[derive(Debug)]
struct BindingSlot {
    binding: Mutex<HashlineBinding>,
    drain: Condvar,
}

impl BindingSlot {
    fn new(binding: HashlineBinding) -> Self {
        Self {
            binding: Mutex::new(binding),
            drain: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashlineBinding> {
        self.binding
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn drain_in_flight(&self) {
        let mut binding = self.lock();
        while binding.in_flight > 0 {
            binding = self
                .drain
                .wait(binding)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn release_guard(&self) {
        {
            let mut binding = self.lock();
            binding.in_flight = binding.in_flight.saturating_sub(1);
        }
        self.drain.notify_all();
    }
}

/// Shared handle to an installed binding. Capture one per request.
#[derive(Clone, Debug)]
pub struct BindingHandle {
    inner: Arc<BindingSlot>,
}

impl BindingHandle {
    pub fn with_binding<R>(&self, f: impl FnOnce(&HashlineBinding) -> R) -> R {
        let guard = self.inner.lock();
        f(&guard)
    }

    pub fn with_binding_mut<R>(&self, f: impl FnOnce(&mut HashlineBinding) -> R) -> R {
        let mut guard = self.inner.lock();
        f(&mut guard)
    }

    pub fn effective(&self) -> bool {
        self.with_binding(|b| b.effective())
    }

    pub fn session_key(&self) -> SessionKey {
        self.with_binding(|b| b.key().clone())
    }
}

/// RAII guard that keeps a binding alive for one request and participates in
/// the rebind drain refcount.
pub struct BindingGuard {
    handle: BindingHandle,
}

impl BindingGuard {
    pub fn handle(&self) -> &BindingHandle {
        &self.handle
    }

    pub fn effective(&self) -> bool {
        self.handle.effective()
    }

    pub fn with_binding<R>(&self, f: impl FnOnce(&HashlineBinding) -> R) -> R {
        self.handle.with_binding(f)
    }

    pub fn with_binding_mut<R>(&self, f: impl FnOnce(&mut HashlineBinding) -> R) -> R {
        self.handle.with_binding_mut(f)
    }
}

impl Drop for BindingGuard {
    fn drop(&mut self) {
        self.handle.inner.release_guard();
    }
}

struct BindingRegistryInner {
    state: Mutex<RegistryState>,
}

#[derive(Default)]
struct RegistryState {
    bindings: HashMap<SessionKey, Arc<BindingSlot>>,
}

/// Process-wide (or test-local) registry of session hashline bindings.
pub struct BindingRegistry {
    inner: Arc<BindingRegistryInner>,
}

impl Default for BindingRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl BindingRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(BindingRegistryInner {
                state: Mutex::new(RegistryState::default()),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, RegistryState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Atomically install or replace the binding for one session.
    ///
    /// Same-effective re-registration preserves snapshot and register stores.
    /// Effective-value changes drain in-flight guards, clear stores, then install.
    /// Failed callers must not call this with a partial request — the install is
    /// all-or-nothing once invoked.
    pub fn register(
        &self,
        root: impl AsRef<Path>,
        session_id: impl Into<String>,
        request: RegistrationRequest,
    ) -> RegistrationOutcome {
        let key = SessionKey::new(root.as_ref().to_path_buf(), session_id.into());
        self.register_key(key, request, || {})
    }

    fn register_key(
        &self,
        key: SessionKey,
        request: RegistrationRequest,
        after_existing_read: impl FnOnce(),
    ) -> RegistrationOutcome {
        let effective = request.effective();
        let downgrade = request.downgrade_warning();

        // Serialize the existing-value read, comparison, and binding update. A
        // guard may finish while this lock is held because guard release only
        // takes the binding lock and signals the drain condition variable.
        let mut state = self.lock();
        let existing = state.bindings.get(&key).cloned();
        let previous_effective = existing.as_ref().map(|binding| binding.lock().effective());
        after_existing_read();

        let (stores_cleared, stores_preserved) = if let Some(existing) = existing {
            if previous_effective != Some(effective) {
                self.drain_in_flight(&existing);
                {
                    let mut binding = existing.lock();
                    binding.configured_enabled = request.configured_enabled;
                    binding.edit_slot_survives = request.edit_slot_survives;
                    binding.read_slot_survives = request.read_slot_survives;
                    binding.effective = effective;
                    binding.clear_stores();
                }
                state.bindings.insert(key, existing);
                (true, false)
            } else {
                {
                    let mut binding = existing.lock();
                    binding.configured_enabled = request.configured_enabled;
                    binding.edit_slot_survives = request.edit_slot_survives;
                    binding.read_slot_survives = request.read_slot_survives;
                    // effective unchanged; stores preserved.
                }
                state.bindings.insert(key, existing);
                (false, true)
            }
        } else {
            let binding = Arc::new(BindingSlot::new(HashlineBinding::new(key.clone(), request)));
            state.bindings.insert(key, binding);
            (false, false)
        };

        RegistrationOutcome {
            configured_enabled: request.configured_enabled,
            edit_slot_survives: request.edit_slot_survives,
            read_slot_survives: request.read_slot_survives,
            effective,
            downgrade,
            stores_cleared,
            stores_preserved,
        }
    }

    /// Capture the installed binding for one request. Unregistered sessions
    /// yield `None` and must behave as effective-off.
    pub fn capture(
        &self,
        root: impl AsRef<Path>,
        session_id: impl Into<String>,
    ) -> Option<BindingGuard> {
        let key = SessionKey::new(root.as_ref().to_path_buf(), session_id.into());
        let handle = {
            let state = self.lock();
            let arc = state.bindings.get(&key)?.clone();
            {
                let mut binding = arc.lock();
                binding.in_flight = binding.in_flight.saturating_add(1);
            }
            BindingHandle { inner: arc }
        };
        Some(BindingGuard { handle })
    }

    /// Look up without incrementing the in-flight refcount (diagnostics only).
    pub fn peek(
        &self,
        root: impl AsRef<Path>,
        session_id: impl Into<String>,
    ) -> Option<BindingHandle> {
        let key = SessionKey::new(root.as_ref().to_path_buf(), session_id.into());
        let state = self.lock();
        state
            .bindings
            .get(&key)
            .map(|arc| BindingHandle { inner: arc.clone() })
    }

    /// Remove one session binding (teardown / restart). In-flight guards drain first.
    pub fn teardown(&self, root: impl AsRef<Path>, session_id: impl Into<String>) -> bool {
        let key = SessionKey::new(root.as_ref().to_path_buf(), session_id.into());
        let existing = {
            let mut state = self.lock();
            state.bindings.remove(&key)
        };
        let Some(existing) = existing else {
            return false;
        };

        // Remove the slot before waiting so no new request can capture the
        // binding while teardown is draining its existing guards. A concurrent
        // registration may install a new slot without being removed afterward.
        self.drain_in_flight(&existing);
        true
    }

    /// Number of installed bindings (test/diagnostics).
    pub fn len(&self) -> usize {
        self.lock().bindings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn drain_in_flight(&self, binding: &Arc<BindingSlot>) {
        binding.drain_in_flight();
    }
}

/// Effective mode for a request: unregistered sessions are always off.
pub fn effective_for_capture(guard: Option<&BindingGuard>) -> bool {
    guard.map(|g| g.effective()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashline::scan::scan_bytes;
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn draining_two_sessions_uses_each_sessions_mutex_partner() {
        let registry = BindingRegistry::new();
        let root = Path::new("/tmp/hashline-condvar-partners");
        registry.register(
            root,
            "first",
            RegistrationRequest {
                configured_enabled: true,
                edit_slot_survives: true,
                read_slot_survives: true,
            },
        );
        registry.register(
            root,
            "second",
            RegistrationRequest {
                configured_enabled: true,
                edit_slot_survives: true,
                read_slot_survives: true,
            },
        );
        let first = registry.peek(root, "first").expect("first binding");
        let second = registry.peek(root, "second").expect("second binding");

        for handle in [&first, &second] {
            let guard = handle.inner.lock();
            let (_guard, timeout) = handle
                .inner
                .drain
                .wait_timeout(guard, Duration::from_millis(1))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(timeout.timed_out());
        }
    }

    #[test]
    fn teardown_without_a_binding_returns_immediately() {
        let registry = BindingRegistry::new();
        assert!(!registry.teardown("/tmp/hashline-no-binding", "missing"));
    }

    #[test]
    fn teardown_notifies_each_session_through_its_own_slot() {
        let registry = Arc::new(BindingRegistry::new());
        let root = Path::new("/tmp/hashline-session-drains");
        for session in ["first", "second"] {
            registry.register(
                root,
                session,
                RegistrationRequest {
                    configured_enabled: true,
                    edit_slot_survives: true,
                    read_slot_survives: true,
                },
            );
        }
        let first_guard = registry.capture(root, "first").expect("first guard");
        let second_guard = registry.capture(root, "second").expect("second guard");

        let (done_tx, done_rx) = mpsc::channel();
        let first_registry = Arc::clone(&registry);
        let first_done_tx = done_tx.clone();
        let first_root = root.to_path_buf();
        let first_teardown = thread::spawn(move || {
            assert!(first_registry.teardown(&first_root, "first"));
            first_done_tx.send("first").expect("signal first teardown");
        });
        let second_registry = Arc::clone(&registry);
        let second_root = root.to_path_buf();
        let second_teardown = thread::spawn(move || {
            assert!(second_registry.teardown(&second_root, "second"));
            done_tx.send("second").expect("signal second teardown");
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while (registry.peek(root, "first").is_some() || registry.peek(root, "second").is_some())
            && std::time::Instant::now() < deadline
        {
            thread::yield_now();
        }
        assert!(registry.peek(root, "first").is_none());
        assert!(registry.peek(root, "second").is_none());

        drop(second_guard);
        assert_eq!(
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("second teardown completes"),
            "second"
        );
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        ));

        drop(first_guard);
        assert_eq!(
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("first teardown completes"),
            "first"
        );
        first_teardown.join().expect("first teardown thread");
        second_teardown.join().expect("second teardown thread");
    }

    #[test]
    fn concurrent_same_session_registration_serializes_read_compare_write() {
        let registry = Arc::new(BindingRegistry::new());
        let key = SessionKey::new("/tmp/hashline-register-race", "shared-session");
        registry.register(
            &key.root,
            key.session_id.clone(),
            RegistrationRequest {
                configured_enabled: true,
                edit_slot_survives: true,
                read_slot_survives: true,
            },
        );
        registry
            .peek(&key.root, key.session_id.clone())
            .expect("initial binding")
            .with_binding_mut(|binding| {
                binding
                    .snapshots_mut()
                    .publish("race.rs", scan_bytes(b"before race\n"));
            });

        let (first_read_tx, first_read_rx) = mpsc::channel();
        let (release_first_tx, release_first_rx) = mpsc::channel();
        let first_registry = Arc::clone(&registry);
        let first_key = key.clone();
        let first = thread::spawn(move || {
            first_registry.register_key(
                first_key,
                RegistrationRequest {
                    configured_enabled: false,
                    edit_slot_survives: true,
                    read_slot_survives: true,
                },
                || {
                    first_read_tx.send(()).expect("signal first read");
                    release_first_rx.recv().expect("release first registration");
                },
            )
        });

        first_read_rx
            .recv()
            .expect("first registration read existing binding");
        let (second_started_tx, second_started_rx) = mpsc::channel();
        let (second_read_tx, second_read_rx) = mpsc::channel();
        let (second_done_tx, second_done_rx) = mpsc::channel();
        let second_registry = Arc::clone(&registry);
        let second_key = key.clone();
        let second = thread::spawn(move || {
            second_started_tx.send(()).expect("signal second start");
            let outcome = second_registry.register_key(
                second_key,
                RegistrationRequest {
                    configured_enabled: true,
                    edit_slot_survives: false,
                    read_slot_survives: true,
                },
                || second_read_tx.send(()).expect("signal second read"),
            );
            second_done_tx.send(outcome).expect("send second outcome");
        });

        second_started_rx
            .recv()
            .expect("second registration started");
        assert!(matches!(
            second_read_rx.recv_timeout(Duration::from_secs(1)),
            Err(RecvTimeoutError::Timeout)
        ));
        release_first_tx
            .send(())
            .expect("release first registration");

        let first_outcome = first.join().expect("first registration");
        let second_outcome = second_done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second registration completes after first");
        second.join().expect("second registration");

        assert!(first_outcome.stores_cleared);
        assert!(second_outcome.stores_preserved);
        let final_binding = registry
            .peek(&key.root, key.session_id)
            .expect("final binding");
        final_binding.with_binding(|binding| {
            assert!(binding.configured_enabled());
            assert!(!binding.edit_slot_survives());
            assert!(!binding.effective());
            assert!(binding.snapshots().is_empty());
        });
    }
}
