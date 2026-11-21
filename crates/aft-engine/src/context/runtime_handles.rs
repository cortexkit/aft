use super::*;

impl App {
    #[doc(hidden)]
    pub fn set_subc_connection_file(&self, path: PathBuf) {
        *self.subc_connection_file.write() = Some(path);
    }

    #[doc(hidden)]
    pub fn subc_connection_file(&self) -> Option<PathBuf> {
        self.subc_connection_file.read().clone()
    }
}

pub(super) fn configure_background_scope(registry: &BgTaskRegistry, app: &App, config: &Config) {
    registry.configure_task_scope_runtime(
        app.subc_connection_file().is_some(),
        Duration::from_secs(config.bash.linux_scope_max_runtime_secs),
    );
}

impl AppContext {
    pub fn app(&self) -> Arc<App> {
        Arc::clone(&self.app)
    }

    pub fn is_daemon_mode(&self) -> bool {
        self.app().subc_connection_file().is_some()
    }

    /// Clone the LSP child registry handle. Used by main.rs to give the
    /// signal handler thread a way to SIGKILL LSP children on shutdown.
    pub fn lsp_child_registry(&self) -> crate::lsp::child_registry::LspChildRegistry {
        self.app.lsp_child_registry()
    }

    pub fn stdout_writer(&self) -> SharedStdoutWriter {
        self.app.stdout_writer()
    }
}
