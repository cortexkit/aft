use std::sync::Once;

use crate::{config::Config, context::AppContext, language::LanguageProvider};

/// Construct an application with isolated test storage.
pub fn test_app() -> std::sync::Arc<crate::context::App> {
    aft_engine::test_bridge::test_storage::with_isolated_context(
        crate::context::App::default_shared,
    )
}

/// Construct an isolated handler context and install the shell handlers it may use.
pub fn test_context(provider: Box<dyn LanguageProvider>, config: Config) -> AppContext {
    // These handlers belong to the upper layer. Engine fixtures can build a
    // context without installing them.
    static REGISTER_SHELL_HANDLERS: Once = Once::new();
    REGISTER_SHELL_HANDLERS.call_once(crate::commands::register_shell_handlers);
    aft_engine::test_bridge::test_storage::with_isolated_context(|| {
        AppContext::new(provider, config).with_test_checkpoint_store()
    })
}

#[test]
fn registering_helper_uses_private_temporary_storage() {
    let ctx = test_context(
        crate::context::default_language_provider_factory(),
        Config::default(),
    );
    let config = ctx.config();
    let storage = config.storage_dir.as_ref().expect("test storage default");
    assert!(storage.is_dir());
    assert!(storage
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("aft-test-context-"));
    crate::test_storage::assert_context(&config);
}
