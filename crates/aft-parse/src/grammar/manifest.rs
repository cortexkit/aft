use std::{collections::HashMap, path::Path, sync::LazyLock};

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct GrammarManifest {
    pub language: String,
    pub extensions: Vec<String>,
    pub grammar_repository: String,
    pub grammar_commit: String,
    pub abi: usize,
    pub license: String,
    pub sha256: String,
    pub size: u64,
    pub url: String,
}

#[derive(Deserialize)]
struct ManifestList {
    grammars: Vec<GrammarManifest>,
}

impl ManifestList {
    fn load(json: &[u8], production: bool) -> Result<Self, String> {
        let list: Self = serde_json::from_slice(json).map_err(|error| error.to_string())?;
        list.validate(production)?;
        Ok(list)
    }

    fn validate(&self, production: bool) -> Result<(), String> {
        let mut languages = std::collections::HashSet::new();
        let mut extensions = HashMap::new();
        for grammar in &self.grammars {
            let language = grammar.language.as_str();
            if !languages.insert(language) {
                return Err(format!("duplicate grammar language: {language}"));
            }
            if production && super::extractor_language(language).is_none() {
                return Err(format!("no extractor for grammar: {language}"));
            }
            for extension in &grammar.extensions {
                if crate::lang_id::native_language_by_extension(extension).is_some() {
                    return Err(format!("grammar extension is already linked: {extension}"));
                }
                if let Some(other) = extensions.insert(extension, language) {
                    if other != language {
                        return Err(format!("duplicate grammar extension: {extension}"));
                    }
                }
            }
        }
        Ok(())
    }
}

static PRODUCTION_MANIFEST: LazyLock<ManifestList> = LazyLock::new(|| {
    ManifestList::load(include_bytes!("manifest.json"), true).expect("compiled grammar manifest")
});

pub fn manifests() -> &'static [GrammarManifest] {
    #[cfg(feature = "test-hooks")]
    {
        static OVERRIDE: LazyLock<Option<ManifestList>> = LazyLock::new(|| {
            let path = std::env::var_os("AFT_TEST_GRAMMAR_MANIFEST")?;
            Some(
                ManifestList::load(&std::fs::read(path).expect("test grammar manifest"), false)
                    .expect("valid test grammar manifest"),
            )
        });
        if let Some(value) = OVERRIDE.as_ref() {
            return &value.grammars;
        }
    }
    &PRODUCTION_MANIFEST.grammars
}

pub fn manifest(language: &str) -> Option<&'static GrammarManifest> {
    manifests().iter().find(|entry| entry.language == language)
}

#[cfg(feature = "test-support")]
pub fn production_manifest(language: &str) -> Option<&'static GrammarManifest> {
    PRODUCTION_MANIFEST
        .grammars
        .iter()
        .find(|entry| entry.language == language)
}

/// Resolve a downloaded grammar from the file name without opening the file.
pub fn manifest_for_path(path: &Path) -> Option<&'static GrammarManifest> {
    let extension = path.extension()?.to_str()?;
    manifests()
        .iter()
        .find(|entry| entry.extensions.iter().any(|ext| ext == extension))
}

/// Initialize the test manifest's hash/URL and the Luau extractor version in
/// their process-wide LazyLocks. The binary calls this before handling any
/// requests, so later environment changes cannot change its grammar identity.
/// Production has no environment-controlled manifest or extractor version.
#[cfg(feature = "test-hooks")]
pub fn initialize_test_identity() {
    for entry in manifests() {
        let _ = super::extractor_version(&entry.language);
    }
}

#[cfg(test)]
mod tests;
