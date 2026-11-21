pub use aft_parse::grammar::disabled::*;

// Shared import scope for the unit tests below.
#[cfg(test)]
#[allow(unused_imports)]
use super::{manifest, GrammarState, GrammarUnavailable};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::{Arc, LazyLock};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar_disabled_message_is_owner_approved() {
        assert_eq!(crate::grammar::state::SUPPORT_DISABLED, "This AFT build has no downloaded-grammar support, so <language> files can't be parsed. Use a released AFT build, or rebuild with cargo install agent-file-tools --features wasm-grammars (needs CMake 3.18 or later).");
        assert_eq!(unavailable("luau").message, "This AFT build has no downloaded-grammar support, so Luau files can't be parsed. Use a released AFT build, or rebuild with cargo install agent-file-tools --features wasm-grammars (needs CMake 3.18 or later).");
    }

    #[test]
    fn grammar_disabled_build_refuses_without_touching_storage() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("never-created");
        configure(&crate::config::Config {
            storage_dir: Some(storage.clone()),
            ..Default::default()
        });
        let registry = registry("luau").unwrap();
        for error in [
            registry.stored().unwrap_err(),
            registry.request().unwrap_err(),
            prepare_parser(&mut tree_sitter::Parser::new(), "luau").unwrap_err(),
        ] {
            assert_eq!(error.state, GrammarState::SupportDisabled);
            assert_eq!(error.cause, None);
            assert_eq!(error.retry_after_ms, None);
            let value = crate::error::AftError::from(error).to_error_json();
            assert_eq!(value["code"], "grammar_unavailable");
            assert_eq!(value["state"], "support_disabled");
        }
        assert!(!storage.exists());
        assert_eq!(registry.compile_ms(), 0.0);
        assert!(registry.subscribe().recv().is_err());
    }
}
