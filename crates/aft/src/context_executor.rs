//! Runtime implementation of the scheduler's context boundary.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{App, AppContext, ConfigPinGuard};
use crate::executor::{
    ExecutorApp, ExecutorContext, RootMaintenanceExecutor, SubcLifecycleAdmission,
};

impl ExecutorContext for AppContext {
    type App = App;
    type ConfigPin = ConfigPinGuard;

    fn bind_status_context(self: &Arc<Self>) {
        AppContext::bind_status_context(self)
    }
    fn install_callgraph_executor(&self, executor: RootMaintenanceExecutor) {
        AppContext::install_callgraph_executor(self, executor)
    }
    fn app(&self) -> Arc<App> {
        AppContext::app(self)
    }
    fn storage_dir(&self) -> PathBuf {
        AppContext::storage_dir(self)
    }
    fn teardown_deleted_root(&self) {
        AppContext::teardown_deleted_root(self)
    }
    fn subc_unbound_quiesced(&self) -> bool {
        AppContext::subc_unbound_quiesced(self)
    }
    fn subc_lifecycle_admission(&self) -> SubcLifecycleAdmission {
        AppContext::subc_lifecycle_admission(self)
    }
    fn run_if_subc_bound_generation<R>(
        &self,
        generation: u64,
        run: impl FnOnce() -> R,
    ) -> Option<R> {
        AppContext::run_if_subc_bound_generation(self, generation, run)
    }
    fn configure_generation(&self) -> u64 {
        AppContext::configure_generation(self)
    }
    fn begin_configure_ack_phase(&self, phase: &'static str) {
        AppContext::begin_configure_ack_phase(self, phase)
    }
    fn configure_ack_phase_snapshot(&self) -> String {
        AppContext::configure_ack_phase_snapshot(self)
    }
    fn configure_tail_stage_snapshot(&self) -> Option<String> {
        AppContext::configure_tail_stage_snapshot(self)
    }
    fn pin_config(&self) -> ConfigPinGuard {
        AppContext::pin_config(self)
    }
}

impl ExecutorApp<AppContext> for App {
    fn register_memory_context(&self, root: PathBuf, ctx: &Arc<AppContext>) {
        App::register_memory_context(self, root, ctx)
    }
    fn unregister_memory_context(&self, root: &Path, ctx: &Arc<AppContext>) {
        App::unregister_memory_context(self, root, ctx)
    }
    fn actor_root_registered(&self) {
        App::actor_root_registered(self);
        aft_base::scope_watch_events::context_changed();
    }
    fn actor_root_unregistered(&self) {
        App::actor_root_unregistered(self);
        aft_base::scope_watch_events::context_changed();
    }
}
