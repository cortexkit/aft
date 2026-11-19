use super::*;

/// Resolve the extractor for a downloaded grammar without opening the file.
pub fn dynamic_language_for_extension(path: &Path, dynamic_languages: bool) -> Option<LangId> {
    if !dynamic_languages {
        return None;
    }
    crate::grammar::extractor_language(&crate::grammar::manifest_for_path(path)?.language)
}
