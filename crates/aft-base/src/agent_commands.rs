//! Best-effort attribution of agent shell roots over the supervised connection.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use subc_client_rs::{AgentCommandFinished, AgentCommandRoot, AgentCommandsError};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::process_identity::{LinuxBirth, ProcessIdentity};

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedRoot {
    #[serde(flatten)]
    root: AgentCommandRoot,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    native_birth: Option<LinuxBirth>,
}

impl SavedRoot {
    fn is_current(&self) -> bool {
        root_identity(self.root.pid).is_some_and(|current| {
            current.same_process(&ProcessIdentity {
                pid: self.root.pid,
                started_at_ms: self.root.start_time_ms,
                native_birth: self.native_birth.clone(),
            })
        })
    }
}

fn root_identity(pid: u32) -> Option<ProcessIdentity> {
    #[cfg(target_os = "linux")]
    {
        use crate::process_identity::{linux_start_time_ms, read_boot_time_secs};
        let ticks = subc_os::start_time(pid)?;
        Some(ProcessIdentity {
            pid,
            // Declarations to the daemon use a fresh epoch conversion at launch.
            // Restored roots still send their original epoch, not this reading.
            started_at_ms: linux_start_time_ms(ticks, read_boot_time_secs()?)?,
            native_birth: LinuxBirth::from_start_ticks(ticks),
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        Some(ProcessIdentity::from_epoch(
            pid,
            subc_os::process_start_time_ms(pid)?,
        ))
    }
}

#[derive(Clone, Debug, Default)]
pub struct Batch {
    pub started: Vec<AgentCommandRoot>,
    pub finished: Vec<AgentCommandFinished>,
    pub known: Option<Vec<AgentCommandRoot>>,
    through: u64,
}

enum Event {
    Started(AgentCommandRoot),
    Finished(AgentCommandFinished),
    Restored,
}

#[derive(Default)]
struct State {
    live: BTreeMap<AgentCommandRoot, Option<LinuxBirth>>,
    adopted: BTreeSet<AgentCommandRoot>,
    events: VecDeque<(u64, Event)>,
    sequence: u64,
    connection: u64,
    cancel: Option<CancellationToken>,
    storage: Option<PathBuf>,
    unsupported: bool,
}

#[derive(Default)]
struct Declarations {
    state: Mutex<State>,
    changed: Notify,
    persisting: tokio::sync::Mutex<()>,
}

impl State {
    fn push(&mut self, event: Event) {
        self.sequence += 1;
        if !self.unsupported {
            self.events.push_back((self.sequence, event));
        }
    }
}

impl Declarations {
    fn load(&self, storage: &Path) {
        let path = storage.join("agent-command-roots.json");
        let roots = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Vec<SavedRoot>>(&bytes).ok())
            .unwrap_or_default();
        self.state.lock().unwrap_or_else(|e| e.into_inner()).storage = Some(path);
        for root in roots {
            if root.is_current() {
                self.restored(root);
            }
        }
    }

    fn started(&self, root: AgentCommandRoot, native_birth: Option<LinuxBirth>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.live.insert(root, native_birth);
        state.adopted.remove(&root);
        state.push(Event::Started(root));
        drop(state);
        self.changed.notify_one();
    }

    fn restored(&self, saved: SavedRoot) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let std::collections::btree_map::Entry::Vacant(entry) = state.live.entry(saved.root) else {
            return;
        };
        entry.insert(saved.native_birth);
        state.adopted.insert(saved.root);
        state.push(Event::Restored);
        drop(state);
        self.changed.notify_one();
    }

    fn finished(&self, root: AgentCommandRoot, cpu_ms: Option<u64>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.live.remove(&root).is_none() {
            return;
        }
        state.adopted.remove(&root);
        state.push(Event::Finished(AgentCommandFinished {
            pid: root.pid,
            start_time_ms: root.start_time_ms,
            cpu_ms,
        }));
        drop(state);
        self.changed.notify_one();
    }

    fn batch(&self, initial: bool) -> Batch {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut batch = Batch {
            through: state.sequence,
            ..Batch::default()
        };
        let mut known = initial;
        for (_, event) in &state.events {
            match event {
                Event::Started(root) => batch.started.push(*root),
                Event::Finished(root) => batch.finished.push(*root),
                Event::Restored => known = true,
            }
        }
        if known {
            batch.known = Some(state.live.keys().copied().collect());
        }
        batch
    }

    fn connect<F, Fut>(self: &Arc<Self>, send: F) -> Connection
    where
        F: Fn(Batch) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), AgentCommandsError>> + Send,
    {
        let cancel = CancellationToken::new();
        let generation = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(old) = state.cancel.replace(cancel.clone()) {
                old.cancel();
            }
            state.connection += 1;
            state.unsupported = false;
            state.connection
        };
        let declarations = self.clone();
        let stopped = cancel.clone();
        let task = tokio::spawn(async move {
            declarations.drive(generation, stopped, send).await;
        });
        Connection {
            cancel,
            task: Some(task),
        }
    }

    async fn drive<F, Fut>(&self, generation: u64, cancel: CancellationToken, send: F)
    where
        F: Fn(Batch) -> Fut,
        Fut: Future<Output = Result<(), AgentCommandsError>>,
    {
        let mut initial = true;
        loop {
            self.prune_exited();
            let changed = self.changed.notified();
            let batch = self.batch(initial);
            if !initial
                && batch.started.is_empty()
                && batch.finished.is_empty()
                && batch.known.is_none()
            {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = changed => continue,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => continue,
                }
            }
            self.persist().await;
            let mut backoff = Duration::from_millis(50);
            loop {
                let result = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return,
                    result = send(batch.clone()) => result,
                };
                match result {
                    Ok(()) => break,
                    Err(AgentCommandsError::Unsupported) => {
                        static LOGGED: OnceLock<()> = OnceLock::new();
                        LOGGED.get_or_init(|| {
                            crate::slog_debug!("daemon does not support agent command declarations")
                        });
                        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                        state.unsupported = true;
                        state.events.clear();
                        return;
                    }
                    Err(error) if error.is_retryable() => {
                        tokio::select! {
                            _ = cancel.cancelled() => return,
                            _ = tokio::time::sleep(backoff) => {}
                        }
                        backoff = (backoff * 2).min(Duration::from_secs(2));
                    }
                    Err(error) => {
                        crate::slog_warn!("agent command declaration refused: {error}");
                        break;
                    }
                }
            }
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.connection != generation {
                return;
            }
            state
                .events
                .retain(|(sequence, _)| *sequence > batch.through);
            initial = false;
        }
    }

    fn prune_exited(&self) {
        // Only adopted roots need liveness polling. An owned child's waiter must
        // publish its CPU before a missing process-table entry can erase it.
        let roots: Vec<_> = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state
                .adopted
                .iter()
                .filter_map(|root| {
                    state.live.get(root).map(|birth| SavedRoot {
                        root: *root,
                        native_birth: birth.clone(),
                    })
                })
                .collect()
        };
        for root in roots {
            if !root.is_current() {
                self.finished(root.root, None);
            }
        }
    }

    async fn persist(&self) {
        // A replaced connection can still be finishing an atomic write. Serialize
        // snapshots and writes so it cannot overwrite the new connection's roots.
        let _order = self.persisting.lock().await;
        let (path, roots) = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                state.storage.clone(),
                state
                    .live
                    .iter()
                    .map(|(root, birth)| SavedRoot {
                        root: *root,
                        native_birth: birth.clone(),
                    })
                    .collect::<Vec<_>>(),
            )
        };
        if let Some(path) = path {
            let result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let bytes = serde_json::to_vec(&roots)?;
                crate::durability::atomic_write(
                    &path,
                    &bytes,
                    crate::durability::AtomicWriteDurability::FileAndDirectory,
                )
            })
            .await;
            if !matches!(result, Ok(Ok(()))) {
                crate::slog_warn!("failed to retain live agent command identities: {result:?}");
            }
        }
    }
}

