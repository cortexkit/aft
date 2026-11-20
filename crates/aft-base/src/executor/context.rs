//! Runtime operations the scheduler needs from its actor context.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{RootMaintenanceExecutor, SubcLifecycleAdmission};

/// Process census callbacks used when an executor adds or removes a root.
pub trait ExecutorApp<C: ExecutorContext>: Send + Sync + 'static {
    fn register_memory_context(&self, root: PathBuf, ctx: &Arc<C>);
    fn unregister_memory_context(&self, root: &Path, ctx: &Arc<C>);
    fn actor_root_registered(&self);
    fn actor_root_unregistered(&self);
}

/// The context operations already performed at scheduler admission and execution.
/// Associated types let each runtime define the application handle and config-pin guard it uses.
pub trait ExecutorContext: Send + Sync + Sized + 'static {
    type App: ExecutorApp<Self>;
    type ConfigPin;

    fn bind_status_context(self: &Arc<Self>);
    fn install_callgraph_executor(&self, executor: RootMaintenanceExecutor);
    fn app(&self) -> Arc<Self::App>;
    fn storage_dir(&self) -> PathBuf;
    fn teardown_deleted_root(&self);
    fn subc_unbound_quiesced(&self) -> bool;
    fn subc_lifecycle_admission(&self) -> SubcLifecycleAdmission;
    fn run_if_subc_bound_generation<R>(
        &self,
        generation: u64,
        run: impl FnOnce() -> R,
    ) -> Option<R>;
    fn configure_generation(&self) -> u64;
    fn begin_configure_ack_phase(&self, phase: &'static str);
    fn configure_ack_phase_snapshot(&self) -> String;
    fn configure_tail_stage_snapshot(&self) -> Option<String>;
    fn pin_config(&self) -> Self::ConfigPin;
}
