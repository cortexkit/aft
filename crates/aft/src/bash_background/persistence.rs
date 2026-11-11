#[cfg(all(test, unix))]
use std::ffi::CString;
use std::ffi::{OsStr, OsString};
#[cfg(any(windows, test))]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(all(test, unix))]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::backup::hash_session;
use crate::bash_permissions::PermissionAsk;
use crate::db::bash_tasks::BashTaskRow;

use super::process::{LiveDescendant, LiveDescendantsUnavailable, ProcessIdentity};
use super::BgTaskStatus;

pub(crate) use crate::pinned_dir::*;

#[cfg(test)]
type SessionCreateGate = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);

#[cfg(test)]
static SESSION_CREATE_GATES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, SessionCreateGate>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
fn install_session_create_gate_for_test(
    session_dir: &Path,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let previous = SESSION_CREATE_GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session_dir.to_path_buf(), (entered_tx, release_rx));
    assert!(previous.is_none(), "session create gate already installed");
    (entered_rx, release_tx)
}

#[cfg(test)]
fn wait_on_session_create_gate_for_test(session_dir: &Path) {
    let gate = SESSION_CREATE_GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(session_dir);
    if let Some((entered, release)) = gate {
        let _ = entered.send(());
        let _ = release.recv();
    }
}

pub const SCHEMA_VERSION: u32 = 7;
pub(crate) const CONTROL_DIR: &str = "control";
const IO_DIR: &str = "io";
pub(crate) const METADATA_FILE: &str = "metadata.json";
const TASK_LAYOUT_NOT_FOUND: &str = "background task layout not found";
pub const COMMAND_FILE: &str = "command.sh";
pub const WRAPPER_FILE: &str = "wrapper.sh";
pub const ENVIRONMENT_FILE: &str = "environment.bin";
pub const MANIFEST_FILE: &str = "manifest.blake3";
pub const SANDBOX_PROFILE_FILE: &str = "sandbox-profile.json";

// Directory-layout and legacy flat-layout names share one visibility list.
// The launch environment is deliberately absent: it can contain call secrets.
const READABLE_CONTROL_FILES: [(&str, &str); 5] = [
    (METADATA_FILE, "json"),
    (COMMAND_FILE, "sh"),
    (WRAPPER_FILE, "wrapper.sh"),
    (MANIFEST_FILE, "manifest"),
    (SANDBOX_PROFILE_FILE, "sandbox-profile.json"),
];

pub fn readable_task_file_names(layout: TaskLayout) -> Vec<String> {
    READABLE_CONTROL_FILES
        .iter()
        .map(|(directory, flat)| match layout {
            TaskLayout::Directory => format!("{CONTROL_DIR}/{directory}"),
            TaskLayout::Flat => flat.to_string(),
        })
        .chain(TaskArtifact::ALL.into_iter().map(|artifact| match layout {
            TaskLayout::Directory => format!("{IO_DIR}/{}", artifact.file_name()),
            TaskLayout::Flat => artifact.flat_extension().to_string(),
        }))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskLayout {
    Flat,
    Directory,
}

#[derive(Debug, Clone)]
pub struct TaskPaths {
    pub layout: TaskLayout,
    pub task_id: String,
    pub session_dir: PathBuf,
    /// Root directory for this task's persisted artifacts; legacy flat-layout tasks
    /// use the session directory instead of a per-task directory.
    pub dir: PathBuf,
    pub control_dir: PathBuf,
    pub io_dir: PathBuf,
    pub json: PathBuf,
    pub stdout: PathBuf,
    pub stderr: PathBuf,
    pub exit: PathBuf,
    pub pipeline_status: PathBuf,
    pub pty: PathBuf,
    pub sandbox_unavailable: PathBuf,
    pub command: PathBuf,
    pub wrapper: PathBuf,
    pub environment: PathBuf,
    pub manifest: PathBuf,
    pub sandbox_profile: PathBuf,
}

impl TaskPaths {
    fn directory(session_dir: PathBuf, task_id: &str) -> Self {
        let dir = session_dir.join(task_id);
        let control_dir = dir.join(CONTROL_DIR);
        let io_dir = dir.join(IO_DIR);
        Self {
            layout: TaskLayout::Directory,
            task_id: task_id.to_string(),
            session_dir,
            dir,
            json: control_dir.join(METADATA_FILE),
            stdout: io_dir.join(TaskArtifact::Stdout.file_name()),
            stderr: io_dir.join(TaskArtifact::Stderr.file_name()),
            exit: io_dir.join(TaskArtifact::Exit.file_name()),
            pipeline_status: io_dir.join(TaskArtifact::PipelineStatus.file_name()),
            pty: io_dir.join(TaskArtifact::Pty.file_name()),
            sandbox_unavailable: io_dir.join(TaskArtifact::SandboxUnavailable.file_name()),
            command: control_dir.join(COMMAND_FILE),
            wrapper: control_dir.join(WRAPPER_FILE),
            environment: control_dir.join(ENVIRONMENT_FILE),
            manifest: control_dir.join(MANIFEST_FILE),
            sandbox_profile: control_dir.join(SANDBOX_PROFILE_FILE),
            control_dir,
            io_dir,
        }
    }

    fn flat(session_dir: PathBuf, task_id: &str) -> Self {
        let prefix = |extension: &str| session_dir.join(format!("{task_id}.{extension}"));
        Self {
            layout: TaskLayout::Flat,
            task_id: task_id.to_string(),
            dir: session_dir.clone(),
            control_dir: session_dir.clone(),
            io_dir: session_dir.clone(),
            json: prefix("json"),
            stdout: prefix("stdout"),
            stderr: prefix("stderr"),
            exit: prefix("exit"),
            pipeline_status: prefix("pipeline-status"),
            pty: prefix("pty"),
            sandbox_unavailable: prefix("sandbox-unavailable"),
            command: prefix("sh"),
            wrapper: prefix("wrapper.sh"),
            environment: prefix("env"),
            manifest: prefix("manifest"),
            sandbox_profile: prefix("sandbox-profile.json"),
            session_dir,
        }
    }

    pub fn artifact_path(&self, artifact: TaskArtifact) -> &Path {
        match artifact {
            TaskArtifact::Stdout => &self.stdout,
            TaskArtifact::Stderr => &self.stderr,
            TaskArtifact::Exit => &self.exit,
            TaskArtifact::PipelineStatus => &self.pipeline_status,
            TaskArtifact::Pty => &self.pty,
            TaskArtifact::SandboxUnavailable => &self.sandbox_unavailable,
        }
    }

    /// Exact bundle files visible to authorized readers. The launch environment
    /// is intentionally absent even when the caller may inspect task records.
    pub(crate) fn readable_files(&self) -> impl Iterator<Item = PathBuf> + '_ {
        readable_task_file_names(self.layout)
            .into_iter()
            .map(|name| match self.layout {
                TaskLayout::Directory => self.dir.join(name),
                TaskLayout::Flat => self.session_dir.join(format!("{}.{name}", self.task_id)),
            })
    }

    fn artifact_name(&self, artifact: TaskArtifact) -> OsString {
        match self.layout {
            TaskLayout::Directory => OsString::from(artifact.file_name()),
            TaskLayout::Flat => {
                OsString::from(format!("{}.{}", self.task_id, artifact.flat_extension()))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskArtifact {
    Stdout,
    Stderr,
    Exit,
    PipelineStatus,
    Pty,
    SandboxUnavailable,
}

impl TaskArtifact {
    pub const ALL: [Self; 6] = [
        Self::Stdout,
        Self::Stderr,
        Self::Exit,
        Self::PipelineStatus,
        Self::Pty,
        Self::SandboxUnavailable,
    ];

    pub fn file_name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Exit => "exit",
            Self::PipelineStatus => "pipeline-status",
            Self::Pty => "pty",
            Self::SandboxUnavailable => "sandbox-unavailable",
        }
    }

    fn flat_extension(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Exit => "exit",
            Self::PipelineStatus => "pipeline-status",
            Self::Pty => "pty",
            Self::SandboxUnavailable => "sandbox-unavailable",
        }
    }
}

#[derive(Debug)]
pub struct TaskDirs {
    pub session: Arc<PinnedDir>,
    pub task: Arc<PinnedDir>,
    pub control: Arc<PinnedDir>,
    pub io: Arc<PinnedDir>,
}

impl Clone for TaskDirs {
    fn clone(&self) -> Self {
        Self {
            session: Arc::clone(&self.session),
            task: Arc::clone(&self.task),
            control: Arc::clone(&self.control),
            io: Arc::clone(&self.io),
        }
    }
}

#[derive(Debug)]
pub struct ResolvedTask {
    pub paths: TaskPaths,
    pub dirs: TaskDirs,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum BgMode {
    #[default]
    Pipes,
    Pty,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedCompletionPreview {
    pub output_preview: String,
    pub output_truncated: bool,
    pub bash_output_list_envelope: Option<crate::list_envelope::ListEnvelope>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedTask {
    pub schema_version: u32,
    /// Storage namespace captured at spawn, independent of later route binds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    pub task_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launching_identity: Option<super::TaskIdentity>,
    pub command: String,
    /// Scanner-derived segment labels retained so completion rendering does not
    /// need to parse the command again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pipeline_segments: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline_status_unavailable: Option<String>,
    #[serde(default)]
    pub mode: BgMode,
    pub workdir: PathBuf,
    #[serde(default)]
    pub project_root: Option<PathBuf>,
    pub status: BgTaskStatus,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    pub duration_ms: Option<u64>,
    pub timeout_ms: Option<u64>,
    /// Whether `timeout_ms` is AFT's default background limit rather than a
    /// caller's `timeout`. Only the default may be extended while a delegated
    /// worker waits on the task, and that must still hold after a restart
    /// reloads the task from this record. Absent (false) on records written
    /// before AFT recorded it; such a task is never extended.
    #[serde(default, skip_serializing_if = "is_false")]
    pub default_hard_kill: bool,
    /// Windows remote exit codes can exceed i32::MAX. Using i64 preserves all
    /// their bits and also stores signed local process exit codes unchanged.
    pub exit_code: Option<i64>,
    /// Why the exit marker file is absent; an exact known code may still be in `exit_code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_marker_unavailable: Option<String>,
    pub child_pid: Option<u32>,
    pub pgid: Option<i32>,
    /// Process start time recorded for `child_pid`, in Unix milliseconds.
    /// Used to detect when the OS reuses that PID for a different process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_start_time_ms: Option<u64>,
    /// `Some` records a completed check of the task's process group, including
    /// an empty group. `None` means it could not be checked; the reason is in
    /// `live_descendants_unavailable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_descendants: Option<Vec<LiveDescendant>>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub live_descendants_omitted: usize,
    /// Why the last check of the process group failed, was not possible, or
    /// (`listing_incomplete`) saw only part of the group; the members it did
    /// find stay in `live_descendants`. Cleared by a complete check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_descendants_unavailable: Option<LiveDescendantsUnavailable>,
    /// AFT sent the kill signal to the task's process group (a timeout,
    /// `bash_kill`, or a cancelled call), so any member still listed in
    /// `live_descendants` survived that kill.
    #[serde(default, skip_serializing_if = "is_false")]
    pub group_kill_sent: bool,
    /// The identities of the group members the latest check found: every one
    /// of them, not only the members shown, up to `GROUP_IDENTITY_CAP`. A
    /// later kill of the finished task signals its group only if the leader or
    /// one of these processes is still running in it; otherwise the group
    /// number may by then belong to an unrelated group. Kept through a failed
    /// or incomplete check that found nobody, cleared by a clean one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_member_identities: Vec<ProcessIdentity>,
    /// The latest check found more members than `GROUP_IDENTITY_CAP`, so
    /// `group_member_identities` holds only the first of them by pid.
    #[serde(default, skip_serializing_if = "is_false")]
    pub group_member_identities_truncated: bool,
    pub completion_delivered: bool,
    #[serde(default = "default_notify_on_completion")]
    pub notify_on_completion: bool,
    #[serde(default = "default_compressed")]
    pub compressed: bool,
    /// Whether the launch command contained secret placeholders. Missing on older
    /// records, which cannot prove that restart previews are safe to show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_has_secret_placeholders: Option<bool>,
    /// Whether the child environment held any completion secret, including
    /// AFT's memory-only gh-shim ticket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_has_env_secrets: Option<bool>,
    /// Whether completion secrets include values other than AFT's revoked-on-
    /// restart gh-shim ticket. On older records this field is absent, so the
    /// completion preview remains withheld unless `completion_has_env_secrets`
    /// explicitly says the child environment held no secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_has_non_ticket_env_secrets: Option<bool>,
    /// BLAKE3 digest of this task's AFT gh-shim ticket; the ticket itself is
    /// never persisted. Used only to redact that exact value from replay output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_ticket_digest: Option<String>,
    /// Fully rendered, credential-masked notice text, saved before delivery so
    /// replay never needs the original environment values to reproduce it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_preview: Option<PersistedCompletionPreview>,
    #[serde(default)]
    pub pty_rows: Option<u16>,
    #[serde(default)]
    pub pty_cols: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scanner_report: Vec<PermissionAsk>,
    #[serde(default)]
    pub sandbox_native: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_temp_dir: Option<PathBuf>,
    /// When the finished command's scratch directory (`sandbox_temp_dir`)
    /// may be deleted, in Unix milliseconds. Scratch is kept for a while after
    /// the command exits so the same session can still read files it wrote
    /// there. Stored in the record so a restarted daemon deletes it on time
    /// instead of either forgetting it or deleting it early. Absent on running
    /// tasks and on records written before AFT kept scratch after exit; those
    /// derive the time from `finished_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_temp_delete_at: Option<u64>,
    pub status_reason: Option<String>,
    /// Capture did not finish cleanly; independent of the command's exit status.
    #[serde(default, skip_serializing_if = "is_false")]
    pub output_incomplete: bool,
    /// Who started the task and the key they gave the call. Absent on records
    /// written before AFT recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_key: Option<super::TaskCallKey>,
    /// The v1 `bash` schema pin the call carried, if any. Absent on pinless
    /// calls and on records written before AFT recorded it. An absent pin
    /// means the task gets no view record. The task-record format keeps its
    /// version: older JSON without this field still reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_pin: Option<String>,
    /// Original display identity and once-only terminal publication fence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) display: Option<crate::subc::rich_ui::display::TaskDisplay>,
    /// The daemon's scope owner, reference and epoch at spawn. A reconnect
    /// must not relabel a running command as belonging to the new route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<subc_protocol::scope::ScopeStamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) remote: Option<super::registry::remote::RemoteTask>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) memory_policy: Option<super::memory_limit::MemoryPolicy>,
    /// Missing job-wide output ranges, independent of execution outcome.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incomplete_output: Vec<(u64, u64)>,
    /// Caller-side removals, names only; never environment values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stripped_env_names: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub local_fallback_started: bool,
    /// The call that was waiting on this task was cancelled (client cancel,
    /// transport loss, restart) while the task kept running, and the session
    /// has not yet been told. Its next bash-family reply names the task once
    /// and clears this. Persisted so the notice survives an AFT restart.
    #[serde(default, skip_serializing_if = "is_false")]
    pub cancelled_call_unannounced: bool,
    /// Successful completion offers to an acknowledging runner, including the
    /// first offer. Persisted so reconnect budgets survive daemon restarts.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) runner_completion_offers: usize,
    /// The saved folder grants this command's sandbox allowed, so revoking
    /// one can stop it. Persisted so a restarted AFT still knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) folder_grants: Option<super::registry::folder_grants::TaskFolderGrants>,
}

fn default_notify_on_completion() -> bool {
    true
}

fn default_compressed() -> bool {
    true
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitMarker {
    Code(i32),
    Killed,
}

impl PersistedTask {
    #[allow(clippy::too_many_arguments)]
    pub fn starting(
        task_id: String,
        session_id: String,
        command: String,
        workdir: PathBuf,
        project_root: Option<PathBuf>,
        timeout_ms: Option<u64>,
        notify_on_completion: bool,
        compressed: bool,
    ) -> Self {
        let call_key = Some(super::call_key_for_new_task(&task_id));
        let completion_has_secret_placeholders = Some(!super::secrets::labels(&command).is_empty());
        let metadata = Self {
            schema_version: SCHEMA_VERSION,
            harness: super::route_harness().map(|harness| harness.storage_segment()),
            launching_identity: super::TaskIdentity::current(),
            task_id,
            session_id,
            command,
            pipeline_segments: Vec::new(),
            pipeline_status_unavailable: None,
            mode: BgMode::Pipes,
            workdir,
            project_root,
            status: BgTaskStatus::Starting,
            started_at: unix_millis(),
            finished_at: None,
            duration_ms: None,
            timeout_ms,
            default_hard_kill: false,
            exit_code: None,
            exit_marker_unavailable: None,
            child_pid: None,
            pgid: None,
            child_start_time_ms: None,
            live_descendants: None,
            live_descendants_omitted: 0,
            live_descendants_unavailable: None,
            group_kill_sent: false,
            group_member_identities: Vec::new(),
            group_member_identities_truncated: false,
            completion_delivered: !notify_on_completion,
            notify_on_completion,
            compressed,
            completion_has_secret_placeholders,
            completion_has_env_secrets: None,
            completion_has_non_ticket_env_secrets: None,
            completion_ticket_digest: None,
            completion_preview: None,
            pty_rows: None,
            pty_cols: None,
            scanner_report: Vec::new(),
            sandbox_native: false,
            sandbox_temp_dir: None,
            sandbox_temp_delete_at: None,
            status_reason: None,
            output_incomplete: false,
            call_key,
            schema_pin: super::current_schema_pin(),
            display: crate::subc::rich_ui::display::current_task(),
            scope: super::registry::scope_for_spawn(),
            remote: None,
            execution_note: None,
            memory_policy: None,
            incomplete_output: Vec::new(),
            stripped_env_names: Vec::new(),
            local_fallback_started: false,
            cancelled_call_unannounced: false,
            runner_completion_offers: 0,
            folder_grants: super::registry::folder_grants::for_spawn(),
        };
        crate::gh_shim_ticket::capture_spawn(&metadata);
        metadata
    }

    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    pub(crate) fn has_safe_completion_redaction_after_restart(&self) -> bool {
        if self.completion_has_secret_placeholders != Some(false) {
            return false;
        }
        if self.completion_has_non_ticket_env_secrets != Some(false) {
            return false;
        }
        match self.completion_has_env_secrets {
            Some(false) => true,
            Some(true) => self
                .completion_ticket_digest
                .as_deref()
                .is_some_and(|digest| {
                    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                }),
            None => false,
        }
    }

    pub fn mark_running(&mut self, child_pid: u32, pgid: i32) {
        self.status = BgTaskStatus::Running;
        self.child_pid = Some(child_pid);
        self.pgid = Some(pgid);
        self.child_start_time_ms = crate::process_identity::process_start_time_ms(child_pid);
        crate::gh_shim_ticket::seal_spawn(self);
    }

    pub fn mark_terminal(
        &mut self,
        status: BgTaskStatus,
        exit_code: Option<i64>,
        reason: Option<String>,
    ) {
        let finished_at = unix_millis();
        self.status = status;
        self.exit_code = exit_code;
        self.finished_at = Some(finished_at);
        self.duration_ms = Some(finished_at.saturating_sub(self.started_at));
        self.child_pid = None;
        self.status_reason = reason;
        self.completion_delivered = !self.notify_on_completion;
    }

    pub fn to_bash_task_row(
        &self,
        harness: &str,
        paths: &TaskPaths,
    ) -> Result<BashTaskRow, serde_json::Error> {
        let project_root = self.project_root.as_deref().unwrap_or(&self.workdir);
        let output_bytes = capture_output_bytes(&self.mode, paths);
        let stdout_path = match self.mode {
            BgMode::Pipes => Some(paths.stdout.display().to_string()),
            BgMode::Pty => Some(paths.pty.display().to_string()),
        };
        let stderr_path = match self.mode {
            BgMode::Pipes => Some(paths.stderr.display().to_string()),
            BgMode::Pty => None,
        };
        let mut metadata = self.clone();
        metadata.schema_version = SCHEMA_VERSION;
        Ok(BashTaskRow {
            harness: harness.to_string(),
            session_id: self.session_id.clone(),
            task_id: self.task_id.clone(),
            project_key: crate::path_identity::project_scope_key(project_root),
            command: self.command.clone(),
            cwd: self.workdir.display().to_string(),
            status: status_name(&self.status).to_string(),
            exit_code: self.exit_code,
            pid: self.child_pid.map(i64::from),
            pgid: self.pgid.map(i64::from),
            started_at: self.started_at as i64,
            completed_at: self.finished_at.map(|value| value as i64),
            stdout_path,
            stderr_path,
            compressed: self.compressed,
            timeout_ms: self.timeout_ms.map(|value| value as i64),
            completion_delivered: self.completion_delivered,
            output_bytes,
            metadata: serde_json::to_string(&metadata)?,
        })
    }
}

impl From<BashTaskRow> for PersistedTask {
    fn from(row: BashTaskRow) -> Self {
        if let Ok(mut task) = serde_json::from_str::<PersistedTask>(&row.metadata) {
            task.harness = Some(row.harness.clone());
            // A foreign registry acknowledges the row without rewriting its
            // metadata snapshot, so the durable column may be newer.
            task.completion_delivered |= row.completion_delivered;
            return task;
        }
        let status = match row.status.as_str() {
            "starting" => BgTaskStatus::Starting,
            "running" => BgTaskStatus::Running,
            "killing" => BgTaskStatus::Killing,
            "completed" => BgTaskStatus::Completed,
            "failed" => BgTaskStatus::Failed,
            "killed" => BgTaskStatus::Killed,
            "timed_out" => BgTaskStatus::TimedOut,
            "fate_unknown" => BgTaskStatus::FateUnknown,
            _ => BgTaskStatus::Failed,
        };
        let started_at = u64::try_from(row.started_at).unwrap_or_default();
        let finished_at = row.completed_at.and_then(|value| u64::try_from(value).ok());
        Self {
            schema_version: SCHEMA_VERSION,
            harness: Some(row.harness.clone()),
            task_id: row.task_id,
            session_id: row.session_id,
            launching_identity: None,
            command: row.command,
            pipeline_segments: Vec::new(),
            pipeline_status_unavailable: None,
            mode: BgMode::Pipes,
            workdir: PathBuf::from(row.cwd),
            project_root: None,
            status,
            started_at,
            finished_at,
            duration_ms: finished_at.map(|finished_at| finished_at.saturating_sub(started_at)),
            timeout_ms: row.timeout_ms.and_then(|value| u64::try_from(value).ok()),
            default_hard_kill: false,
            exit_code: row.exit_code,
            exit_marker_unavailable: None,
            child_pid: row.pid.and_then(|value| u32::try_from(value).ok()),
            pgid: row.pgid.and_then(|value| i32::try_from(value).ok()),
            child_start_time_ms: None,
            live_descendants: None,
            live_descendants_omitted: 0,
            live_descendants_unavailable: None,
            group_kill_sent: false,
            group_member_identities: Vec::new(),
            group_member_identities_truncated: false,
            completion_delivered: row.completion_delivered,
            notify_on_completion: !row.completion_delivered,
            compressed: row.compressed,
            completion_has_secret_placeholders: None,
            completion_has_env_secrets: None,
            completion_has_non_ticket_env_secrets: None,
            completion_ticket_digest: None,
            completion_preview: None,
            pty_rows: None,
            pty_cols: None,
            scanner_report: Vec::new(),
            sandbox_native: false,
            sandbox_temp_dir: None,
            sandbox_temp_delete_at: None,
            status_reason: None,
            output_incomplete: false,
            call_key: None,
            schema_pin: None,
            display: None,
            scope: None,
            remote: None,
            execution_note: None,
            memory_policy: None,
            incomplete_output: Vec::new(),
            stripped_env_names: Vec::new(),
            local_fallback_started: false,
            cancelled_call_unannounced: false,
            runner_completion_offers: 0,
            folder_grants: None,
        }
    }
}

fn status_name(status: &BgTaskStatus) -> &'static str {
    match status {
        BgTaskStatus::Starting => "starting",
        BgTaskStatus::Running => "running",
        BgTaskStatus::Killing => "killing",
        BgTaskStatus::Completed => "completed",
        BgTaskStatus::Failed => "failed",
        BgTaskStatus::Killed => "killed",
        BgTaskStatus::TimedOut => "timed_out",
        BgTaskStatus::FateUnknown => "fate_unknown",
    }
}

fn capture_output_bytes(mode: &BgMode, paths: &TaskPaths) -> Option<i64> {
    // Both streams belong to one verified layout. Resolving each artifact
    // separately rereads and reparses the same task record for every DB row.
    let resolved = retry_artifact_read(|| {
        let resolved = resolve_task_layout(&paths.session_dir, &paths.task_id)?;
        if resolved.paths.layout != paths.layout {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "background task layout identity changed",
            ));
        }
        Ok(resolved)
    })
    .ok()?;
    let len = |artifact| {
        retry_artifact_read(|| {
            let file = resolved
                .dirs
                .io
                .open_file(&resolved.paths.artifact_name(artifact), false)?;
            ValidatedArtifact::new(file)?.len()
        })
        .ok()
    };
    match mode {
        BgMode::Pipes => match (len(TaskArtifact::Stdout), len(TaskArtifact::Stderr)) {
            (Some(stdout), Some(stderr)) => Some(stdout.saturating_add(stderr) as i64),
            (Some(bytes), None) | (None, Some(bytes)) => Some(bytes as i64),
            (None, None) => None,
        },
        BgMode::Pty => len(TaskArtifact::Pty).map(|bytes| bytes as i64),
    }
}

pub fn validate_task_id(task_id: &str) -> io::Result<()> {
    let bytes = task_id.as_bytes();
    if bytes.len() == 21
        && bytes.starts_with(b"bash-")
        && bytes[5..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "background task id must match ^bash-[0-9a-f]{16}$",
        ))
    }
}

