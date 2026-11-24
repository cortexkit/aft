#[cfg(unix)]
pub trait BusyRootTestExt {
    fn ctx(&self) -> &Arc<crate::context::AppContext>;
}

#[cfg(unix)]
impl BusyRootTestExt for crate::shell_test_fixtures::push_projection::BusyRoot {
    fn ctx(&self) -> &Arc<crate::context::AppContext> {
        &self.ctx
    }
}
use std::sync::Arc;
