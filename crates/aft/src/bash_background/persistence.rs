#[cfg(unix)]
use std::ffi::CString;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::backup::hash_session;
use crate::bash_permissions::PermissionAsk;
use crate::db::bash_tasks::BashTaskRow;

use super::process::LiveDescendant;
use super::BgTaskStatus;

pub const SCHEMA_VERSION: u32 = 7;
const CONTROL_DIR: &str = "control";
const IO_DIR: &str = "io";
const METADATA_FILE: &str = "metadata.json";
pub const COMMAND_FILE: &str = "command.sh";
pub const WRAPPER_FILE: &str = "wrapper.sh";
pub const ENVIRONMENT_FILE: &str = "environment.bin";
pub const MANIFEST_FILE: &str = "manifest.blake3";
pub const SANDBOX_PROFILE_FILE: &str = "sandbox-profile.json";

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
pub struct PinnedDir {
    file: File,
    path: PathBuf,
}

impl PinnedDir {
    pub fn open(path: &Path) -> io::Result<Self> {
        #[cfg(test)]
        work_counts::record_open();
        #[cfg(unix)]
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        #[cfg(windows)]
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        validate_directory_handle(&file)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn modified(&self) -> io::Result<SystemTime> {
        self.file.metadata()?.modified()
    }

    fn same_identity(&self, other: &Self) -> io::Result<bool> {
        #[cfg(unix)]
        {
            let left = self.file.metadata()?;
            let right = other.file.metadata()?;
            Ok(left.dev() == right.dev() && left.ino() == right.ino())
        }
        #[cfg(windows)]
        {
            let left = windows_file_information(&self.file)?;
            let right = windows_file_information(&other.file)?;
            Ok(left.volume_serial_number == right.volume_serial_number
                && left.file_index_high == right.file_index_high
                && left.file_index_low == right.file_index_low)
        }
    }

    #[cfg(unix)]
    fn open_dir_at(&self, name: &OsStr) -> io::Result<Self> {
        #[cfg(test)]
        work_counts::record_open();
        let file = openat_file(
            self.file.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )?;
        validate_directory_handle(&file)?;
        Ok(Self {
            file,
            path: self.path.join(name),
        })
    }

    #[cfg(windows)]
    fn open_dir_at(&self, name: &OsStr) -> io::Result<Self> {
        self.ensure_current_identity()?;
        let child = Self::open(&self.path.join(name))?;
        self.ensure_current_identity()?;
        Ok(child)
    }

    #[cfg(unix)]
    fn create_dir_at(&self, name: &OsStr) -> io::Result<Self> {
        let name = os_cstring(name)?;
        let result = unsafe {
            libc::mkdirat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                crate::private_storage::DIR_MODE as libc::mode_t,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        self.open_dir_at(OsStr::from_bytes(name.as_bytes()))
    }

    #[cfg(windows)]
    fn create_dir_at(&self, name: &OsStr) -> io::Result<Self> {
        self.ensure_current_identity()?;
        let path = self.path.join(name);
        fs::create_dir(&path)?;
        self.ensure_current_identity()?;
        Self::open(&path)
    }

    pub fn open_new_file(&self, name: &OsStr) -> io::Result<File> {
        #[cfg(unix)]
        let file = openat_file(
            self.file.as_raw_fd(),
            name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            crate::private_storage::FILE_MODE as libc::mode_t,
        )?;
        #[cfg(windows)]
        self.ensure_current_identity()?;
        #[cfg(windows)]
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(self.path.join(name))?;
        validate_regular_handle(&file)?;
        #[cfg(windows)]
        self.ensure_current_identity()?;
        Ok(file)
    }

    pub fn open_file(&self, name: &OsStr, write: bool) -> io::Result<File> {
        #[cfg(test)]
        work_counts::record_open();
        #[cfg(unix)]
        let file = openat_file(
            self.file.as_raw_fd(),
            name,
            (if write {
                libc::O_RDWR
            } else {
                libc::O_RDONLY | libc::O_NONBLOCK
            }) | libc::O_NOFOLLOW
                | libc::O_CLOEXEC,
            0,
        )?;
        #[cfg(windows)]
        self.ensure_current_identity()?;
        #[cfg(windows)]
        let file = OpenOptions::new()
            .read(true)
            .write(write)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(self.path.join(name))?;
        validate_regular_handle(&file)?;
        #[cfg(unix)]
        if !write {
            clear_nonblocking(&file)?;
        }
        #[cfg(windows)]
        self.ensure_current_identity()?;
        Ok(file)
    }

    pub fn list_names(&self) -> io::Result<Vec<OsString>> {
        #[cfg(unix)]
        {
            let dot = b".\0";
            let fresh = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    dot.as_ptr().cast(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if fresh < 0 {
                return Err(io::Error::last_os_error());
            }
            let directory = unsafe { libc::fdopendir(fresh) };
            if directory.is_null() {
                let error = io::Error::last_os_error();
                unsafe { libc::close(fresh) };
                return Err(error);
            }
            let mut names = Vec::new();
            loop {
                let entry = unsafe { libc::readdir(directory) };
                if entry.is_null() {
                    break;
                }
                let bytes = unsafe {
                    std::ffi::CStr::from_ptr((*entry).d_name.as_ptr())
                        .to_bytes()
                        .to_vec()
                };
                if bytes != b"." && bytes != b".." {
                    names.push(OsString::from_vec(bytes));
                }
            }
            unsafe { libc::closedir(directory) };
            Ok(names)
        }
        #[cfg(windows)]
        {
            self.ensure_current_identity()?;
            let names = fs::read_dir(&self.path)?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<io::Result<Vec<_>>>()?;
            self.ensure_current_identity()?;
            Ok(names)
        }
    }

    fn rename(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
        self.rename_to(from, self, to)
    }

    fn rename_to(&self, from: &OsStr, target: &PinnedDir, to: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            let from = os_cstring(from)?;
            let to = os_cstring(to)?;
            let result = unsafe {
                libc::renameat(
                    self.file.as_raw_fd(),
                    from.as_ptr(),
                    target.file.as_raw_fd(),
                    to.as_ptr(),
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            self.ensure_current_identity()?;
            target.ensure_current_identity()?;
            fs::rename(self.path.join(from), target.path.join(to))?;
            self.ensure_current_identity()?;
            target.ensure_current_identity()
        }
    }

    #[cfg(windows)]
    fn ensure_current_identity(&self) -> io::Result<()> {
        let current = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(&self.path)?;
        validate_directory_handle(&current)?;
        let held = windows_file_information(&self.file)?;
        let observed = windows_file_information(&current)?;
        if held.volume_serial_number != observed.volume_serial_number
            || held.file_index_high != observed.file_index_high
            || held.file_index_low != observed.file_index_low
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "pinned directory path identity changed",
            ));
        }
        Ok(())
    }

    fn remove_file(&self, name: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            let name = os_cstring(name)?;
            let result = unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            let file = self.open_file(name, false)?;
            windows_delete_pinned_handle(&file, false)?;
            self.ensure_current_identity()
        }
    }

