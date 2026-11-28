//! Coordinate local foreground shells with remote checkout capture.

use super::{remote::RemoteTask, BgTask};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

const WAIT_CAP: Duration = Duration::from_secs(30);
const COMMAND_CHARS: usize = 80;

pub(super) type Overlaps = BTreeMap<String, String>;

pub(super) fn render_warning(
    metadata: &super::PersistedTask,
    note: Option<String>,
) -> Option<String> {
    let Some(warning) = metadata.remote.as_ref().map(|remote| &remote.copy_warning) else {
        return note;
    };
    if !warning.active {
        return note;
    }
    let commands = &warning.commands;
    let detail = match commands.first_key_value() {
        None => String::new(),
        Some((_, command)) if commands.len() == 1 => format!(" (`{command}`)"),
        Some((_, command)) => format!(" (`{command}` and {} more)", commands.len() - 1),
    };
    let warning = format!("Copied while another command in this checkout was still running{detail}; its changes may be missing from the job.");
    Some(match note {
        Some(note) => format!("{note}\n{warning}"),
        None => warning,
    })
}

pub(super) fn store_warning(task: &BgTask, remote: &mut RemoteTask) {
    let overlaps = task
        .runon_copy_overlaps
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !overlaps.is_empty() {
        remote.copy_warning.active = true;
        remote.copy_warning.commands.extend(overlaps.clone());
    }
}

fn short_command(command: &str) -> String {
    // Mask credentials before shortening, since a partial token may not match.
    let command = crate::log_redact::redact_command(command);
    let line = command.lines().next().unwrap_or_default();
    match line.char_indices().nth(COMMAND_CHARS) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_owned(),
    }
}

struct Command {
    harness: Option<String>,
    session: String,
    workdir: PathBuf,
    short_line: String,
}

struct CopyWindow {
    harness: Option<String>,
    session: String,
    checkout: PathBuf,
    overlaps: Overlaps,
}

impl CopyWindow {
    fn includes(&self, command: &Command) -> bool {
        self.harness == command.harness
            && self.session == command.session
            && command.workdir.starts_with(&self.checkout)
    }
}

#[derive(Default)]
struct Activity {
    foreground: HashMap<String, Command>,
    copying: HashMap<u64, CopyWindow>,
    next_copy: u64,
}

static ACTIVITY: LazyLock<Mutex<Activity>> = LazyLock::new(Mutex::default);
static FINISHED: Notify = Notify::const_new();

fn activity() -> MutexGuard<'static, Activity> {
    ACTIVITY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) fn track_local(task: &BgTask, workdir: &Path, command: &str) {
    task.local_capture_command
        .get_or_init(|| LocalCommand::start(task, workdir, command));
}

pub(super) struct LocalCommand(String);

impl LocalCommand {
    pub(super) fn start(task: &BgTask, workdir: &Path, command: &str) -> Self {
        let command = Command {
            harness: task.db_harness.clone(),
            session: task.session_id.clone(),
            workdir: super::canonicalized_path(workdir),
            short_line: short_command(command),
        };
        let mut activity = activity();
        for copy in activity.copying.values_mut() {
            // Latch the start even if the command ends before acceptance arrives.
            if copy.includes(&command) {
                copy.overlaps
                    .insert(task.task_id.clone(), command.short_line.clone());
            }
        }
        activity.foreground.insert(task.task_id.clone(), command);
        Self(task.task_id.clone())
    }

    pub(super) fn finish(&self) {
        if activity().foreground.remove(&self.0).is_some() {
            FINISHED.notify_waiters();
        }
    }
}

impl Drop for LocalCommand {
    fn drop(&mut self) {
        self.finish();
    }
}

#[derive(Clone)]
pub(crate) struct Submission {
    task: Arc<BgTask>,
    checkout: PathBuf,
}

impl Submission {
    pub(super) fn new(task: Arc<BgTask>, checkout: &Path) -> Self {
        Self {
            task,
            checkout: checkout.to_owned(),
        }
    }

