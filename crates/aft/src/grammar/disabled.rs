use std::sync::{Arc, LazyLock};

use super::{manifest, GrammarState, GrammarUnavailable};

pub struct GrammarRegistry;

static REGISTRY: LazyLock<Arc<GrammarRegistry>> = LazyLock::new(|| Arc::new(GrammarRegistry));

pub fn registry() -> &'static Arc<GrammarRegistry> {
    &REGISTRY
}

/// A disabled build must not resolve, create or inspect grammar storage.
pub fn configure(_config: &crate::config::Config) -> Arc<GrammarRegistry> {
    Arc::clone(registry())
}

fn unavailable() -> GrammarUnavailable {
    GrammarUnavailable::new(&manifest().language, GrammarState::SupportDisabled)
}

impl GrammarRegistry {
    pub fn stored(&self) -> Result<tree_sitter::Language, GrammarUnavailable> {
        Err(unavailable())
    }

    pub fn request(self: &Arc<Self>) -> Result<tree_sitter::Language, GrammarUnavailable> {
        Err(unavailable())
    }

    pub fn identity(&self) -> &str {
        &manifest().sha256
    }

    pub fn compile_ms(&self) -> f64 {
        0.0
    }

    pub fn subscribe(&self) -> std::sync::mpsc::Receiver<String> {
        let (_, receiver) = std::sync::mpsc::channel();
        receiver
    }
}

pub fn prepare_parser(_parser: &mut tree_sitter::Parser) -> Result<(), GrammarUnavailable> {
    Err(unavailable())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar_disabled_message_is_owner_approved() {
        assert_eq!(crate::grammar::state::SUPPORT_DISABLED, "This AFT build has no downloaded-grammar support, so <language> files can't be parsed. Use a released AFT build, or rebuild with cargo install agent-file-tools --features wasm-grammars (needs CMake 3.12 or later).");
        assert_eq!(unavailable().message, "This AFT build has no downloaded-grammar support, so Luau files can't be parsed. Use a released AFT build, or rebuild with cargo install agent-file-tools --features wasm-grammars (needs CMake 3.12 or later).");
    }

    #[test]
    fn grammar_disabled_build_refuses_without_touching_storage() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("never-created");
        let registry = configure(&crate::config::Config {
            storage_dir: Some(storage.clone()),
            ..Default::default()
        });
        for error in [
            registry.stored().unwrap_err(),
            registry.request().unwrap_err(),
            prepare_parser(&mut tree_sitter::Parser::new()).unwrap_err(),
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
