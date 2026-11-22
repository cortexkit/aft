use super::socket;
use aft_base::config::{Config, SandboxMode};
use aft_engine::bash_background::{persistence::create_task_layout, BgTaskRegistry, HardKill};
use aft_engine::context::AppContext;
use aft_engine::parser::TreeSitterProvider;
use aft_engine::sandbox_spawn::{
    resolve_sandbox_spawn, with_worker_jail, AuthenticatedPrincipal, RequestedSandboxTier,
    SandboxTaskKind, SpawnPlan,
};
use aft_engine::test_bridge::bash_remote::sandbox_spawn::install_sandbox_launcher_for_test;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const SESSION: &str = "sandbox-acceptance";

pub(super) struct Shell {
    ctx: AppContext,
    project: PathBuf,
    storage: PathBuf,
    account: PathBuf,
    worker: bool,
}

impl Shell {
    pub(super) fn new(root: &std::path::Path, worker: bool) -> Self {
        let project = root.join("project");
        let storage = root.join("storage");
        let account = root.join("account");
        fs::create_dir(&storage).unwrap();
        fs::create_dir(&account).unwrap();
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project.clone()),
                storage_dir: Some(storage.clone()),
                ..Config::default()
            },
        );
        ctx.update_config(|config| {
            config.github.shim = false;
            config.git.co_author = "off".into();
            config.sandbox.read_deny = vec![root.join("private-notes")];
        });
        Self {
            ctx,
            project,
            storage,
            account,
            worker,
        }
    }

    pub(super) fn run(&self, mode: SandboxMode, script: &str, check_call_socket: bool) -> String {
        // A distinct level on the other role makes a wrong selection observable.
        let other = if mode == SandboxMode::Off {
            SandboxMode::Standard
        } else {
            SandboxMode::Off
        };
        self.ctx.update_config(|config| {
            (config.sandbox.profile.head, config.sandbox.profile.worker) = if self.worker {
                (other, mode)
            } else {
                (mode, other)
            };
        });
        let _launcher =
            install_sandbox_launcher_for_test(&self.project, env!("CARGO_BIN_EXE_aft").into());
        let task = create_task_layout(&self.storage, SESSION, &format!("bash-{:016x}", mode as u8))
            .unwrap();
        let registry = BgTaskRegistry::new(Default::default());
        with_worker_jail(self.worker, || {
            let canary_reads =
                usize::from(matches!(mode, SandboxMode::Standard | SandboxMode::Strict));
            // Use the production canary preparation and verification on a
            // disposable account-home path, not the runner's own canary.
            let plan = aft_base::worker_jail_canary::with_test_preparation_home(
                &self.account,
                canary_reads,
                || {
                    resolve_sandbox_spawn(
                        &self.ctx,
                        &AuthenticatedPrincipal::FirstParty,
                        RequestedSandboxTier::Native,
                        SandboxTaskKind::BashForeground,
                        &task.paths.io_dir,
                        None,
                    )
                },
            );
            match &plan {
                SpawnPlan::Unsandboxed if mode == SandboxMode::Off && !self.worker => {}
                SpawnPlan::Launcher { profile, .. } => {
                    assert_eq!(profile.mode, mode);
                    if mode != SandboxMode::Off {
                        assert_eq!(profile.worker_jail, self.worker);
                    }
                }
                _ => panic!(
                    "worker={} {mode}: unexpected spawn plan: {plan:?}",
                    self.worker
                ),
            }
            let mut script = script.to_owned();
            let call = check_call_socket.then(|| {
                let call = socket::Call::bind(plan.temp_dir().expect("native task scratch"));
                script.push_str(&call.command());
                call
            });
            let id = registry
                .spawn(
                    plan,
                    &script,
                    SESSION.into(),
                    self.project.clone(),
                    Default::default(),
                    HardKill::After(Duration::from_secs(45)),
                    None,
                    self.storage.clone(),
                    1,
                    false,
                    false,
                    Some(self.project.clone()),
                )
                .unwrap_or_else(|error| {
                    panic!("worker={} {mode}: launch failed: {error}", self.worker)
                });
            let deadline = Instant::now() + Duration::from_secs(50);
            let snapshot = loop {
                let snapshot = registry
                    .status(&id, SESSION, Some(&self.project), Some(&self.storage), 0)
                    .expect("observe acceptance task");
                if snapshot.info.status.is_terminal() {
                    break snapshot;
                }
                assert!(
                    Instant::now() < deadline,
                    "native shell did not finish: {snapshot:?}"
                );
                std::thread::sleep(Duration::from_millis(15));
            };
            let stdout = snapshot
                .output_path
                .as_ref()
                .map(fs::read_to_string)
                .transpose()
                .unwrap()
                .unwrap_or_default();
            let stderr = snapshot
                .stderr_path
                .as_ref()
                .map(fs::read_to_string)
                .transpose()
                .unwrap()
                .unwrap_or_default();
            let transcript = format!("{stdout}\n{stderr}");
            assert!(
                snapshot.exit_code == Some(0)
                    && !snapshot.sandbox_unavailable
                    && !snapshot.output_incomplete,
                "worker={} {mode}: shell did not complete: {snapshot:?}\n{transcript}",
                self.worker
            );
            #[cfg(target_os = "linux")]
            if mode != SandboxMode::Off {
                let backend = if mode == SandboxMode::Strict {
                    "bwrap+landlock"
                } else {
                    "bwrap"
                };
                assert!(transcript.lines().any(|line| line.starts_with(&format!("sandbox-launch: profile={mode} backend={backend} "))),
                    "worker={} {mode}: required active {backend} backend not reported:\n{transcript}", self.worker);
            }
            if let Some(call) = call {
                call.finish();
            }
            transcript
        })
    }
}
