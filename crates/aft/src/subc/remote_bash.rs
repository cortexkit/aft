//! A federated shell is not a local background task and has no task ID.

use super::*;
use crate::context::confinement;
use crate::db::remote_grant_rows::Mode;
use cortexkit_role_tool_provider::{call::ToolCallRequest, errors};
use tokio_util::sync::CancellationToken;

pub(super) const BACKGROUND: &str = "Remote shells can't run in the background.";
pub(super) const TIME_LIMIT: &str = "The command was stopped at the remote time limit.";
pub(super) const MAX_RUNTIME: Duration = Duration::from_secs(600);

#[cfg(all(test, target_os = "linux"))]
tokio::task_local! {
    pub(super) static TEST_LAUNCHER: PathBuf;
}

pub(super) struct Shell {
    pub(super) command: String,
    pub(super) timeout: Duration,
    pub(super) workdir: PathBuf,
    pub(super) folder: PathBuf,
}

impl Shell {
    pub(super) fn prepare(
        call: ToolCallRequest,
        scope: ScopeStamp,
        folder: PathBuf,
        session: String,
        digest: String,
        ctx: &AppContext,
    ) -> Result<(Self, Preflight), ErrorBody> {
        let args = call.arguments;
        if args["background"] == true || args["pty"] == true {
            return Err(ErrorBody::new("invalid_request", BACKGROUND));
        }
        if args.get("sandbox").is_some()
            || args.get("runon").is_some()
            || args.get("shell").is_some()
        {
            return Err(tool_unavailable());
        }
        if !cfg!(target_os = "linux") {
            return Err(unavailable());
        }
        let workdir = args["workdir"]
            .as_str()
            .filter(|path| !path.is_empty())
            .map_or_else(|| folder.clone(), |path| folder.join(path));
        let shell = Self {
            command: args["command"]
                .as_str()
                .ok_or_else(|| errors::invalid_request("command", "expected a shell command"))?
                .to_owned(),
            timeout: args["timeout"]
                .as_u64()
                .map_or(MAX_RUNTIME, |ms| Duration::from_millis(ms).min(MAX_RUNTIME)),
            workdir,
            folder: folder.clone(),
        };
        shell.check(ctx)?;
        Ok((
            shell,
            Preflight {
                scope,
                session,
                folder,
                mode: Mode::ReadWrite,
                tool: call.name,
                args_hash: digest,
            },
        ))
    }

    pub(super) fn check(&self, ctx: &AppContext) -> Result<PathBuf, ErrorBody> {
        let _config = ctx.pin_config();
        if crate::tool_gate::refusal_by_name("remote-bash", "bash", &ctx.config().disabled_tools)
            .is_some()
        {
            return Err(tool_unavailable());
        }
        let workdir = self
            .workdir
            .canonicalize()
            .map_err(|_| dispatch::error(&confinement::refusal("remote-bash", &self.workdir)))?;
        if !workdir.starts_with(&self.folder) || !workdir.is_dir() {
            return Err(dispatch::error(&confinement::refusal(
                "remote-bash",
                &self.workdir,
            )));
        }
        confinement::with_root(ctx, "remote-bash", || {
            confinement::preflight(ctx, "remote-bash", "remote-bash", &json!({"path":workdir}))
        })
        .map_err(|error| dispatch::error(&error))?;
        Ok(workdir)
    }
}

pub(super) fn unavailable() -> ErrorBody {
    #[cfg(target_os = "macos")]
    return ErrorBody::new("sandbox_unavailable", "Remote shells aren't available on macOS because AFT can't confine the entire process tree there.");
    #[cfg(not(target_os = "macos"))]
    ErrorBody::new("sandbox_unavailable", tool_unavailable().message)
}

#[derive(Clone, Default)]
pub(in crate::subc) struct Running(Arc<StdMutex<HashMap<(RouteChannel, String), Entry>>>);

struct Entry {
    grant: Option<String>,
    revoked: CancellationToken,
    yield_tx: Option<oneshot::Sender<Frame>>,
    running: Arc<std::sync::atomic::AtomicBool>,
}

pub(super) struct Job {
    jobs: Running,
    key: (RouteChannel, String),
    pub(super) revoked: CancellationToken,
    pub(super) yield_rx: oneshot::Receiver<Frame>,
    pub(super) running: Arc<std::sync::atomic::AtomicBool>,
}

impl Running {
    pub(super) fn register(
        &self,
        route: RouteChannel,
        call_key: String,
        grant: Option<String>,
    ) -> Job {
        let (yield_tx, yield_rx) = oneshot::channel();
        let key = (route, call_key);
        let revoked = CancellationToken::new();
        let running = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.0.lock().unwrap().insert(
            key.clone(),
            Entry {
                grant,
                revoked: revoked.clone(),
                yield_tx: Some(yield_tx),
                running: running.clone(),
            },
        );
        Job {
            jobs: self.clone(),
            key,
            revoked,
            yield_rx,
            running,
        }
    }

    pub(super) fn request_yield(
        &self,
        route: RouteChannel,
        call_key: &str,
        frame: Frame,
    ) -> Result<(), (Frame, call_yield::YieldAnswer)> {
        use call_yield::{NotYieldable, YieldAnswer};
        let mut jobs = self.0.lock().unwrap();
        let Some(entry) = jobs.get_mut(&(route, call_key.to_owned())) else {
            return Err((frame, YieldAnswer::UnknownCall));
        };
        if !entry.running.load(std::sync::atomic::Ordering::Acquire) {
            return Err((frame, YieldAnswer::NotYieldable(NotYieldable::NotWaiting)));
        }
        match entry.yield_tx.take() {
            Some(tx) => tx
                .send(frame)
                .map_err(|frame| (frame, YieldAnswer::UnknownCall)),
            None => Err((frame, YieldAnswer::UnknownCall)),
        }
    }

    pub(in crate::subc) fn revoke(&self, grant: &str) -> usize {
        self.0
            .lock()
            .unwrap()
            .values()
            .filter(|entry| {
                if entry.grant.as_deref() != Some(grant) || entry.revoked.is_cancelled() {
                    return false;
                }
                entry.revoked.cancel();
                entry.running.load(std::sync::atomic::Ordering::Acquire)
            })
            .count()
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.jobs.0.lock().unwrap().remove(&self.key);
    }
}
