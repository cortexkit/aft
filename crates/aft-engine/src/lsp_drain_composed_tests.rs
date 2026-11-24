use std::sync::Arc;

use aft_inspect::inspect::diagnostics_host::DiagnosticsHost;

use crate::context::{default_language_provider_factory, AppContext};
use crate::lsp::manager::instance_regression_tests as checks;

fn context() -> Arc<AppContext> {
    Arc::new(AppContext::new(
        default_language_provider_factory(),
        Default::default(),
    ))
}

#[test]
fn drain_dispatch_rechecks_instance_after_restart() {
    let ctx = context();
    checks::drain_dispatch_rechecks_instance_after_restart(&ctx.lsp_manager(), move |limit| {
        let result = crate::runtime_drain::drain_lsp_events_bounded(&ctx, limit);
        (result.processed, result.has_more)
    });
}

#[test]
fn drain_dispatch_does_not_wait_for_a_contended_manager() {
    let ctx = context();
    checks::drain_dispatch_does_not_wait_for_a_contended_manager(
        &ctx.lsp_manager(),
        move |limit| {
            let result = crate::runtime_drain::drain_lsp_events_bounded(&ctx, limit);
            (result.processed, result.has_more)
        },
    );
}