    #[cfg(windows)]
    fn remove_self(&self) -> io::Result<()> {
        self.ensure_current_identity()?;
        windows_delete_pinned_handle(&self.file, true)
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
pub struct PersistedTask {
    pub schema_version: u32,
    /// Storage namespace captured at spawn, independent of later route binds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    pub task_id: String,
    pub session_id: String,
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
    pub exit_code: Option<i32>,
    pub child_pid: Option<u32>,
    pub pgid: Option<i32>,
    /// `Some` records a completed Unix sample, including an empty group. `None`
    /// means sampling is unavailable (Windows and unsupported Unix targets).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_descendants: Option<Vec<LiveDescendant>>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub live_descendants_omitted: usize,
    pub completion_delivered: bool,
    #[serde(default = "default_notify_on_completion")]
    pub notify_on_completion: bool,
    #[serde(default = "default_compressed")]
    pub compressed: bool,
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
    pub status_reason: Option<String>,
    /// Capture did not finish cleanly; independent of the command's exit status.
    #[serde(default, skip_serializing_if = "is_false")]
    pub output_incomplete: bool,
    /// Who started the task and the key they gave the call. Absent on records
    /// written before AFT recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_key: Option<super::TaskCallKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) remote: Option<super::registry::remote::RemoteTask>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_note: Option<String>,
    /// Missing job-wide output ranges, independent of execution outcome.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incomplete_output: Vec<(u64, u64)>,
    /// Caller-side removals, names only; never environment values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stripped_env_names: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub local_fallback_started: bool,
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
        Self {
            schema_version: SCHEMA_VERSION,
            harness: super::route_harness().map(|harness| harness.storage_segment()),
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
            child_pid: None,
            pgid: None,
            live_descendants: None,
            live_descendants_omitted: 0,
            completion_delivered: !notify_on_completion,
            notify_on_completion,
            compressed,
            pty_rows: None,
            pty_cols: None,
            scanner_report: Vec::new(),
            sandbox_native: false,
            sandbox_temp_dir: None,
            status_reason: None,
            output_incomplete: false,
            call_key,
            remote: None,
            execution_note: None,
            incomplete_output: Vec::new(),
            stripped_env_names: Vec::new(),
            local_fallback_started: false,
        }
    }

    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    pub fn mark_running(&mut self, child_pid: u32, pgid: i32) {
        self.status = BgTaskStatus::Running;
        self.child_pid = Some(child_pid);
        self.pgid = Some(pgid);
    }

    pub fn mark_terminal(
        &mut self,
        status: BgTaskStatus,
        exit_code: Option<i32>,
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
            child_pid: row.pid.and_then(|value| u32::try_from(value).ok()),
            pgid: row.pgid.and_then(|value| i32::try_from(value).ok()),
            live_descendants: None,
            live_descendants_omitted: 0,
            completion_delivered: row.completion_delivered,
            notify_on_completion: !row.completion_delivered,
            compressed: row.compressed,
            pty_rows: None,
            pty_cols: None,
            scanner_report: Vec::new(),
            sandbox_native: false,
            sandbox_temp_dir: None,
            status_reason: None,
            output_incomplete: false,
            call_key: None,
            remote: None,
            execution_note: None,
            incomplete_output: Vec::new(),
            stripped_env_names: Vec::new(),
            local_fallback_started: false,
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
    let len = |artifact| {
        open_task_artifact(paths, artifact)
            .ok()
            .and_then(|file| file.len().ok())
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
    create_private_task_store(&session_dir)?;
    create_task_layout_from_session(Arc::new(PinnedDir::open(&session_dir)?), task_id)
}

fn create_private_task_store(session_dir: &Path) -> io::Result<()> {
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
        let control = task.open_dir_at(OsStr::new(CONTROL_DIR))?;
        let io_dir = task.open_dir_at(OsStr::new(IO_DIR))?;
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
        "background task layout not found",
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
    let mut attempts = 0;
    loop {
        attempts += 1;
        match dir.open_file(name, false) {
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
    let mut file = open_validated_path(path, false)?;
    read_task_file(&mut file, path)
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
    // The version is read before the full shape, so metadata written by a
    // newer build is refused by name (and kept out of quarantine) instead of
    // failing as an unparseable task.
    #[cfg(test)]
    work_counts::record_parse();
    let version = serde_json::from_str::<serde_json::Value>(&content)
        .ok()
        .and_then(|value| value.get("schema_version")?.as_u64());
    crate::persisted_format::gate(
        crate::persisted_format::PersistedStore::BashTask,
        path,
        path,
        version,
    )
    .map_err(crate::persisted_format::UnsupportedPersistedFormat::into_io_error)?;
    #[cfg(test)]
    work_counts::record_parse();
    let task: PersistedTask = serde_json::from_str(&content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
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
    validate_task_id(&task.task_id)?;
    if let Some(parent) = path.parent() {
        crate::private_storage::create_dir_all(parent)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let dir = PinnedDir::open(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "metadata path has no name"))?;
    write_task_in_dir(&dir, name, task)
}

pub fn write_task_at(task: &ResolvedTask, metadata: &PersistedTask) -> io::Result<()> {
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
    write_task_in_dir(&task.dirs.control, &name, metadata)
}

fn write_task_in_dir(dir: &PinnedDir, name: &OsStr, task: &PersistedTask) -> io::Result<()> {
    #[cfg(test)]
    if task.status == BgTaskStatus::Running {
        task_io_fault_for_test(true)?;
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
    atomic_replace(
        dir,
        name,
        &content,
        task.remote.is_some() || task.local_fallback_started,
    )
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum TaskIoFault {
    LayoutEnospc,
    RunningEnospc,
    RunningDelay(std::time::Duration),
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
        Some(TaskIoFault::LayoutEnospc) if !running => Err(io::Error::from_raw_os_error(28)),
        Some(TaskIoFault::RunningEnospc) if running => Err(io::Error::from_raw_os_error(28)),
        Some(TaskIoFault::RunningDelay(delay)) if running => {
            std::thread::sleep(delay);
            Ok(())
        }
        _ => Ok(()),
    })
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
        let completion_delivered = metadata.completion_delivered;
        metadata = original;
        metadata.completion_delivered = completion_delivered;
        metadata.schema_version = SCHEMA_VERSION;
    }
    write_task_at(task, &metadata)?;
    Ok(metadata)
}

pub fn delete_task_bundle(paths: &TaskPaths) -> io::Result<()> {
    validate_task_id(&paths.task_id)?;
    crate::gh_shim_ticket::revoke_task(&paths.task_id);
    #[cfg(windows)]
    if paths.layout == TaskLayout::Directory {
        return delete_windows_directory_bundle(paths);
    }
    let resolved = resolve_task_layout(&paths.session_dir, &paths.task_id)?;
    if resolved.paths.layout != paths.layout {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background task layout changed before deletion",
        ));
    }
    delete_resolved_task(&resolved)
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
    task.remove_self()?;
    session.ensure_current_identity()
}

fn remove_tree_contents(dir: &PinnedDir) -> io::Result<()> {
    for name in dir.list_names()? {
        match dir.open_dir_at(&name) {
            Ok(child) => {
                remove_tree_contents(&child)?;
                #[cfg(unix)]
                {
                    let name = os_cstring(&name)?;
                    let result = unsafe {
                        libc::unlinkat(dir.file.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR)
                    };
                    if result != 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                #[cfg(windows)]
                child.remove_self()?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotADirectory => {
                dir.remove_file(&name)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn rename_task_to_tombstone(task: &ResolvedTask) -> io::Result<OsString> {
    for _ in 0..32 {
        let tombstone = random_temp_name()?;
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
    let mut file = match open_task_artifact(paths, TaskArtifact::Exit) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
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

pub fn randomized_atomic_replace(dir: &PinnedDir, name: &OsStr, content: &[u8]) -> io::Result<()> {
    atomic_replace(dir, name, content, false)
}

fn atomic_replace(dir: &PinnedDir, name: &OsStr, content: &[u8], durable: bool) -> io::Result<()> {
    for _ in 0..32 {
        let temporary = random_temp_name()?;
        let mut file = match dir.open_new_file(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let result = (|| {
            file.write_all(content)?;
            if durable {
                file.sync_all()?;
            }
            // Atomic replacement protects readers after a daemon kill. Task
            // history is mirrored in a weaker database and is not a durable log.
            validate_regular_handle(&file)?;
            dir.rename(&temporary, name)?;
            if durable {
                dir.file.sync_all()?;
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
pub(crate) mod work_counts {
    use std::cell::Cell;

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct Counts {
        pub opens: usize,
        pub metadata_reads: usize,
        pub parses: usize,
    }

    thread_local! {
        static COUNTS: Cell<Counts> = Cell::new(Counts::default());
    }

    fn record(update: impl FnOnce(&mut Counts)) {
        COUNTS.with(|cell| {
            let mut counts = cell.get();
            update(&mut counts);
            cell.set(counts);
        });
    }

    pub(crate) fn record_open() {
        record(|counts| counts.opens += 1);
    }
    pub(crate) fn record_read() {
        record(|counts| counts.metadata_reads += 1);
    }
    pub(crate) fn record_parse() {
        record(|counts| counts.parses += 1);
    }
    pub(crate) fn reset() {
        COUNTS.with(|cell| cell.set(Counts::default()));
    }
    pub(crate) fn get() -> Counts {
        COUNTS.with(Cell::get)
    }
}

#[derive(Debug)]
pub struct ValidatedArtifact {
    file: File,
}

impl ValidatedArtifact {
    fn new(file: File) -> io::Result<Self> {
        validate_regular_handle(&file)?;
        Ok(Self { file })
    }

    pub fn len(&self) -> io::Result<u64> {
        validate_regular_handle(&self.file)?;
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
        Ok((bytes, len > max_bytes as u64))
    }

    pub fn read_range(&mut self, start: u64, len: u64) -> io::Result<Vec<u8>> {
        self.file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::with_capacity(len.min(usize::MAX as u64) as usize);
        Read::by_ref(&mut self.file)
            .take(len)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    pub fn read_all(&mut self) -> io::Result<Vec<u8>> {
        self.rewind()?;
        let mut bytes = Vec::new();
        self.file.read_to_end(&mut bytes)?;
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
    open_task_artifact_with_access(paths, artifact, false)
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
    let mut source = open_task_artifact(paths, artifact)?;
    let len = source.len()?;
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

fn random_temp_name() -> io::Result<OsString> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(OsString::from(format!(
        ".aft-tmp-{}",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )))
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

#[cfg(unix)]
fn clear_nonblocking(file: &File) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::O_NONBLOCK != 0 {
        let result =
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn open_validated_path(path: &Path, write: bool) -> io::Result<File> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    PinnedDir::open(parent)?.open_file(name, write)
}

#[cfg(unix)]
fn openat_file(dirfd: RawFd, name: &OsStr, flags: i32, mode: libc::mode_t) -> io::Result<File> {
    let name = os_cstring(name)?;
    let fd = unsafe { libc::openat(dirfd, name.as_ptr(), flags, libc::c_uint::from(mode)) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn os_cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

fn validate_directory_handle(file: &File) -> io::Result<()> {
    // Reject reparse points before classifying a non-directory as a file that
    // recursive cleanup may unlink. Junctions must never enter that fallback.
    #[cfg(windows)]
    validate_windows_handle(file, true)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() {
        #[cfg(unix)]
        let kind = io::ErrorKind::InvalidData;
        // CreateFile's BACKUP_SEMANTICS permits opening directories, but unlike
        // Unix O_DIRECTORY it also opens regular files. Match O_DIRECTORY's
        // error so remove_tree_contents can unlink ordinary IO artifacts.
        #[cfg(windows)]
        let kind = io::ErrorKind::NotADirectory;
        return Err(io::Error::new(
            kind,
            "expected a non-reparse directory handle",
        ));
    }
    Ok(())
}

fn validate_regular_handle(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "task artifact is not a regular file",
        ));
    }
    // Link-count semantics: >1 means the artifact is aliased somewhere else on
    // disk (tamper suspicion, refuse); exactly 0 means the file was unlinked or
    // atomically replaced AFTER we opened it — on Windows the child's own
    // temp+rename exit write races a daemon kill-marker write and leaves the
    // superseded handle at zero links. That is a benign concurrent replace, not
    // an attack; report it distinctly so callers can re-open the replacement.
    #[cfg(unix)]
    match metadata.nlink() {
        1 => {}
        0 => {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                ARTIFACT_CONCURRENTLY_REPLACED,
            ));
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "task artifact has multiple hard links",
            ));
        }
    }
    #[cfg(windows)]
    validate_windows_handle(file, false)?;
    Ok(())
}

/// Marker message for a validated-open that lost a race against an atomic
/// replacement of the same artifact (see link-count semantics above).
pub(crate) const ARTIFACT_CONCURRENTLY_REPLACED: &str = "task artifact was concurrently replaced";

#[cfg(unix)]
pub fn set_close_on_exec(fd: RawFd, enabled: bool) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = if enabled {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
#[cfg(windows)]
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
#[cfg(windows)]
const FILE_TYPE_DISK: u32 = 0x0001;
#[cfg(windows)]
const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
#[cfg(windows)]
const GENERIC_READ: u32 = 0x8000_0000;
#[cfg(windows)]
const DELETE_ACCESS: u32 = 0x0001_0000;
#[cfg(windows)]
const FILE_SHARE_READ_WRITE_DELETE: u32 = 0x0000_0007;
#[cfg(windows)]
const FILE_DISPOSITION_INFO: i32 = 4;
#[cfg(windows)]
const FILE_DISPOSITION_INFO_EX: i32 = 21;
#[cfg(windows)]
const FILE_DISPOSITION_FLAG_DELETE: u32 = 0x0000_0001;
#[cfg(windows)]
const FILE_DISPOSITION_FLAG_POSIX_SEMANTICS: u32 = 0x0000_0002;

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowsDeleteDisposition {
    Posix,
    Legacy,
}

#[cfg(any(windows, test))]
fn windows_delete_with_fallback(
    mut set_disposition: impl FnMut(WindowsDeleteDisposition) -> io::Result<()>,
) -> io::Result<()> {
    match set_disposition(WindowsDeleteDisposition::Posix) {
        Ok(()) => Ok(()),
        // ERROR_NOT_SUPPORTED / ERROR_INVALID_PARAMETER are capability failures,
        // not permission, sharing, or non-empty-directory errors.
        Err(error) if matches!(error.raw_os_error(), Some(50 | 87)) => {
            set_disposition(WindowsDeleteDisposition::Legacy)?;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "POSIX delete unsupported on this volume; entry pending until handles close",
            ))
        }
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn windows_delete_pinned_handle(file: &File, directory: bool) -> io::Result<()> {
    // ReOpenFile targets the pinned object, not a path that could be swapped.
    // A distinct DELETE handle is necessary: POSIX disposition unlinks the name
    // when that handle closes, even if the original pins remain open.
    let handle = unsafe {
        ReOpenFile(
            file.as_raw_handle(),
            GENERIC_READ | DELETE_ACCESS,
            FILE_SHARE_READ_WRITE_DELETE,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
        )
    };
    if handle as isize == -1 {
        return Err(io::Error::last_os_error());
    }
    let deletion = unsafe { File::from_raw_handle(handle) };
    if directory {
        validate_directory_handle(&deletion)?;
    } else {
        validate_regular_handle(&deletion)?;
    }
    let result = windows_delete_with_fallback(|disposition| {
        let success = match disposition {
            WindowsDeleteDisposition::Posix => {
                let mut flags =
                    FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS;
                unsafe {
                    SetFileInformationByHandle(
                        deletion.as_raw_handle(),
                        FILE_DISPOSITION_INFO_EX,
                        (&mut flags as *mut u32).cast(),
                        std::mem::size_of_val(&flags) as u32,
                    )
                }
            }
            WindowsDeleteDisposition::Legacy => {
                let mut delete_file = 1_u8; // FILE_DISPOSITION_INFO::DeleteFile (BOOLEAN)
                unsafe {
                    SetFileInformationByHandle(
                        deletion.as_raw_handle(),
                        FILE_DISPOSITION_INFO,
                        (&mut delete_file as *mut u8).cast(),
                        std::mem::size_of_val(&delete_file) as u32,
                    )
                }
            }
        };
        if success == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    });
    drop(deletion);
    result
}

#[cfg(windows)]
fn windows_file_information(file: &File) -> io::Result<ByHandleFileInformation> {
    let mut information = std::mem::MaybeUninit::<ByHandleFileInformation>::zeroed();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { information.assume_init() })
}

#[cfg(windows)]
fn validate_windows_handle(file: &File, directory: bool) -> io::Result<()> {
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "task path is a reparse point",
        ));
    }
    let handle = file.as_raw_handle();
    let file_type = unsafe { GetFileType(handle) };
    if file_type != FILE_TYPE_DISK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "task artifact is not a regular disk file",
        ));
    }
    let information = windows_file_information(file)?;
    if !directory && information.number_of_links == 0 {
        // Zero links = this handle points at a file that was unlinked or
        // rename-replaced after open (e.g. the child's temp+rename exit write
        // racing a daemon kill-marker write). Benign; caller may re-open.
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            ARTIFACT_CONCURRENTLY_REPLACED,
        ));
    }
    if !directory && information.number_of_links > 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "task artifact has multiple hard links",
        ));
    }
    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut flags = 0_u32;
    if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & HANDLE_FLAG_INHERIT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "validated task handles must not be inherited",
        ));
    }
    Ok(())
}

#[cfg(windows)]
#[repr(C)]
struct ByHandleFileInformation {
    file_attributes: u32,
    creation_time: [u32; 2],
    last_access_time: [u32; 2],
    last_write_time: [u32; 2],
    volume_serial_number: u32,
    file_size_high: u32,
    file_size_low: u32,
    number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn ReOpenFile(
        original: std::os::windows::io::RawHandle,
        access: u32,
        share: u32,
        flags: u32,
    ) -> std::os::windows::io::RawHandle;
    fn SetFileInformationByHandle(
        file: std::os::windows::io::RawHandle,
        class: i32,
        information: *mut std::ffi::c_void,
        size: u32,
    ) -> i32;
    fn GetFileType(file: std::os::windows::io::RawHandle) -> u32;
    fn GetFileInformationByHandle(
        file: std::os::windows::io::RawHandle,
        information: *mut ByHandleFileInformation,
    ) -> i32;
    fn SetHandleInformation(object: std::os::windows::io::RawHandle, mask: u32, flags: u32) -> i32;
    fn GetHandleInformation(object: std::os::windows::io::RawHandle, flags: *mut u32) -> i32;
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn windows_delete_unsupported_uses_legacy_and_reports_pending() {
        for code in [50, 87] {
            let mut calls = Vec::new();
            let error = windows_delete_with_fallback(|disposition| {
                calls.push(disposition);
                match disposition {
                    WindowsDeleteDisposition::Posix => Err(io::Error::from_raw_os_error(code)),
                    WindowsDeleteDisposition::Legacy => Ok(()),
                }
            })
            .unwrap_err();
            assert_eq!(
                calls,
                [
                    WindowsDeleteDisposition::Posix,
                    WindowsDeleteDisposition::Legacy
                ]
            );
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert_eq!(
                error.to_string(),
                "POSIX delete unsupported on this volume; entry pending until handles close"
            );
        }
    }

