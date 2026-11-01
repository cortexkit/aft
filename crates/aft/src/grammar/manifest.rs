use std::sync::LazyLock;

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

pub static PRODUCTION_MANIFEST: LazyLock<GrammarManifest> = LazyLock::new(|| {
    serde_json::from_str(include_str!("manifest.json")).expect("compiled grammar manifest")
});

pub const LUAU_EXTRACTOR_VERSION: u32 = 1;

pub fn manifest() -> &'static GrammarManifest {
    #[cfg(feature = "test-grammar-seams")]
    {
        static OVERRIDE: LazyLock<Option<GrammarManifest>> = LazyLock::new(|| {
            let path = std::env::var_os("AFT_TEST_GRAMMAR_MANIFEST")?;
            Some(
                serde_json::from_slice(&std::fs::read(path).expect("test grammar manifest"))
                    .expect("test grammar manifest JSON"),
            )
        });
        if let Some(value) = OVERRIDE.as_ref() {
            return value;
        }
    }
    &PRODUCTION_MANIFEST
}

pub fn extractor_version() -> u32 {
    #[cfg(feature = "test-grammar-seams")]
    {
        static VERSION: LazyLock<u32> = LazyLock::new(|| {
            std::env::var("AFT_TEST_GRAMMAR_EXTRACTOR_VERSION")
                .ok()
                .map(|v| v.parse().expect("test extractor version"))
                .unwrap_or(LUAU_EXTRACTOR_VERSION)
        });
        return *VERSION;
    }
    #[cfg(not(feature = "test-grammar-seams"))]
    LUAU_EXTRACTOR_VERSION
}

/// Initialize the test manifest's hash/URL and the Luau extractor version in
/// their process-wide LazyLocks. The binary calls this before handling any
/// requests, so later environment changes cannot change its grammar identity.
/// Production has no environment-controlled manifest or extractor version.
#[cfg(feature = "test-grammar-seams")]
pub fn initialize_test_identity() {
    let _ = manifest();
    let _ = extractor_version();
}