pub fn session_tasks_dir(storage_dir: &Path, session_id: &str) -> PathBuf {
    let session_hash = hash_session(session_id);
    let direct = storage_dir.join("bash-tasks").join(&session_hash);
    if direct.exists() {
        return direct;
    }
    // Configure replays through a harness root. Live pre-namespace tasks must
    // retain their original paths until their detached writers have stopped.
    if storage_dir
        .file_name()
        .is_some_and(|name| name == "opencode" || name == "pi")
    {
        if let Some(root) = storage_dir.parent() {
            let legacy = root.join("bash-tasks").join(&session_hash);
            if legacy.exists() {
                return legacy;
            }
        }
    }
    let mut harness_matches = ["opencode", "pi"]
        .into_iter()
        .map(|harness| {
            storage_dir
                .join(harness)
                .join("bash-tasks")
                .join(&session_hash)
        })
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    if harness_matches.len() == 1 {
        return harness_matches.remove(0);
    }
    direct
}

pub fn task_paths(storage_dir: &Path, session_id: &str, task_id: &str) -> io::Result<TaskPaths> {
    validate_task_id(task_id)?;
    Ok(TaskPaths::flat(
        session_tasks_dir(storage_dir, session_id),
        task_id,
    ))
}

pub fn allocate_task_layout(storage_dir: &Path, session_id: &str) -> io::Result<ResolvedTask> {
    let session_dir = session_tasks_dir(storage_dir, session_id);
    let _session_lock = lock_session_directory(&session_dir)?;
    create_private_task_store(&session_dir)?;
    let session = Arc::new(PinnedDir::open(&session_dir)?);
    for _ in 0..32 {
        let task_id = random_task_id()?;
        match create_task_layout_from_session(Arc::clone(&session), &task_id) {
            Ok(task) => return Ok(task),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to allocate unique background task id after 32 attempts",
    ))
}

pub fn create_task_layout(
    storage_dir: &Path,
    session_id: &str,
    task_id: &str,
) -> io::Result<ResolvedTask> {
    validate_task_id(task_id)?;
    let session_dir = session_tasks_dir(storage_dir, session_id);
    let _session_lock = lock_session_directory(&session_dir)?;
    create_private_task_store(&session_dir)?;
    create_task_layout_from_session(Arc::new(PinnedDir::open(&session_dir)?), task_id)
}

fn lock_session_directory(session_dir: &Path) -> io::Result<File> {
    lock_session_directories(&[session_dir])?
        .pop()
        .ok_or_else(|| io::Error::other("task session lock was not acquired"))
}

pub(crate) fn lock_session_directories(session_dirs: &[&Path]) -> io::Result<Vec<File>> {
    let mut roots = Vec::with_capacity(session_dirs.len());
    for session_dir in session_dirs {
        let task_root = session_dir
            .parent()
            .ok_or_else(|| io::Error::other("task session directory has no parent"))?;
        let storage_root = task_root
            .parent()
            .ok_or_else(|| io::Error::other("task root has no storage parent"))?;
        let lock_path = storage_root.join(".bash-task-session.lock");
        for path in [storage_root, task_root, *session_dir, lock_path.as_path()] {
            crate::production_storage::refuse_write(path)?;
        }
        roots.push((storage_root.to_path_buf(), task_root.to_path_buf()));
    }
    roots.sort();
    roots.dedup();
    for (storage_root, task_root) in &roots {
        crate::private_storage::open_dir(storage_root, task_root)?;
    }
    let mut storage_roots = roots
        .into_iter()
        .map(|(storage_root, _)| storage_root)
        .collect::<Vec<_>>();
    storage_roots.sort();
    storage_roots.dedup();
    storage_roots
        .into_iter()
        .map(|storage_root| {
            let lock = crate::private_storage::options()
                .create(true)
                .read(true)
                .write(true)
                .open(storage_root.join(".bash-task-session.lock"))?;
            lock.lock()?;
            Ok(lock)
        })
        .collect()
}

/// Remove a session directory only when no task or quarantine writer owns it.
/// The OS rmdir is the emptiness check and removal, so a racing entry survives.
#[cfg(test)]
pub(crate) fn remove_empty_session_directory(session_dir: &Path) -> io::Result<bool> {
    let _session_lock = lock_session_directory(session_dir)?;
    remove_empty_session_directory_locked(session_dir)
}

pub(crate) fn remove_empty_session_directory_locked(session_dir: &Path) -> io::Result<bool> {
    match fs::remove_dir(session_dir) {
        Ok(()) => Ok(true),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn create_private_task_store(session_dir: &Path) -> io::Result<()> {
    crate::production_storage::refuse_write(session_dir)?;
    #[cfg(test)]
    task_io_fault_for_test(false)?;
    let root = session_dir.parent().and_then(Path::parent).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "task directory has no storage root",
        )
    })?;
    crate::private_storage::open_dir(root, session_dir)
}

fn create_task_layout_from_session(
    session: Arc<PinnedDir>,
    task_id: &str,
) -> io::Result<ResolvedTask> {
    validate_task_id(task_id)?;
    #[cfg(test)]
    wait_on_session_create_gate_for_test(&session.path);
    let task = session.create_dir_at(OsStr::new(task_id))?;
    let control = task.create_dir_at(OsStr::new(CONTROL_DIR))?;
    let io_dir = task.create_dir_at(OsStr::new(IO_DIR))?;
    let paths = TaskPaths::directory(session.path.clone(), task_id);
    Ok(ResolvedTask {
        paths,
        dirs: TaskDirs {
            session,
            task: Arc::new(task),
            control: Arc::new(control),
            io: Arc::new(io_dir),
        },
    })
}

pub fn resolve_task_layout(session_dir: &Path, task_id: &str) -> io::Result<ResolvedTask> {
    let task = resolve_uninitialized_task_layout(session_dir, task_id)?;
    let metadata = read_task_at(&task)?;
    if metadata.task_id != task_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background task metadata identity mismatch",
        ));
    }
    Ok(task)
}

pub fn resolve_uninitialized_task_layout(
    session_dir: &Path,
    task_id: &str,
) -> io::Result<ResolvedTask> {
    validate_task_id(task_id)?;
    let session = Arc::new(PinnedDir::open(session_dir)?);
    let directory = session.open_dir_at(OsStr::new(task_id));
    let flat_name = OsString::from(format!("{task_id}.json"));
    let flat = open_metadata_through_replacement(&session, &flat_name);
    let has_directory = match &directory {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("invalid task directory: {error}"),
            ))
        }
    };
    let has_flat = match &flat {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("invalid flat task metadata: {error}"),
            ))
        }
    };
    if has_directory && has_flat {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "duplicate flat and directory background task layouts",
        ));
    }
    if has_directory {
        let task = directory.expect("directory result checked");
        let control = match task.open_dir_at(OsStr::new(CONTROL_DIR)) {
            Ok(control) => control,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(task_layout_not_found())
            }
            Err(error) => return Err(error),
        };
        let io_dir = match task.open_dir_at(OsStr::new(IO_DIR)) {
            Ok(io_dir) => io_dir,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(task_layout_not_found())
            }
            Err(error) => return Err(error),
        };
        let paths = TaskPaths::directory(session_dir.to_path_buf(), task_id);
        return Ok(ResolvedTask {
            paths,
            dirs: TaskDirs {
                session,
                task: Arc::new(task),
                control: Arc::new(control),
                io: Arc::new(io_dir),
            },
        });
    }
    if has_flat {
        let paths = TaskPaths::flat(session_dir.to_path_buf(), task_id);
        return Ok(ResolvedTask {
            paths,
            dirs: TaskDirs {
                session: Arc::clone(&session),
                task: Arc::clone(&session),
                control: Arc::clone(&session),
                io: session,
            },
        });
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        TASK_LAYOUT_NOT_FOUND,
    ))
}

pub fn resolve_task(
    storage_dir: &Path,
    session_id: &str,
    task_id: &str,
) -> io::Result<ResolvedTask> {
    resolve_task_layout(&session_tasks_dir(storage_dir, session_id), task_id)
}

pub fn discover_task_ids(session_dir: &Path) -> io::Result<(Vec<String>, Vec<OsString>)> {
    let session = PinnedDir::open(session_dir)?;
    let mut ids = std::collections::BTreeSet::new();
    let mut invalid = Vec::new();
    for name in session.list_names()? {
        let Some(text) = name.to_str() else {
            invalid.push(name);
            continue;
        };
        if validate_task_id(text).is_ok() {
            ids.insert(text.to_string());
            continue;
        }
        if let Some((task_id, _suffix)) = text.split_once('.') {
            if validate_task_id(task_id).is_ok() {
                ids.insert(task_id.to_string());
            } else if task_id.starts_with("bash-") {
                invalid.push(name);
            }
        } else if text.starts_with("bash-") {
            invalid.push(name);
        }
    }
    Ok((ids.into_iter().collect(), invalid))
}

pub fn uninitialized_layout_is_recent(
    session_dir: &Path,
    task_id: &str,
    grace: std::time::Duration,
) -> io::Result<bool> {
    // A spawn creates the task directory a few syscalls before `control/` and
    // the metadata exist, and the persisted-task GC runs concurrently with
    // spawns (in this process since it left the replay thread, and always from
    // sibling processes sharing the storage root). Mid-creation, the layout
    // resolver finds nothing to resolve; the directory's own mtime still says
    // how young it is, and a young directory must be skipped, not quarantined.
    let task = match resolve_uninitialized_task_layout(session_dir, task_id) {
        Ok(task) => task,
        Err(_) => {
            // The directory's age is an answer either way: young means a spawn
            // in progress, old means an abandoned layout that may be reclaimed.
            // Only a failing metadata probe (the directory is already gone)
            // propagates as an error.
            let modified = session_dir.join(task_id).metadata()?.modified()?;
            let age = SystemTime::now()
                .duration_since(modified)
                .unwrap_or_default();
            return Ok(age < grace);
        }
    };
    // Exit files are reserved empty at spawn. A nonempty marker means the
    // wrapper finished, so missing metadata cannot be the initial-write window.
    if task
        .dirs
        .io
        .open_file(&task.paths.artifact_name(TaskArtifact::Exit), false)
        .and_then(|file| file.metadata())
        .is_ok_and(|meta| meta.len() > 0)
    {
        return Ok(false);
    }
    let modified = match task.paths.layout {
        TaskLayout::Directory => task.dirs.control.modified()?,
        TaskLayout::Flat => task
            .dirs
            .session
            .open_file(&task.paths.artifact_name(TaskArtifact::Exit), false)
            .and_then(|file| file.metadata()?.modified())
            .or_else(|_| {
                task.dirs
                    .session
                    .open_file(OsStr::new(&format!("{task_id}.json")), false)
                    .and_then(|file| file.metadata()?.modified())
            })?,
    };
    Ok(SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default()
        < grace)
}

