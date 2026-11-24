use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    access_policy::AccessPolicy,
    bash_background::BgTaskRegistry,
    config::Config,
    context::AppContext,
    harness::Harness,
    protocol::{RawRequest, Response},
};
use aft_shell::{
    bash_background::{SpawnHost, TaskRegistryAccess},
    bash_changed_files::Capture,
    bash_permissions::PermissionScanHost,
    bash_rewrite::RewriteHost,
    exec_remote::SiblingReadPolicy,
    output_store::{OutputFiles, OutputStoreHost},
    pinned_config::PinnedConfig,
    sandbox_spawn::{SandboxHost, SpawnPlan},
};

impl PinnedConfig for AppContext {
    fn config(&self) -> Arc<Config> {
        AppContext::config(self)
    }
}

impl TaskRegistryAccess for AppContext {
    fn bash_background(&self) -> &BgTaskRegistry {
        AppContext::bash_background(self)
    }
}

#[cfg(target_os = "linux")]
impl aft_shell::sandbox_spawn::ConfigPin for crate::context::ConfigPinGuard {}

impl SandboxHost for AppContext {
    fn access_policy(&self, id: &str) -> Result<AccessPolicy, Response> {
        AppContext::access_policy(self, id)
    }

    fn storage_dir(&self) -> PathBuf {
        AppContext::storage_dir(self)
    }

    #[cfg(unix)]
    fn escalation_grants(&self) -> &Arc<aft_shell::sandbox_spawn::EscalationGrantState> {
        AppContext::escalation_grants(self)
    }

    #[cfg(target_os = "linux")]
    fn pin_config(&self) -> Box<dyn aft_shell::sandbox_spawn::ConfigPin + '_> {
        Box::new(AppContext::pin_config(self))
    }
}

impl SpawnHost for AppContext {
    fn is_daemon_mode(&self) -> bool {
        AppContext::is_daemon_mode(self)
    }

    fn reload_released_filter_registry(&self) {
        AppContext::reload_released_filter_registry(self);
    }

    fn start_changed_files(&self, id: &str, plan: &SpawnPlan) -> Option<Arc<Capture>> {
        crate::bash_changed_files::start_for_spawn(self, id, plan)
    }

    fn check_spawn_folder_revocation(&self, task: &str, session: &str) {
        self.bash_background()
            .stop_if_folder_grant_revoked(task, session, self.db());
    }
}

impl OutputStoreHost for AppContext {
    fn storage_dir(&self) -> PathBuf {
        AppContext::storage_dir(self)
    }

    fn harness_opt(&self) -> Option<Harness> {
        AppContext::harness_opt(self)
    }

    fn output_files_arc(&self) -> Arc<OutputFiles> {
        AppContext::output_files_arc(self)
    }
}

impl PermissionScanHost for AppContext {}

impl SiblingReadPolicy for AppContext {
    fn validate_read_path_without_approval(
        &self,
        id: &str,
        session: &str,
        path: &Path,
    ) -> Result<PathBuf, Response> {
        AppContext::validate_read_path_without_approval(self, id, session, path)
    }
}

impl RewriteHost for AppContext {
    fn run_rewritten(&self, request: &RawRequest) -> Response {
        crate::bash_rewrite::dispatch::run(request, self)
    }

    fn cache_root(&self) -> Option<PathBuf> {
        self.canonical_cache_root_opt()
    }

    fn hashline_effective(&self, root: PathBuf, session: &str) -> bool {
        self.hashline_bindings()
            .capture(root, session)
            .is_some_and(|binding| binding.effective())
    }

    fn ignores_directory(&self, path: &Path) -> bool {
        self.gitignore()
            .is_some_and(|matcher| matcher.matched_path_or_any_parents(path, true).is_ignore())
    }
}
