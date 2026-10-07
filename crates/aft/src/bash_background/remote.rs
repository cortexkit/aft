//! Remote jobs use the ordinary task registry and its raw output artifacts.
#[cfg(unix)]
use super::*;
use crate::exec_remote::types::*;
#[cfg(unix)]
use crate::exec_remote::{
    self as exec, ExecRemoteClient, OutputSink, ResumePoint, StreamProgress, Verdict,
};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::ffi::OsStr;
#[cfg(unix)]
use std::io::{self, Seek, SeekFrom, Write};
use std::path::PathBuf;

#[cfg(unix)]
const REMOTE_REATTACH_BUDGET: Duration = Duration::from_secs(5 * 60);

/// The remote runner AFT dispatches to; named in every remote reply header.
#[cfg(unix)]
const RUNNER_ID: &str = "ck-motor";

/// This machine's operating system as a reader names it, for the header of a
/// run that fell back to it.
#[cfg(unix)]
fn local_os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        "freebsd" => "FreeBSD",
        other => other,
    }
}

/// Bound consecutive empty recovery attempts, not silence on an open stream.
/// Accepted jobs can disappear with a wiped runner state; they must never rerun.
#[cfg(unix)]
struct ReattachBudget {
    limit: Duration,
    deadline: Option<tokio::time::Instant>,
}

#[cfg(unix)]
impl ReattachBudget {
    fn new(_root: &Path) -> Self {
        #[cfg(test)]
        let limit = tests::take_reattach_budget(_root).unwrap_or(REMOTE_REATTACH_BUDGET);
        #[cfg(not(test))]
        let limit = REMOTE_REATTACH_BUDGET;
        Self {
            limit,
            deadline: None,
        }
    }

    fn reset(&mut self) {
        self.deadline = None;
    }

    fn expired(job_id: Uuid) -> String {
        format!("remote outcome unknown: job {job_id} could not be re-attached for 5 minutes; command not rerun")
    }