    pub(crate) async fn before_send(&self) -> Result<Copy, crate::exec_remote::Error> {
        let started = Instant::now();
        let cap = WAIT_CAP;
        #[cfg(any(test, feature = "test-support"))]
        let test = TEST_CAPS.lock().unwrap().get(&self.checkout).cloned();
        #[cfg(any(test, feature = "test-support"))]
        let cap = test.as_ref().map_or(cap, |test| test.cap);
        let deadline = tokio::time::Instant::now() + cap;
        let mut window = CopyWindow {
            harness: self.task.db_harness.clone(),
            session: self.task.session_id.clone(),
            checkout: self.checkout.clone(),
            overlaps: Overlaps::new(),
        };
        let mut waited_for = BTreeSet::new();
        let (id, cap_hit) = loop {
            let finished = FINISHED.notified();
            let cancelled = self.task.remote_cancel_notify.notified();
            if self.task.state.lock().map_or(true, |state| {
                state.metadata.is_terminal()
                    || state
                        .metadata
                        .remote
                        .as_ref()
                        .is_none_or(|remote| remote.cancel_requested)
            }) {
                return Err(crate::exec_remote::Error::CancelledBeforeCapture);
            }
            {
                let mut activity = activity();
                let matching = activity
                    .foreground
                    .iter()
                    .filter(|(_, command)| window.includes(command));
                let mut running = false;
                for (id, _) in matching.clone() {
                    running = true;
                    waited_for.insert(id.clone());
                }
                let cap_hit = running && tokio::time::Instant::now() >= deadline;
                if !running || cap_hit {
                    // Start observing under the same lock as the last foreground check,
                    // so no command can start between checking and arming the window.
                    if cap_hit {
                        window.overlaps = matching
                            .map(|(id, command)| (id.clone(), command.short_line.clone()))
                            .collect();
                    }
                    let id = activity.next_copy;
                    activity.next_copy += 1;
                    activity.copying.insert(id, window);
                    break (id, cap_hit);
                }
            }
            #[cfg(any(test, feature = "test-support"))]
            if let Some(test) = &test {
                test.started.notify_one();
            }
            tokio::select! {
                biased;
                _ = cancelled => {},
                _ = finished => {},
                _ = tokio::time::sleep_until(deadline) => {},
            }
        };
        if !waited_for.is_empty() {
            crate::slog_info!(
                "runon: foreground capture wait task_id={} waited_ms={} other_task_ids={waited_for:?} cap_hit={cap_hit}",
                self.task.task_id,
                started.elapsed().as_millis(),
            );
        }
        Ok(Copy {
            id,
            task: Arc::clone(&self.task),
        })
    }
}

// The runner copies after exec.run, and Accepted is the first evidence AFT
// gets that it finished. Observe that whole interval rather than guessing at
// the runner's actual start and end times.
pub(crate) struct Copy {
    id: u64,
    task: Arc<BgTask>,
}

impl Copy {
    pub(crate) fn discard(self) {
        activity().copying.remove(&self.id);
    }
}

impl Drop for Copy {
    fn drop(&mut self) {
        let window = activity().copying.remove(&self.id);
        if let Some(window) = window.filter(|window| !window.overlaps.is_empty()) {
            self.task
                .runon_copy_overlaps
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend(window.overlaps);
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Clone)]
struct TestWait {
    cap: Duration,
    started: Arc<Notify>,
}

#[cfg(any(test, feature = "test-support"))]
static TEST_CAPS: LazyLock<Mutex<HashMap<PathBuf, TestWait>>> = LazyLock::new(Mutex::default);

#[cfg(any(test, feature = "test-support"))]
pub struct TestWaitCap {
    checkout: PathBuf,
    started: Arc<Notify>,
}

#[cfg(any(test, feature = "test-support"))]
impl TestWaitCap {
    pub async fn wait_started(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.started.notified())
            .await
            .expect("runon must wait for the foreground editor");
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestWaitCap {
    fn drop(&mut self) {
        TEST_CAPS.lock().unwrap().remove(&self.checkout);
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn set_test_cap(checkout: &Path, cap: Duration) -> TestWaitCap {
    let checkout = super::canonicalized_path(checkout);
    let started = Arc::new(Notify::new());
    TEST_CAPS.lock().unwrap().insert(
        checkout.clone(),
        TestWait {
            cap,
            started: Arc::clone(&started),
        },
    );
    TestWaitCap { checkout, started }
}
