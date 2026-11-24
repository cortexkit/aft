pub mod bash_background {
    use crate::bash_background::SpawnHost;
    use crate::bash_background::{self as engine, BashShell, HardKill, RemoteLaunch};
    use crate::bash_permissions::PermissionAsk;
    use crate::protocol::Response;
    use crate::sandbox_spawn::HostEscalationAttempt;
    use std::{collections::HashMap, path::PathBuf};

    pub fn spawn(
        request_id: &str,
        session_id: &str,
        command: &str,
        shell: BashShell,
        shell_path: PathBuf,
        workdir: Option<PathBuf>,
        env: Option<HashMap<String, String>>,
        hard_kill: HardKill,
        ctx: &dyn SpawnHost,
        require_background_flag: bool,
        notify_on_completion: bool,
        compressed: bool,
        pty: bool,
        pty_rows: u16,
        pty_cols: u16,
        scanner_report: Vec<PermissionAsk>,
        scanner_requires_approval: bool,
        host_escalation: Option<HostEscalationAttempt>,
        remote: Option<RemoteLaunch>,
    ) -> Response {
        engine::spawn(
            request_id,
            session_id,
            command,
            shell,
            shell_path,
            workdir,
            env,
            hard_kill,
            ctx,
            require_background_flag,
            notify_on_completion,
            compressed,
            pty,
            pty_rows,
            pty_cols,
            scanner_report,
            scanner_requires_approval,
            host_escalation,
            remote,
        )
    }

    pub mod registry {
        use crate::bash_background::registry as engine;

        pub fn ledger_spawn_for_test() -> Option<crate::db::call_ledger::Key> {
            engine::ledger_spawn_for_test()
        }

        pub fn install_kill_signal_gate_for_test(
            task_id: &str,
        ) -> (
            crossbeam_channel::Receiver<()>,
            crossbeam_channel::Sender<()>,
        ) {
            engine::install_kill_signal_gate_for_test(task_id)
        }

        pub use engine::test_support::{forbid_task_state_locks_for_test, ARTIFACT_PAGE_DELAY};

        #[cfg(unix)]
        pub mod root_reclaim_tests {
            pub use crate::bash_background::registry::root_reclaim_fixture::*;
        }

        #[cfg(unix)]
        pub mod remote {
            use super::super::super::super::BgTask;
            use crate::bash_background::persistence::PersistedTask;
            pub use crate::bash_background::registry::remote::remote_types::TaskSink;
            use crate::bash_background::registry::{self, remote as engine};
            use crate::exec_remote::{
                self,
                types::{RunRequest, TerminalRecord},
            };
            use std::path::{Path, PathBuf};

            pub const RETURN_PATHS_UNSUPPORTED_MESSAGE: &str =
                engine::RETURN_PATHS_UNSUPPORTED_MESSAGE;

            pub fn rendered_refusal_reason(reason: &str, detail: Option<&str>) -> String {
                engine::rendered_refusal_reason(reason, detail)
            }

            pub fn local_os_name() -> &'static str {
                engine::local_os_name()
            }

            pub fn remote_phase_note(metadata: &PersistedTask, now_ms: u64) -> Option<String> {
                engine::remote_phase_note(metadata, now_ms)
            }

            pub fn render_terminal_report(
                terminal: &TerminalRecord,
                copied: &[&str],
            ) -> Option<String> {
                engine::render_terminal_report(terminal, copied)
            }

            pub fn worktree_root_containing(
                path: &Path,
                kind: exec_remote::DirectoryKind,
            ) -> Result<Option<PathBuf>, exec_remote::Error> {
                engine::worktree_root_containing(path, kind)
            }

            pub fn remote_refusal(metadata: &PersistedTask) -> Option<registry::RemoteRefusal> {
                engine::remote_refusal(metadata)
            }

            pub fn restore_retry_request(
                task: &BgTask,
                expected: &str,
            ) -> Result<RunRequest, String> {
                engine::restore_retry_request(task, expected)
            }
        }
    }

    pub mod frames {
        pub fn registered() -> bool {
            crate::bash_background::frames::registered()
        }
    }

    pub mod persistence {
        pub fn terminal_task_metadata_for_test(
            pin: Option<&str>,
        ) -> crate::bash_background::persistence::PersistedTask {
            crate::bash_background::persistence::terminal_task_metadata_for_test(pin)
        }
    }

    pub mod secrets {
        pub const PROXY_KEYS: [&str; 4] = crate::bash_background::secrets::PROXY_KEYS;
        pub const REMOVE_KEYS: [&str; 4] = crate::bash_background::secrets::REMOVE_KEYS;
    }

    pub mod worker_thread {
        pub fn with_refusal<T>(
            worker: crate::bash_background::worker_thread::Worker,
            run: impl FnOnce() -> T,
        ) -> T {
            crate::bash_background::worker_thread::with_refusal(worker, run)
        }
    }
}

pub mod sandbox_spawn {
    #[cfg(target_os = "macos")]
    pub use super::super::sandbox::probe_command_for_plan;
    #[cfg(unix)]
    pub use crate::sandbox_spawn::launcher_fixture::install_sandbox_launcher_for_test;

    #[cfg(target_os = "linux")]
    pub mod remote {
        use crate::sandbox_spawn::SandboxHost;
        use std::path::{Path, PathBuf};

        pub fn prepare_with_launcher(
            ctx: &dyn SandboxHost,
            folder: &Path,
            workdir: PathBuf,
            command: String,
            launcher: PathBuf,
        ) -> Result<crate::sandbox_spawn::remote::StartForTest, String> {
            crate::sandbox_spawn::remote::prepare_with_launcher(
                ctx, folder, workdir, command, launcher,
            )
        }
    }

    pub use super::super::with_spawn_plan_for_test;
    #[cfg(unix)]
    pub use crate::sandbox_spawn::escalation_grant_type::EscalationGrantStore;

    #[cfg(target_os = "linux")]
    pub fn with_secret_ptrace_scope_for_test<R>(value: Option<&str>, run: impl FnOnce() -> R) -> R {
        crate::sandbox_spawn::with_secret_ptrace_scope_for_test(value, run)
    }
}

pub mod bash_parse {
    pub fn take_parse_calls() -> usize {
        crate::bash_parse::take_parse_calls()
    }
}

pub mod bash_rewrite {

    pub mod rules {
        pub fn ls_request(
            command: &str,
            ctx: &dyn crate::bash_rewrite::RewriteHost,
        ) -> Option<serde_json::Value> {
            crate::bash_rewrite::rules::ls_request(command, ctx)
        }
    }
}

#[cfg(unix)]
pub mod gh_shim_worker_broker {
    pub fn with_available_service_for_test<R>(
        root: &std::path::Path,
        run: impl FnOnce() -> R,
    ) -> R {
        crate::gh_shim_worker_broker::with_available_service_for_test(root, run)
    }

    pub fn without_service_for_test<R>(run: impl FnOnce() -> R) -> R {
        crate::gh_shim_worker_broker::without_service_for_test(run)
    }
}

pub mod gh_shim_speech {
    #[cfg(unix)]
    pub mod service {
        pub use crate::gh_shim_speech::service::RuntimeGuard;
    }
}

pub mod gh_shim_relay {
    pub mod bindings_contract {
        pub use crate::gh_shim_relay::bindings_fixture::{agent_reply, stand_in_reply};
    }

    #[cfg(all(debug_assertions, any(target_os = "linux", target_os = "macos")))]
    pub mod tests {
        pub use crate::gh_shim_relay::daemon_fixture::{fake_daemon_with_reply, FakeDaemon};
    }
}
