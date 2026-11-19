use std::sync::{Arc, LazyLock};

use super::{manifests, GrammarManifest, GrammarState, GrammarUnavailable};

pub struct GrammarRegistry(&'static GrammarManifest);

static REGISTRIES: LazyLock<super::RegistryMap<GrammarRegistry>> =
    LazyLock::new(|| super::registry_map(manifests(), |entry| Arc::new(GrammarRegistry(entry))));

pub fn registry(language: &str) -> Option<&'static Arc<GrammarRegistry>> {
    REGISTRIES.get(language)
}

/// A disabled build must not resolve, create or inspect grammar storage.
pub fn configure(_config: &crate::config::Config) {}

test_support_visibility! {
fn unavailable(language: &str) -> GrammarUnavailable {
    GrammarUnavailable::new(language, GrammarState::SupportDisabled)
}
}

impl GrammarRegistry {
    pub fn stored(&self) -> Result<tree_sitter::Language, GrammarUnavailable> {
        Err(unavailable(&self.0.language))
    }

    pub fn request(self: &Arc<Self>) -> Result<tree_sitter::Language, GrammarUnavailable> {
        Err(unavailable(&self.0.language))
    }

    pub fn identity(&self) -> &str {
        &self.0.sha256
    }

    pub fn compile_ms(&self) -> f64 {
        0.0
    }

    pub fn subscribe(&self) -> std::sync::mpsc::Receiver<String> {
        let (_, receiver) = std::sync::mpsc::channel();
        receiver
    }
}

pub fn prepare_parser(_parser: &mut tree_sitter::Parser, language: &str) -> Result<(), GrammarUnavailable> {
    Err(unavailable(language))
}

pub fn engine_target(language: &str) -> Result<&'static str, GrammarUnavailable> {
    Err(unavailable(language))
}