    #[test]
    fn windows_delete_other_errors_never_fall_back() {
        // Access denied, sharing violation, directory not empty, and a synthetic
        // error must not be mistaken for unsupported POSIX semantics.
        for code in [Some(5), Some(32), Some(145), None] {
            let mut calls = Vec::new();
            let error = windows_delete_with_fallback(|disposition| {
                calls.push(disposition);
                match code {
                    Some(code) => Err(io::Error::from_raw_os_error(code)),
                    None => Err(io::Error::other("injected failure")),
                }
            })
            .unwrap_err();
            assert_eq!(calls, [WindowsDeleteDisposition::Posix]);
            assert_eq!(error.raw_os_error(), code);
            if code.is_none() {
                assert_eq!(error.to_string(), "injected failure");
            }
        }
    }

    #[test]
    fn windows_delete_legacy_failure_is_propagated() {
        let mut calls = Vec::new();
        let error = windows_delete_with_fallback(|disposition| {
            calls.push(disposition);
            match disposition {
                WindowsDeleteDisposition::Posix => Err(io::Error::from_raw_os_error(87)),
                WindowsDeleteDisposition::Legacy => Err(io::Error::from_raw_os_error(5)),
            }
        })
        .unwrap_err();
        assert_eq!(
            calls,
            [
                WindowsDeleteDisposition::Posix,
                WindowsDeleteDisposition::Legacy
            ]
        );
        assert_eq!(error.raw_os_error(), Some(5));
    }