fn declarations() -> &'static Arc<Declarations> {
    static DECLARATIONS: OnceLock<Arc<Declarations>> = OnceLock::new();
    DECLARATIONS.get_or_init(|| Arc::new(Declarations::default()))
}

/// Dropping the connection stops retries before another supervised launch takes over.
pub struct Connection {
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Connection {
    pub async fn stop(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub fn connect<F, Fut>(send: F) -> Connection
where
    F: Fn(Batch) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), AgentCommandsError>> + Send,
{
    declarations().connect(send)
}

pub fn enable(storage: &Path) {
    declarations().load(storage);
}

pub fn started(pid: u32) -> Option<AgentCommandRoot> {
    if declarations()
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .storage
        .is_none()
    {
        return None;
    }
    let identity = root_identity(pid)?;
    let root = AgentCommandRoot {
        pid,
        start_time_ms: identity.started_at_ms,
    };
    declarations().started(root, identity.native_birth);
    Some(root)
}

pub(crate) fn finished(root: Option<AgentCommandRoot>, cpu_ms: Option<u64>) {
    if let Some(root) = root {
        declarations().finished(root, cpu_ms);
    }
}

#[cfg(unix)]
pub(crate) fn wait4(
    pid: u32,
    flags: libc::c_int,
    root: Option<AgentCommandRoot>,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    use std::os::unix::process::ExitStatusExt;
    loop {
        let mut status = 0;
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        // The PID is an owned direct child; wait4 initializes usage only on a successful reap.
        let result =
            unsafe { libc::wait4(pid as libc::pid_t, &mut status, flags, usage.as_mut_ptr()) };
        if result > 0 {
            let usage = unsafe { usage.assume_init() };
            let micros = |time: libc::timeval| {
                (time.tv_sec as u64)
                    .saturating_mul(1_000_000)
                    .saturating_add(time.tv_usec as u64)
            };
            let cpu_ms = micros(usage.ru_utime).saturating_add(micros(usage.ru_stime)) / 1_000;
            finished(root, Some(cpu_ms));
            return Ok(Some(std::process::ExitStatus::from_raw(status)));
        }
        if result == 0 {
            return Ok(None);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(windows)]
pub(crate) fn windows_cpu_ms(handle: std::os::windows::io::RawHandle) -> Option<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // The caller retains the process handle until its wait and this query finish.
    if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return None;
    }
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    Some(ticks(kernel).saturating_add(ticks(user)) / 10_000)
}

#[cfg(windows)]
#[derive(Debug)]
pub struct WindowsChild {
    child: std::process::Child,
    root: Option<AgentCommandRoot>,
}

#[cfg(windows)]
impl From<std::process::Child> for WindowsChild {
    fn from(child: std::process::Child) -> Self {
        Self { child, root: None }
    }
}

#[cfg(windows)]
impl std::ops::Deref for WindowsChild {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

#[cfg(windows)]
impl std::ops::DerefMut for WindowsChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

#[cfg(windows)]
impl WindowsChild {
    pub fn declare_agent(&mut self) {
        self.root = started(self.child.id());
    }
    // Preserve the std::process::Child-compatible wait API without exposing unused methods publicly.
    #[allow(dead_code)]
    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.finished();
        }
        Ok(status)
    }
    #[allow(dead_code)]
    pub(crate) fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = self.child.wait()?;
        self.finished();
        Ok(status)
    }
    #[allow(dead_code)]
    fn finished(&self) {
        use std::os::windows::io::AsRawHandle;
        finished(self.root, windows_cpu_ms(self.child.as_raw_handle()));
    }
}

