use crate::{error::AftError, grammar, lang_id::LangId};
use tree_sitter::Language;

/// Select a linked grammar or load a stored downloaded grammar, without requesting it.
pub fn grammar_for(lang: LangId) -> Result<Language, AftError> {
    if let Some(language) = grammar::extractor_name(lang) {
        return grammar::registry(language)
            .ok_or_else(|| AftError::InvalidRequest {
                message: format!("no manifest entry for grammar: {language}"),
            })?
            .stored()
            .map_err(Into::into);
    }
    Ok(match lang {
        LangId::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        LangId::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        LangId::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        LangId::Python => tree_sitter_python::LANGUAGE.into(),
        LangId::Rust => tree_sitter_rust::LANGUAGE.into(),
        LangId::Go => tree_sitter_go::LANGUAGE.into(),
        LangId::C => tree_sitter_c::LANGUAGE.into(),
        LangId::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        LangId::Cuda => tree_sitter_cuda::LANGUAGE.into(),
        LangId::Metal => tree_sitter_cpp::LANGUAGE.into(),
        LangId::Zig => tree_sitter_zig::LANGUAGE.into(),
        LangId::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
        LangId::Bash => tree_sitter_bash::LANGUAGE.into(),
        LangId::Html => tree_sitter_html::LANGUAGE.into(),
        LangId::Markdown => tree_sitter_md::LANGUAGE.into(),
        LangId::Solidity => tree_sitter_solidity::LANGUAGE.into(),
        LangId::Scss => tree_sitter_scss::language(),
        LangId::Vue => tree_sitter_vue::LANGUAGE.into(),
        LangId::Json => tree_sitter_json::LANGUAGE.into(),
        LangId::Scala => tree_sitter_scala::LANGUAGE.into(),
        LangId::Java => tree_sitter_java::LANGUAGE.into(),
        LangId::Ruby => tree_sitter_ruby::LANGUAGE.into(),
        LangId::Kotlin => tree_sitter_kotlin_sg::LANGUAGE.into(),
        LangId::Swift => tree_sitter_swift::LANGUAGE.into(),
        LangId::Php => tree_sitter_php::LANGUAGE_PHP.into(),
        LangId::Lua => tree_sitter_lua::LANGUAGE.into(),
        LangId::Perl => tree_sitter_perl::LANGUAGE.into(),
        LangId::Yaml => tree_sitter_yaml::LANGUAGE.into(),
        LangId::Pascal => tree_sitter_pascal::LANGUAGE.into(),
        LangId::R => tree_sitter_r::LANGUAGE.into(),
        LangId::Groovy => dekobon_tree_sitter_groovy::LANGUAGE.into(),
        LangId::ObjC => tree_sitter_objc::LANGUAGE.into(),
        LangId::Toml => tree_sitter_toml::LANGUAGE.into(),
        _ => unreachable!("downloaded languages use the extractor table"),
    })
}
