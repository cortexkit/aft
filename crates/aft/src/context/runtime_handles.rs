use super::*;

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
