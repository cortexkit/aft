use super::*;

/// Whether `path` is a Luau file, decided from its name alone. It gives the
/// same answer as `detect_language_any(..) == Some(LangId::Luau)` but never
/// opens the file: `detect_language` reads and parses every `.h` header to
/// choose between C, C++ and Objective-C, and a header is never Luau. Callers
/// that test every path of a checkout must use this one; with the header
/// reads, a Linux kernel cold build spent over a minute on one thread here.
pub fn is_luau_path(path: &Path, dynamic_languages: bool) -> bool {
    language_for_extension(path).is_none()
        && dynamic_language_for_extension(path, dynamic_languages) == Some(LangId::Luau)
}

pub(super) fn dynamic_language_for_extension(
    path: &Path,
    dynamic_languages: bool,
) -> Option<LangId> {
    if !dynamic_languages {
        return None;
    }
    let extension = path.extension()?.to_str()?;
    crate::grammar::manifest()
        .extensions
        .iter()
        .any(|ext| ext == extension)
        .then_some(LangId::Luau)
}
