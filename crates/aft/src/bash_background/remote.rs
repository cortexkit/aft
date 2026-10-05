//! Remote jobs use the ordinary task registry and its raw output artifacts.
use super::*;
use crate::exec_remote::{
    self as exec, types::*, ExecRemoteClient, OutputSink, ResumePoint, StreamProgress, Verdict,
};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::io::{self, Seek, SeekFrom, Write};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RemoteTask {
    pub connection_file: Option<PathBuf>,
    pub harness: String,
    pub session: String,
    pub job_id: Option<Uuid>,
    pub last_seq: Option<u64>,
    pub stdout_len: u64,
    pub stderr_len: u64,
    pub cancel_requested: bool,
    pub terminal: Option<TerminalRecord>,
}

impl RemoteTask {
    fn point(&self) -> Option<ResumePoint> {
        Some(ResumePoint {
            job_id: self.job_id?,
            last_seq: self.last_seq,
        })
    }
}

struct TaskSink {
    registry: BgTaskRegistry,
    task: Arc<BgTask>,
    stdout: fs::File,
    stderr: fs::File,
}

impl TaskSink {
    fn new(registry: &BgTaskRegistry, task: Arc<BgTask>) -> io::Result<Self> {
        let remote = task
            .state
            .lock()
            .map_err(|_| io::Error::other("task lock poisoned"))?
            .metadata
            .remote
            .clone()
            .ok_or_else(|| io::Error::other("remote record absent"))?;
        let stdout = open_task_artifact_for_remote(&task, TaskArtifact::Stdout)?;
        let stderr = open_task_artifact_for_remote(&task, TaskArtifact::Stderr)?;
        // Bytes beyond the committed cursor can exist if AFT died between a
        // file append and the atomic metadata replacement. Discard only those.
        stdout.set_len(remote.stdout_len)?;
        stderr.set_len(remote.stderr_len)?;
        Ok(Self {
            registry: registry.clone(),
            task,
            stdout,
            stderr,
        })
    }

    fn commit(&self, update: impl FnOnce(&mut RemoteTask)) -> io::Result<()> {
        let mut db = DeferredDbWrites::new(&self.registry, &self.task);
        let mut state = self
            .task
            .state
            .lock()
            .map_err(|_| io::Error::other("task lock poisoned"))?;
        let previous = state.metadata.remote.clone();
        update(
            state
                .metadata
                .remote
                .as_mut()
                .ok_or_else(|| io::Error::other("remote record absent"))?,
        );
        let result = self
            .registry
            .persist_task_locked(&self.task, &state.metadata, &mut db);
        if result.is_err() {
            state.metadata.remote = previous;
        }
        result
    }
}

fn open_task_artifact_for_remote(task: &BgTask, artifact: TaskArtifact) -> io::Result<fs::File> {
    if let Some(handles) = &task
        .state
        .lock()
        .map_err(|_| io::Error::other("task lock poisoned"))?
        .io_handles
    {
        return handles.clone_file(artifact);
    }
    // Replay uses the same pinned-directory validation as local artifact reads.
    let resolved = resolve_task_layout(&task.paths.session_dir, &task.task_id)?;
    resolved
        .dirs
        .io
        .open_file(OsStr::new(artifact.file_name()), true)
}

