use std::sync::Arc;

use crate::context::{App, AppContext};

/// Standalone and subc contexts share the same owned handle so deferred work
/// can outlive the synchronous dispatch stack without borrowing the runtime.
pub type ProjectRuntime = Arc<AppContext>;

pub struct RuntimeRegistry {
    app: Arc<App>,
    single: ProjectRuntime,
}

impl RuntimeRegistry {
    pub fn standalone(app: Arc<App>, rt: ProjectRuntime) -> Self {
        rt.bind_status_context();
        Self { app, single: rt }
    }

    pub fn app(&self) -> Arc<App> {
        Arc::clone(&self.app)
    }

    pub fn current(&self) -> &ProjectRuntime {
        &self.single
    }

    pub fn current_mut(&mut self) -> &mut ProjectRuntime {
        &mut self.single
    }

    pub fn iter(&self) -> impl Iterator<Item = &ProjectRuntime> {
        std::iter::once(&self.single)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, parser::TreeSitterProvider};

    #[test]
    fn standalone_current_and_iter_return_single_runtime() {
        let ctx = Arc::new(AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config::default(),
        ));
        let app = ctx.app();
        let mut registry = RuntimeRegistry::standalone(Arc::clone(&app), ctx);
        assert!(Arc::ptr_eq(&app, &registry.app()));
        assert!(Arc::ptr_eq(&app, &registry.current().app()));

        let current_ptr = Arc::as_ptr(registry.current());
        let iter_ptrs = registry.iter().map(Arc::as_ptr).collect::<Vec<_>>();
        assert_eq!(iter_ptrs, vec![current_ptr]);

        let current_mut_ptr = Arc::as_ptr(registry.current_mut());
        assert_eq!(current_mut_ptr, current_ptr);
    }
}
