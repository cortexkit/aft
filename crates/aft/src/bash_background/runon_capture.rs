//! Coordinate local foreground shells with remote checkout capture.

use super::BgTask;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

const WAIT_CAP: Duration = Duration::from_secs(30);
pub(super) const WARNING: &str =
    "Copied while another command in this checkout was still running; its changes may be missing from the job.";

pub(super) fn render_warning(
    metadata: &super::PersistedTask,
    note: Option<String>,
) -> Option<String> {
    if metadata
        .remote
        .as_ref()
        .is_some_and(|remote| remote.copy_warning)
    {
        Some(note.map_or_else(|| WARNING.to_owned(), |note| format!("{note}\n{WARNING}")))
    } else {
        note
    }
}

struct Command {
    harness: Option<String>,
    session: String,
    workdir: PathBuf,
}

struct CopyWindow {
    harness: Option<String>,
    session: String,
    checkout: PathBuf,
    overlap: bool,
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

pub(super) struct LocalCommand(String);

impl LocalCommand {
    pub(super) fn start(task: &BgTask, workdir: &Path) -> Self {
        let command = Command {
            harness: task.db_harness.clone(),
            session: task.session_id.clone(),
            workdir: super::canonicalized_path(workdir),
        };
        let mut activity = activity();
        for copy in activity.copying.values_mut() {
            // Latch the start even if the command ends before acceptance arrives.
            copy.overlap |= copy.includes(&command);
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
        #[cfg(test)]
        let test = TEST_CAPS.lock().unwrap().get(&self.checkout).cloned();
        #[cfg(test)]
        let cap = test.as_ref().map_or(cap, |test| test.cap);
        let deadline = tokio::time::Instant::now() + cap;
        let mut window = CopyWindow {
            harness: self.task.db_harness.clone(),
            session: self.task.session_id.clone(),
            checkout: self.checkout.clone(),
            overlap: false,
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
                let mut running = false;
                for (id, command) in &activity.foreground {
                    if window.includes(command) {
                        running = true;
                        waited_for.insert(id.clone());
                    }
                }
                let cap_hit = running && tokio::time::Instant::now() >= deadline;
                if !running || cap_hit {
                    // Start observing under the same lock as the last foreground check,
                    // so no command can start between checking and arming the window.
                    window.overlap = cap_hit;
                    let id = activity.next_copy;
                    activity.next_copy += 1;
                    activity.copying.insert(id, window);
                    break (id, cap_hit);
                }
            }
            #[cfg(test)]
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
        if activity()
            .copying
            .remove(&self.id)
            .is_some_and(|window| window.overlap)
        {
            self.task.runon_copy_warning.store(true, Ordering::Release);
        }
    }
}

#[cfg(test)]
#[derive(Clone)]
struct TestWait {
    cap: Duration,
    started: Arc<Notify>,
}

#[cfg(test)]
static TEST_CAPS: LazyLock<Mutex<HashMap<PathBuf, TestWait>>> = LazyLock::new(Mutex::default);

#[cfg(test)]
pub(super) struct TestWaitCap {
    checkout: PathBuf,
    started: Arc<Notify>,
}

#[cfg(test)]
impl TestWaitCap {
    pub(super) async fn wait_started(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.started.notified())
            .await
            .expect("runon must wait for the foreground editor");
    }
}

#[cfg(test)]
impl Drop for TestWaitCap {
    fn drop(&mut self) {
        TEST_CAPS.lock().unwrap().remove(&self.checkout);
    }
}

#[cfg(test)]
pub(super) fn set_test_cap(checkout: &Path, cap: Duration) -> TestWaitCap {
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