impl OutputSink for TaskSink {
    fn accepted(&mut self, accepted: &Accepted) -> io::Result<()> {
        self.commit(|r| r.job_id = Some(accepted.job_id))
    }
    fn output(&mut self, seq: u64, stream: OutputStream, bytes: &[u8]) -> io::Result<()> {
        let remote = self
            .task
            .state
            .lock()
            .map_err(|_| io::Error::other("task lock poisoned"))?
            .metadata
            .remote
            .clone()
            .unwrap();
        let stdout = stream == OutputStream::Stdout;
        let (file, offset) = if stdout {
            (&mut self.stdout, remote.stdout_len)
        } else {
            (&mut self.stderr, remote.stderr_len)
        };
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(bytes)?;
        file.set_len(offset + bytes.len() as u64)?;
        file.sync_all()?;
        self.commit(|r| {
            if stdout {
                r.stdout_len = offset + bytes.len() as u64;
            } else {
                r.stderr_len = offset + bytes.len() as u64;
            }
            r.last_seq = Some(seq);
        })?;
        let _ = self.registry.inner.wake_tx.try_send(());
        Ok(())
    }
    fn truncated(&mut self, before_seq: u64) -> io::Result<()> {
        self.commit(|r| r.last_seq = before_seq.checked_sub(1))?;
        let mut db = DeferredDbWrites::new(&self.registry, &self.task);
        let mut state = self
            .task
            .state
            .lock()
            .map_err(|_| io::Error::other("task lock poisoned"))?;
        state.metadata.execution_note=Some(format!("ran remotely on ck-motor; retained output starts at seq {before_seq} (earlier output expired)"));
        self.registry
            .persist_task_locked(&self.task, &state.metadata, &mut db)
    }
    fn unknown_output(&mut self, seq: u64, _bytes: &[u8]) -> io::Result<()> {
        self.commit(|r| r.last_seq = Some(seq))
    }
    fn terminal(&mut self, record: &TerminalRecord, _verdict: &Verdict) -> io::Result<()> {
        self.commit(|r| {
            r.job_id = Some(record.job_id);
            r.terminal = Some(record.clone());
        })
    }
}

#[derive(Clone)]
struct LocalFallback {
    plan: SpawnPlan,
    shell_path: PathBuf,
    env: HashMap<String, String>,
    capture_pipeline: bool,
    linux_scope: bool,
}

#[cfg(unix)]
impl BgTaskRegistry {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_remote(
        &self,
        launch: super::super::RemoteLaunch,
        mut plan: SpawnPlan,
        command: &str,
        shell_path: PathBuf,
        session_id: String,
        workdir: PathBuf,
        mut env: HashMap<String, String>,
        hard_kill: super::super::HardKill,
        storage_dir: PathBuf,
        max_running: usize,
        notify: bool,
        compressed: bool,
        root: Option<PathBuf>,
    ) -> Result<String, String> {
        if self.running_count() >= max_running {
            return Err("background bash task limit exceeded".into());
        }
        let layout = match plan.prepared_task() {
            Some(p) => p.resolved_task(),
            None => allocate_task_layout(&storage_dir, &session_id).map_err(|e| e.to_string())?,
        };
        if plan.prepared_task().is_none() {
            let environment =
                crate::sandbox_spawn::approved_payload_environment(&env, &std::env::temp_dir());
            let prepared = crate::sandbox_spawn::prepare_task_payload(
                &layout,
                command.as_bytes(),
                root.as_deref().unwrap_or(&workdir),
                &workdir,
                &crate::sandbox_spawn::current_authenticated_principal(),
                &shell_path,
                &environment,
            )?;
            plan = plan.with_prepared_task(prepared);
        }
        let task_id = layout.paths.task_id.clone();
        let mut metadata = PersistedTask::starting(
            task_id.clone(),
            session_id,
            command.into(),
            workdir.clone(),
            root.clone(),
            Some(hard_kill.limit().as_millis() as u64),
            notify,
            compressed,
        );
        metadata.harness = metadata.harness.or_else(|| self.fallback_db_harness());
        metadata.default_hard_kill = hard_kill.renewable();
        metadata.status = BgTaskStatus::Running;
        metadata.execution_note = Some("ran remotely on ck-motor".into());
        metadata.remote = Some(RemoteTask {
            connection_file: launch.connection_file,
            harness: launch.harness,
            session: launch.session,
            job_id: None,
            last_seq: None,
            stdout_len: 0,
            stderr_len: 0,
            cancel_requested: false,
            terminal: None,
        });
        metadata.pipeline_segments = single_top_level_pipeline(command)
            .map(|p| p.segments.into_iter().map(|s| s.label).collect())
            .unwrap_or_default();
        let capture_pipeline = should_capture_pipeline_status(
            &plan,
            !metadata.pipeline_segments.is_empty(),
            &shell_path,
        );
        attach_sandbox_metadata(&mut metadata, &plan);
        let handles =
            TaskIoHandles::create(&layout, BgMode::Pipes, true).map_err(|e| e.to_string())?;
        write_task_at(&layout, &metadata).map_err(|e| e.to_string())?;
        self.dual_write_task(&layout.paths, &metadata);
        self.insert_rehydrated_task(metadata, layout.paths, false)?;
        let task = self.task(&task_id).unwrap();
        task.state
            .lock()
            .map_err(|_| "task lock poisoned")?
            .io_handles = Some(handles);
        let request_root = root.as_deref().unwrap_or(&workdir);
        let linux_scope = env
            .remove("AFT_INTERNAL_LINUX_SCOPE")
            .is_some_and(|v| v == "1");
        let environment = if !plan.is_native_launcher() && plan.host_shell_path().is_none() {
            std::env::vars().chain(env.clone()).collect()
        } else {
            crate::sandbox_spawn::approved_environment_for_plan(&plan, &env)
                .into_iter()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .collect()
        };
        let request = exec::build_request(
            request_root,
            &repository_root(request_root),
            &workdir,
            command,
            environment,
            Some(hard_kill.limit().as_secs().max(1)),
            &exec::PresetParams {
                siblings: launch.params.siblings,
                ..Default::default()
            },
        );
        let fallback = LocalFallback {
            plan,
            shell_path,
            linux_scope,
            env,
            capture_pipeline,
        };
        self.start_remote_worker(task, Some((request, fallback)))?;
        self.start_watchdog();
        Ok(task_id)
    }

