//! Language names and extension-only classification, without loading grammars.

use std::path::Path;

/// Supported language identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LangId {
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Rust,
    Go,
    C,
    Cpp,
    Cuda,
    Metal,
    Zig,
    CSharp,
    Bash,
    Html,
    Markdown,
    Solidity,
    Scss,
    Vue,
    Json,
    Scala,
    Java,
    Ruby,
    Kotlin,
    Swift,
    Php,
    Lua,
    Perl,
    Yaml,
    Pascal,
    R,
    Groovy,
    ObjC,
    Toml,
    Luau,
}

#[cfg(test)]
pub(crate) const TEST_EXTENSIONS: &[&str] = &[
    "ts", "mts", "cts", "tsx", "js", "jsx", "mjs", "cjs", "py", "pyi", "rs", "go", "c", "h", "cc",
    "cpp", "cxx", "hpp", "hh", "cu", "cuh", "metal", "zig", "cs", "sh", "bash", "zsh", "html",
    "htm", "md", "markdown", "mdx", "qmd", "Qmd", "rmd", "Rmd", "sol", "scss", "vue", "json",
    "jsonc", "scala", "sc", "java", "rb", "kt", "kts", "swift", "inc", "php", "lua", "pl", "pm",
    "t", "yaml", "yml", "pas", "pp", "dpr", "dpk", "lpr", "R", "r", "groovy", "gvy", "gy", "gsh",
    "gradle", "m", "mm", "toml", "luau", "H", "unknown", "",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_lookup_preserves_parser_decisions() {
        for path in TEST_EXTENSIONS
            .iter()
            .map(|ext| Path::new("file").with_extension(ext))
            .chain(["Jenkinsfile", "file", ".hidden"].map(std::path::PathBuf::from))
        {
            assert_eq!(
                language_by_extension(&path),
                crate::parser::detect_language(&path),
                "{}",
                path.display()
            );
        }
        let temp = tempfile::tempdir().unwrap();
        let header = temp.path().join("api.h");
        for (source, expected) in [
            ("int plain(void);", LangId::C),
            (
                "namespace api { template <typename T> class Widget {}; }",
                LangId::Cpp,
            ),
            ("@interface Greeter\n- (void)run;\n@end", LangId::ObjC),
            ("int broken( {", LangId::C),
        ] {
            std::fs::write(&header, source).unwrap();
            let resolved = crate::parser::detect_language(&header);
            assert_eq!(resolved, Some(expected));
            assert_eq!(language_by_extension(&header), Some(LangId::C));
            let is_source = |lang| matches!(lang, Some(lang) if !matches!(lang, LangId::Markdown | LangId::Html | LangId::Yaml | LangId::Toml));
            assert_eq!(
                is_source(language_by_extension(&header)),
                is_source(resolved)
            );
            assert_eq!(language_by_extension(&header).is_some(), resolved.is_some());
        }
    }
}

/// Classify by name without reading the file. A `.h` result is provisional C;
/// the parser resolves C, C++ or Objective-C from the header's contents.
pub fn language_by_extension(path: &Path) -> Option<LangId> {
    if path.file_name().and_then(|name| name.to_str()) == Some("Jenkinsfile") {
        return Some(LangId::Groovy);
    }

    let ext = path.extension()?.to_str()?;
    match ext {
        "ts" | "mts" | "cts" => Some(LangId::TypeScript),
        "tsx" => Some(LangId::Tsx),
        "js" | "jsx" | "mjs" | "cjs" => Some(LangId::JavaScript),
        "py" | "pyi" => Some(LangId::Python),
        "rs" => Some(LangId::Rust),
        "go" => Some(LangId::Go),
        "c" | "h" => Some(LangId::C),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => Some(LangId::Cpp),
        "cu" | "cuh" => Some(LangId::Cuda),
        "metal" => Some(LangId::Metal),
        "zig" => Some(LangId::Zig),
        "cs" => Some(LangId::CSharp),
        "sh" | "bash" | "zsh" => Some(LangId::Bash),
        "html" | "htm" => Some(LangId::Html),
        "md" | "markdown" | "mdx" | "qmd" | "Qmd" | "rmd" | "Rmd" => Some(LangId::Markdown),
        "sol" => Some(LangId::Solidity),
        "scss" => Some(LangId::Scss),
        "vue" => Some(LangId::Vue),
        "json" | "jsonc" => Some(LangId::Json),
        "scala" | "sc" => Some(LangId::Scala),
        "java" => Some(LangId::Java),
        "rb" => Some(LangId::Ruby),
        "kt" | "kts" => Some(LangId::Kotlin),
        "swift" => Some(LangId::Swift),
        "inc" | "php" => Some(LangId::Php),
        "lua" => Some(LangId::Lua),
        "pl" | "pm" | "t" => Some(LangId::Perl),
        "yaml" | "yml" => Some(LangId::Yaml),
        "pas" | "pp" | "dpr" | "dpk" | "lpr" => Some(LangId::Pascal),
        "R" | "r" => Some(LangId::R),
        "groovy" | "gvy" | "gy" | "gsh" | "gradle" => Some(LangId::Groovy),
        "m" | "mm" => Some(LangId::ObjC),
        "toml" => Some(LangId::Toml),
        _ => None,
    }
}
