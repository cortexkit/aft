use super::BgTaskRegistry;
use crate::{
    bash_changed_files::Capture,
    sandbox_spawn::{SandboxHost, SpawnPlan},
};
use std::sync::Arc;

/// The task registry used by shell launches, artifact scans, and progress.
#[doc(hidden)]
pub trait TaskRegistryAccess {
    fn bash_background(&self) -> &BgTaskRegistry;
}

impl<T: TaskRegistryAccess + ?Sized> TaskRegistryAccess for Arc<T> {
    fn bash_background(&self) -> &BgTaskRegistry {
        (**self).bash_background()
    }
}

/// The composition operations at the boundaries of preparing and launching a task.
#[doc(hidden)]
pub trait SpawnHost: SandboxHost + TaskRegistryAccess {
    fn is_daemon_mode(&self) -> bool;
    fn reload_released_filter_registry(&self);
    fn start_changed_files(&self, id: &str, plan: &SpawnPlan) -> Option<Arc<Capture>>;
    fn check_spawn_folder_revocation(&self, task: &str, session: &str);
}

impl<T: SpawnHost + Send + ?Sized> SpawnHost for Arc<T> {
    fn is_daemon_mode(&self) -> bool {
        (**self).is_daemon_mode()
    }

    fn reload_released_filter_registry(&self) {
        (**self).reload_released_filter_registry();
    }

    fn start_changed_files(&self, id: &str, plan: &SpawnPlan) -> Option<Arc<Capture>> {
        (**self).start_changed_files(id, plan)
    }

    fn check_spawn_folder_revocation(&self, task: &str, session: &str) {
        (**self).check_spawn_folder_revocation(task, session);
    }
}