    fn check(&self, job_id: Uuid) -> Result<(), String> {
        if self
            .deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            return Err(Self::expired(job_id));
        }
        Ok(())
    }

    async fn wait<T>(
        &mut self,
        job_id: Uuid,
        attempt: impl std::future::Future<Output = T>,
    ) -> Result<T, String> {
        self.check(job_id)?;
        let deadline = *self
            .deadline
            .get_or_insert_with(|| tokio::time::Instant::now() + self.limit);
        tokio::time::timeout_at(deadline, attempt)
            .await
            .map_err(|_| Self::expired(job_id))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RemoteTask {
    pub connection_file: Option<PathBuf>,
    #[serde(default)]
    pub explicit_runon: bool,
    pub harness: String,
    pub session: String,
    pub job_id: Option<Uuid>,
    pub last_seq: Option<u64>,
    pub stdout_len: u64,
    pub stderr_len: u64,
    #[serde(default)]
    pub unknown_len: u64,
    pub cancel_requested: bool,
    pub terminal: Option<TerminalRecord>,
    #[serde(default)]
    pub fallback_digest: Option<String>,
    #[serde(default)]
    pub env_not_forwarded: Option<Vec<String>>,
}

#[cfg(unix)]
impl RemoteTask {
    fn point(&self) -> Option<ResumePoint> {
        Some(ResumePoint {
            job_id: self.job_id?,
            last_seq: self.last_seq,
        })
    }
}

#[cfg(unix)]
struct TaskSink {
    registry: BgTaskRegistry,
    task: Arc<BgTask>,
    stdout: fs::File,
    stderr: fs::File,
    unknown: Option<fs::File>,
}

#[cfg(unix)]
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
            unknown: None,
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

#[cfg(unix)]
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

#[cfg(unix)]
impl OutputSink for TaskSink {
    fn accepted(&mut self, accepted: &Accepted) -> io::Result<()> {
        self.commit(|r| {
            r.job_id = Some(accepted.job_id);
            if accepted.env_not_forwarded.is_some() {
                r.env_not_forwarded = accepted.env_not_forwarded.clone();
            }
        })?;
        let mut db = DeferredDbWrites::new(&self.registry, &self.task);
        let mut state = self
            .task
            .state
            .lock()
            .map_err(|_| io::Error::other("task lock poisoned"))?;
        append_executor_environment_disclosure(&mut state.metadata);
        self.registry
            .persist_task_locked(&self.task, &state.metadata, &mut db)
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
        let mut db = DeferredDbWrites::new(&self.registry, &self.task);
        let mut state = self
            .task
            .state
            .lock()
            .map_err(|_| io::Error::other("task lock poisoned"))?;
        let previous = state.metadata.clone();
        let remote = state
            .metadata
            .remote
            .as_mut()
            .ok_or_else(|| io::Error::other("remote record absent"))?;
        let first = remote.last_seq.map_or(0, |seq| seq.saturating_add(1));
        remote.last_seq = before_seq.checked_sub(1);
        if first < before_seq {
            state
                .metadata
                .incomplete_output
                .push((first, before_seq - 1));
        }
        append_output_loss(&mut state.metadata);
        let result = self
            .registry
            .persist_task_locked(&self.task, &state.metadata, &mut db);
        if result.is_err() {
            state.metadata = previous;
        }
        result
    }
    fn unknown_output(&mut self, seq: u64, bytes: &[u8]) -> io::Result<()> {
        let remote = self
            .task
            .state
            .lock()
            .map_err(|_| io::Error::other("task lock poisoned"))?
            .metadata
            .remote
            .clone()
            .unwrap();
        if !bytes.is_empty() {
            if self.unknown.is_none() {
                let layout = resolve_task_layout(&self.task.paths.session_dir, &self.task.task_id)?;
                let name = OsStr::new("remote-unknown-output");
                let file = match layout.dirs.io.open_new_file(name) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        layout.dirs.io.open_file(name, true)?
                    }
                    Err(error) => return Err(error),
                };
                self.unknown = Some(file);
            }
            let file = self.unknown.as_mut().unwrap();
            file.seek(SeekFrom::Start(remote.unknown_len))?;
            file.write_all(bytes)?;
            file.set_len(remote.unknown_len + bytes.len() as u64)?;
            file.sync_all()?;
        }
        self.commit(|r| {
            r.last_seq = Some(seq);
            r.unknown_len = remote.unknown_len + bytes.len() as u64;
        })
    }
    fn terminal(&mut self, record: &TerminalRecord, _verdict: &Verdict) -> io::Result<()> {
        self.commit(|r| {
            r.job_id = Some(record.job_id);
            r.terminal = Some(record.clone());
        })
    }
}