    #[test]
    fn windows_delete_posix_success_never_falls_back() {
        let mut calls = Vec::new();
        windows_delete_with_fallback(|disposition| {
            calls.push(disposition);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, [WindowsDeleteDisposition::Posix]);
    }

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
        assert_eq!(counts.parses, 2);
        #[cfg(unix)]
        assert_eq!(counts.opens, 7);
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
        assert_eq!(counts.parses, 2);
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
    fn directory_handle_validation_reports_not_a_directory_for_regular_files() {
        let storage = tempfile::tempdir().unwrap();
        let dir = PinnedDir::open(storage.path()).unwrap();
        fs::write(storage.path().join("output.bin"), b"output").unwrap();

        let error = dir
            .open_dir_at(OsStr::new("output.bin"))
            .err()
            .expect("a regular file must not become a pinned directory");
        assert_eq!(error.kind(), io::ErrorKind::NotADirectory);
        remove_tree_contents(&dir).unwrap();
        assert!(!storage.path().join("output.bin").exists());
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

    #[cfg(windows)]
    #[test]
    fn directory_handle_validation_refuses_junctions() {
        let storage = tempfile::tempdir().unwrap();
        let io_path = storage.path().join("io");
        let target = storage.path().join("outside-task");
        fs::create_dir(&io_path).unwrap();
        fs::create_dir(&target).unwrap();
        let victim = target.join("victim");
        fs::write(&victim, b"victim-bytes").unwrap();
        let junction = io_path.join("junction");
        // Directory junctions do not require the symbolic-link privilege.
        let output = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "mklink /J failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(&junction)
            .unwrap();
        assert_ne!(
            file.metadata().unwrap().file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT,
            0,
            "the handle must refer to the junction itself, not its target"
        );
        let error = validate_directory_handle(&file).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "task path is a reparse point");
        drop(file);

        let dir = PinnedDir::open(&io_path).unwrap();
        let error = remove_tree_contents(&dir).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "task path is a reparse point");
        assert_eq!(fs::read(&victim).unwrap(), b"victim-bytes");
        fs::remove_dir(&junction).unwrap();
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
            include_str!("watches.rs"),
            include_str!("watchdog.rs"),
            include_str!("../commands/bash_status.rs"),
        ];
        for source in rust_sources {
            let production = source
                .split("#[cfg(test)]\nmod tests")
                .next()
                .unwrap_or(source);
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