    pub(super) fn resume_remote_task(&self, task_id: &str) -> Result<(), String> {
        self.start_remote_worker(self.task(task_id).ok_or("remote task missing")?, None)
    }

    fn start_remote_worker(
        &self,
        task: Arc<BgTask>,
        initial: Option<(Result<RunRequest, exec::Error>, LocalFallback)>,
    ) -> Result<(), String> {
        let registry = self.clone();
        std::thread::Builder::new()
            .name(format!("aft-remote-{}", task.task_id))
            .spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| e.to_string())
                    .and_then(|rt| rt.block_on(registry.run_remote_task(task.clone(), initial)));
                if let Err(error) = result {
                    registry.remote_terminal(&task, Verdict::OutcomeUnknown, Some(error));
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn run_remote_task(
        &self,
        task: Arc<BgTask>,
        mut initial: Option<(Result<RunRequest, exec::Error>, LocalFallback)>,
    ) -> Result<(), String> {
        let remote = task
            .state
            .lock()
            .map_err(|_| "task lock poisoned")?
            .metadata
            .remote
            .clone()
            .unwrap();
        let root = task
            .state
            .lock()
            .map_err(|_| "task lock poisoned")?
            .metadata
            .project_root
            .clone()
            .unwrap_or_else(|| PathBuf::from("/"));
        let mut sink = TaskSink::new(self, task.clone()).map_err(|e| e.to_string())?;
        if let Some(record) = remote.terminal.as_ref() {
            self.remote_terminal(&task, exec::grade(record), None);
            return Ok(());
        }
        let connection = remote
            .connection_file
            .as_ref()
            .ok_or_else(|| "daemon connection file unavailable".to_string());
        let client = match connection {
            Ok(path) => ExecRemoteClient::connect(
                path,
                subc_protocol::BindIdentity::new(
                    root.display().to_string(),
                    remote.harness.clone(),
                    remote.session.clone(),
                ),
            )
            .await
            .map_err(|e| e.to_string()),
            Err(error) => Err(error),
        };
        let mut client = match client {
            Ok(c) => c,
            Err(error) => {
                if let Some((_, fallback)) = initial.take() {
                    return self.remote_fallback(&task, fallback, &error);
                }
                return Err(format!("remote outcome unknown; attach required: {error}"));
            }
        };
        let mut fallback = None;
        let mut stream = if let Some((request, local)) = initial.take() {
            let request = match request {
                Ok(r) => r,
                Err(error) => return self.remote_fallback(&task, local, &error.to_string()),
            };
            fallback = Some(local);
            client.run(&request).await.map_err(|e| e.to_string())?
        } else {
            client.attach(remote.point().ok_or("remote outcome unknown: acceptance job ID was not persisted; command was not resubmitted")?).await.map_err(|e|e.to_string())?
        };
        let mut cancelled = false;
        loop {
            let result = tokio::select! {
                result=stream.next(&mut sink)=>result,
                _=tokio::time::sleep(Duration::from_millis(50))=>{
                    let cancel=task.state.lock().map_err(|_|"task lock poisoned")?.metadata.remote.as_ref().is_some_and(|r|r.cancel_requested);
                    if cancel && !cancelled {
                        if let Some(point)=stream.resume_point() {
                            client.cancel_job(point.job_id).await.map_err(|e|e.to_string())?;
                            stream=client.attach(point).await.map_err(|e|e.to_string())?;
                            cancelled=true;
                        }
                    }
                    continue;
                }
            };
            match result {
                Ok(StreamProgress::Record) => {}
                Ok(StreamProgress::Complete(Verdict::RunLocally { reason })) => {
                    if let Some(fallback) = fallback.take() {
                        return self.remote_fallback(
                            &task,
                            fallback,
                            &serde_json::to_value(reason)
                                .unwrap()
                                .as_str()
                                .unwrap_or("unknown")
                                .to_owned(),
                        );
                    }
                    return Err("remote executor refused after restart; no local launch context retained; command was not run".into());
                }
                Ok(StreamProgress::Complete(verdict)) => {
                    self.remote_terminal(&task, verdict, None);
                    return Ok(());
                }
                Err(error) => {
                    let Some(point) = stream.resume_point() else {
                        return Err(format!("remote outcome unknown; not resubmitted: {error}"));
                    };
                    // A known accepted job is recovered only by attach. Never
                    // convert a lost stream into another run or local fallback.
                    loop {
                        match client.attach(point.clone()).await {
                            Ok(attached) => {
                                stream = attached;
                                break;
                            }
                            Err(_) => {
                                tokio::time::sleep(Duration::from_secs(1)).await;
                                if let Some(path) = &remote.connection_file {
                                    if let Ok(reconnected) = ExecRemoteClient::connect(
                                        path,
                                        subc_protocol::BindIdentity::new(
                                            root.display().to_string(),
                                            remote.harness.clone(),
                                            remote.session.clone(),
                                        ),
                                    )
                                    .await
                                    {
                                        client = reconnected;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn remote_fallback(
        &self,
        task: &Arc<BgTask>,
        local: LocalFallback,
        reason: &str,
    ) -> Result<(), String> {
        let mut db = DeferredDbWrites::new(self, task);
        let mut state = task.state.lock().map_err(|_| "task lock poisoned")?;
        let metadata = state.metadata.clone();
        let child = spawn_detached_child(
            &local.plan,
            &metadata.command,
            super::super::BashShell::Bash,
            &local.shell_path,
            &task.paths,
            &metadata.workdir,
            &local.env,
            state.io_handles.as_mut().ok_or("task handles absent")?,
            local.capture_pipeline,
            local.linux_scope,
        )?;
        state.metadata.remote = None;
        state.metadata.execution_note =
            Some(format!("ran locally: remote executor refused ({reason})"));
        state.metadata.mark_running(child.id(), child.id() as i32);
        state.runtime = TaskRuntime::Piped(Some(child));
        state.detached = false;
        self.persist_task_locked(task, &state.metadata, &mut db)
            .map_err(|e| e.to_string())
    }

    fn remote_terminal(&self, task: &Arc<BgTask>, verdict: Verdict, error: Option<String>) {
        let (status, code, reason) = match verdict {
            Verdict::Exited { code } => (
                if code == 0 {
                    BgTaskStatus::Completed
                } else {
                    BgTaskStatus::Failed
                },
                Some(code),
                None,
            ),
            Verdict::Signalled { signal } => (
                BgTaskStatus::Failed,
                Some(128 + signal),
                Some(format!("remote signal {signal}")),
            ),
            Verdict::DeadlineKilled => (
                BgTaskStatus::TimedOut,
                None,
                Some("remote deadline kill".into()),
            ),
            Verdict::Cancelled | Verdict::CancelKilled => {
                (BgTaskStatus::Killed, None, Some("remote cancel".into()))
            }
            Verdict::HistoryExpired => (
                BgTaskStatus::FateUnknown,
                None,
                Some("remote history_expired; command not run".into()),
            ),
            _ => (
                BgTaskStatus::FateUnknown,
                None,
                Some(error.unwrap_or_else(|| "remote outcome_unknown; never rerun".into())),
            ),
        };
        {
            let mut db = DeferredDbWrites::new(self, task);
            let Ok(mut state) = task.state.lock() else {
                return;
            };
            if state.metadata.is_terminal() {
                return;
            }
            if let Some(terminal) = state
                .metadata
                .remote
                .as_ref()
                .and_then(|r| r.terminal.as_ref())
                .cloned()
            {
                let changes = terminal.workspace_changes.as_ref();
                if let Some(changes) = changes.filter(|c| !c.is_empty()) {
                    state.metadata.execution_note = Some(format!(
                        "ran remotely on ck-motor\nworkspace_changes (not copied back): {}",
                        changes.join(", ")
                    ));
                }
                if let Some(pipestatus) = &terminal.pipestatus {
                    if let Some(handles) = state.io_handles.as_mut() {
                        let _ = handles.write(
                            TaskArtifact::PipelineStatus,
                            format!(
                                "{}\n",
                                pipestatus
                                    .iter()
                                    .map(i32::to_string)
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            )
                            .as_bytes(),
                        );
                    }
                }
            }
            state.metadata.mark_terminal(status, code, reason.clone());
            if let Some(reason) = reason {
                let note = state
                    .metadata
                    .execution_note
                    .get_or_insert_with(|| "ran remotely on ck-motor".into());
                note.push_str(&format!("\n{reason}"));
            }
            task.mark_terminal_now();
            let _ = self.persist_task_locked(task, &state.metadata, &mut db);
            task.kill_settled.notify_all();
        }
        let _ = self.post_terminal_transition(task, true);
        self.inner.terminal_transition.notify_waiters();
        let _ = self.inner.wake_tx.try_send(());
    }

    pub(super) fn kill_remote_task(&self, task: &Arc<BgTask>) -> Result<BgTaskSnapshot, String> {
        {
            let mut db = DeferredDbWrites::new(self, task);
            let mut state = task.state.lock().map_err(|_| "task lock poisoned")?;
            if !state.metadata.is_terminal() {
                state.metadata.remote.as_mut().unwrap().cancel_requested = true;
                self.persist_task_locked(task, &state.metadata, &mut db)
                    .map_err(|e| e.to_string())?;
            }
        }
        let state = task.state.lock().map_err(|_| "task lock poisoned")?;
        let (state, _) = task
            .kill_settled
            .wait_timeout_while(state, Duration::from_secs(30), |s| {
                !s.metadata.is_terminal()
            })
            .map_err(|_| "task lock poisoned")?;
        if !state.metadata.is_terminal() {
            return Err(
                "remote cancel sent; terminal pending; inspect bash_status (never rerun)".into(),
            );
        }
        drop(state);
        Ok(self.snapshot_with_terminal_cache(task, 8 * 1024))
    }
}

fn repository_root(root: &Path) -> PathBuf {
    let Some(gitdir) = fs::read_to_string(root.join(".git"))
        .ok()
        .and_then(|s| s.strip_prefix("gitdir: ").map(|s| root.join(s.trim())))
    else {
        return root.into();
    };
    fs::read_to_string(gitdir.join("commondir"))
        .ok()
        .and_then(|s| gitdir.join(s.trim()).canonicalize().ok())
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| root.into())
}

#[cfg(all(test, unix))]
#[path = "remote_tests.rs"]
mod tests;