#[derive(Clone)]
#[cfg(unix)]
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
        metadata.execution_note = Some(format!("remote execution requested on {RUNNER_ID}"));
        metadata.remote = Some(RemoteTask {
            connection_file: launch.connection_file,
            explicit_runon: launch.explicit_runon,
            harness: launch.harness,
            session: launch.session,
            job_id: None,
            last_seq: None,
            stdout_len: 0,
            stderr_len: 0,
            unknown_len: 0,
            cancel_requested: false,
            terminal: None,
            fallback_digest: None,
            env_not_forwarded: None,
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
        let digest = crate::sandbox_spawn::save_local_launch(
            &plan,
            root.as_deref().unwrap_or(&workdir),
            &workdir,
            &crate::sandbox_spawn::current_authenticated_principal(),
            &shell_path,
            env.get("AFT_INTERNAL_LINUX_SCOPE")
                .is_some_and(|v| v == "1"),
        )?;
        metadata.remote.as_mut().unwrap().fallback_digest = Some(digest);
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
        let shell_environment: crate::sandbox_spawn::ChildEnvironment =
            if !plan.is_native_launcher() && plan.host_shell_path().is_none() {
                std::env::vars_os()
                    .chain(env.clone().into_iter().map(|(k, v)| (k.into(), v.into())))
                    .collect()
            } else {
                crate::sandbox_spawn::approved_environment_for_plan(&plan, &env)
            };
        let mut stripped = Vec::new();
        let environment = shell_environment
            .into_iter()
            .filter(|(key, _)| {
                if let Some(key) = key
                    .to_str()
                    .filter(|name| exec::denied_environment_name(name))
                {
                    stripped.push(key.to_owned());
                    false
                } else {
                    true
                }
            })
            .map(|(key, value)| {
                Ok((
                    key.into_string().map_err(|_| {
                        exec::Error::Protocol("shell environment contains a non-Unicode key".into())
                    })?,
                    value.into_string().map_err(|_| {
                        exec::Error::Protocol(
                            "shell environment contains a non-Unicode value".into(),
                        )
                    })?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>, exec::Error>>();
        {
            let mut db = DeferredDbWrites::new(self, &task);
            let mut state = task.state.lock().map_err(|_| "task lock poisoned")?;
            state.metadata.stripped_env_names = stripped;
            append_environment_disclosure(&mut state.metadata);
            self.persist_task_locked(&task, &state.metadata, &mut db)
                .map_err(|e| e.to_string())?;
        }
        let request = environment.and_then(|environment| {
            exec::build_request(
                request_root,
                &repository_root(request_root),
                &workdir,
                command,
                environment,
                Some(
                    hard_kill
                        .limit()
                        .as_secs()
                        .saturating_add(u64::from(hard_kill.limit().subsec_nanos() > 0)),
                ),
                &exec::PresetParams {
                    siblings: launch.params.siblings,
                    ..Default::default()
                },
            )
        });
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
        let mut recovery = ReattachBudget::new(&root);
        if let Some(record) = remote.terminal.as_ref() {
            if let Verdict::RunLocally { reason } = exec::grade(record) {
                return self.restored_remote_fallback(
                    &task,
                    &remote,
                    &serde_json::to_value(reason)
                        .unwrap()
                        .as_str()
                        .unwrap_or("unknown")
                        .to_owned(),
                );
            }
            self.remote_terminal(&task, exec::grade(record), None);
            return Ok(());
        }
        let connection = remote
            .connection_file
            .as_ref()
            .ok_or_else(|| "daemon connection file unavailable".to_string());
        let mut client = loop {
            let connect = async {
                match &connection {
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
                    Err(error) => Err(error.clone()),
                }
            };
            let result = if initial.is_none() {
                if let Some(point) = remote.point() {
                    recovery.wait(point.job_id, connect).await?
                } else {
                    connect.await
                }
            } else {
                connect.await
            };
            match result {
                Ok(c) => break c,
                Err(error) => {
                    if let Some((_, fallback)) = initial.take() {
                        return if remote.explicit_runon {
                            self.refuse_remote_before_dispatch(&task, &error)
                        } else {
                            self.remote_fallback(&task, fallback, &error)
                        };
                    }
                    if let Some(point) = remote.point().filter(|_| remote.connection_file.is_some())
                    {
                        recovery
                            .wait(point.job_id, tokio::time::sleep(Duration::from_secs(1)))
                            .await?;
                        continue;
                    }
                    return Err(format!("remote outcome unknown; attach required: {error}"));
                }
            }
        };
        let mut fallback = None;
        let mut stream = if let Some((request, local)) = initial.take() {
            let request = match request {
                Ok(r) => r,
                Err(error) => {
                    return if remote.explicit_runon {
                        self.refuse_remote_before_dispatch(&task, &error.to_string())
                    } else {
                        self.remote_fallback(&task, local, &error.to_string())
                    }
                }
            };
            fallback = Some(local);
            match client.run(&request).await {
                Ok(stream) => stream,
                Err(error) if proves_no_start(&error) => {
                    return if remote.explicit_runon {
                        self.refuse_remote_before_dispatch(&task, &error.to_string())
                    } else {
                        self.remote_fallback(&task, fallback.take().unwrap(), &error.to_string())
                    }
                }
                Err(error) => {
                    return Err(format!("remote outcome unknown; not resubmitted: {error}"))
                }
            }
        } else {
            let point = remote.point().ok_or("remote outcome unknown: acceptance job ID was not persisted; command was not resubmitted")?;
            self.attach_remote(&mut client, point, &remote, &root, &task, &mut recovery)
                .await?
        };
        let mut cancelled = false;
        loop {
            // Poll intent even when every stream read is immediately ready.
            // A new sleep per read cannot provide fairness under steady output.
            let cancel = task
                .state
                .lock()
                .map_err(|_| "task lock poisoned")?
                .metadata
                .remote
                .as_ref()
                .is_some_and(|r| r.cancel_requested);
            if cancel && !cancelled {
                if let Some(point) = stream.resume_point() {
                    let _ = client.cancel_job(point.job_id).await;
                    stream = self
                        .attach_remote(&mut client, point, &remote, &root, &task, &mut recovery)
                        .await?;
                    cancelled = true;
                }
            }
            let result = tokio::select! {
                result=stream.next(&mut sink)=>result,
                _=tokio::time::sleep(Duration::from_millis(50))=>{
                    continue;
                }
            };
            match result {
                Ok(StreamProgress::Record) => recovery.reset(),
                Ok(StreamProgress::Complete(Verdict::RunLocally { reason })) => {
                    if let Some(fallback) = fallback.take() {
                        // A live refusal uses the launch plan captured before
                        // dispatch. The private disk snapshot is for restarts.
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
                    return self.restored_remote_fallback(
                        &task,
                        &remote,
                        &serde_json::to_value(reason)
                            .unwrap()
                            .as_str()
                            .unwrap_or("unknown")
                            .to_owned(),
                    );
                }
                Ok(StreamProgress::Complete(verdict)) => {
                    self.remote_terminal(&task, verdict, None);
                    return Ok(());
                }
                Err(error) => {
                    let Some(point) = stream.resume_point() else {
                        return Err(format!("remote outcome unknown; not resubmitted: {error}"));
                    };
                    // Even an undecodable frame proves this was not an empty
                    // attach. A quiet but open stream has no output deadline.
                    if stream.received_frame() {
                        recovery.reset();
                    }
                    // A known accepted job is recovered only by attach. Never
                    // convert a lost stream into another run or local fallback.
                    recovery
                        .wait(point.job_id, tokio::time::sleep(Duration::from_millis(100)))
                        .await?;
                    stream = self
                        .attach_remote(&mut client, point, &remote, &root, &task, &mut recovery)
                        .await?;
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn attach_remote(
        &self,
        client: &mut ExecRemoteClient,
        point: ResumePoint,
        remote: &RemoteTask,
        root: &Path,
        task: &Arc<BgTask>,
        recovery: &mut ReattachBudget,
    ) -> Result<exec::RemoteStream, String> {
        point.attach_request().map_err(|e| e.to_string())?;
        recovery
            .wait(point.job_id, async {
                loop {
                    let cancel = task
                        .state
                        .lock()
                        .map_err(|_| "task lock poisoned")?
                        .metadata
                        .remote
                        .as_ref()
                        .is_some_and(|r| r.cancel_requested);
                    if cancel {
                        let _ = client.cancel_job(point.job_id).await;
                    }
                    if let Ok(stream) = client.attach(point.clone()).await {
                        return Ok(stream);
                    }
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
                            *client = reconnected;
                        }
                    }
                }
            })
            .await?
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
        let layout = resolve_task_layout(&task.paths.session_dir, &task.task_id)
            .map_err(|e| e.to_string())?;
        let durable = read_task_at(&layout).map_err(|e| e.to_string())?;
        if durable.remote.as_ref().is_some_and(|r| r.cancel_requested) {
            if let Some(remote) = state.metadata.remote.as_mut() {
                remote.cancel_requested = true;
            }
            drop(state);
            drop(db);
            self.remote_terminal(task,Verdict::RunLocally { reason:RefusalReason::Unknown(reason.into()) },Some("remote refused before start; local fallback skipped because cancellation was requested".into()));
            return Ok(());
        }
        // Commit the switch to local before spawning. A crash before the PID
        // write must report an uncertain local start, never submit it again.
        state.metadata.remote = None;
        state.metadata.local_fallback_started = true;
        state.metadata.started_at = unix_millis();
        let reason = reason.replace(['\n', '\r'], " ");
        state.metadata.execution_note = Some(format!(
            "ran locally on {}: remote refused: {reason}",
            local_os_name()
        ));
        if let Some(changes) = metadata
            .remote
            .as_ref()
            .and_then(|r| r.terminal.as_ref())
            .and_then(|t| t.workspace_changes.as_ref())
            .filter(|c| !c.is_empty())
        {
            state
                .metadata
                .execution_note
                .as_mut()
                .unwrap()
                .push_str(&format!(
                    "\nremote workspace_changes (not copied back): {}",
                    serde_json::to_string(changes).unwrap()
                ));
        }
        append_output_loss(&mut state.metadata);
        self.persist_task_locked(task, &state.metadata, &mut db)
            .map_err(|e| e.to_string())?;
        #[cfg(test)]
        tests::after_local_fallback_marker(&task.task_id);
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
        #[cfg(test)]
        if tests::simulate_crash_before_local_pid(&task.task_id) {
            return Ok(());
        }
        state.metadata.mark_running(child.id(), child.id() as i32);
        state.runtime = TaskRuntime::Piped(Some(child));
        state.detached = false;
        let persisted = (|| {
            #[cfg(test)]
            tests::fail_running_metadata_after_spawn(&task.task_id)?;
            self.persist_task_locked(task, &state.metadata, &mut db)
                .map_err(|e| e.to_string())
        })();
        drop(state);
        drop(db);
        if let Err(error) = persisted {
            // Spawn succeeded. This is not a no-start refusal, even when the
            // PID publication fails; both live and restarted callers end here.
            self.remote_terminal(
                task,
                Verdict::OutcomeUnknown,
                Some(format!(
                    "the command started locally; AFT lost track of it: {error}; never rerun"
                )),
            );
        }
        Ok(())
    }

    fn restored_remote_fallback(
        &self,
        task: &Arc<BgTask>,
        remote: &RemoteTask,
        reason: &str,
    ) -> Result<(), String> {
        let restore = (|| {
            let layout = resolve_task_layout(&task.paths.session_dir, &task.task_id)
                .map_err(|e| e.to_string())?;
            let (plan, shell_path, env, linux_scope) = crate::sandbox_spawn::restore_local_launch(
                &layout,
                remote
                    .fallback_digest
                    .as_deref()
                    .ok_or("local snapshot digest absent")?,
            )?;
            let metadata = task
                .state
                .lock()
                .map_err(|_| "task lock poisoned")?
                .metadata
                .clone();
            let capture_pipeline = should_capture_pipeline_status(
                &plan,
                !metadata.pipeline_segments.is_empty(),
                &shell_path,
            );
            // Replay has validated handles, but no local runtime retained them.
            let handles = TaskIoHandles::reopen(&layout).map_err(|e| e.to_string())?;
            task.state
                .lock()
                .map_err(|_| "task lock poisoned")?
                .io_handles = Some(handles);
            self.remote_fallback(
                task,
                LocalFallback {
                    plan,
                    shell_path,
                    env,
                    capture_pipeline,
                    linux_scope,
                },
                reason,
            )
        })();
        if let Err(error) = restore {
            self.remote_terminal(task,Verdict::RunLocally { reason:RefusalReason::Unknown(reason.into()) },Some(format!("remote refused before start; the local fallback could not be restored after restart, so the command did not run: {error}")));
        }
        Ok(())
    }

    fn refuse_remote_before_dispatch(&self, task: &Arc<BgTask>, error: &str) -> Result<(), String> {
        // Missing discovery providers and other proven pre-dispatch failures
        // cannot satisfy an explicit remote demand. Do not turn them into a
        // local run; executor-authored refusals still follow their own policy.
        self.remote_terminal(
            task,
            Verdict::RunLocally {
                reason: RefusalReason::Unreachable,
            },
            Some(format!(
                "runon refused: remote execution is unavailable; command did not run: {error}"
            )),
        );
        Ok(())
    }

    fn remote_terminal(&self, task: &Arc<BgTask>, verdict: Verdict, error: Option<String>) {
        // Known terminals keep their real status. Only unknown outcomes lose
        // their fate; no non-refusal terminal permits another execution.
        let refused = matches!(&verdict, Verdict::RunLocally { .. });
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
                128_i32.checked_add(signal),
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
                Some("remote history_expired; prior outcome unavailable; command not rerun".into()),
            ),
            Verdict::RunLocally { .. } => (BgTaskStatus::Failed, None, error),
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
            if refused {
                state.metadata.execution_note = Some(
                    if state
                        .metadata
                        .remote
                        .as_ref()
                        .is_some_and(|remote| remote.terminal.is_some())
                    {
                        "remote executor refused before start"
                    } else {
                        "runon refused before remote dispatch; command did not run"
                    }
                    .into(),
                );
            } else if matches!(
                status,
                BgTaskStatus::Completed
                    | BgTaskStatus::Failed
                    | BgTaskStatus::Killed
                    | BgTaskStatus::TimedOut
            ) {
                state.metadata.execution_note = Some(format!("ran remotely on {RUNNER_ID}"));
            }
            if let Some(terminal) = state
                .metadata
                .remote
                .as_ref()
                .and_then(|r| r.terminal.as_ref())
                .cloned()
            {
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
            append_output_loss(&mut state.metadata);
            if let Some(reason) = reason {
                let note = state
                    .metadata
                    .execution_note
                    .get_or_insert_with(|| format!("ran remotely on {RUNNER_ID}"));
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

    fn record_remote_cancel(&self, task: &Arc<BgTask>) -> Result<bool, String> {
        {
            let mut db = DeferredDbWrites::new(self, task);
            let mut state = task.state.lock().map_err(|_| "task lock poisoned")?;
            if !state.metadata.is_terminal() {
                let Some(remote) = state.metadata.remote.as_mut() else {
                    return Ok(false);
                };
                remote.cancel_requested = true;
                self.persist_task_locked(task, &state.metadata, &mut db)
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(true)
    }

    pub(super) fn kill_remote_task(&self, task: &Arc<BgTask>) -> Result<BgTaskSnapshot, String> {
        if !self.record_remote_cancel(task)? {
            return self.kill(&task.task_id, &task.session_id);
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
                "remote cancellation requested; terminal pending; inspect bash_status (never rerun)".into(),
            );
        }
        drop(state);
        Ok(self.snapshot_with_terminal_cache(task, 8 * 1024))
    }
}

#[cfg(unix)]
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

#[cfg(unix)]
fn append_output_loss(metadata: &mut PersistedTask) {
    append_environment_disclosure(metadata);
    append_executor_environment_disclosure(metadata);
    for (first, last) in &metadata.incomplete_output {
        let warning = format!(
            "output lost between seq {first} and {last}: the executor no longer retained it"
        );
        let note = metadata
            .execution_note
            .get_or_insert_with(|| format!("remote execution on {RUNNER_ID}"));
        if !note.contains(&warning) {
            note.push_str(&format!("\n{warning}"));
        }
    }
}

/// What the runner reported about the workspace after a remote run, printed
/// after the command's output. Writes on the server are never copied back,
/// so every change it reports is named. A kind of change the runner reported
/// as empty says nothing; a kind it did not report says so, on its own line,
/// instead of reading as "nothing changed". `None` when the command did not
/// run remotely or there is nothing to say.
#[cfg(unix)]
pub(crate) fn remote_report(
    metadata: &crate::bash_background::persistence::PersistedTask,
) -> Option<String> {
    let terminal = metadata.remote.as_ref()?.terminal.as_ref()?;
    if matches!(terminal.outcome, Outcome::RefusedBeforeStart { .. })
        || terminal.ran == Some(Ran::None)
    {
        return None;
    }
    render_terminal_report(terminal)
}

/// [`remote_report`] for one terminal record.
#[cfg(unix)]
fn render_terminal_report(terminal: &TerminalRecord) -> Option<String> {
    // Paths come from another machine; escaping keeps one per line.
    fn path(path: &str) -> String {
        path.escape_default().to_string()
    }
    fn id(id: Option<&str>) -> String {
        id.map_or_else(|| "none".to_string(), |id| id.chars().take(12).collect())
    }
    let mut blocks: Vec<String> = Vec::new();
    let mut unreported: Vec<&str> = Vec::new();

    match terminal.workspace_changes.as_deref() {
        Some([]) => {}
        Some(changes) => {
            let mut block =
                String::from("These files changed on the server and were NOT copied back:");
            for changed in changes {
                block.push_str(&format!("\n  {}", path(changed)));
            }
            blocks.push(block);
        }
        None => unreported.push("changed files"),
    }

    match &terminal.git_state_changed {
        Some(git) if git.changed() => {
            let mut block =
                String::from("Git state changed on the server and was NOT copied back:");
            if git.head_before != git.head_after {
                block.push_str(&format!(
                    "\n  HEAD: {} -> {}",
                    id(git.head_before.as_deref()),
                    id(git.head_after.as_deref())
                ));
            }
            if git.ref_before != git.ref_after {
                let name = |r: Option<&str>| r.map_or_else(|| "detached".to_string(), path);
                block.push_str(&format!(
                    "\n  ref: {} -> {}",
                    name(git.ref_before.as_deref()),
                    name(git.ref_after.as_deref())
                ));
            }
            if git.index_tree_before != git.index_tree_after {
                block.push_str("\n  index tree changed (staged changes differ)");
            }
            if git.stash_count_before != git.stash_count_after {
                let delta = i64::from(git.stash_count_after) - i64::from(git.stash_count_before);
                block.push_str(&format!(
                    "\n  stash count: {} -> {} ({delta:+})",
                    git.stash_count_before, git.stash_count_after
                ));
            }
            blocks.push(block);
        }
        Some(_) => {}
        None => unreported.push("git state"),
    }

    match &terminal.untracked_files {
        Some(untracked) if !untracked.paths.is_empty() || untracked.truncated => {
            let mut block = String::from(
                "These untracked files were created on the server and were NOT copied back:",
            );
            for created in &untracked.paths {
                block.push_str(&format!("\n  {}", path(created)));
            }
            if untracked.truncated {
                block.push_str("\n  (the runner listed only some of them; more were created)");
            }
            blocks.push(block);
        }
        Some(_) => {}
        None => unreported.push("untracked files"),
    }

    match &terminal.ignored_writes {
        Some(ignored) if ignored.count > 0 => {
            let mut block = format!(
                "{} write{} under ignored paths on the server {} NOT copied back",
                ignored.count,
                if ignored.count == 1 { "" } else { "s" },
                if ignored.count == 1 { "was" } else { "were" },
            );
            if ignored.sample_paths.is_empty() {
                block.push('.');
            } else {
                block.push_str(", for example:");
                for written in &ignored.sample_paths {
                    block.push_str(&format!("\n  {}", path(written)));
                }
            }
            blocks.push(block);
        }
        Some(_) => {}
        None => unreported.push("ignored writes"),
    }

    for field in unreported {
        blocks.push(format!("{field}: not reported by the runner"));
    }
    (!blocks.is_empty()).then(|| blocks.join("\n"))
}

#[cfg(not(unix))]
pub(crate) fn remote_report(
    _metadata: &crate::bash_background::persistence::PersistedTask,
) -> Option<String> {
    None
}

#[cfg(unix)]
fn append_environment_disclosure(metadata: &mut PersistedTask) {
    if metadata.stripped_env_names.is_empty() {
        return;
    }
    let disclosure = format!(
        "AFT stripped env names: {}",
        serde_json::to_string(&metadata.stripped_env_names).unwrap()
    );
    let note = metadata
        .execution_note
        .get_or_insert_with(|| format!("remote execution on {RUNNER_ID}"));
    // Each disclosure gets its own line, so the header stays the first line.
    if !note.contains(&disclosure) {
        note.push_str(&format!("\n{disclosure}"));
    }
}

#[cfg(unix)]
fn append_executor_environment_disclosure(metadata: &mut PersistedTask) {
    let Some(names) = metadata
        .remote
        .as_ref()
        .and_then(|r| r.env_not_forwarded.as_ref())
        .filter(|n| !n.is_empty())
    else {
        return;
    };
    let shown = names.len().min(10);
    let mut disclosure = format!(
        "the remote job does not receive: {}",
        names[..shown]
            .iter()
            .map(|name| name.escape_default().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if names.len() > shown {
        disclosure.push_str(&format!("; +{} more", names.len() - shown));
    }
    let note = metadata
        .execution_note
        .get_or_insert_with(|| format!("remote execution on {RUNNER_ID}"));
    // Each disclosure gets its own line, so the header stays the first line.
    if !note.contains(&disclosure) {
        note.push_str(&format!("\n{disclosure}"));
    }
}

#[cfg(unix)]
fn proves_no_start(error: &exec::Error) -> bool {
    matches!(
        error,
        exec::Error::Transport(
            subc_client_rs::CallError::NotSent(_)
                | subc_client_rs::CallError::StaleRouteHandle(_)
                | subc_client_rs::CallError::CapabilityUnprovided { .. }
                | subc_client_rs::CallError::CapabilityAmbiguous { .. }
                | subc_client_rs::CallError::InvalidCapabilityIdentifier { .. }
        ) | exec::Error::Protocol(_)
    )
}

#[cfg(all(test, unix))]
#[path = "remote_tests.rs"]
mod tests;