pub fn quarantine_task_layout(
    storage_dir: &Path,
    session_dir: &Path,
    task_id: &str,
    reason: &str,
) -> io::Result<()> {
    let result = (|| -> io::Result<()> {
        validate_task_id(task_id)?;
        let quarantine_session = storage_dir.join("bash-tasks-quarantine").join(
            session_dir
                .file_name()
                .ok_or_else(|| io::Error::other("task session directory has no identity"))?,
        );
        let _session_locks = lock_session_directories(&[session_dir, &quarantine_session])?;
        let session = PinnedDir::open(session_dir)?;
        let names = session.list_names()?;
        let flat_prefix = format!("{task_id}.");
        let selected = names
            .into_iter()
            .filter(|name| {
                name == OsStr::new(task_id)
                    || name
                        .to_str()
                        .is_some_and(|name| name.starts_with(&flat_prefix))
            })
            .collect::<Vec<_>>();
        refuse_quarantine_of_newer_task(session_dir, &selected)?;
        quarantine_names(storage_dir, session_dir, &session, selected, reason)
    })();
    result.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "failed to quarantine background task {task_id} from {}: {error}",
                session_dir.display()
            ),
        )
    })
}

pub fn quarantine_invalid_entry(
    storage_dir: &Path,
    session_dir: &Path,
    entry: &OsStr,
) -> io::Result<()> {
    let result = (|| -> io::Result<()> {
        let quarantine_session = storage_dir.join("bash-tasks-quarantine").join(
            session_dir
                .file_name()
                .ok_or_else(|| io::Error::other("task session directory has no identity"))?,
        );
        let _session_locks = lock_session_directories(&[session_dir, &quarantine_session])?;
        let session = PinnedDir::open(session_dir)?;
        refuse_quarantine_of_newer_task(session_dir, &[entry.to_os_string()])?;
        quarantine_names(
            storage_dir,
            session_dir,
            &session,
            vec![entry.to_os_string()],
            "invalid",
        )
    })();
    result.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "failed to quarantine invalid background task entry {}: {error}",
                session_dir.join(entry).display()
            ),
        )
    })
}

/// Task metadata written by a newer build is not invalid; it is unreadable by
/// this build only. Replay, relaxed lookup and GC all quarantine through the
/// two functions above, so refusing here keeps such a task in place under its
/// original path whichever of them got here. Each entry is either a task
/// directory (`<id>/control/metadata.json`) or a flat-layout file
/// (`<id>.json`).
fn refuse_quarantine_of_newer_task(session_dir: &Path, names: &[OsString]) -> io::Result<()> {
    for name in names {
        let entry = session_dir.join(name);
        let metadata_path = if entry.is_dir() {
            entry.join(CONTROL_DIR).join(METADATA_FILE)
        } else if entry.extension() == Some(OsStr::new("json")) {
            entry
        } else {
            continue;
        };
        let version = fs::read(&metadata_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| value.get("schema_version")?.as_u64());
        let Some(version) = version else {
            continue;
        };
        if let Some(refusal) = crate::persisted_format::UnsupportedPersistedFormat::check(
            crate::persisted_format::PersistedStore::BashTask,
            &metadata_path,
            version,
        ) {
            return Err(crate::persisted_format::refuse(refusal).into_io_error());
        }
    }
    Ok(())
}

fn quarantine_names(
    storage_dir: &Path,
    session_dir: &Path,
    session: &PinnedDir,
    names: Vec<OsString>,
    reason: &str,
) -> io::Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let session_hash = session_dir.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "session dir has no identity")
    })?;
    let quarantine_path = storage_dir.join("bash-tasks-quarantine").join(session_hash);
    crate::private_storage::create_dir_all(&quarantine_path)?;
    let quarantine = PinnedDir::open(&quarantine_path)?;
    for name in names {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let target = OsString::from(format!(
            "{}.{}-{}",
            name.to_string_lossy(),
            reason,
            hex_lower(&random)
        ));
        session.rename_to(&name, &quarantine, &target)?;
    }
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// How many times an open of a task's metadata re-tries a concurrent
/// replacement before giving up. An attempt only loses if another rename lands
/// in the microseconds between this open and its link-count check, so a handful
/// of attempts outlasts even a writer republishing metadata in a loop —
/// provided the attempts actually sample different moments, which is what the
/// yield between them is for.
const METADATA_OPEN_ATTEMPTS: u32 = 8;

/// Open a task's metadata file by name, tolerating a concurrent atomic replace.
///
/// Metadata is republished by writing a temporary file and renaming it over the
/// old name (see `randomized_atomic_replace`), so a reader that opened the
/// previous file a moment earlier finds it at zero links and is told the
/// artifact was concurrently replaced (see the link-count semantics on
/// `validate_regular_handle`). The name already points at the replacement by
/// then, so re-opening it succeeds; the race is a single rename, not a
/// sustained condition.
///
/// This matters because callers use the layout resolver to decide whether a
/// task on disk is intact. Reporting a routine metadata write as a resolution
/// failure makes a healthy task look like a damaged layout, and callers that
/// quarantine damaged layouts — most destructively the persisted-task GC —
/// would then rename a live task's whole bundle away just because its metadata
/// was being written at that moment.
fn open_metadata_through_replacement(dir: &PinnedDir, name: &OsStr) -> io::Result<File> {
    retry_artifact_read(|| {
        #[cfg(test)]
        work_counts::record_metadata_open();
        dir.open_file(name, false)
    })
}

/// Retry a complete read attempt so validation never reuses an unlinked handle.
/// Other errors, including hard-link and symlink refusals, remain failures.
pub(crate) fn retry_artifact_read<T>(mut read: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        match read() {
            Err(error)
                if attempts < METADATA_OPEN_ATTEMPTS
                    && error.kind() == io::ErrorKind::Interrupted
                    && error.to_string().contains(ARTIFACT_CONCURRENTLY_REPLACED) =>
            {
                // Retrying in a tight loop samples one instant several times:
                // every attempt can land inside the same scheduling quantum, so
                // eight of them are worth about one whenever the writer is
                // descheduled mid-rename -- exactly the case on a loaded
                // machine, which is the case this retry exists for. Yield first,
                // then sleep in growing steps, so the attempts span the rename
                // rather than racing it. The total stays under ~2ms, and losing
                // here is not cosmetic: a caller that reads this as a damaged
                // layout can quarantine a live task's whole bundle.
                if attempts <= 2 {
                    std::thread::yield_now();
                } else {
                    std::thread::sleep(Duration::from_micros(50 << (attempts - 3).min(5)));
                }
            }
            other => return other,
        }
    }
}

pub fn read_task(path: &Path) -> io::Result<PersistedTask> {
    retry_artifact_read(|| read_task_once(path))
}

fn read_task_once(path: &Path) -> io::Result<PersistedTask> {
    #[cfg(all(test, unix))]
    if let Some(error) = take_injected_read_task_replacement_error_for_test() {
        return Err(error);
    }
    let mut file = open_validated_path(path, false)?;
    read_task_file(&mut file, path)
}

#[cfg(all(test, unix))]
std::thread_local! {
    static READ_TASK_REPLACEMENT_FAILURES_FOR_TEST: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(all(test, unix))]
fn inject_read_task_replacement_failures_for_test(count: u32) {
    READ_TASK_REPLACEMENT_FAILURES_FOR_TEST.with(|remaining| remaining.set(count));
}

#[cfg(all(test, unix))]
fn take_injected_read_task_replacement_error_for_test() -> Option<io::Error> {
    READ_TASK_REPLACEMENT_FAILURES_FOR_TEST.with(|remaining| {
        let count = remaining.get();
        if count == 0 {
            None
        } else {
            remaining.set(count - 1);
            Some(io::Error::new(
                io::ErrorKind::Interrupted,
                ARTIFACT_CONCURRENTLY_REPLACED,
            ))
        }
    })
}

#[cfg(all(test, unix))]
fn injected_read_task_replacement_failures_for_test() -> u32 {
    READ_TASK_REPLACEMENT_FAILURES_FOR_TEST.with(std::cell::Cell::get)
}

/// Terminal status describes the leader's outcome, not the lifetime of its
/// descendants. Require the process group to have disappeared before deleting
/// scratch space; a cached `live_descendants` sample can already be out of date.
pub(crate) fn task_temp_quiescent(metadata: &PersistedTask) -> bool {
    if !metadata.is_terminal() {
        return false;
    }
    #[cfg(unix)]
    if let Some(pgid) = metadata.pgid {
        return pgid > 0
            && unsafe { libc::kill(-pgid, 0) } == -1
            && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    }
    // A spawn that failed before releasing a child has no process group.
    metadata.child_pid.is_none() && metadata.pgid.is_none()
}

pub fn read_task_at(task: &ResolvedTask) -> io::Result<PersistedTask> {
    let name = match task.paths.layout {
        TaskLayout::Directory => OsString::from(METADATA_FILE),
        TaskLayout::Flat => OsString::from(format!("{}.json", task.paths.task_id)),
    };
    let mut file = open_metadata_through_replacement(&task.dirs.control, &name)?;
    let metadata = read_task_file(&mut file, &task.dirs.control.path().join(&name))?;
    if metadata.task_id != task.paths.task_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background task metadata identity does not match its layout name",
        ));
    }
    Ok(metadata)
}

fn read_task_file(file: &mut File, path: &Path) -> io::Result<PersistedTask> {
    #[cfg(test)]
    work_counts::record_read();
    file.seek(SeekFrom::Start(0))?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    // Ordinary records need only the typed decode, not an allocated Value
    // tree followed by the same parse again. On decode failure, still check
    // the version before returning that error: future formats may have a
    // different shape and must be refused, not quarantined as corrupt tasks.
    #[cfg(test)]
    work_counts::record_parse();
    let decoded = serde_json::from_str::<PersistedTask>(&content);
    let version = match &decoded {
        Ok(task) => Some(u64::from(task.schema_version)),
        Err(_) => {
            #[cfg(test)]
            work_counts::record_parse();
            serde_json::from_str::<serde_json::Value>(&content)
                .ok()
                .and_then(|value| value.get("schema_version")?.as_u64())
        }
    };
    crate::persisted_format::gate(
        crate::persisted_format::PersistedStore::BashTask,
        path,
        path,
        version,
    )
    .map_err(crate::persisted_format::UnsupportedPersistedFormat::into_io_error)?;
    let task = decoded.map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !matches!(task.schema_version, 2 | 3 | 4 | 5 | 6 | SCHEMA_VERSION) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported background task schema_version {} (expected 2, 3, 4, 5, 6, or {SCHEMA_VERSION})",
                task.schema_version
            ),
        ));
    }
    validate_task_id(&task.task_id)?;
    Ok(task)
}

pub fn write_task(path: &Path, task: &PersistedTask) -> io::Result<()> {
    crate::production_storage::refuse_write(path)?;
    validate_task_id(&task.task_id)?;
    if let Some(parent) = path.parent() {
        crate::private_storage::create_dir_all(parent)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let dir = PinnedDir::open(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "metadata path has no name"))?;
    write_task_in_dir(&dir, name, task, ReplaceSync::for_task(task))
}

pub fn write_task_at(task: &ResolvedTask, metadata: &PersistedTask) -> io::Result<()> {
    write_task_at_with(task, metadata, ReplaceSync::for_task(metadata))
}

/// Writes a remote task's output-progress checkpoint: a record whose only
/// change since the last durable write is the saved output cursor (sequence
/// number and byte lengths).
///
/// The new file's contents are synced before the rename, so a power loss can
/// never expose a partly written record under the metadata name. The
/// directory is not synced after the rename, so a power loss may leave the
/// previous record in place instead. That is still a correct checkpoint: each
/// saved cursor only ever covers output already synced to disk, and recovery
/// truncates the output files to whichever cursor it finds and asks the
/// runner to replay from there. The next state-change write syncs the
/// directory and makes any earlier rename durable.
pub fn write_task_checkpoint_at(task: &ResolvedTask, metadata: &PersistedTask) -> io::Result<()> {
    write_task_at_with(task, metadata, ReplaceSync::File)
}

fn write_task_at_with(
    task: &ResolvedTask,
    metadata: &PersistedTask,
    sync: ReplaceSync,
) -> io::Result<()> {
    if metadata.task_id != task.paths.task_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to write metadata under a different task identity",
        ));
    }
    let name = match task.paths.layout {
        TaskLayout::Directory => OsString::from(METADATA_FILE),
        TaskLayout::Flat => OsString::from(format!("{}.json", task.paths.task_id)),
    };
    write_task_in_dir(&task.dirs.control, &name, metadata, sync)
}

/// How much of an atomic file replacement is synced to disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplaceSync {
    /// Nothing is synced. The rename still protects readers from a partly
    /// written file after a process kill, but not after a power loss.
    None,
    /// The new file's contents are synced before the rename; the directory
    /// entry is not synced after it.
    File,
    /// The new file is synced before the rename and its directory after it,
    /// so the replacement itself survives a power loss.
    FileAndDirectory,
}

impl ReplaceSync {
    /// Remote tasks and local fallbacks of remote tasks resume from their
    /// metadata after a crash, so their records are written durably.
    fn for_task(task: &PersistedTask) -> Self {
        if task.remote.is_some() || task.local_fallback_started {
            Self::FileAndDirectory
        } else {
            Self::None
        }
    }
}

fn write_task_in_dir(
    dir: &PinnedDir,
    name: &OsStr,
    task: &PersistedTask,
    sync: ReplaceSync,
) -> io::Result<()> {
    crate::production_storage::refuse_write(&dir.path().join(name))?;
    #[cfg(test)]
    record_writes::record(&task.task_id);
    #[cfg(test)]
    TASK_IO_FAULT.with(|slot| {
        if matches!(slot.get(), Some(TaskIoFault::RecordWriteNotFound)) {
            // Fail one record write without moving live output directories.
            // Keep "not found" in the storage diagnostic to exercise typed
            // task-absence classification, not just generic write failures.
            slot.set(None);
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "injected task record storage not found",
            ))
        } else {
            Ok(())
        }
    })?;
    #[cfg(test)]
    if task.status == BgTaskStatus::Running {
        task_io_fault_for_test(true)?;
    }
    #[cfg(test)]
    if task.status == BgTaskStatus::Starting {
        starting_write_delay_for_test();
    }
    // Every terminal transition is persisted through here, so this is the one
    // place that retires the task's gh shim ticket on completion, kill,
    // timeout and unknown fate alike. It runs before the write so a failed
    // write still leaves the ended task unable to speak.
    if task.is_terminal() {
        crate::gh_shim_ticket::revoke_task(&task.task_id);
    }
    let mut upgraded = task.clone();
    upgraded.schema_version = SCHEMA_VERSION;
    let content = serde_json::to_vec_pretty(&upgraded).map_err(io::Error::other)?;
    if sync_unchanged_metadata(dir, name, &content, sync) {
        return Ok(());
    }
    #[cfg(test)]
    work_counts::record_metadata_write();
    super::gc_memo::metadata_written(&dir.path().join(name));
    let result = atomic_replace(dir, name, &content, sync);
    super::gc_memo::metadata_written(&dir.path().join(name));
    result
}

/// Compare the file, not an in-memory cache: another registry can acknowledge
/// a completed task through the same pinned directory. Equal bytes need no
/// replacement, but a durable state write must still sync an earlier checkpoint
/// whose rename may not have reached disk yet. A failed shortcut sync falls
/// back to replacement with the same durability requirements.
fn sync_unchanged_metadata(
    dir: &PinnedDir,
    name: &OsStr,
    content: &[u8],
    sync: ReplaceSync,
) -> bool {
    let Ok(mut file) = dir.open_file(name, sync != ReplaceSync::None) else {
        // Missing, unreadable or non-regular metadata can still be replaced.
        return false;
    };
    let mut previous = vec![0; content.len()];
    if file.read_exact(&mut previous).is_err()
        || previous != content
        || file.read(&mut [0]).ok() != Some(0)
    {
        return false;
    }
    if sync != ReplaceSync::None && sync_task_file(&file).is_err() {
        return false;
    }
    if sync == ReplaceSync::FileAndDirectory && sync_task_directory(dir).is_err() {
        return false;
    }
    true
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum TaskIoFault {
    LayoutEnospc,
    RecordWriteNotFound,
    OutputHandlesEnospc,
    OutputHandlesMisleadingText,
    SyncFailureAfter(usize),
    RunningEnospc,
    #[cfg(unix)]
    RunningDelay(std::time::Duration),
    RunningHook(fn() -> io::Result<()>),
    #[cfg(unix)]
    StartingHook(fn()),
    /// Delays the starting-record write, which happens before the spawn
    /// receipt commits, so a test can push that commit past the reply deadline.
    /// Only the Unix startup tests construct it.
    #[cfg_attr(not(unix), allow(dead_code))]
    StartingDelay(std::time::Duration),
}

#[cfg(test)]
thread_local! {
    static TASK_IO_FAULT: std::cell::Cell<Option<TaskIoFault>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_task_io_fault<T>(fault: TaskIoFault, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<TaskIoFault>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TASK_IO_FAULT.with(|slot| slot.set(self.0));
        }
    }
    let previous = TASK_IO_FAULT.with(|slot| slot.replace(Some(fault)));
    let _restore = Restore(previous);
    run()
}

#[cfg(test)]
fn task_io_fault_for_test(running: bool) -> io::Result<()> {
    TASK_IO_FAULT.with(|slot| match slot.get() {
        Some(TaskIoFault::LayoutEnospc) if !running => Err(task_storage_full_for_test()),
        Some(TaskIoFault::RunningEnospc) if running => Err(task_storage_full_for_test()),
        #[cfg(unix)]
        Some(TaskIoFault::RunningDelay(delay)) if running => {
            std::thread::sleep(delay);
            Ok(())
        }
        Some(TaskIoFault::RunningHook(hook)) if running => hook(),
        _ => Ok(()),
    })
}

#[cfg(test)]
fn starting_write_delay_for_test() {
    #[cfg(unix)]
    if let Some(TaskIoFault::StartingHook(hook)) = TASK_IO_FAULT.with(std::cell::Cell::get) {
        hook();
    }
    if let Some(TaskIoFault::StartingDelay(delay)) = TASK_IO_FAULT.with(std::cell::Cell::get) {
        std::thread::sleep(delay);
    }
}

#[cfg(test)]
fn task_storage_full_for_test() -> io::Error {
    // OS code 28 is ENOSPC on Unix but a printer error on Windows. Inject the
    // portable error kind and a stable message so the test exercises disk-full
    // handling on every host without depending on localized system text.
    io::Error::new(io::ErrorKind::StorageFull, "No space left on device")
}

