pub use aft_base::lang_id::*;

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
