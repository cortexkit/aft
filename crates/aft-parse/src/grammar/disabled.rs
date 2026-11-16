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

pub fn engine_target() -> Result<&'static str, GrammarUnavailable> {
    Err(unavailable())
}
