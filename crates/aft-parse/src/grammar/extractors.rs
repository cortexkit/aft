use crate::{lang_id::LangId, luau};

struct Extractor {
    language: &'static str,
    id: LangId,
    version: fn() -> u32,
    parse_deadline: fn() -> std::time::Duration,
    #[cfg(feature = "wasm-grammars")]
    queries: &'static [&'static str],
}

// To add a downloaded language, add its LangId and extractor, then register
// them here under the language name used in the manifest.
const EXTRACTORS: &[Extractor] = &[Extractor {
    language: "luau",
    id: LangId::Luau,
    version: luau::extractor_version,
    parse_deadline: crate::parser::luau_parse_deadline,
    #[cfg(feature = "wasm-grammars")]
    queries: &[
        luau::EXTRACTION_QUERY,
        luau::COMPLEXITY_QUERY,
        luau::COMMENT_QUERY,
    ],
}];

fn extractor(language: &str) -> Option<&'static Extractor> {
    EXTRACTORS.iter().find(|entry| entry.language == language)
}

pub fn extractor_language(language: &str) -> Option<LangId> {
    extractor(language).map(|entry| entry.id)
}

pub fn extractor_name(id: LangId) -> Option<&'static str> {
    EXTRACTORS
        .iter()
        .find(|entry| entry.id == id)
        .map(|entry| entry.language)
}

pub fn extractor_version(language: &str) -> Option<u32> {
    extractor(language).map(|entry| (entry.version)())
}

pub fn parse_deadline(language: &str) -> Option<std::time::Duration> {
    extractor(language).map(|entry| (entry.parse_deadline)())
}

#[cfg(feature = "wasm-grammars")]
pub(super) fn queries(language: &str) -> &'static [&'static str] {
    extractor(language).map_or(&[], |entry| entry.queries)
}