pub fn wait_pty(
    child: &mut dyn portable_pty::Child,
    root: Option<AgentCommandRoot>,
) -> std::io::Result<portable_pty::ExitStatus> {
    #[cfg(unix)]
    if let Some(pid) = child.process_id() {
        return wait4(pid, 0, root)?
            .map(Into::into)
            .ok_or_else(|| std::io::Error::other("wait4 returned no status"));
    }
    let status = child.wait()?;
    if root.is_some() {
        #[cfg(windows)]
        let cpu = child.as_raw_handle().and_then(windows_cpu_ms);
        #[cfg(not(windows))]
        let cpu = None;
        finished(root, cpu);
    }
    Ok(status)
}

pub fn try_wait_pty(
    child: &mut dyn portable_pty::Child,
    root: Option<AgentCommandRoot>,
) -> std::io::Result<Option<portable_pty::ExitStatus>> {
    #[cfg(unix)]
    if let Some(pid) = child.process_id() {
        return wait4(pid, libc::WNOHANG, root).map(|status| status.map(Into::into));
    }
    let status = child.try_wait()?;
    if status.is_some() {
        if root.is_some() {
            #[cfg(windows)]
            let cpu = child.as_raw_handle().and_then(windows_cpu_ms);
            #[cfg(not(windows))]
            let cpu = None;
            finished(root, cpu);
        }
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{mpsc, oneshot};

    type Reply = oneshot::Sender<Result<(), AgentCommandsError>>;

    fn sender(hub: &Arc<Declarations>) -> (Connection, mpsc::UnboundedReceiver<(Batch, Reply)>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let connection = hub.connect(move |batch| {
            let (reply, result) = oneshot::channel();
            tx.send((batch, reply)).unwrap();
            async {
                result
                    .await
                    .unwrap_or(Err(AgentCommandsError::ConnectionClosed))
            }
        });
        (connection, rx)
    }

    async fn receive(rx: &mut mpsc::UnboundedReceiver<(Batch, Reply)>) -> (Batch, Reply) {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap()
    }

    #[cfg(unix)]
    async fn receive_after_persist(
        rx: &mut mpsc::UnboundedReceiver<(Batch, Reply)>,
        phase: &str,
    ) -> (Batch, Reply) {
        // The sender waits for the file and directory to sync. The deadline
        // guards against a hang, not slow filesystem I/O on a shared runner.
        tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{phase}: no batch received within 30 seconds"))
            .unwrap_or_else(|| panic!("{phase}: sender closed"))
    }

    fn root(pid: u32) -> AgentCommandRoot {
        AgentCommandRoot {
            pid,
            start_time_ms: 1_700_000_000_000 + u64::from(pid),
        }
    }

    #[cfg(unix)]
    fn current_root() -> AgentCommandRoot {
        let pid = std::process::id();
        AgentCommandRoot {
            pid,
            start_time_ms: subc_os::process_start_time_ms(pid).unwrap(),
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn clock_step_restores_agent_command_root_and_sends_original_epoch() {
        use crate::process_identity::with_linux_boot_time_for_test;
        let directory = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let pid = std::process::id();
        let saved = with_linux_boot_time_for_test(100, || root_identity(pid).unwrap());
        let root = AgentCommandRoot {
            pid,
            start_time_ms: saved.started_at_ms,
        };
        let legacy = AgentCommandRoot {
            start_time_ms: root.start_time_ms + 1,
            ..root
        };
        let first = Declarations::default();
        first.load(directory.path());
        first.started(root, saved.native_birth);
        first.started(legacy, None);
        rt.block_on(first.persist());

        let bytes = std::fs::read(directory.path().join("agent-command-roots.json")).unwrap();
        let rollback: Vec<AgentCommandRoot> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(rollback, vec![root, legacy]);
        let saved: Vec<SavedRoot> = serde_json::from_slice(&bytes).unwrap();
        assert!(saved[0].native_birth.is_some());
        assert!(saved[1].native_birth.is_none());

        with_linux_boot_time_for_test(200, || {
            rt.block_on(async {
                assert_ne!(
                    root_identity(pid).unwrap().started_at_ms,
                    root.start_time_ms
                );
                let replay = Arc::new(Declarations::default());
                replay.load(directory.path());
                let (connection, mut rx) = sender(&replay);
                let (batch, reply) = receive_after_persist(&mut rx, "clock-step replay").await;
                assert_eq!(batch.known, Some(vec![root]));
                assert!(batch.started.is_empty());
                assert!(batch.finished.is_empty());
                reply.send(Ok(())).unwrap();
                connection.stop().await;

                replay
                    .state
                    .lock()
                    .unwrap()
                    .live
                    .get_mut(&root)
                    .unwrap()
                    .as_mut()
                    .unwrap()
                    .start_ticks += 1;
                replay.prune_exited();
                let finished = replay.batch(true);
                assert!(finished.known.unwrap().is_empty());
                assert_eq!(
                    finished.finished,
                    vec![AgentCommandFinished {
                        pid,
                        start_time_ms: root.start_time_ms,
                        cpu_ms: None,
                    }]
                );
            })
        });
    }

    #[test]
    fn agent_commands_reused_pid_finishes_only_the_original_kernel_identity() {
        let hub = Declarations::default();
        let old = root(42);
        let new = AgentCommandRoot {
            start_time_ms: old.start_time_ms + 1,
            ..old
        };
        hub.started(old, None);
        hub.started(new, None);
        hub.finished(old, Some(250));
        let batch = hub.batch(true);
        assert_eq!(batch.known, Some(vec![new]));
        assert_eq!(
            batch.finished,
            [AgentCommandFinished {
                pid: old.pid,
                start_time_ms: old.start_time_ms,
                cpu_ms: Some(250)
            }]
        );
        hub.finished(old, Some(250));
        assert_eq!(
            hub.batch(true).finished.len(),
            1,
            "cached waits must not finish twice or settle the reused PID"
        );
    }

    #[tokio::test]
    async fn agent_commands_start_finish_cpu_and_known_after_reconnect() {
        let hub = Arc::new(Declarations::default());
        let live = root(42);
        let ended = root(43);
        hub.started(live, None);
        hub.started(ended, None);
        hub.finished(ended, Some(250));
        let (connection, mut rx) = sender(&hub);
        let (batch, reply) = receive(&mut rx).await;
        assert_eq!(batch.started, [live, ended]);
        assert_eq!(
            batch.finished,
            [AgentCommandFinished {
                pid: ended.pid,
                start_time_ms: ended.start_time_ms,
                cpu_ms: Some(250)
            }]
        );
        assert_eq!(batch.known, Some(vec![live]));
        reply.send(Ok(())).unwrap();
        tokio::task::yield_now().await;
        drop(connection);
        let (_connection, mut reconnected) = sender(&hub);
        let (batch, reply) = receive(&mut reconnected).await;
        assert!(batch.started.is_empty());
        assert!(batch.finished.is_empty());
        assert_eq!(batch.known, Some(vec![live]));
        reply.send(Ok(())).unwrap();
        hub.finished(live, None);
        let (batch, reply) = receive(&mut reconnected).await;
        assert_eq!(batch.finished[0].cpu_ms, None);
        let wire = serde_json::to_value(batch.finished[0]).unwrap();
        assert!(wire.get("cpu_ms").is_none());
        reply.send(Ok(())).unwrap();
    }

    #[tokio::test]
    async fn agent_commands_busy_retries_the_same_roots_before_later_finishes() {
        let hub = Arc::new(Declarations::default());
        let live = root(42);
        hub.started(live, None);
        let (_connection, mut rx) = sender(&hub);
        let (first, reply) = receive(&mut rx).await;
        reply
            .send(Err(AgentCommandsError::Refused(Box::new(
                subc_protocol::ErrorBody::new("busy", "try again"),
            ))))
            .unwrap();
        hub.finished(live, Some(19));
        let (retry, reply) = receive(&mut rx).await;
        assert_eq!(retry.started, first.started);
        assert_eq!(retry.finished, first.finished);
        assert_eq!(retry.known, first.known);
        reply.send(Ok(())).unwrap();
        let (next, reply) = receive(&mut rx).await;
        assert!(next.started.is_empty());
        assert_eq!(next.finished[0].cpu_ms, Some(19));
        assert!(next.known.is_none());
        reply.send(Ok(())).unwrap();
    }

    #[tokio::test]
    async fn agent_commands_unsupported_is_quiet_and_stops_retrying() {
        let hub = Arc::new(Declarations::default());
        let (_connection, mut rx) = sender(&hub);
        let (_, reply) = receive(&mut rx).await;
        reply.send(Err(AgentCommandsError::Unsupported)).unwrap();
        tokio::task::yield_now().await;
        hub.started(root(42), None);
        assert!(rx.recv().await.is_none());
        assert!(hub.state.lock().unwrap().events.is_empty());
    }

    #[tokio::test]
    async fn agent_commands_reconnect_cancels_the_stale_connection_and_keeps_unacked_roots() {
        let hub = Arc::new(Declarations::default());
        hub.started(root(42), None);
        let (_old, mut first) = sender(&hub);
        let (_, old_reply) = receive(&mut first).await;
        let (_current, mut current) = sender(&hub);
        let (batch, reply) = receive(&mut current).await;
        assert_eq!(batch.started, [root(42)]);
        assert_eq!(batch.known, Some(vec![root(42)]));
        tokio::task::yield_now().await;
        assert!(old_reply.is_closed());
        reply.send(Ok(())).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_commands_persists_before_send_and_recovers_without_a_project_bind() {
        let storage = tempfile::tempdir().unwrap();
        let root = current_root();
        let first = Arc::new(Declarations::default());
        first.load(storage.path());
        first.started(root, None);
        let (connection, mut rx) = sender(&first);
        let (_, reply) = receive_after_persist(&mut rx, "first send before restart").await;
        let saved: Vec<AgentCommandRoot> = serde_json::from_slice(
            &std::fs::read(storage.path().join("agent-command-roots.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved, [root]);
        reply.send(Ok(())).unwrap();
        connection.stop().await;

        let recovered = Arc::new(Declarations::default());
        recovered.load(storage.path());
        let (connection, mut rx) = sender(&recovered);
        let (batch, reply) = receive_after_persist(&mut rx, "first send after restart").await;
        assert_eq!(batch.known, Some(vec![root]));
        assert!(batch.started.is_empty());
        reply.send(Ok(())).unwrap();
        connection.stop().await;
    }

    #[cfg(unix)]
    #[test]
    fn agent_commands_waits_for_slow_persistence_before_and_after_restart() {
        // Use one blocking thread so the atomic write runs on the thread
        // where the durability hook is installed.
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .max_blocking_threads(1)
            .build()
            .unwrap()
            .block_on(async {
                let storage = tempfile::tempdir().unwrap();
                let root = current_root();
                let first = Arc::new(Declarations::default());
                first.load(storage.path());
                first.started(root, None);
                assert_slow_write_blocks_send(&first, root, "first send before restart").await;

                let recovered = Arc::new(Declarations::default());
                recovered.load(storage.path());
                assert_slow_write_blocks_send(&recovered, root, "first send after restart").await;
            });
    }

    #[cfg(unix)]
    async fn assert_slow_write_blocks_send(
        hub: &Arc<Declarations>,
        root: AgentCommandRoot,
        phase: &str,
    ) {
        let (entered, writing) = oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        tokio::task::spawn_blocking(move || {
            crate::durability::BEFORE_TEMP_CREATE.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move |_| {
                    entered.send(()).unwrap();
                    let _ = blocked.recv();
                }));
            });
        })
        .await
        .unwrap();

        let (connection, mut rx) = sender(hub);
        let receive = receive_after_persist(&mut rx, phase);
        tokio::pin!(receive);
        tokio::select! {
            biased;
            _ = &mut receive => panic!("{phase}: batch arrived before persistence"),
            entered = writing => entered.unwrap(),
        }
        // The old two-second receive deadline expired while the write was held.
        tokio::time::advance(Duration::from_millis(2_001)).await;
        tokio::select! {
            biased;
            _ = &mut receive => panic!("{phase}: batch arrived before persistence"),
            () = std::future::ready(()) => {},
        }
        drop(release);

        let (batch, reply) = receive.await;
        assert_eq!(batch.known, Some(vec![root]));
        reply.send(Ok(())).unwrap();
        connection.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_commands_restored_roots_use_kernel_identity_and_prune_when_gone() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .unwrap();
        let root = AgentCommandRoot {
            pid: child.id(),
            start_time_ms: subc_os::process_start_time_ms(child.id()).unwrap(),
        };
        let hub = Arc::new(Declarations::default());
        hub.restored(SavedRoot {
            root,
            native_birth: None,
        });
        let (_connection, mut rx) = sender(&hub);
        let (batch, reply) = receive(&mut rx).await;
        assert_eq!(batch.known, Some(vec![root]));
        assert!(
            batch.started.is_empty(),
            "known must not introduce a new start after restart"
        );
        reply.send(Ok(())).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        hub.prune_exited();
        let (batch, reply) = receive(&mut rx).await;
        assert_eq!(
            batch.finished,
            [AgentCommandFinished {
                pid: root.pid,
                start_time_ms: root.start_time_ms,
                cpu_ms: None
            }]
        );
        reply.send(Ok(())).unwrap();
    }
}
