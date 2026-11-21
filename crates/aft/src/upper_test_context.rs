use std::sync::Once;

use crate::{config::Config, context::AppContext, language::LanguageProvider};

pub(crate) fn test_context(provider: Box<dyn LanguageProvider>, config: Config) -> AppContext {
    // These handlers belong to the upper layer. Engine fixtures can build a
    // context without installing them.
    static REGISTER_SHELL_HANDLERS: Once = Once::new();
    REGISTER_SHELL_HANDLERS.call_once(crate::commands::register_shell_handlers);
    AppContext::new(provider, config)
}