pub fn update_task_at<F>(task: &ResolvedTask, update: F) -> io::Result<PersistedTask>
where
    F: FnOnce(&mut PersistedTask),
{
    let mut metadata = read_task_at(task)?;
    let original_terminal = metadata.is_terminal();
    let original = metadata.clone();
    update(&mut metadata);
    metadata.schema_version = SCHEMA_VERSION;
    if original_terminal {
        // Terminal execution facts are immutable; delivery bookkeeping can
        // still advance as a consumer receives or acknowledges the result.
        let completion_delivered = metadata.completion_delivered;
        let runner_completion_offers = metadata
            .runner_completion_offers
            .max(original.runner_completion_offers);
        metadata = original;
        metadata.completion_delivered = completion_delivered;
        metadata.runner_completion_offers = runner_completion_offers;
        metadata.schema_version = SCHEMA_VERSION;
    }
    write_task_at(task, &metadata)?;
    Ok(metadata)
}

pub fn delete_task_bundle(paths: &TaskPaths) -> io::Result<()> {
    validate_task_id(&paths.task_id)?;
    crate::gh_shim_ticket::revoke_task(&paths.task_id);
    delete_task_bundle_at_root(paths)
}

/// Delete a task from the direct storage root or its harness-scoped legacy root.
pub fn delete_task_bundle_with_legacy_harness_root(
    paths: &TaskPaths,
    session_id: &str,
    harness: Option<&str>,
) -> io::Result<()> {
    validate_task_id(&paths.task_id)?;
    crate::gh_shim_ticket::revoke_task(&paths.task_id);
    #[cfg(unix)]
    if delete_task_tombstone(&paths.session_dir, &paths.task_id)? {
        return Ok(());
    }
    if task_layout_entry_exists(&paths.session_dir, &paths.task_id)? {
        return delete_task_bundle_at_root(paths);
    }

    let Some(legacy_session_dir) =
        legacy_harness_session_dir(&paths.session_dir, session_id, harness)
    else {
        return Err(task_layout_not_found());
    };
    if task_layout_entry_exists(&legacy_session_dir, &paths.task_id)? {
        let resolved = resolve_task_for_bundle_delete(&legacy_session_dir, paths)?;
        if resolved.paths.layout != paths.layout {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "background task layout changed before deletion",
            ));
        }
        return delete_resolved_task(&resolved);
    }
    #[cfg(unix)]
    if delete_task_tombstone(&legacy_session_dir, &paths.task_id)? {
        return Ok(());
    }
    Err(task_layout_not_found())
}

fn delete_task_bundle_at_root(paths: &TaskPaths) -> io::Result<()> {
    #[cfg(unix)]
    if delete_task_tombstone(&paths.session_dir, &paths.task_id)? {
        return Ok(());
    }
    #[cfg(windows)]
    if paths.layout == TaskLayout::Directory {
        return delete_windows_directory_bundle(paths);
    }
    let resolved = resolve_task_for_bundle_delete(&paths.session_dir, paths)?;
    if resolved.paths.layout != paths.layout {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background task layout changed before deletion",
        ));
    }
    delete_resolved_task(&resolved)
}

fn resolve_task_for_bundle_delete(
    session_dir: &Path,
    paths: &TaskPaths,
) -> io::Result<ResolvedTask> {
    match resolve_task_layout(session_dir, &paths.task_id) {
        Ok(resolved) => Ok(resolved),
        Err(error) if is_task_layout_not_found(&error) => {
            if paths.layout == TaskLayout::Directory {
                let incomplete = TaskPaths::directory(session_dir.to_path_buf(), &paths.task_id);
                remove_empty_task_directory(&incomplete)?;
            }
            Err(error)
        }
        Err(error) => Err(error),
    }
}

fn remove_empty_task_directory(paths: &TaskPaths) -> io::Result<bool> {
    if paths.layout != TaskLayout::Directory {
        return Ok(false);
    }
    let _session_lock = lock_session_directory(&paths.session_dir)?;
    match fs::remove_dir(&paths.dir) {
        Ok(()) => Ok(true),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn task_layout_entry_exists(session_dir: &Path, task_id: &str) -> io::Result<bool> {
    let session = match PinnedDir::open(session_dir) {
        Ok(session) => session,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    match session.open_dir_at(OsStr::new(task_id)) {
        Ok(_) => return Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let metadata = OsString::from(format!("{task_id}.json"));
    match open_metadata_through_replacement(&session, &metadata) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn delete_task_tombstone(session_dir: &Path, task_id: &str) -> io::Result<bool> {
    validate_task_id(task_id)?;
    let session = match PinnedDir::open(session_dir) {
        Ok(session) => session,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let Some(name) = find_task_tombstone(&session, task_id)? else {
        return Ok(false);
    };
    let task = match session.open_dir_at(&name) {
        Ok(task) => task,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    validate_and_cleanup_task_tombstone_metadata(&task, task_id)?;
    remove_optional_task_directory(&task, IO_DIR, remove_tree_contents)?;
    remove_optional_task_directory(&task, CONTROL_DIR, |control| {
        let mut names = control.list_names()?;
        names.sort_by_key(|name| name == OsStr::new(METADATA_FILE));
        for name in names {
            control.remove_file(&name)?;
        }
        Ok(())
    })?;

    let current = session.open_dir_at(&name)?;
    if !current.same_identity(&task)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "task tombstone identity changed before deletion",
        ));
    }
    let name = os_cstring(&name)?;
    let result =
        unsafe { libc::unlinkat(session.file.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}

#[cfg(unix)]
fn find_task_tombstone(session: &PinnedDir, task_id: &str) -> io::Result<Option<OsString>> {
    let prefix = task_tombstone_prefix(task_id);
    let mut matches = session
        .list_names()?
        .into_iter()
        .filter(|name| name.to_str().is_some_and(|name| name.starts_with(&prefix)));
    let Some(name) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "multiple background task deletion tombstones found",
        ));
    }
    Ok(Some(name))
}

#[cfg(unix)]
fn validate_and_cleanup_task_tombstone_metadata(task: &PinnedDir, task_id: &str) -> io::Result<()> {
    let control = match task.open_dir_at(OsStr::new(CONTROL_DIR)) {
        Ok(control) => control,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut file = match control.open_file(OsStr::new(METADATA_FILE), false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let metadata_path = control.path.join(METADATA_FILE);
    let metadata = read_task_file(&mut file, &metadata_path)?;
    if metadata.task_id != task_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background task tombstone metadata identity mismatch",
        ));
    }
    if let Some(temp_dir) = &metadata.sandbox_temp_dir {
        if !task_temp_quiescent(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "task scratch still in use",
            ));
        }
        crate::sandbox_spawn::cleanup_task_temp_dir(temp_dir)?;
    }
    Ok(())
}

#[cfg(unix)]
fn remove_optional_task_directory(
    task: &PinnedDir,
    name: &str,
    remove_contents: impl FnOnce(&PinnedDir) -> io::Result<()>,
) -> io::Result<()> {
    match task.open_dir_at(OsStr::new(name)) {
        Ok(directory) => {
            remove_contents(&directory)?;
            remove_dir_entry(task, name)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn legacy_harness_session_dir(
    session_dir: &Path,
    session_id: &str,
    harness: Option<&str>,
) -> Option<PathBuf> {
    let harness = harness?
        .parse::<crate::harness::Harness>()
        .ok()?
        .storage_segment();
    let session_hash = hash_session(session_id);
    if session_dir.file_name() != Some(OsStr::new(&session_hash)) {
        return None;
    }
    let task_root = session_dir.parent()?;
    if task_root.file_name() != Some(OsStr::new("bash-tasks")) {
        return None;
    }
    let storage_root = task_root.parent()?;
    Some(
        storage_root
            .join(harness)
            .join("bash-tasks")
            .join(session_hash),
    )
}

fn task_layout_not_found() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, TASK_LAYOUT_NOT_FOUND)
}

pub(crate) fn is_task_layout_not_found(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound && error.to_string() == TASK_LAYOUT_NOT_FOUND
}

#[cfg(windows)]
fn delete_windows_directory_bundle(paths: &TaskPaths) -> io::Result<()> {
    // A legacy disposition may finish removing IO/control directories only
    // after the previous sweep's pins close. Resume that partial layout rather
    // than requiring directories or metadata that cleanup already removed.
    let session = match PinnedDir::open(&paths.session_dir) {
        Ok(session) => session,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    match open_metadata_through_replacement(
        &session,
        &OsString::from(format!("{}.json", paths.task_id)),
    ) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "background task layout changed before deletion",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let Some(task) = open_optional_windows_directory(&session, OsStr::new(&paths.task_id))? else {
        return Ok(());
    };
    let io_dir = open_optional_windows_directory(&task, OsStr::new(IO_DIR))?;
    let control = open_optional_windows_directory(&task, OsStr::new(CONTROL_DIR))?;
    if let Some(control) = &control {
        match control.open_file(OsStr::new(METADATA_FILE), false) {
            Ok(mut file) => {
                let metadata = read_task_file(&mut file, &paths.json)?;
                if metadata.task_id != paths.task_id {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "background task metadata identity mismatch",
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && io_dir.is_none() => {}
            Err(error) => return Err(error),
        }
    } else if io_dir.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background task control directory is missing before IO cleanup",
        ));
    }
    remove_windows_directory_task(
        &session,
        &task,
        io_dir.as_ref(),
        control.as_ref(),
        remove_tree_contents,
    )
}

#[cfg(windows)]
fn open_optional_windows_directory(dir: &PinnedDir, name: &OsStr) -> io::Result<Option<PinnedDir>> {
    match dir.open_dir_at(name) {
        Ok(child) => Ok(Some(child)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn delete_resolved_task(task: &ResolvedTask) -> io::Result<()> {
    validate_task_id(&task.paths.task_id)?;
    match task.paths.layout {
        TaskLayout::Flat => {
            for path in task_bundle_files(&task.paths) {
                let Some(name) = path.file_name() else {
                    continue;
                };
                match task.dirs.session.remove_file(name) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        }
        TaskLayout::Directory => remove_directory_task(task),
    }
}

fn remove_directory_task(task: &ResolvedTask) -> io::Result<()> {
    remove_directory_task_with_io_cleanup(task, remove_tree_contents)
}

fn remove_directory_task_with_io_cleanup(
    task: &ResolvedTask,
    remove_io_contents: impl FnOnce(&PinnedDir) -> io::Result<()>,
) -> io::Result<()> {
    let current = task
        .dirs
        .session
        .open_dir_at(OsStr::new(&task.paths.task_id))?;
    if !current.same_identity(&task.dirs.task)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "task directory identity changed before deletion",
        ));
    }
    #[cfg(windows)]
    return remove_windows_directory_task(
        &task.dirs.session,
        &task.dirs.task,
        Some(&task.dirs.io),
        Some(&task.dirs.control),
        remove_io_contents,
    );

    #[cfg(unix)]
    {
        // The short scratch directory is outside the bundle, so retire it
        // before removing the metadata that records its ownership.
        if let Ok(metadata) = read_task_at(task) {
            if let Some(temp_dir) = &metadata.sandbox_temp_dir {
                if !task_temp_quiescent(&metadata) {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "task scratch still in use",
                    ));
                }
                crate::sandbox_spawn::cleanup_task_temp_dir(temp_dir)?;
            }
        }
        let tombstone = rename_task_to_tombstone(task)?;
        // Windows may refuse to delete an output file while the killed process
        // still has it open. Remove the child-writable IO tree before control
        // metadata so a partial failure leaves the task resolvable for the next
        // cleanup pass. Keep metadata until every other control file is gone too.
        remove_io_contents(&task.dirs.io)?;
        let mut control_names = task.dirs.control.list_names()?;
        control_names.sort_by_key(|name| name == OsStr::new(METADATA_FILE));
        for name in control_names {
            task.dirs.control.remove_file(&name)?;
        }
        remove_dir_entry(&task.dirs.task, IO_DIR)?;
        remove_dir_entry(&task.dirs.task, CONTROL_DIR)?;
        let name = os_cstring(&tombstone)?;
        let result = unsafe {
            libc::unlinkat(
                task.dirs.session.file.as_raw_fd(),
                name.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
fn remove_windows_directory_task(
    session: &PinnedDir,
    task: &PinnedDir,
    io_dir: Option<&PinnedDir>,
    control: Option<&PinnedDir>,
    remove_io_contents: impl FnOnce(&PinnedDir) -> io::Result<()>,
) -> io::Result<()> {
    session.ensure_current_identity()?;
    task.ensure_current_identity()?;
    // Keep control metadata until IO names are actually unlinked. Unsupported
    // POSIX deletion must leave the task registered, not claim pending removal
    // as success. A later sweep can resume after the retained handles close.
    if let Some(io_dir) = io_dir {
        remove_io_contents(io_dir)?;
        io_dir.remove_self()?;
    }
    if let Some(control) = control {
        let mut names = control.list_names()?;
        names.sort_by_key(|name| name == OsStr::new(METADATA_FILE));
        for name in names {
            control.remove_file(&name)?;
        }
        control.remove_self()?;
    }
    // Native Windows shell/PTY wrappers also live directly in the task root.
    // Remove those remaining entries through the same pinned, no-reparse walk
    // before unlinking the directory that contains them.
    remove_tree_contents(task)?;
    task.remove_self()?;
    session.ensure_current_identity()
}

#[cfg(unix)]
fn task_tombstone_prefix(task_id: &str) -> String {
    format!(".aft-task-delete-{task_id}-")
}

#[cfg(unix)]
fn rename_task_to_tombstone(task: &ResolvedTask) -> io::Result<OsString> {
    for _ in 0..32 {
        let tombstone = OsString::from(format!(
            "{}{}",
            task_tombstone_prefix(&task.paths.task_id),
            random_temp_name()?.to_string_lossy()
        ));
        match task.dirs.session.open_dir_at(&tombstone) {
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        task.dirs
            .session
            .rename(OsStr::new(&task.paths.task_id), &tombstone)?;
        let moved = task.dirs.session.open_dir_at(&tombstone)?;
        if !moved.same_identity(&task.dirs.task)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "task directory identity changed during deletion",
            ));
        }
        return Ok(tombstone);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to allocate randomized task deletion name",
    ))
}

#[cfg(unix)]
fn remove_dir_entry(task: &PinnedDir, child: &str) -> io::Result<()> {
    let child = os_cstring(OsStr::new(child))?;
    let result =
        unsafe { libc::unlinkat(task.file.as_raw_fd(), child.as_ptr(), libc::AT_REMOVEDIR) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn task_bundle_files(paths: &TaskPaths) -> Vec<PathBuf> {
    if paths.layout == TaskLayout::Directory {
        return vec![paths.dir.clone()];
    }
    vec![
        paths.json.clone(),
        paths.stdout.clone(),
        paths.stderr.clone(),
        paths.exit.clone(),
        paths.pipeline_status.clone(),
        paths.pty.clone(),
        paths.sandbox_unavailable.clone(),
        paths.command.clone(),
        paths.wrapper.clone(),
        paths.environment.clone(),
        paths.manifest.clone(),
        paths.sandbox_profile.clone(),
        paths.dir.join(format!("{}.ps1", paths.task_id)),
        paths.dir.join(format!("{}.bat", paths.task_id)),
    ]
}

pub fn write_kill_marker_if_absent(paths: &TaskPaths) -> io::Result<()> {
    // A concurrent replace (child exit write racing this kill marker) shows up
    // as a zero-link validated open; the replacement file is the child's real
    // exit marker, so re-opening resolves the race in either direction. Bounded
    // retries: the race is a single rename, not a sustained condition.
    let mut attempts = 0;
    loop {
        attempts += 1;
        // The exit file is opened writable so an empty one can be filled in
        // place; a read-only handle cannot be truncated (ftruncate fails with
        // EINVAL on macOS and Linux). Writing in place keeps the file the
        // child's inherited exit handle points at. If the path is replaced
        // after this open, the handle refers to the unlinked original, whose
        // zero link count makes validation report a concurrent replace, and
        // the retry below re-opens the replacement.
        let result = match open_task_artifact_for_write(paths, TaskArtifact::Exit) {
            Ok(file) if file.len()? > 0 => Ok(()),
            Ok(mut file) => file.replace_contents(b"killed"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let resolved = resolve_task_layout(&paths.session_dir, &paths.task_id)?;
                randomized_atomic_replace(
                    &resolved.dirs.io,
                    &resolved.paths.artifact_name(TaskArtifact::Exit),
                    b"killed",
                )
            }
            Err(error) => Err(error),
        };
        match result {
            Err(error)
                if attempts < 3
                    && error.kind() == io::ErrorKind::Interrupted
                    && error.to_string().contains(ARTIFACT_CONCURRENTLY_REPLACED) => {}
            other => return other,
        }
    }
}

pub fn read_exit_marker(paths: &TaskPaths) -> io::Result<Option<ExitMarker>> {
    read_exit_marker_with_reason(paths, super::watchdog::MarkerReadReason::Unprobed)
}

pub(super) fn read_exit_marker_with_reason(
    paths: &TaskPaths,
    reason: super::watchdog::MarkerReadReason,
) -> io::Result<Option<ExitMarker>> {
    super::watchdog::record_marker_read(reason);
    let mut file = match open_task_artifact(paths, TaskArtifact::Exit) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    #[cfg(test)]
    work_counts::record_marker_read();
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    let content = content.trim();
    if content.is_empty() {
        return Ok(None);
    }
    if content == "killed" {
        return Ok(Some(ExitMarker::Killed));
    }
    Ok(content.parse::<i32>().ok().map(ExitMarker::Code))
}

/// Memory of the last full exit-marker read that found an empty marker file,
/// so the watchdog can skip re-resolving a marker that has not changed.
///
/// The marker file is created empty when a task starts and stays empty until
/// the task exits, so for a running task every full read (pinning the session,
/// task and I/O directories, reading and parsing the task metadata, then
/// opening and reading the marker) finds nothing. One `lstat` of the marker
/// path compares it with the empty file seen last time: by device and inode
/// on Unix, by creation and modification time on Windows, which has no stable
/// file ID in `std`. Either way, a marker with any content is never skipped.
#[derive(Debug, Default)]
pub(crate) struct ExitMarkerProbe {
    /// Identity of the marker file when a full read last found it empty.
    empty: Option<EmptyMarkerIdentity>,
    polled: bool,
}

/// Identity of an empty, regular marker file. Only empty files are recorded:
/// any byte written to the marker changes its size, which forces a full read,
/// so a partially written marker is re-read on every poll exactly as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EmptyMarkerIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(windows)]
    created: u64,
    modified: Option<SystemTime>,
}

impl EmptyMarkerIdentity {
    fn of(metadata: &fs::Metadata) -> Option<Self> {
        if !metadata.file_type().is_file() || metadata.len() != 0 {
            return None;
        }
        Some(Self {
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
            #[cfg(windows)]
            created: metadata.creation_time(),
            modified: metadata.modified().ok(),
        })
    }
}

/// Watchdog form of [`read_exit_marker`]: the same result, without the full
/// validated read while the marker is missing or is still the empty file a
/// previous full read already found empty.
///
/// - A missing marker returns `None` after one `lstat`, which is what the
///   full read returns for a missing marker or task layout.
/// - A marker that appears or gains bytes no longer matches the remembered
///   empty file, so the next poll does the full read and sees it. A file
///   renamed over the marker with content is seen the same way; one renamed
///   over it empty is told apart by inode on Unix, but may look unchanged on
///   Windows, where it would read as `None` anyway.
/// - Anything other than an unchanged empty regular file (content, a symlink,
///   a stat error) goes through the full read, whose path validation and
///   errors are unchanged.
pub(crate) fn poll_exit_marker(
    paths: &TaskPaths,
    probe: &mut ExitMarkerProbe,
) -> io::Result<Option<ExitMarker>> {
    let first_poll = !std::mem::replace(&mut probe.polled, true);
    #[cfg(test)]
    work_counts::record_stat();
    let mut nonempty = false;
    let identity = match fs::symlink_metadata(&paths.exit) {
        Ok(metadata) => {
            nonempty = metadata.is_file() && metadata.len() > 0;
            EmptyMarkerIdentity::of(&metadata)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            probe.empty = None;
            return Ok(None);
        }
        Err(_) => None,
    };
    if identity.is_some() && identity == probe.empty {
        return Ok(None);
    }
    probe.empty = None;
    let reason = if first_poll {
        super::watchdog::MarkerReadReason::FirstPoll
    } else if nonempty {
        super::watchdog::MarkerReadReason::NonemptyMarker
    } else {
        super::watchdog::MarkerReadReason::ChangedProbe
    };
    let marker = read_exit_marker_with_reason(paths, reason)?;
    if marker.is_none() {
        // `identity` was sampled before the read. If bytes landed after the
        // sample, the size no longer matches it and the next poll reads again.
        probe.empty = identity;
    }
    Ok(marker)
}

/// Publish a terminal marker through its validated I/O directory. When proof is
/// unavailable, remove only an empty placeholder, never an existing result.
pub(crate) fn record_terminal_artifact(
    paths: &TaskPaths,
    artifact: TaskArtifact,
    content: Option<&[u8]>,
) -> io::Result<()> {
    let resolved = resolve_task_layout(&paths.session_dir, &paths.task_id)?;
    let name = resolved.paths.artifact_name(artifact);
    if let Some(content) = content {
        return randomized_atomic_replace(&resolved.dirs.io, &name, content);
    }
    match resolved.dirs.io.open_file(&name, false) {
        Ok(file) if file.metadata()?.len() == 0 => {
            drop(file);
            resolved.dirs.io.remove_file(&name)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) fn persist_terminal_exit(paths: &TaskPaths, metadata: &mut PersistedTask) {
    let (content, unavailable) = match metadata.exit_code {
        Some(code) if i32::try_from(code).is_ok() => (Some(format!("{code}\n")), None),
        Some(code) => (None, Some(format!(
            "exit code {code} is recorded in task metadata but exceeds the local exit marker's i32 range",
        ))),
        None if metadata.status == BgTaskStatus::Killed => (Some("killed\n".into()), None),
        None => (None, Some(metadata.status_reason.clone().unwrap_or_else(||
            "outcome unknown: no exit status was received".into()
        ))),
    };
    metadata.exit_marker_unavailable = match record_terminal_artifact(
        paths,
        TaskArtifact::Exit,
        content.as_deref().map(str::as_bytes),
    ) {
        Ok(()) => unavailable,
        Err(error) => Some(format!("exit marker persistence failed: {error}")),
    };
}

pub fn randomized_atomic_replace(dir: &PinnedDir, name: &OsStr, content: &[u8]) -> io::Result<()> {
    atomic_replace(dir, name, content, ReplaceSync::None)
}

pub(crate) fn sync_task_file(file: &File) -> io::Result<()> {
    #[cfg(test)]
    work_counts::record_sync();
    #[cfg(test)]
    if TASK_IO_FAULT.with(|slot| match slot.get() {
        Some(TaskIoFault::SyncFailureAfter(0)) => {
            slot.set(None);
            true
        }
        Some(TaskIoFault::SyncFailureAfter(remaining)) => {
            slot.set(Some(TaskIoFault::SyncFailureAfter(remaining - 1)));
            false
        }
        _ => false,
    }) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "injected task sync failure",
        ));
    }
    #[cfg(test)]
    let started = std::time::Instant::now();
    let result = file.sync_all();
    #[cfg(test)]
    work_counts::record_sync_time(started.elapsed());
    result
}

fn sync_task_directory(dir: &PinnedDir) -> io::Result<()> {
    #[cfg(unix)]
    {
        sync_task_file(&dir.file)
    }
    #[cfg(windows)]
    {
        // FlushFileBuffers requires GENERIC_WRITE even for a directory. Keep
        // ordinary pinned opens read-only; acquire write access only to flush,
        // and verify that the reopened handle still names the pinned folder.
        let writable = PinnedDir {
            file: OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
                .open(&dir.path)?,
            path: dir.path.clone(),
            #[cfg(test)]
            cache_device_override: dir.cache_device_override,
        };
        validate_directory_handle(&writable.file)?;
        if !dir.same_identity(&writable)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "pinned directory path identity changed",
            ));
        }
        sync_task_file(&writable.file)?;
        dir.ensure_current_identity()
    }
}

