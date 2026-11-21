use super::*;
use crate::config::Config;
use crate::context::default_language_provider_factory;

pub fn watcher_context_with(
    root: &Path,
    make_context: impl FnOnce(Box<dyn crate::language::LanguageProvider>, Config) -> AppContext,
) -> (AppContext, crossbeam_channel::Sender<WatcherDispatchEvent>) {
    let ctx = make_context(default_language_provider_factory(), Config::default());
    ctx.update_config(|config| {
        config.project_root = Some(root.to_path_buf());
    });
    ctx.set_canonical_cache_root(root.to_path_buf());
    let (tx, rx) = crossbeam_channel::unbounded();
    *ctx.watcher_rx().lock() = Some(rx);
    (ctx, tx)
}
