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
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::backup::hash_session;
use crate::bash_permissions::PermissionAsk;
use crate::db::bash_tasks::BashTaskRow;

use super::BgTaskStatus;

pub const SCHEMA_VERSION: u32 = 6;
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
        let result = unsafe { libc::mkdirat(self.file.as_raw_fd(), name.as_ptr(), 0o700) };
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
            0o600,
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
            self.ensure_current_identity()?;
            fs::remove_file(self.path.join(name))?;
            self.ensure_current_identity()
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
pub struct PersistedTask {
    pub schema_version: u32,
    pub task_id: String,
    pub session_id: String,
    pub command: String,
    /// Scanner-derived segment labels retained so completion rendering does not
    /// need to parse the command again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pipeline_segments: Vec<String>,
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
    pub exit_code: Option<i32>,
    pub child_pid: Option<u32>,
    pub pgid: Option<i32>,
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
}

fn default_notify_on_completion() -> bool {
    true
}

fn default_compressed() -> bool {
    true
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
        Self {
            schema_version: SCHEMA_VERSION,
            task_id,
            session_id,
            command,
            pipeline_segments: Vec::new(),
            mode: BgMode::Pipes,
            workdir,
            project_root,
            status: BgTaskStatus::Starting,
            started_at: unix_millis(),
            finished_at: None,
            duration_ms: None,
            timeout_ms,
            exit_code: None,
            child_pid: None,
            pgid: None,
            completion_delivered: !notify_on_completion,
            notify_on_completion,
            compressed,
            pty_rows: None,
            pty_cols: None,
            scanner_report: Vec::new(),
            sandbox_native: false,
            sandbox_temp_dir: None,
            status_reason: None,
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
        if let Ok(task) = serde_json::from_str::<PersistedTask>(&row.metadata) {
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
            task_id: row.task_id,
            session_id: row.session_id,
            command: row.command,
            pipeline_segments: Vec::new(),
            mode: BgMode::Pipes,
            workdir: PathBuf::from(row.cwd),
            project_root: None,
            status,
            started_at,
            finished_at,
            duration_ms: finished_at.map(|finished_at| finished_at.saturating_sub(started_at)),
            timeout_ms: row.timeout_ms.and_then(|value| u64::try_from(value).ok()),
            exit_code: row.exit_code,
            child_pid: row.pid.and_then(|value| u32::try_from(value).ok()),
            pgid: row.pgid.and_then(|value| i32::try_from(value).ok()),
            completion_delivered: row.completion_delivered,
            notify_on_completion: !row.completion_delivered,
            compressed: row.compressed,
            pty_rows: None,
            pty_cols: None,
            scanner_report: Vec::new(),
            sandbox_native: false,
            sandbox_temp_dir: None,
            status_reason: None,
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
    fs::create_dir_all(session_dir)?;
    #[cfg(unix)]
    {
        let parent = session_dir.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "session task directory has no bash-tasks parent",
            )
        })?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(session_dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
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
    let flat = session.open_file(&flat_name, false);
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
    quarantine_names(storage_dir, session_dir, &session, selected, reason)
}

pub fn quarantine_invalid_entry(
    storage_dir: &Path,
    session_dir: &Path,
    entry: &OsStr,
) -> io::Result<()> {
    let session = PinnedDir::open(session_dir)?;
    quarantine_names(
        storage_dir,
        session_dir,
        &session,
        vec![entry.to_os_string()],
        "invalid",
    )
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
    fs::create_dir_all(&quarantine_path)?;
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

pub fn read_task(path: &Path) -> io::Result<PersistedTask> {
    let mut file = open_validated_path(path, false)?;
    read_task_file(&mut file)
}

pub fn read_task_at(task: &ResolvedTask) -> io::Result<PersistedTask> {
    let name = match task.paths.layout {
        TaskLayout::Directory => OsString::from(METADATA_FILE),
        TaskLayout::Flat => OsString::from(format!("{}.json", task.paths.task_id)),
    };
    let mut file = task.dirs.control.open_file(&name, false)?;
    let metadata = read_task_file(&mut file)?;
    if metadata.task_id != task.paths.task_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background task metadata identity does not match its layout name",
        ));
    }
    Ok(metadata)
}

fn read_task_file(file: &mut File) -> io::Result<PersistedTask> {
    file.seek(SeekFrom::Start(0))?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    let task: PersistedTask = serde_json::from_str(&content).map_err(io::Error::other)?;
    if !matches!(task.schema_version, 2 | 3 | 4 | 5 | SCHEMA_VERSION) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported background task schema_version {} (expected 2, 3, 4, 5, or {SCHEMA_VERSION})",
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
        fs::create_dir_all(parent)?;
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
    let mut upgraded = task.clone();
    upgraded.schema_version = SCHEMA_VERSION;
    let content = serde_json::to_vec_pretty(&upgraded).map_err(io::Error::other)?;
    randomized_atomic_replace(dir, name, &content)
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
    let resolved = resolve_task_layout(&paths.session_dir, &paths.task_id)?;
    if resolved.paths.layout != paths.layout {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background task layout changed before deletion",
        ));
    }
    delete_resolved_task(&resolved)
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
    #[cfg(unix)]
    let tombstone = rename_task_to_tombstone(task)?;
    for name in task.dirs.control.list_names()? {
        task.dirs.control.remove_file(&name)?;
    }
    remove_tree_contents(&task.dirs.io)?;
    #[cfg(unix)]
    {
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
    #[cfg(windows)]
    {
        task.dirs.io.ensure_current_identity()?;
        task.dirs.control.ensure_current_identity()?;
        task.dirs.session.ensure_current_identity()?;
        fs::remove_dir(task.dirs.io.path())?;
        fs::remove_dir(task.dirs.control.path())?;
        fs::remove_dir(&task.paths.dir)?;
        task.dirs.session.ensure_current_identity()
    }
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
                fs::remove_dir(child.path())?;
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
        let result = match open_task_artifact(paths, TaskArtifact::Exit) {
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
    for _ in 0..32 {
        let temporary = random_temp_name()?;
        let mut file = match dir.open_new_file(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let result = (|| {
            file.write_all(content)?;
            file.sync_all()?;
            validate_regular_handle(&file)?;
            dir.rename(&temporary, name)
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
    file.sync_all()?;
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

    pub fn replace_contents(&mut self, content: &[u8]) -> io::Result<()> {
        validate_regular_handle(&self.file)?;
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(content)?;
        self.file.sync_all()
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
    validate_task_id(&paths.task_id)?;
    let resolved = resolve_task_layout(&paths.session_dir, &paths.task_id)?;
    if resolved.paths.layout != paths.layout {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background task layout identity changed",
        ));
    }
    let metadata = read_task_at(&resolved)?;
    if metadata.task_id != paths.task_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background task metadata identity mismatch",
        ));
    }
    let file = resolved
        .dirs
        .io
        .open_file(&resolved.paths.artifact_name(artifact), false)?;
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
}

impl TaskIoHandles {
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
        set_close_on_exec(file.as_raw_fd(), false)?;
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
        file.sync_all()
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
    let metadata = file.metadata()?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a non-reparse directory handle",
        ));
    }
    #[cfg(windows)]
    validate_windows_handle(file, true)?;
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
    use super::*;

    fn valid_id(suffix: u64) -> String {
        format!("bash-{suffix:016x}")
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