fn atomic_replace(
    dir: &PinnedDir,
    name: &OsStr,
    content: &[u8],
    sync: ReplaceSync,
) -> io::Result<()> {
    for _ in 0..32 {
        let temporary = random_temp_name()?;
        let mut file = match dir.open_new_file(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let result = (|| {
            file.write_all(content)?;
            if sync != ReplaceSync::None {
                sync_task_file(&file)?;
            }
            // Atomic replacement protects readers after a daemon kill. Task
            // history is mirrored in a weaker database and is not a durable log.
            validate_regular_handle(&file)?;
            #[cfg(test)]
            atomic_replace_crash_for_test(false);
            dir.rename(&temporary, name)?;
            #[cfg(test)]
            atomic_replace_crash_for_test(true);
            if sync == ReplaceSync::FileAndDirectory {
                sync_task_directory(dir)?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = dir.remove_file(&temporary);
        }
        return result;
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to allocate a randomized atomic-write name",
    ))
}

#[cfg(test)]
thread_local! {
    static ATOMIC_REPLACE_CRASH: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn atomic_replace_crash_for_test(after_rename: bool) {
    if ATOMIC_REPLACE_CRASH.with(|phase| phase.get()) == Some(after_rename) {
        // Exit without unwinding so the subprocess leaves the interrupted
        // replacement exactly as a killed daemon would, including its temp file.
        std::process::exit(86);
    }
}

pub fn create_control_file(dirs: &TaskDirs, name: &str, content: &[u8]) -> io::Result<File> {
    let mut file = dirs.control.open_new_file(OsStr::new(name))?;
    file.write_all(content)?;
    // These immutable payloads are verified and read through held handles
    // before spawning. They need cross-process visibility, not power-loss
    // durability: recovery never re-executes a task from its payload files.
    file.seek(SeekFrom::Start(0))?;
    validate_regular_handle(&file)?;
    Ok(file)
}

pub fn open_control_file(task: &ResolvedTask, name: &str) -> io::Result<File> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid control file name",
        ));
    }
    task.dirs.control.open_file(OsStr::new(name), false)
}

#[cfg(test)]
#[path = "metadata_persistence_test.rs"]
mod metadata_persistence_tests;

/// Task record writes per task id, counted across threads. A remote task's
/// record is written by its worker thread, which the thread-local
/// [`work_counts`] of the test thread never sees.
#[cfg(test)]
pub(crate) mod record_writes {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    fn counts() -> &'static Mutex<HashMap<String, usize>> {
        static COUNTS: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
        COUNTS.get_or_init(Default::default)
    }

    pub(crate) fn record(task_id: &str) {
        *counts()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(task_id.to_owned())
            .or_default() += 1;
    }

    /// Record writes started for `task_id`, including writes whose content
    /// was unchanged (those still sync the record).
    // Read only by the Unix-only remote task tests.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn get(task_id: &str) -> usize {
        counts()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(task_id)
            .copied()
            .unwrap_or_default()
    }
}

#[derive(Debug)]
pub struct ValidatedArtifact {
    file: File,
}

impl ValidatedArtifact {
    fn new(file: File) -> io::Result<Self> {
        validate_regular_handle(&file)?;
        #[cfg(test)]
        work_counts::record_artifact_open();
        Ok(Self { file })
    }

    pub fn len(&self) -> io::Result<u64> {
        validate_regular_handle(&self.file)?;
        #[cfg(test)]
        work_counts::record_stat();
        Ok(self.file.metadata()?.len())
    }

    pub fn rewind(&mut self) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(0)).map(|_| ())
    }

    pub fn tail(&mut self, max_bytes: usize) -> io::Result<(Vec<u8>, bool)> {
        let len = self.len()?;
        let read_len = len.min(max_bytes as u64);
        self.file
            .seek(SeekFrom::Start(len.saturating_sub(read_len)))?;
        let mut bytes = Vec::with_capacity(read_len as usize);
        Read::by_ref(&mut self.file)
            .take(read_len)
            .read_to_end(&mut bytes)?;
        #[cfg(test)]
        work_counts::record_bytes(bytes.len());
        Ok((bytes, len > max_bytes as u64))
    }

    pub fn read_range(&mut self, start: u64, len: u64) -> io::Result<Vec<u8>> {
        self.file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::with_capacity(len.min(usize::MAX as u64) as usize);
        Read::by_ref(&mut self.file)
            .take(len)
            .read_to_end(&mut bytes)?;
        #[cfg(test)]
        work_counts::record_bytes(bytes.len());
        Ok(bytes)
    }

    pub fn read_all(&mut self) -> io::Result<Vec<u8>> {
        self.rewind()?;
        let mut bytes = Vec::new();
        self.file.read_to_end(&mut bytes)?;
        #[cfg(test)]
        work_counts::record_bytes(bytes.len());
        Ok(bytes)
    }

    /// Requires a handle from `open_task_artifact_for_write`; a read-only
    /// handle fails the truncate with EINVAL.
    pub fn replace_contents(&mut self, content: &[u8]) -> io::Result<()> {
        validate_regular_handle(&self.file)?;
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(content)?;
        Ok(())
    }

    pub fn try_clone_file(&self) -> io::Result<File> {
        validate_regular_handle(&self.file)?;
        self.file.try_clone()
    }
}

impl Read for ValidatedArtifact {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

impl Seek for ValidatedArtifact {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}

pub fn open_task_artifact(
    paths: &TaskPaths,
    artifact: TaskArtifact,
) -> io::Result<ValidatedArtifact> {
    retry_artifact_read(|| open_task_artifact_once(paths, artifact))
}

pub(super) fn open_task_artifact_once(
    paths: &TaskPaths,
    artifact: TaskArtifact,
) -> io::Result<ValidatedArtifact> {
    open_task_artifact_with_access(paths, artifact, false)
}

/// Open and sample the same artifact handle, reopening if either validation
/// races an atomic replacement. Callers calculate offsets only after this step.
pub(crate) fn sample_task_artifact(
    paths: &TaskPaths,
    artifact: TaskArtifact,
) -> io::Result<(ValidatedArtifact, u64)> {
    retry_artifact_read(|| {
        let file = open_task_artifact_once(paths, artifact)?;
        let len = file.len()?;
        Ok((file, len))
    })
}

/// Like `open_task_artifact`, but the handle can also rewrite the artifact.
fn open_task_artifact_for_write(
    paths: &TaskPaths,
    artifact: TaskArtifact,
) -> io::Result<ValidatedArtifact> {
    open_task_artifact_with_access(paths, artifact, true)
}

fn open_task_artifact_with_access(
    paths: &TaskPaths,
    artifact: TaskArtifact,
    write: bool,
) -> io::Result<ValidatedArtifact> {
    validate_task_id(&paths.task_id)?;
    let resolved = resolve_task_layout(&paths.session_dir, &paths.task_id)?;
    if resolved.paths.layout != paths.layout {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background task layout identity changed",
        ));
    }
    // The resolver has already read the current metadata and checked its task
    // identity. Re-reading that same file adds no artifact-path validation;
    // the open below remains relative to the freshly pinned I/O directory.
    let file = resolved
        .dirs
        .io
        .open_file(&resolved.paths.artifact_name(artifact), write)?;
    ValidatedArtifact::new(file)
}

pub fn replace_artifact_with_tail(
    paths: &TaskPaths,
    artifact: TaskArtifact,
    retain_bytes: u64,
) -> io::Result<u64> {
    let (mut source, len) = sample_task_artifact(paths, artifact)?;
    if len <= retain_bytes {
        return Ok(0);
    }
    let mut tail = source.read_range(len.saturating_sub(retain_bytes), retain_bytes)?;
    align_tail_start(&mut tail);
    let resolved = resolve_task_layout(&paths.session_dir, &paths.task_id)?;
    randomized_atomic_replace(
        &resolved.dirs.io,
        &resolved.paths.artifact_name(artifact),
        &tail,
    )?;
    Ok(len.saturating_sub(tail.len() as u64))
}

#[derive(Debug)]
pub struct TaskIoHandles {
    pub dirs: TaskDirs,
    stdout: Option<File>,
    stderr: Option<File>,
    exit: File,
    pty: Option<File>,
    pipeline_status: Option<File>,
    sandbox_unavailable: File,
    write_counter: crate::write_ledger::Counter,
}

impl TaskIoHandles {
    /// Replay fallback retains the same validated task artifacts rather than
    /// creating a new task or replacing its output with another set of files.
    #[cfg(unix)]
    pub(crate) fn reopen(task: &ResolvedTask) -> io::Result<Self> {
        let open = |a: TaskArtifact| task.dirs.io.open_file(OsStr::new(a.file_name()), true);
        let mut stdout = open(TaskArtifact::Stdout)?;
        stdout.seek(SeekFrom::End(0))?;
        let mut stderr = open(TaskArtifact::Stderr)?;
        stderr.seek(SeekFrom::End(0))?;
        Ok(Self {
            dirs: task.dirs.clone(),
            stdout: Some(stdout),
            stderr: Some(stderr),
            exit: open(TaskArtifact::Exit)?,
            pipeline_status: Some(open(TaskArtifact::PipelineStatus)?),
            pty: None,
            sandbox_unavailable: open(TaskArtifact::SandboxUnavailable)?,
            write_counter: crate::write_ledger::register(
                crate::write_ledger::Domain::BashTaskIo,
                task.paths.dir.display().to_string(),
            ),
        })
    }
    pub fn create(
        task: &ResolvedTask,
        mode: BgMode,
        capture_pipeline_status: bool,
    ) -> io::Result<Self> {
        #[cfg(test)]
        match TASK_IO_FAULT.with(std::cell::Cell::get) {
            Some(TaskIoFault::OutputHandlesEnospc) => return Err(task_storage_full_for_test()),
            Some(TaskIoFault::OutputHandlesMisleadingText) => {
                return Err(io::Error::other(
                    "storage limit exceeded; startup deadline expired",
                ));
            }
            _ => {}
        }
        if task.paths.layout != TaskLayout::Directory {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "new task output handles require the directory layout",
            ));
        }
        let (stdout, stderr, pty) = match mode {
            BgMode::Pipes => (
                Some(
                    task.dirs
                        .io
                        .open_new_file(OsStr::new(TaskArtifact::Stdout.file_name()))?,
                ),
                Some(
                    task.dirs
                        .io
                        .open_new_file(OsStr::new(TaskArtifact::Stderr.file_name()))?,
                ),
                None,
            ),
            BgMode::Pty => (
                None,
                None,
                Some(
                    task.dirs
                        .io
                        .open_new_file(OsStr::new(TaskArtifact::Pty.file_name()))?,
                ),
            ),
        };
        Ok(Self {
            dirs: task.dirs.clone(),
            stdout,
            stderr,
            exit: task
                .dirs
                .io
                .open_new_file(OsStr::new(TaskArtifact::Exit.file_name()))?,
            pipeline_status: capture_pipeline_status
                .then(|| {
                    task.dirs
                        .io
                        .open_new_file(OsStr::new(TaskArtifact::PipelineStatus.file_name()))
                })
                .transpose()?,
            pty,
            sandbox_unavailable: task
                .dirs
                .io
                .open_new_file(OsStr::new(TaskArtifact::SandboxUnavailable.file_name()))?,
            write_counter: crate::write_ledger::register(
                crate::write_ledger::Domain::BashTaskIo,
                task.paths.dir.display().to_string(),
            ),
        })
    }

    pub fn clone_file(&self, artifact: TaskArtifact) -> io::Result<File> {
        let file = match artifact {
            TaskArtifact::Stdout => self.stdout.as_ref(),
            TaskArtifact::Stderr => self.stderr.as_ref(),
            TaskArtifact::Exit => Some(&self.exit),
            TaskArtifact::PipelineStatus => self.pipeline_status.as_ref(),
            TaskArtifact::Pty => self.pty.as_ref(),
            TaskArtifact::SandboxUnavailable => Some(&self.sandbox_unavailable),
        }
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "task artifact is not pre-opened")
        })?;
        validate_regular_handle(file)?;
        file.try_clone()
    }

    #[cfg(unix)]
    pub fn inheritable_file(&self, artifact: TaskArtifact) -> io::Result<File> {
        let file = self.clone_file(artifact)?;
        // Only the child's pre-exec allowlist may make a marker inheritable.
        // Clearing CLOEXEC here would expose it to unrelated concurrent spawns
        // in the daemon before the owning wrapper has even been launched.
        set_close_on_exec(file.as_raw_fd(), true)?;
        Ok(file)
    }

    pub fn write(&mut self, artifact: TaskArtifact, content: &[u8]) -> io::Result<()> {
        let file = match artifact {
            TaskArtifact::Stdout => self.stdout.as_mut(),
            TaskArtifact::Stderr => self.stderr.as_mut(),
            TaskArtifact::Exit => Some(&mut self.exit),
            TaskArtifact::PipelineStatus => self.pipeline_status.as_mut(),
            TaskArtifact::Pty => self.pty.as_mut(),
            TaskArtifact::SandboxUnavailable => Some(&mut self.sandbox_unavailable),
        }
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "task artifact is not pre-opened")
        })?;
        validate_regular_handle(file)?;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(content)?;
        self.write_counter.credit_logical(content.len() as u64);
        Ok(())
    }

    pub fn artifact_len(&self, artifact: TaskArtifact) -> io::Result<u64> {
        let file = match artifact {
            TaskArtifact::Stdout => self.stdout.as_ref(),
            TaskArtifact::Stderr => self.stderr.as_ref(),
            TaskArtifact::Exit => Some(&self.exit),
            TaskArtifact::PipelineStatus => self.pipeline_status.as_ref(),
            TaskArtifact::Pty => self.pty.as_ref(),
            TaskArtifact::SandboxUnavailable => Some(&self.sandbox_unavailable),
        }
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "task artifact is not pre-opened")
        })?;
        validate_regular_handle(file)?;
        #[cfg(test)]
        work_counts::record_stat();
        Ok(file.metadata()?.len())
    }
}

