use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::backup::hash_session;
use crate::db::bash_tasks::BashTaskRow;

use super::BgTaskStatus;

pub const SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone)]
pub struct TaskPaths {
    pub dir: PathBuf,
    pub json: PathBuf,
    pub stdout: PathBuf,
    pub stderr: PathBuf,
    pub exit: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedTask {
    pub schema_version: u32,
    pub task_id: String,
    pub session_id: String,
    pub command: String,
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
    /// Per-call output compression opt-in. Defaults to `true` so existing
    /// behavior (compression when `experimental.bash.compress=true`) is
    /// unchanged. Agents can pass `compressed: false` to disable compression
    /// for a single bash call without flipping the global flag.
    #[serde(default = "default_compressed")]
    pub compressed: bool,
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
        let output_bytes = capture_output_bytes(paths);
        Ok(BashTaskRow {
            harness: harness.to_string(),
            session_id: self.session_id.clone(),
            task_id: self.task_id.clone(),
            project_key: crate::search_index::project_cache_key(project_root),
            command: self.command.clone(),
            cwd: self.workdir.display().to_string(),
            status: status_name(&self.status).to_string(),
            exit_code: self.exit_code,
            pid: self.child_pid.map(i64::from),
            pgid: self.pgid.map(i64::from),
            started_at: self.started_at as i64,
            completed_at: self.finished_at.map(|value| value as i64),
            stdout_path: Some(paths.stdout.display().to_string()),
            stderr_path: Some(paths.stderr.display().to_string()),
            compressed: self.compressed,
            timeout_ms: self.timeout_ms.map(|value| value as i64),
            completion_delivered: self.completion_delivered,
            output_bytes,
            metadata: serde_json::to_string(self)?,
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
            _ => BgTaskStatus::Failed,
        };
        let started_at = u64::try_from(row.started_at).unwrap_or_default();
        let finished_at = row.completed_at.and_then(|value| u64::try_from(value).ok());

        PersistedTask {
            schema_version: SCHEMA_VERSION,
            task_id: row.task_id,
            session_id: row.session_id,
            command: row.command,
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
    }
}

fn capture_output_bytes(paths: &TaskPaths) -> Option<i64> {
    let stdout = fs::metadata(&paths.stdout)
        .ok()
        .map(|metadata| metadata.len());
    let stderr = fs::metadata(&paths.stderr)
        .ok()
        .map(|metadata| metadata.len());
    match (stdout, stderr) {
        (Some(stdout), Some(stderr)) => Some(stdout.saturating_add(stderr) as i64),
        (Some(bytes), None) | (None, Some(bytes)) => Some(bytes as i64),
        (None, None) => None,
    }
}

pub fn session_tasks_dir(storage_dir: &Path, session_id: &str) -> PathBuf {
    storage_dir
        .join("bash-tasks")
        .join(hash_session(session_id))
}

pub fn task_paths(storage_dir: &Path, session_id: &str, task_id: &str) -> TaskPaths {
    let dir = session_tasks_dir(storage_dir, session_id);
    TaskPaths {
        json: dir.join(format!("{task_id}.json")),
        stdout: dir.join(format!("{task_id}.stdout")),
        stderr: dir.join(format!("{task_id}.stderr")),
        exit: dir.join(format!("{task_id}.exit")),
        dir,
    }
}

pub fn read_task(path: &Path) -> io::Result<PersistedTask> {
    let content = fs::read_to_string(path)?;
    let task: PersistedTask = serde_json::from_str(&content).map_err(io::Error::other)?;
    if task.schema_version != SCHEMA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported background task schema_version {} (expected {SCHEMA_VERSION})",
                task.schema_version
            ),
        ));
    }
    Ok(task)
}

pub fn write_task(path: &Path, task: &PersistedTask) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_vec_pretty(task).map_err(io::Error::other)?;
    atomic_write(path, &content, &task.task_id)
}

pub(super) fn delete_task_bundle(paths: &TaskPaths) -> io::Result<()> {
    let mut first_error = None;
    for path in task_bundle_files(paths) {
        if let Err(error) = remove_file_if_present(&path) {
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }

    if let Some(error) = first_error {
        return Err(error);
    }

    match fs::remove_dir(&paths.dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(error) => Err(error),
    }
}

fn task_bundle_files(paths: &TaskPaths) -> Vec<PathBuf> {
    let mut files = vec![
        paths.json.clone(),
        paths.stdout.clone(),
        paths.stderr.clone(),
        paths.exit.clone(),
    ];
    if let Some(stem) = paths.json.file_stem().and_then(|stem| stem.to_str()) {
        // Windows background bash writes per-task wrapper scripts next to the
        // capture files as `<task-id>.ps1`, `<task-id>.bat`, or `<task-id>.sh`
        // depending on the shell selected in `detached_shell_command_for`.
        for extension in ["ps1", "bat", "sh"] {
            files.push(paths.dir.join(format!("{stem}.{extension}")));
        }
    }
    files
}

fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn update_task<F>(path: &Path, update: F) -> io::Result<PersistedTask>
where
    F: FnOnce(&mut PersistedTask),
{
    let mut task = read_task(path)?;
    let original_terminal = task.is_terminal();
    let original = task.clone();
    update(&mut task);
    if original_terminal {
        let completion_delivered = task.completion_delivered;
        task = original;
        task.completion_delivered = completion_delivered;
    }
    write_task(path, &task)?;
    Ok(task)
}

pub fn write_kill_marker_if_absent(path: &Path) -> io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    atomic_write(path, b"killed", "kill")
}

pub fn read_exit_marker(path: &Path) -> io::Result<Option<ExitMarker>> {
    let mut file = match File::open(path) {
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
    match content.parse::<i32>() {
        Ok(code) => Ok(Some(ExitMarker::Code(code))),
        Err(_) => Ok(None),
    }
}

pub fn atomic_write(path: &Path, content: &[u8], task_id: &str) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("task");
    let tmp = parent.join(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        sanitize_task_id(task_id)
    ));
    {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        file.write_all(content)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn sanitize_task_id(task_id: &str) -> String {
    task_id
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '_',
        })
        .collect()
}

pub fn create_capture_file(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    File::create(path)
}

pub fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    #[test]
    fn atomic_write_temp_names_include_task_id() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("task.json");

        let left_path = path.clone();
        let left = thread::spawn(move || atomic_write(&left_path, b"left", "task-left"));
        let right_path = path.clone();
        let right = thread::spawn(move || atomic_write(&right_path, b"right", "task-right"));

        left.join().expect("join left").expect("write left");
        right.join().expect("join right").expect("write right");

        let content = fs::read_to_string(&path).expect("read final content");
        assert!(content == "left" || content == "right");
        assert!(!dir
            .path()
            .join(format!(".task.json.tmp.{}", std::process::id()))
            .exists());
    }
}
