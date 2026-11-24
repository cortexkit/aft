use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aft_inspect::inspect::diagnostics_host::DiagnosticsHost;
use aft_inspect::lsp::test_support::idle_fixture::IdleLspFixture as Setup;

use crate::context::{default_language_provider_factory, AppContext};

pub struct IdleLspFixture {
    pub(crate) ctx: Arc<AppContext>,
    setup: Setup,
}

impl IdleLspFixture {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::with_server_binary(crate::fake_lsp_test_helper::fake_server_binary)
    }

    pub fn with_server_binary(server_binary: impl FnOnce() -> PathBuf) -> Self {
        let mut ctx = None;
        let setup = Setup::with_manager(server_binary, |config| {
            let app = Arc::new(AppContext::new(
                default_language_provider_factory(),
                config.clone(),
            ));
            let manager = app.lsp_manager();
            ctx = Some(app);
            manager
        });
        let ctx = ctx.expect("the LSP fixture initializes its context");
        ctx.note_request();
        Self { ctx, setup }
    }

    pub fn hover(&self, file: &Path) {
        self.ctx.note_request();
        self.setup.hover(file);
    }
}

impl Deref for IdleLspFixture {
    type Target = Setup;

    fn deref(&self) -> &Self::Target {
        &self.setup
    }
}
