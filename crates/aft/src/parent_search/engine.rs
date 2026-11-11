use super::{Child, ParentSession};
use crate::context::AppContext;

/// One child's search engine context and what is installed in it.
pub(crate) struct ChildEngine {
    pub(crate) ctx: AppContext,
}

impl ChildEngine {
    pub(crate) fn new(session: &ParentSession, child: &Child) -> Self {
        let mut config = crate::config::Config::default();
        config.project_root = Some(child.root.clone());
        // The engine reads the indexes installed below, never a view of its own.
        config.indexes.trigram = session.planes.trigram;
        config.indexes.semantic = session.planes.semantic;
        config.indexes.callgraph = false;
        config.semantic = session.semantic_config.clone();
        Self {
            ctx: AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config),
        }
    }
}