pub fn repin_task_io(paths: &TaskPaths) -> io::Result<TaskDirs> {
    let resolved = resolve_task_layout(&paths.session_dir, &paths.task_id)?;
    let metadata = read_task_at(&resolved)?;
    if metadata.task_id != paths.task_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background task metadata identity mismatch",
        ));
    }
    Ok(resolved.dirs)
}

pub fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn random_task_id() -> io::Result<String> {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(format!(
        "bash-{}",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

#[cfg(test)]
pub(crate) fn open_unregistered_artifact(path: &Path) -> io::Result<ValidatedArtifact> {
    ValidatedArtifact::new(open_validated_path(path, false)?)
}

#[cfg(test)]
pub(crate) fn replace_unregistered_with_tail(path: &Path, retain_bytes: u64) -> io::Result<u64> {
    let mut source = open_unregistered_artifact(path)?;
    let len = source.len()?;
    if len <= retain_bytes {
        return Ok(0);
    }
    let mut tail = source.read_range(len.saturating_sub(retain_bytes), retain_bytes)?;
    align_tail_start(&mut tail);
    let parent = PinnedDir::open(path.parent().unwrap_or_else(|| Path::new(".")))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    randomized_atomic_replace(&parent, name, &tail)?;
    Ok(len.saturating_sub(tail.len() as u64))
}

fn align_tail_start(bytes: &mut Vec<u8>) {
    let prefix = bytes
        .iter()
        .take_while(|byte| **byte & 0xc0 == 0x80)
        .count();
    if prefix > 0 {
        bytes.drain(..prefix);
    }
}

fn open_validated_path(path: &Path, write: bool) -> io::Result<File> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    PinnedDir::open(parent)?.open_file(name, write)
}

#[cfg(all(test, unix))]
pub(crate) fn metadata_open_attempts_for_test() -> u32 {
    METADATA_OPEN_ATTEMPTS
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn refused_task_layout_creation_leaves_storage_root_untouched() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = tempfile::tempdir().unwrap();
        for (name, allocate) in [("create", false), ("allocate", true)] {
            let storage = fixture.path().join(name);
            std::fs::create_dir(&storage).unwrap();
            std::fs::set_permissions(&storage, std::fs::Permissions::from_mode(0o755)).unwrap();
            let before_mode = std::fs::metadata(&storage).unwrap().permissions().mode() & 0o777;
            let before_entries = std::fs::read_dir(&storage).unwrap().count();

            let error = crate::production_storage::with_test_account(&storage, false, || {
                let result = if allocate {
                    super::allocate_task_layout(&storage, "session")
                } else {
                    super::create_task_layout(&storage, "session", "bash-0000000000000040")
                };
                result.unwrap_err()
            });

            assert!(
                error.to_string().contains(crate::production_storage::CODE),
                "protected task write was not refused: {error}"
            );
            assert_eq!(
                std::fs::metadata(&storage).unwrap().permissions().mode() & 0o777,
                before_mode,
                "refused task creation changed storage permissions"
            );
            assert_eq!(
                std::fs::read_dir(&storage).unwrap().count(),
                before_entries,
                "refused task creation added a storage entry"
            );
            assert!(!storage.join("bash-tasks").exists());
            assert!(!storage.join(".bash-task-session.lock").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn empty_task_directory_is_classified_and_removed_on_delete() {
        let storage = tempfile::tempdir().unwrap();
        let task =
            super::create_task_layout(storage.path(), "empty-layout", "bash-0000000000000010")
                .unwrap();
        let paths = task.paths.clone();
        drop(task);
        std::fs::remove_dir(&paths.control_dir).unwrap();
        std::fs::remove_dir(&paths.io_dir).unwrap();

        let error = super::resolve_uninitialized_task_layout(&paths.session_dir, &paths.task_id)
            .unwrap_err();
        assert!(
            super::is_task_layout_not_found(&error),
            "missing required layout directories were not classified: {error}"
        );
        let error = super::delete_task_bundle(&paths).unwrap_err();
        assert!(super::is_task_layout_not_found(&error));
        assert!(!paths.dir.exists(), "empty task directory was left behind");
    }

    #[test]
    fn task_session_rmdir_waits_for_creator_with_an_open_session_directory() {
        let storage = tempfile::tempdir().unwrap();
        let initial =
            super::create_task_layout(storage.path(), "rmdir-race", "bash-0000000000000001")
                .unwrap();
        let initial_paths = initial.paths.clone();
        let session_dir = initial_paths.session_dir.clone();
        drop(initial);
        std::fs::remove_dir_all(&initial_paths.dir).unwrap();

        let (creator_entered, release_creator) =
            super::install_session_create_gate_for_test(&session_dir);
        std::thread::scope(|scope| {
            let creator = scope.spawn(|| {
                super::create_task_layout(storage.path(), "rmdir-race", "bash-0000000000000002")
            });
            creator_entered
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("creator did not open the session directory");

            let (remove_started_tx, remove_started_rx) = std::sync::mpsc::channel();
            let (removed_tx, removed_rx) = std::sync::mpsc::channel();
            let remove_session_dir = session_dir.clone();
            let remover = scope.spawn(move || {
                remove_started_tx.send(()).unwrap();
                removed_tx
                    .send(super::remove_empty_session_directory(&remove_session_dir))
                    .unwrap();
            });
            remove_started_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            assert!(
                removed_rx
                    .recv_timeout(std::time::Duration::from_millis(100))
                    .is_err(),
                "session removal passed a task creator holding the shared gate"
            );

            release_creator.send(()).unwrap();
            let created = creator.join().unwrap().unwrap();
            remover.join().unwrap();
            let removed = removed_rx.recv().unwrap().unwrap();
            assert!(!removed, "a session containing a new task was removed");
            assert!(created.paths.dir.is_dir());
        });
        assert!(session_dir.join("bash-0000000000000002").is_dir());
    }

    #[test]
    fn parallel_session_layout_creators_keep_their_own_gates() {
        let storage = tempfile::tempdir().unwrap();
        let session_a = super::session_tasks_dir(storage.path(), "parallel-gate-a");
        let session_b = super::session_tasks_dir(storage.path(), "parallel-gate-b");
        let (entered_a, release_a) = super::install_session_create_gate_for_test(&session_a);
        let (entered_b, release_b) = super::install_session_create_gate_for_test(&session_b);

        std::thread::scope(|scope| {
            let creator_a = scope.spawn(|| {
                super::create_task_layout(
                    storage.path(),
                    "parallel-gate-a",
                    "bash-0000000000000030",
                )
            });
            entered_a
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("session A creator lost its gate");

            let creator_b = scope.spawn(|| {
                super::create_task_layout(
                    storage.path(),
                    "parallel-gate-b",
                    "bash-0000000000000031",
                )
            });
            release_a.send(()).unwrap();
            let created_a = creator_a.join().unwrap().unwrap();
            entered_b
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("session B creator lost its gate");
            release_b.send(()).unwrap();
            let created_b = creator_b.join().unwrap().unwrap();
            assert!(created_a.paths.dir.is_dir());
            assert!(created_b.paths.dir.is_dir());
        });
    }

    // Freeze the scope-blind version-7 task shape rather than deserializing
    // through PersistedTask, whose new scope field would hide rollback behavior.
    // Serde's unknown-field handling lets that reader ignore the added stamp.
    #[derive(Serialize, Deserialize)]
    struct ScopeBlindV7Task {
        schema_version: u32,
        harness: Option<String>,
        task_id: String,
        session_id: String,
        command: String,
        #[serde(default)]
        pipeline_segments: Vec<String>,
        pipeline_status_unavailable: Option<String>,
        #[serde(default)]
        mode: BgMode,
        workdir: PathBuf,
        project_root: Option<PathBuf>,
        status: BgTaskStatus,
        started_at: u64,
        finished_at: Option<u64>,
        duration_ms: Option<u64>,
        timeout_ms: Option<u64>,
        #[serde(default)]
        default_hard_kill: bool,
        exit_code: Option<i64>,
        child_pid: Option<u32>,
        pgid: Option<i32>,
        live_descendants: Option<Vec<LiveDescendant>>,
        #[serde(default)]
        live_descendants_omitted: usize,
        completion_delivered: bool,
        #[serde(default = "default_notify_on_completion")]
        notify_on_completion: bool,
        #[serde(default = "default_compressed")]
        compressed: bool,
        pty_rows: Option<u16>,
        pty_cols: Option<u16>,
        #[serde(default)]
        scanner_report: Vec<PermissionAsk>,
        #[serde(default)]
        sandbox_native: bool,
        sandbox_temp_dir: Option<PathBuf>,
        status_reason: Option<String>,
        #[serde(default)]
        output_incomplete: bool,
        call_key: Option<crate::bash_background::TaskCallKey>,
        remote: Option<crate::bash_background::registry::remote::RemoteTask>,
        execution_note: Option<String>,
        #[serde(default)]
        incomplete_output: Vec<(u64, u64)>,
        #[serde(default)]
        stripped_env_names: Vec<String>,
        #[serde(default)]
        local_fallback_started: bool,
        #[serde(default)]
        cancelled_call_unannounced: bool,
    }

    fn read_scope_blind_v7(bytes: &[u8]) -> Result<ScopeBlindV7Task, String> {
        let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        let version = value["schema_version"].as_u64().ok_or("missing version")?;
        if !(2..=7).contains(&version) {
            return Err(format!(
                "unsupported background task schema_version {version}"
            ));
        }
        let task: ScopeBlindV7Task = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        validate_task_id(&task.task_id).map_err(|e| e.to_string())?;
        Ok(task)
    }

    #[test]
    fn stamped_v7_is_readable_by_scope_blind_v7_reader() {
        let directory = tempfile::tempdir().unwrap();
        let (resolved, mut task) = counted_task(directory.path());
        task.scope = Some(subc_protocol::scope::ScopeStamp::new(
            subc_protocol::Principal::Direct,
            "scope",
            1,
            subc_protocol::scope::ScopeKind::Head,
        ));
        write_task_at(&resolved, &task).unwrap();
        let bytes = std::fs::read(&resolved.paths.json).unwrap();
        let written: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(written["schema_version"], 7);
        assert_eq!(written["scope"]["ref"], "scope");
        let old = read_scope_blind_v7(&bytes).expect("scope-blind reader accepts stamped v7");
        assert_eq!(old.task_id, task.task_id);
        assert_eq!(old.session_id, task.session_id);
        assert_eq!(old.command, task.command);
        assert_eq!(old.status, task.status);
        assert!(serde_json::to_value(old).unwrap().get("scope").is_none());
        assert_eq!(read_task_at(&resolved).unwrap().scope, task.scope);
    }

    #[test]
    fn scope_stamp_task_versions_and_unstamped_v7_are_compatible() {
        let directory = tempfile::tempdir().unwrap();
        let (resolved, mut task) = counted_task(directory.path());
        assert_eq!(SCHEMA_VERSION, 7);
        let stamp = subc_protocol::scope::ScopeStamp::new(
            subc_protocol::Principal::Reserved {
                module_id: "owner".into(),
            },
            "scope",
            1,
            subc_protocol::scope::ScopeKind::Head,
        )
        .with_owner_authorized(true);
        task.scope = Some(stamp.clone());
        task.mark_terminal(
            BgTaskStatus::Killed,
            Some(137),
            Some("scope revoked".into()),
        );
        write_task_at(&resolved, &task).unwrap();
        assert_eq!(read_task_at(&resolved).unwrap().scope, Some(stamp.clone()));
        assert_eq!(
            read_task_at(&resolved).unwrap().status_reason.as_deref(),
            Some("scope revoked")
        );
        let mut value = serde_json::to_value(&task).unwrap();
        // Version-7 background-task files also predate scope tracking.
        // A file without scope must remain readable and must not gain one.
        value.as_object_mut().unwrap().remove("scope");
        std::fs::write(&resolved.paths.json, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(read_task_at(&resolved).unwrap().scope.is_none());
        value["schema_version"] = serde_json::json!(6);
        std::fs::write(&resolved.paths.json, serde_json::to_vec(&value).unwrap()).unwrap();
        let old = read_task_at(&resolved).unwrap();
        assert_eq!(old.schema_version, 6);
        write_task_at(&resolved, &old).unwrap();
        assert_eq!(read_task_at(&resolved).unwrap().schema_version, 7);
        value["schema_version"] = serde_json::json!(SCHEMA_VERSION + 1);
        std::fs::write(&resolved.paths.json, serde_json::to_vec(&value).unwrap()).unwrap();
        let error = read_task_at(&resolved).unwrap_err().to_string();
        assert!(error.contains("storage_requires_newer_reader"), "{error}");
        assert!(error.contains("7"), "{error}");
    }
    #[test]
    fn schema_pin_is_an_optional_defaulted_field_and_keeps_schema_version() {
        // The task-record format keeps version 7: older files without the
        // field still read, with no pin.
        assert_eq!(SCHEMA_VERSION, 7);
        let directory = tempfile::tempdir().unwrap();
        let (resolved, task) = counted_task(directory.path());
        assert!(task.schema_pin.is_none());
        let mut value = serde_json::to_value(&task).unwrap();
        assert!(
            value.get("schema_pin").is_none(),
            "pinless tasks omit the field"
        );
        // A file written before the field existed reads with no pin.
        value.as_object_mut().unwrap().remove("schema_pin");
        std::fs::write(&resolved.paths.json, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(read_task_at(&resolved).unwrap().schema_pin.is_none());
        // The dispatch pin is recorded at spawn when the call carried one.
        let pinned = crate::bash_background::with_schema_pin(Some("sha256:pin".into()), || {
            PersistedTask::starting(
                "bash-0000000000000043".into(),
                "counted".into(),
                "true".into(),
                directory.path().to_path_buf(),
                None,
                None,
                true,
                false,
            )
        });
        assert_eq!(pinned.schema_pin.as_deref(), Some("sha256:pin"));
        // A pin survives a disk round trip.
        let mut task = task;
        task.schema_pin = Some("sha256:pin".into());
        write_task_at(&resolved, &task).unwrap();
        assert_eq!(
            read_task_at(&resolved).unwrap().schema_pin.as_deref(),
            Some("sha256:pin")
        );
    }
    #[test]
    fn durability_bash_record_payload_count() {
        let storage = tempfile::tempdir().unwrap();
        let task =
            create_task_layout(storage.path(), "durability", "bash-0000000000000601").unwrap();
        crate::durability::take();
        for name in [
            "command.sh",
            "wrapper.sh",
            "environment.bin",
            "manifest.blake3",
        ] {
            create_control_file(&task.dirs, name, b"payload").unwrap();
        }
        for state in ["starting", "running", "exited", "completed"] {
            randomized_atomic_replace(
                &task.dirs.control,
                OsStr::new("metadata.json"),
                state.as_bytes(),
            )
            .unwrap();
        }
        let events = crate::durability::take();
        assert_eq!(crate::durability::sync_count(&events), 0, "{events:?}");
        assert_eq!(
            fs::read(task.dirs.control.path.join("metadata.json")).unwrap(),
            b"completed"
        );
    }
    use super::*;

    fn valid_id(suffix: u64) -> String {
        format!("bash-{suffix:016x}")
    }

    fn counted_task(storage: &Path) -> (ResolvedTask, PersistedTask) {
        let task = create_task_layout(storage, "counted", &valid_id(42)).unwrap();
        let metadata = PersistedTask::starting(
            task.paths.task_id.clone(),
            "counted".into(),
            "printf 'a realistic long build command'; ".repeat(128),
            storage.into(),
            None,
            None,
            true,
            false,
        );
        write_task_at(&task, &metadata).unwrap();
        (task, metadata)
    }

    #[test]
    fn artifact_open_reads_metadata_once() {
        let storage = tempfile::tempdir().unwrap();
        let (task, _) = counted_task(storage.path());
        let mut handles = TaskIoHandles::create(&task, BgMode::Pipes, false).unwrap();
        let output = b"build output\n".repeat(10_000);
        handles.write(TaskArtifact::Stdout, &output).unwrap();
        work_counts::reset();
        let mut file = open_task_artifact(&task.paths, TaskArtifact::Stdout).unwrap();
        assert_eq!(file.read_all().unwrap(), output);
        let counts = work_counts::get();
        eprintln!("artifact open work: {counts:?}");
        assert_eq!(counts.metadata_reads, 1);
        // Pin the read cost: version validation shares the typed task decode.
        assert_eq!(counts.parses, 1);
        #[cfg(unix)]
        assert_eq!(counts.opens, 7);
    }

    #[test]
    fn terminal_exit_persistence_reports_missing_proof_wide_codes_and_write_failures() {
        for code in [None, Some(7), Some(i64::from(u32::MAX))] {
            let dir = tempfile::tempdir().unwrap();
            let (task, mut metadata) = counted_task(dir.path());
            fs::write(&task.paths.exit, "").unwrap();
            metadata.mark_terminal(
                if code.is_some() {
                    BgTaskStatus::Failed
                } else {
                    BgTaskStatus::FateUnknown
                },
                code,
                Some("terminal proof lost after restart".into()),
            );
            persist_terminal_exit(&task.paths, &mut metadata);
            assert_eq!(metadata.exit_code, code);
            assert_eq!(metadata.schema_version, SCHEMA_VERSION);
            if code == Some(7) {
                assert_eq!(
                    read_exit_marker(&task.paths).unwrap(),
                    Some(ExitMarker::Code(7))
                );
                assert!(metadata.exit_marker_unavailable.is_none());
            } else {
                assert!(!task.paths.exit.exists());
                assert!(metadata.exit_marker_unavailable.is_some());
            }
            write_task_at(&task, &metadata).unwrap();
            assert_eq!(
                read_task_at(&task).unwrap().exit_marker_unavailable,
                metadata.exit_marker_unavailable
            );
            let mut old = serde_json::to_value(&metadata).unwrap();
            old.as_object_mut()
                .unwrap()
                .remove("exit_marker_unavailable");
            assert!(serde_json::from_value::<PersistedTask>(old)
                .unwrap()
                .exit_marker_unavailable
                .is_none());
        }
        let dir = tempfile::tempdir().unwrap();
        let (task, mut metadata) = counted_task(dir.path());
        fs::create_dir(&task.paths.exit).unwrap();
        metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
        persist_terminal_exit(&task.paths, &mut metadata);
        assert_eq!(metadata.exit_code, Some(0));
        assert!(metadata
            .exit_marker_unavailable
            .unwrap()
            .contains("persistence failed"));
    }

    #[test]
    fn missing_exit_marker_reads_metadata_once() {
        let storage = tempfile::tempdir().unwrap();
        let (task, _) = counted_task(storage.path());
        work_counts::reset();
        assert_eq!(read_exit_marker(&task.paths).unwrap(), None);
        let counts = work_counts::get();
        eprintln!("missing marker work: {counts:?}");
        assert_eq!(counts.metadata_reads, 1);
        // A missing marker still needs only one typed metadata decode.
        assert_eq!(counts.parses, 1);
    }

    #[cfg(unix)]
    #[test]
    fn task_lifecycle_round_trips_payloads_and_metadata() {
        let storage = tempfile::tempdir().unwrap();
        let (task, mut metadata) = counted_task(storage.path());
        work_counts::reset();
        // Task metadata uses atomic replacement without a disk flush (the
        // durability design treats bash task history as rebuildable; no
        // flush helper remains in this module), and payload files are only
        // consumed through verified handles.
        write_task_at(&task, &metadata).unwrap();
        for (name, bytes) in [
            (COMMAND_FILE, metadata.command.as_bytes()),
            (WRAPPER_FILE, super::super::process::PAYLOAD_WRAPPER),
            (ENVIRONMENT_FILE, b"PATH=/usr/bin".as_slice()),
            (MANIFEST_FILE, b"verified digest".as_slice()),
        ] {
            let mut held = create_control_file(&task.dirs, name, bytes).unwrap();
            let mut actual = Vec::new();
            held.read_to_end(&mut actual).unwrap();
            assert_eq!(actual, bytes);
        }
        metadata.status = super::super::BgTaskStatus::Running;
        write_task_at(&task, &metadata).unwrap();
        metadata.status = super::super::BgTaskStatus::Completed;
        write_task_at(&task, &metadata).unwrap();
        let counts = work_counts::get();
        eprintln!("task lifecycle work: {counts:?}");
        assert!(read_task_at(&task).unwrap().is_terminal());
    }

    #[cfg(unix)]
    #[test]
    fn read_task_retries_injected_errors_up_to_attempt_limit() {
        let storage = tempfile::tempdir().unwrap();
        let (task, metadata) = counted_task(storage.path());
        let expected_task_id = metadata.task_id.clone();

        inject_read_task_replacement_failures_for_test(3);
        let read = read_task(&task.paths.json).unwrap();
        assert_eq!(read.task_id, expected_task_id);
        assert_eq!(injected_read_task_replacement_failures_for_test(), 0);

        inject_read_task_replacement_failures_for_test(METADATA_OPEN_ATTEMPTS);
        let error = read_task(&task.paths.json).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(error.to_string(), ARTIFACT_CONCURRENTLY_REPLACED);
        assert_eq!(injected_read_task_replacement_failures_for_test(), 0);
    }

    #[cfg(unix)]
    fn with_replacements<T>(
        path: &Path,
        validation: u32,
        replacements: u32,
        content: Vec<u8>,
        read: impl FnOnce() -> io::Result<T>,
    ) -> (io::Result<T>, u32) {
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                BEFORE_ARTIFACT_VALIDATION.with(|slot| slot.replace(None));
            }
        }
        let path = path.to_path_buf();
        let replaced = std::rc::Rc::new(std::cell::Cell::new(0));
        let count = replaced.clone();
        let mut seen = 0;
        BEFORE_ARTIFACT_VALIDATION.with(|slot| {
            assert!(slot.borrow().is_none());
            slot.replace(Some(Box::new(move |file| {
                let opened = file.metadata().unwrap();
                let current = std::fs::metadata(&path).unwrap();
                if (opened.dev(), opened.ino()) != (current.dev(), current.ino()) {
                    return;
                }
                seen += 1;
                if seen == validation && count.get() < replacements {
                    let parent = PinnedDir::open(path.parent().unwrap()).unwrap();
                    randomized_atomic_replace(&parent, path.file_name().unwrap(), &content)
                        .unwrap();
                    count.set(count.get() + 1);
                    seen = 0;
                }
            })));
        });
        let _restore = Restore;
        (read(), replaced.get())
    }

    #[cfg(unix)]
    #[test]
    fn read_task_reopens_atomically_replaced_metadata() {
        let storage = tempfile::tempdir().unwrap();
        let (task, mut metadata) = counted_task(storage.path());
        metadata.command = "replacement metadata".to_string();
        let (read, replaced) = with_replacements(
            &task.paths.json,
            1,
            1,
            serde_json::to_vec(&metadata).unwrap(),
            || read_task(&task.paths.json),
        );
        assert_eq!(
            replaced, 1,
            "the rename must land after open, before validation"
        );
        assert_eq!(read.unwrap().command, "replacement metadata");
    }

    #[cfg(unix)]
    #[test]
    fn artifact_reads_reopen_after_atomic_replacement() {
        let storage = tempfile::tempdir().unwrap();
        let (task, _) = counted_task(storage.path());
        TaskIoHandles::create(&task, BgMode::Pipes, false).unwrap();
        // The first two checks validate the open; the third samples the length.
        for validation in 1..=3 {
            randomized_atomic_replace(
                &task.dirs.io,
                &task.paths.artifact_name(TaskArtifact::Stdout),
                b"original output",
            )
            .unwrap();
            let (read, replaced) = with_replacements(
                &task.paths.stdout,
                validation,
                1,
                b"replacement output".to_vec(),
                || {
                    let (mut file, len) = sample_task_artifact(&task.paths, TaskArtifact::Stdout)?;
                    file.read_range(0, len)
                },
            );
            assert_eq!(replaced, 1, "validation {validation} must observe a rename");
            assert_eq!(read.unwrap(), b"replacement output");
        }
        for validation in 1..=2 {
            randomized_atomic_replace(
                &task.dirs.io,
                &task.paths.artifact_name(TaskArtifact::Stdout),
                b"original output",
            )
            .unwrap();
            let (read, replaced) = with_replacements(
                &task.paths.stdout,
                validation,
                1,
                b"reopened output".to_vec(),
                || open_task_artifact(&task.paths, TaskArtifact::Stdout)?.read_all(),
            );
            assert_eq!(replaced, 1);
            assert_eq!(read.unwrap(), b"reopened output");
        }
    }

    #[cfg(unix)]
    #[test]
    fn buffer_reads_reopen_after_atomic_replacement() {
        use super::super::buffer::{BgBuffer, StreamKind};

        let storage = tempfile::tempdir().unwrap();
        let (task, _) = counted_task(storage.path());
        TaskIoHandles::create(&task, BgMode::Pipes, false).unwrap();
        let buffer = BgBuffer::registered(&task.paths, BgMode::Pipes);
        for validation in 1..=3 {
            for operation in ["bounded", "tail", "empty_tail", "len", "notice"] {
                randomized_atomic_replace(
                    &task.dirs.io,
                    &task.paths.artifact_name(TaskArtifact::Stdout),
                    b"original output",
                )
                .unwrap();
                let (read, replaced) = with_replacements(
                    &task.paths.stdout,
                    validation,
                    1,
                    b"replacement output".to_vec(),
                    || {
                        let text = match operation {
                            "bounded" => buffer.read_stream_bounded(StreamKind::Stdout, 64).text,
                            "tail" => buffer.read_tail(64).0,
                            "empty_tail" => {
                                let (text, truncated) = buffer.read_tail(0);
                                assert!(text.is_empty());
                                assert!(truncated);
                                return Ok(());
                            }
                            "len" => {
                                assert_eq!(buffer.stream_len(StreamKind::Stdout), 18);
                                return Ok(());
                            }
                            "notice" => buffer.read_complete_for_notice().unwrap().0,
                            _ => unreachable!(),
                        };
                        assert_eq!(text, "replacement output", "{operation}");
                        Ok(())
                    },
                );
                assert_eq!(replaced, 1, "{operation} validation {validation}");
                read.unwrap();
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn artifact_read_replacement_retries_are_bounded() {
        let storage = tempfile::tempdir().unwrap();
        let (task, metadata) = counted_task(storage.path());
        TaskIoHandles::create(&task, BgMode::Pipes, false).unwrap();
        for validation in 1..=3 {
            let (read, replaced) = with_replacements(
                &task.paths.stdout,
                validation,
                METADATA_OPEN_ATTEMPTS + 1,
                b"repeated replacement".to_vec(),
                || sample_task_artifact(&task.paths, TaskArtifact::Stdout),
            );
            let error = read.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
            assert_eq!(error.to_string(), ARTIFACT_CONCURRENTLY_REPLACED);
            assert_eq!(replaced, METADATA_OPEN_ATTEMPTS);
        }
        let (read, replaced) = with_replacements(
            &task.paths.json,
            1,
            METADATA_OPEN_ATTEMPTS + 1,
            serde_json::to_vec(&metadata).unwrap(),
            || read_task(&task.paths.json),
        );
        assert_eq!(read.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(replaced, METADATA_OPEN_ATTEMPTS);
    }

    #[test]
    fn artifact_read_retry_preserves_other_errors() {
        for (kind, message) in [
            (io::ErrorKind::Interrupted, "unrelated interruption"),
            (
                io::ErrorKind::InvalidData,
                "task artifact has multiple hard links",
            ),
            (
                io::ErrorKind::PermissionDenied,
                ARTIFACT_CONCURRENTLY_REPLACED,
            ),
            (io::ErrorKind::NotFound, "task removed"),
        ] {
            let mut calls = 0;
            let read = retry_artifact_read::<()>(|| {
                calls += 1;
                Err(io::Error::new(kind, message))
            });
            assert_eq!(read.unwrap_err().kind(), kind);
            assert_eq!(calls, 1);
        }
    }

    /// Manual check: race real metadata readers against atomic replacement writes.
    #[cfg(unix)]
    #[test]
    #[ignore = "manual real-race stress check"]
    fn read_task_replacement_retry_handles_concurrent_metadata_writes() {
        let storage = tempfile::tempdir().unwrap();
        let (task, metadata) = counted_task(storage.path());
        let path = task.paths.json.clone();
        let writing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let writer_writing = writing.clone();
        let writer_path = path.clone();
        let started = std::sync::Arc::new(std::sync::Barrier::new(2));
        let writer_started = started.clone();
        let writer = std::thread::spawn(move || {
            write_task(&writer_path, &metadata).unwrap();
            writer_started.wait();
            while writer_writing.load(std::sync::atomic::Ordering::Relaxed) {
                write_task(&writer_path, &metadata).unwrap();
                std::thread::yield_now();
            }
        });
        started.wait();

        let mut replacement_error = None;
        let mut other_error = None;
        let read_deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut plain_reads = 0;
        while replacement_error.is_none() && std::time::Instant::now() < read_deadline {
            plain_reads += 1;
            if let Err(error) = read_task_once(&path) {
                if error.kind() == io::ErrorKind::Interrupted
                    && error.to_string().contains(ARTIFACT_CONCURRENTLY_REPLACED)
                {
                    replacement_error.get_or_insert_with(|| error.to_string());
                } else {
                    other_error.get_or_insert_with(|| error.to_string());
                }
            }
        }

        let mut retry_error = None;
        for _ in 0..plain_reads.max(3_000) {
            if let Err(error) = read_task(&path) {
                retry_error.get_or_insert_with(|| error.to_string());
            }
        }

        writing.store(false, std::sync::atomic::Ordering::Relaxed);
        writer.join().unwrap();
        assert!(
            other_error.is_none(),
            "unexpected read error: {other_error:?}"
        );
        assert!(
            replacement_error.is_some(),
            "single-attempt read never observed a transient metadata replacement"
        );
        assert!(
            retry_error.is_none(),
            "retrying metadata read failed: {retry_error:?}"
        );
        eprintln!("single-attempt read observed {replacement_error:?}; read_task succeeded");
    }

    #[test]
    fn task_id_validation_is_exact() {
        assert!(validate_task_id("bash-0123456789abcdef").is_ok());
        for invalid in [
            "bash-0123456789abcde",
            "bash-0123456789abcdef0",
            "bash-0123456789ABCDEf",
            "bash-0123456789abcdeg",
            "../bash-0123456789abcdef",
        ] {
            assert!(validate_task_id(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn marker_clones_stay_close_on_exec_until_the_child_allowlist_remaps_them() {
        let temp = tempfile::tempdir().unwrap();
        let task = create_task_layout(temp.path(), "marker-clones", &valid_id(1)).unwrap();
        let handles = TaskIoHandles::create(&task, BgMode::Pipes, true).unwrap();
        let markers = [
            TaskArtifact::Exit,
            TaskArtifact::SandboxUnavailable,
            TaskArtifact::PipelineStatus,
        ]
        .map(|artifact| handles.inheritable_file(artifact).unwrap());
        for marker in &markers {
            let flags = unsafe { libc::fcntl(marker.as_raw_fd(), libc::F_GETFD) };
            assert!(
                flags >= 0 && flags & libc::FD_CLOEXEC != 0,
                "marker clone is inheritable in the parent"
            );
        }
        let output = std::process::Command::new("/bin/bash")
            .args(["-c", "if [ ! -e /dev/fd/1 ] || [ ! -e /dev/fd/2 ]; then echo 'descriptor lookup cannot see open stdio' >&2; exit 1; fi; for fd in \"$@\"; do if [ -e \"/dev/fd/$fd\" ]; then echo \"leaked marker $fd\" >&2; exit 1; fi; done", "marker-probe"])
            .args(markers.iter().map(|marker| marker.as_raw_fd().to_string()))
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn new_layout_separates_control_and_io() {
        let storage = tempfile::tempdir().unwrap();
        let task = create_task_layout(storage.path(), "session", &valid_id(1)).unwrap();
        assert_eq!(
            task.paths.json.parent(),
            Some(task.paths.control_dir.as_path())
        );
        assert_eq!(
            task.paths.stdout.parent(),
            Some(task.paths.io_dir.as_path())
        );
        assert_ne!(task.paths.control_dir, task.paths.io_dir);
    }

    #[cfg(unix)]
    #[test]
    fn task_layout_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let storage = tempfile::tempdir().unwrap();
        let task = create_task_layout(storage.path(), "session", &valid_id(5)).unwrap();
        for path in [
            &task.paths.session_dir,
            &task.paths.dir,
            &task.paths.control_dir,
            &task.paths.io_dir,
        ] {
            let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "unexpected permissions for {}", path.display());
        }
        let bash_tasks = task.paths.session_dir.parent().unwrap();
        let mode = fs::metadata(bash_tasks).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o700,
            "unexpected permissions for {}",
            bash_tasks.display()
        );
    }

    #[test]
    fn resolver_refuses_duplicate_layouts() {
        let storage = tempfile::tempdir().unwrap();
        let task = create_task_layout(storage.path(), "session", &valid_id(2)).unwrap();
        let flat = task
            .paths
            .session_dir
            .join(format!("{}.json", task.paths.task_id));
        fs::write(flat, b"{}").unwrap();
        let error = resolve_task_layout(&task.paths.session_dir, &task.paths.task_id).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn resolver_rejects_metadata_identity_mismatch() {
        let storage = tempfile::tempdir().unwrap();
        let task = create_task_layout(storage.path(), "session", &valid_id(30)).unwrap();
        let metadata = PersistedTask::starting(
            valid_id(31),
            "session".into(),
            "true".into(),
            storage.path().into(),
            None,
            None,
            true,
            false,
        );
        fs::write(&task.paths.json, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let error = resolve_task_layout(&task.paths.session_dir, &task.paths.task_id).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    // A task directory swapped underneath the daemon must never let deletion
    // touch the impostor's control content. The two platforms enforce this the
    // same guarantee through different mechanisms, so each is asserted against
    // its real mechanism rather than a shared code path.
    //
    // Unix: POSIX permits renaming a directory while a fd is held open on it, so
    // the swap succeeds on disk and `remove_directory_task`'s `same_identity`
    // check is what refuses the deletion.
    #[cfg(unix)]
    #[test]
    fn deletion_refuses_replaced_task_directory_without_touching_victim() {
        let storage = tempfile::tempdir().unwrap();
        let first = create_task_layout(storage.path(), "session", &valid_id(40)).unwrap();
        let second = create_task_layout(storage.path(), "session", &valid_id(41)).unwrap();
        let victim = second.paths.control_dir.join("victim");
        fs::write(&victim, b"victim-bytes").unwrap();
        let moved_first = first.paths.session_dir.join("moved-first");
        fs::rename(&first.paths.dir, &moved_first).unwrap();
        fs::rename(&second.paths.dir, &first.paths.dir).unwrap();

        assert!(delete_resolved_task(&first).is_err());
        assert_eq!(
            fs::read(first.paths.control_dir.join("victim")).unwrap(),
            b"victim-bytes"
        );
    }

    // Windows: the daemon's retained `PinnedDir` handle on the task directory
    // makes the OS refuse to rename it (Access denied), so the swap cannot occur
    // at all while the daemon is live — the impostor's content is never reachable
    // for deletion. Assert that structural refusal directly.
    #[cfg(windows)]
    #[test]
    fn deletion_refuses_replaced_task_directory_without_touching_victim() {
        let storage = tempfile::tempdir().unwrap();
        let first = create_task_layout(storage.path(), "session", &valid_id(40)).unwrap();
        let second = create_task_layout(storage.path(), "session", &valid_id(41)).unwrap();
        let victim = second.paths.control_dir.join("victim");
        fs::write(&victim, b"victim-bytes").unwrap();

        // The daemon still holds `first`'s pinned directory handles, so moving
        // its task directory out of the way is refused by the OS.
        let moved_first = first.paths.session_dir.join("moved-first");
        let refusal = fs::rename(&first.paths.dir, &moved_first)
            .expect_err("open pinned-dir handle must block the task-dir rename on Windows");
        assert_eq!(refusal.kind(), io::ErrorKind::PermissionDenied);

        // The victim's control content is untouched because the swap never happened.
        assert_eq!(fs::read(&victim).unwrap(), b"victim-bytes");
    }

    #[cfg(windows)]
    #[test]
    fn windows_bundle_deletion_unlinks_names_while_pins_remain_open() {
        let storage = tempfile::tempdir().unwrap();
        let (task, _) = counted_task(storage.path());
        let handles = TaskIoHandles::create(&task, BgMode::Pipes, false).unwrap();
        fs::write(&task.paths.stdout, b"retained-output").unwrap();
        let mut stdout = handles.clone_file(TaskArtifact::Stdout).unwrap();
        let nested = task.dirs.io.create_dir_at(OsStr::new("nested")).unwrap();
        fs::write(nested.path().join("output"), b"nested-output").unwrap();

        delete_task_bundle(&task.paths).unwrap();

        assert!(!task.paths.dir.exists());
        assert!(!nested.path().exists());
        assert!(task.dirs.session.list_names().unwrap().is_empty());
        stdout.rewind().unwrap();
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).unwrap();
        assert_eq!(output, b"retained-output");
        // Keep all original pins and IO handles live through the assertions.
        drop((stdout, nested, handles, task));
    }

    #[cfg(windows)]
    #[test]
    fn windows_bundle_deletion_removes_native_wrapper_at_task_root() {
        let storage = tempfile::tempdir().unwrap();
        let (task, _) = counted_task(storage.path());
        let wrapper = task.paths.dir.join("metadata.ps1");
        fs::write(&wrapper, b"exit 0").unwrap();

        delete_task_bundle(&task.paths).unwrap();

        assert!(!wrapper.exists());
        assert!(!task.paths.dir.exists());
        assert!(task.dirs.session.list_names().unwrap().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_bundle_deletion_resumes_after_io_or_control_disappears() {
        for remove_control in [false, true] {
            let storage = tempfile::tempdir().unwrap();
            let (task, _) = counted_task(storage.path());
            task.dirs.io.remove_self().unwrap();
            if remove_control {
                remove_tree_contents(&task.dirs.control).unwrap();
                task.dirs.control.remove_self().unwrap();
            }

            delete_task_bundle(&task.paths).unwrap();

            assert!(!task.paths.dir.exists());
            assert!(task.dirs.session.list_names().unwrap().is_empty());
            // A completed legacy deletion is also safe to resume when its
            // final task entry vanished after the previous sweep's pins closed.
            delete_task_bundle(&task.paths).unwrap();
        }
    }

    #[test]
    #[cfg(unix)]
    fn directory_cleanup_removes_short_sandbox_temp() {
        let storage = tempfile::tempdir().unwrap();
        let (task, mut metadata) = counted_task(storage.path());
        let temp = crate::sandbox_spawn::create_task_temp_dir(task.dirs.io.path()).unwrap();
        fs::write(temp.join("scratch"), "payload").unwrap();
        metadata.sandbox_temp_dir = Some(temp.clone());
        write_task_at(&task, &metadata).unwrap();
        assert_eq!(
            delete_resolved_task(&task).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(
            temp.exists(),
            "an unfinished task must keep its scratch directory"
        );
        metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
        write_task_at(&task, &metadata).unwrap();
        delete_resolved_task(&task).unwrap();
        assert!(!temp.exists());
        assert!(!temp.with_extension("owner").exists());
        crate::sandbox_spawn::cleanup_task_temp_dir(&temp).unwrap();
        crate::sandbox_spawn::cleanup_task_temp_dir(
            &task.dirs.io.path().join(".sandbox-tmp-already-removed"),
        )
        .unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn bundle_deletion_restores_owner_access_and_leaves_symlink_targets_alone() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let mode = |path: &Path| fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777;
        let set = |path: &Path, mode: u32| {
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap()
        };
        let storage = tempfile::tempdir().unwrap();
        let (task, mut metadata) = counted_task(storage.path());
        // A directory outside the bundle that links inside it point at. Its
        // contents and its (read-only) mode must survive the deletion.
        let outside = storage.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "outside").unwrap();
        fs::create_dir(outside.join("locked")).unwrap();
        set(&outside.join("locked"), 0o500);
        set(&outside, 0o555);

        // Inside the IO tree: a Go-module-cache-like read-only tree, a
        // directory with no owner access at all, and links to the outside.
        let io = task.dirs.io.path().to_path_buf();
        let module = io.join("gomodcache/example.com/mod@v1");
        fs::create_dir_all(&module).unwrap();
        fs::write(module.join("go.mod"), "module example.com/mod").unwrap();
        symlink(&outside, module.join("outside-link")).unwrap();
        symlink(outside.join("locked"), io.join("locked-link")).unwrap();
        let sealed = io.join("sealed");
        fs::create_dir_all(sealed.join("inner")).unwrap();
        fs::write(sealed.join("inner/file"), "sealed").unwrap();
        set(&module, 0o555);
        set(module.parent().unwrap(), 0o555);
        set(&io.join("gomodcache"), 0o555);
        set(&sealed.join("inner"), 0o311);
        set(&sealed, 0o000);

        // The command's TMPDIR, removed with the bundle, is read-only too.
        let temp = crate::sandbox_spawn::create_task_temp_dir(&io).unwrap();
        fs::create_dir_all(temp.join("build/cache")).unwrap();
        fs::write(temp.join("build/cache/object"), "object").unwrap();
        symlink(&outside, temp.join("build/outside-link")).unwrap();
        set(&temp.join("build/cache"), 0o500);
        set(&temp.join("build"), 0o555);
        set(&temp, 0o500);
        metadata.sandbox_temp_dir = Some(temp.clone());
        metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
        write_task_at(&task, &metadata).unwrap();

        delete_task_bundle(&task.paths).unwrap();

        assert!(!task.paths.dir.exists(), "the bundle was not removed");
        assert!(!temp.exists(), "the task scratch was not removed");
        assert!(!temp.with_extension("owner").exists());
        assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "outside");
        assert_eq!(mode(&outside), 0o555, "a symlink target's mode changed");
        assert_eq!(mode(&outside.join("locked")), 0o500);
        set(&outside, 0o755);
        set(&outside.join("locked"), 0o755);
    }

    #[test]
    #[cfg(unix)]
    fn removal_walk_keeps_a_directory_on_another_filesystem() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("scratch");
        fs::create_dir_all(root.join("build")).unwrap();
        fs::write(root.join("build/object"), "object").unwrap();
        // Stands in for a volume a command mounted under its TMPDIR, with a
        // read-only folder the walk would otherwise unlock and empty.
        let mounted = root.join("mounted");
        fs::create_dir_all(mounted.join("locked")).unwrap();
        fs::write(mounted.join("locked/precious"), "precious").unwrap();
        fs::write(mounted.join("precious"), "precious").unwrap();
        fs::set_permissions(mounted.join("locked"), fs::Permissions::from_mode(0o555)).unwrap();
        // A locked mount root that the walk could only open by changing it,
        // in its own tree so the walk cannot stop at the other mount first.
        let sealed_root = temp.path().join("other-scratch");
        let sealed = sealed_root.join("sealed-mount");
        fs::create_dir_all(&sealed).unwrap();
        fs::write(sealed.join("precious"), "precious").unwrap();
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000)).unwrap();
        REPAIR_IDENTITIES.with(|ids| {
            let mut ids = ids.borrow_mut();
            ids.insert(mounted.clone(), (u64::MAX - 1, 1));
            ids.insert(sealed.clone(), (u64::MAX - 1, 2));
        });

        let first = remove_tree_contents(&PinnedDir::open(&root).unwrap());
        let second = remove_tree_contents(&PinnedDir::open(&sealed_root).unwrap());
        REPAIR_IDENTITIES.with(|ids| ids.borrow_mut().clear());

        for result in [first, second] {
            let error = result.expect_err("a mount inside the tree must stop the walk");
            assert!(
                error
                    .get_ref()
                    .is_some_and(|inner| inner.is::<ForeignFilesystemRetained>()),
                "{error}"
            );
            assert!(error.to_string().contains("another filesystem"), "{error}");
        }
        assert_eq!(
            fs::metadata(&sealed).unwrap().permissions().mode() & 0o7777,
            0,
            "the walk changed permissions on another filesystem"
        );
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            fs::read_to_string(sealed.join("precious")).unwrap(),
            "precious"
        );
        assert_eq!(
            fs::read_to_string(mounted.join("precious")).unwrap(),
            "precious"
        );
        assert_eq!(
            fs::read_to_string(mounted.join("locked/precious")).unwrap(),
            "precious"
        );
        assert_eq!(
            fs::metadata(mounted.join("locked"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o555
        );
        fs::set_permissions(mounted.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_bundle_deletion_removes_read_only_files() {
        let storage = tempfile::tempdir().unwrap();
        let (task, _) = counted_task(storage.path());
        let nested = task.dirs.io.path().join("cache");
        fs::create_dir(&nested).unwrap();
        for path in [
            nested.join("module.zip"),
            task.paths.dir.join("wrapper.ps1"),
        ] {
            fs::write(&path, b"read-only").unwrap();
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&path, permissions).unwrap();
        }

        delete_task_bundle(&task.paths).unwrap();

        assert!(!task.paths.dir.exists());
        assert!(task.dirs.session.list_names().unwrap().is_empty());
    }

    #[test]
    fn directory_cleanup_preserves_metadata_when_io_removal_fails() {
        let storage = tempfile::tempdir().unwrap();
        let (task, metadata) = counted_task(storage.path());
        write_task_at(&task, &metadata).unwrap();
        fs::write(&task.paths.stdout, b"output held by a child").unwrap();

        let error = remove_directory_task_with_io_cleanup(&task, |io_dir| {
            io_dir.remove_file(OsStr::new(TaskArtifact::Stdout.file_name()))?;
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected failure after partial IO cleanup",
            ))
        })
        .expect_err("an injected IO cleanup failure must abort bundle deletion");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            task.dirs
                .control
                .list_names()
                .unwrap()
                .iter()
                .any(|name| name == OsStr::new(METADATA_FILE)),
            "task metadata must remain available so a later sweep can resolve and retry deletion"
        );
        assert_eq!(read_task_at(&task).unwrap().task_id, task.paths.task_id);
    }

    #[cfg(unix)]
    #[test]
    fn directory_cleanup_resumes_tombstone_after_io_removal_failure() {
        let storage = tempfile::tempdir().unwrap();
        let (task, metadata) = counted_task(storage.path());
        write_task_at(&task, &metadata).unwrap();
        fs::write(&task.paths.stdout, b"output held by a child").unwrap();
        let original = task.paths.dir.clone();
        let session = task.paths.session_dir.clone();

        let error = remove_directory_task_with_io_cleanup(&task, |_| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected failure after the task rename",
            ))
        })
        .expect_err("the injected IO cleanup failure must abort the first deletion");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            !original.exists(),
            "the failed deletion has renamed the task"
        );

        delete_task_bundle_with_legacy_harness_root(&task.paths, "counted", None)
            .expect("a later cleanup pass must resume deletion from the renamed task");

        assert_eq!(fs::read_dir(session).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn task_tombstone_cleanup_refuses_symlinks() {
        use std::os::unix::fs::symlink;

        let storage = tempfile::tempdir().unwrap();
        let session_id = "session";
        let session = session_tasks_dir(storage.path(), session_id);
        fs::create_dir_all(&session).unwrap();
        let task_id = valid_id(43);
        let paths = TaskPaths::directory(session.clone(), &task_id);
        let victim = storage.path().join("victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("keep"), b"untouched").unwrap();
        let tombstone = session.join(format!("{}test", task_tombstone_prefix(&task_id)));
        symlink(&victim, &tombstone).unwrap();

        assert!(delete_task_bundle_with_legacy_harness_root(&paths, session_id, None).is_err());
        assert_eq!(fs::read(victim.join("keep")).unwrap(), b"untouched");
        assert!(fs::symlink_metadata(&tombstone)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn legacy_harness_root_fallback_refuses_a_task_symlink() {
        use std::os::unix::fs::symlink;

        let storage = tempfile::tempdir().unwrap();
        let session_id = "session";
        let task_id = valid_id(33);
        let paths = task_paths(storage.path(), session_id, &task_id).unwrap();
        fs::create_dir_all(&paths.session_dir).unwrap();
        let legacy_session = storage
            .path()
            .join("pi")
            .join("bash-tasks")
            .join(hash_session(session_id));
        fs::create_dir_all(&legacy_session).unwrap();
        let victim = storage.path().join("victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("keep"), b"untouched").unwrap();
        let task_link = legacy_session.join(&task_id);
        symlink(&victim, &task_link).unwrap();

        assert!(
            delete_task_bundle_with_legacy_harness_root(&paths, session_id, Some("pi")).is_err()
        );
        assert_eq!(fs::read(victim.join("keep")).unwrap(), b"untouched");
        assert!(fs::symlink_metadata(&task_link)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn legacy_flat_layout_is_readable_and_deleted_as_a_bundle() {
        let storage = tempfile::tempdir().unwrap();
        let task_id = valid_id(32);
        let paths = task_paths(storage.path(), "session", &task_id).unwrap();
        fs::create_dir_all(&paths.session_dir).unwrap();
        let metadata = PersistedTask::starting(
            task_id.clone(),
            "session".into(),
            "true".into(),
            storage.path().into(),
            None,
            None,
            true,
            false,
        );
        write_task(&paths.json, &metadata).unwrap();
        fs::write(&paths.stdout, b"legacy").unwrap();
        fs::write(&paths.stderr, b"").unwrap();
        assert_eq!(
            resolve_task_layout(&paths.session_dir, &task_id)
                .unwrap()
                .paths
                .layout,
            TaskLayout::Flat
        );
        assert_eq!(
            open_task_artifact(&paths, TaskArtifact::Stdout)
                .unwrap()
                .read_all()
                .unwrap(),
            b"legacy"
        );
        delete_task_bundle(&paths).unwrap();
        assert!(!paths.json.exists());
        assert!(!paths.stdout.exists());
    }

    #[cfg(unix)]
    #[test]
    fn live_output_creation_and_later_writes_refuse_link_attacks() {
        use std::os::unix::fs::symlink;

        let storage = tempfile::tempdir().unwrap();
        let task = create_task_layout(storage.path(), "session", &valid_id(33)).unwrap();
        let metadata = PersistedTask::starting(
            task.paths.task_id.clone(),
            "session".into(),
            "true".into(),
            storage.path().into(),
            None,
            None,
            true,
            false,
        );
        write_task_at(&task, &metadata).unwrap();
        let victim = storage.path().join("victim");
        fs::write(&victim, b"victim-bytes").unwrap();

        symlink(&victim, &task.paths.stdout).unwrap();
        assert!(TaskIoHandles::create(&task, BgMode::Pipes, false).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"victim-bytes");
        fs::remove_file(&task.paths.stdout).unwrap();

        let mut handles = TaskIoHandles::create(&task, BgMode::Pipes, false).unwrap();
        fs::hard_link(&task.paths.stdout, task.paths.io_dir.join("linked-stdout")).unwrap();
        assert!(handles
            .write(TaskArtifact::Stdout, b"daemon-write")
            .is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"victim-bytes");

        fs::remove_file(&task.paths.stdout).unwrap();
        symlink(&victim, &task.paths.stdout).unwrap();
        assert!(replace_artifact_with_tail(&task.paths, TaskArtifact::Stdout, 1).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"victim-bytes");
    }

    #[test]
    fn registered_artifact_consumers_do_not_reopen_paths_directly() {
        let rust_sources = [
            include_str!("buffer.rs"),
            include_str!("registry.rs"),
            include_str!("process.rs"),
            include_str!("pty_process.rs"),
            include_str!("../task_watches.rs"),
            include_str!("watchdog.rs"),
            include_str!("../commands/bash_status.rs"),
        ];
        for source in rust_sources {
            // A Windows checkout can have CRLF line endings. Without this the
            // split marker never matches there, so the guard would scan test
            // modules on Windows but not elsewhere. Scan the same production
            // text on every platform.
            let source = source.replace("\r\n", "\n");
            let production = source
                .split("#[cfg(test)]\nmod tests")
                .next()
                .unwrap_or(&source);
            for forbidden in [
                "File::open(&task.paths",
                "fs::read(&task.paths",
                "fs::read_to_string(&task.paths",
                "File::open(path)?",
            ] {
                assert!(
                    !production.contains(forbidden),
                    "registered artifact consumer contains raw path read: {forbidden}"
                );
            }
        }
        for source in [
            include_str!("../../../../packages/opencode-plugin/src/tools/bash.ts"),
            include_str!("../../../../packages/opencode-plugin/src/tools/bash_watch.ts"),
            include_str!("../../../../packages/pi-plugin/src/tools/bash.ts"),
        ] {
            for forbidden in [
                "fs.readFile(outputPath)",
                "fs.readFile(details.output_path)",
                "fs.open(outputPath",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "plugin artifact consumer contains raw path read: {forbidden}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn validated_artifact_refuses_symlink_hardlink_and_fifo() {
        use std::os::unix::fs::symlink;

        let storage = tempfile::tempdir().unwrap();
        let task = create_task_layout(storage.path(), "session", &valid_id(3)).unwrap();
        let metadata = PersistedTask::starting(
            task.paths.task_id.clone(),
            "session".into(),
            "true".into(),
            storage.path().into(),
            None,
            None,
            true,
            false,
        );
        write_task_at(&task, &metadata).unwrap();
        let canary = storage.path().join("canary");
        fs::write(&canary, b"secret").unwrap();

        symlink(&canary, &task.paths.stdout).unwrap();
        assert!(open_task_artifact(&task.paths, TaskArtifact::Stdout).is_err());
        fs::remove_file(&task.paths.stdout).unwrap();

        fs::hard_link(&canary, &task.paths.stdout).unwrap();
        assert!(open_task_artifact(&task.paths, TaskArtifact::Stdout).is_err());
        fs::remove_file(&task.paths.stdout).unwrap();

        let path = CString::new(task.paths.stdout.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(open_task_artifact(&task.paths, TaskArtifact::Stdout).is_err());
    }
}

#[cfg(test)]
mod readable_name_tests {
    use super::*;

    #[test]
    fn shell_read_names_match_file_tool_readable_files() {
        for paths in [
            TaskPaths::directory(PathBuf::from("/session"), "bash-1111111111111111"),
            TaskPaths::flat(PathBuf::from("/session"), "bash-1111111111111111"),
        ] {
            let actual = paths
                .readable_files()
                .collect::<std::collections::BTreeSet<_>>();
            let expected = [
                &paths.json,
                &paths.command,
                &paths.wrapper,
                &paths.manifest,
                &paths.sandbox_profile,
            ]
            .into_iter()
            .cloned()
            .chain(
                TaskArtifact::ALL
                    .into_iter()
                    .map(|artifact| paths.artifact_path(artifact).to_path_buf()),
            )
            .collect();
            assert_eq!(actual, expected);
            assert!(!actual.contains(&paths.environment));
        }
    }
}
