pub use aft_parse::imports::quotes::*;

#[cfg(test)]
mod tests {
    use super::*;

    fn render(source: &str, root: &Path, extension: &str) -> String {
        let file = root.join(format!("file.{extension}"));
        let lang = if extension == "vue" {
            LangId::Vue
        } else {
            LangId::TypeScript
        };
        let tree = parse_source_with_cached_parser(&file, source, lang).unwrap();
        let style = preferred_style(source, &tree, lang, &file, Some(root));
        crate::imports::generate_import_line_with_namespace_and_attribute_clause_and_style(
            lang,
            "../../shared/logger",
            &["log".into()],
            None,
            None,
            false,
            None,
            Some(style),
        )
    }

    #[test]
    fn double_quoted_add() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            render("import { a } from \"a\";", dir.path(), "ts"),
            "import { log } from \"../../shared/logger\";"
        );
    }

    #[test]
    fn single_quoted_add() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            render("import { a } from 'a';", dir.path(), "ts"),
            "import { log } from '../../shared/logger';"
        );
    }

    #[test]
    fn majority_and_first_statement_tie() {
        let dir = tempfile::tempdir().unwrap();
        for source in [
            "import { a } from 'a'; import { b } from \"b\"; export { c } from \"c\";",
            "export { a } from \"a\"; import { b } from 'b';",
            "// import 'ignored';\nimport { a } from \"a\" with { type: 'json' };",
        ] {
            assert_eq!(
                render(source, dir.path(), "ts"),
                "import { log } from \"../../shared/logger\";"
            );
        }
        assert_eq!(
            render(
                "<script setup lang=\"ts\">import { a } from 'a';</script>",
                dir.path(),
                "vue"
            ),
            "import { log } from '../../shared/logger';"
        );
    }

    #[test]
    fn formatter_config_and_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            render("const a = 'ignored';", dir.path(), "ts"),
            "import { log } from \"../../shared/logger\";"
        );
        for (name, content) in [
            (
                "biome.json",
                r#"{"javascript":{"formatter":{"quoteStyle":"single"}}}"#,
            ),
            (
                "biome.jsonc",
                "{ // comment\n\"javascript\": {\"formatter\": {\"quoteStyle\": \"single\",}},}",
            ),
            (".prettierrc", r#"{"singleQuote":true}"#),
            ("package.json", r#"{"prettier":{"singleQuote":true}}"#),
            (".prettierrc.yaml", "singleQuote: true\n"),
            (".prettierrc.cjs", "module.exports = { singleQuote: true };"),
        ] {
            std::fs::write(dir.path().join(name), content).unwrap();
            assert_eq!(
                render("const a = 1;", dir.path(), "ts"),
                "import { log } from '../../shared/logger';",
                "{name}"
            );
            std::fs::remove_file(dir.path().join(name)).unwrap();
        }
    }

    #[test]
    fn config_is_nearest_and_stops_at_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        let nested = root.join("src");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.path().join(".prettierrc"), r#"{"singleQuote":true}"#).unwrap();
        assert_eq!(config_quote(&nested.join("file.ts"), Some(&root)), None);
        std::fs::write(root.join(".prettierrc"), r#"{"singleQuote":true}"#).unwrap();
        assert_eq!(
            config_quote(&nested.join("file.ts"), Some(&root)),
            Some('\'')
        );
        std::fs::write(
            nested.join("biome.json"),
            r#"{"javascript":{"formatter":{"quoteStyle":"double"}}}"#,
        )
        .unwrap();
        assert_eq!(
            config_quote(&nested.join("file.ts"), Some(&root)),
            Some('"')
        );
    }

    #[test]
    fn new_statement_follows_semicolon_majority_then_first_statement() {
        let dir = tempfile::tempdir().unwrap();
        for (source, expected) in [
            (
                "import { a } from 'a'\nimport { b } from 'b'\n",
                "import { log } from '../../shared/logger'",
            ),
            (
                "import { a } from 'a';\nimport { b } from 'b'\nimport { c } from 'c';\n",
                "import { log } from '../../shared/logger';",
            ),
            (
                "import { a } from 'a'\nexport { b } from 'b';\n",
                "import { log } from '../../shared/logger'",
            ),
            (
                "export { a } from 'a';\nimport { b } from 'b'\n",
                "import { log } from '../../shared/logger';",
            ),
            (
                "<script setup lang=\"ts\">\nimport { a } from 'a'\n</script>\n",
                "import { log } from '../../shared/logger'",
            ),
        ] {
            let extension = if source.starts_with('<') { "vue" } else { "ts" };
            assert_eq!(render(source, dir.path(), extension), expected, "{source}");
        }
    }

    #[test]
    fn semicolon_formatter_config_and_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            render("const a = 1", dir.path(), "ts"),
            "import { log } from \"../../shared/logger\";"
        );
        for (name, content, expected) in [
            (
                "biome.json",
                r#"{"javascript":{"formatter":{"semicolons":"asNeeded"}}}"#,
                false,
            ),
            (
                "biome.jsonc",
                "{ // comment\n\"javascript\": {\"formatter\": {\"semicolons\": \"always\",}},}",
                true,
            ),
            (".prettierrc", r#"{"semi":false}"#, false),
            ("package.json", r#"{"prettier":{"semi":false}}"#, false),
            (".prettierrc.yaml", "semi: false\n", false),
            (
                ".prettierrc.cjs",
                "module.exports = { semi: false };",
                false,
            ),
            ("prettier.config.js", "export default { semi: true };", true),
        ] {
            std::fs::write(dir.path().join(name), content).unwrap();
            let terminator = if expected { ";" } else { "" };
            assert_eq!(
                render("const a = 1", dir.path(), "ts"),
                format!("import {{ log }} from \"../../shared/logger\"{terminator}"),
                "{name}"
            );
            std::fs::remove_file(dir.path().join(name)).unwrap();
        }
    }

    #[test]
    fn semicolon_config_does_not_read_unrelated_keys() {
        let dir = tempfile::tempdir().unwrap();
        // `semicolons` in a Prettier file and `semi` in Biome are not the
        // option either tool reads, so neither may decide the style.
        std::fs::write(dir.path().join(".prettierrc.yaml"), "semicolons: false\n").unwrap();
        std::fs::write(
            dir.path().join("biome.json"),
            r#"{"javascript":{"formatter":{"semi":false}}}"#,
        )
        .unwrap();
        assert_eq!(
            config_semicolon(&dir.path().join("file.ts"), Some(dir.path())),
            None
        );
    }

    #[test]
    fn statement_style_reads_the_statement_itself() {
        assert_eq!(
            statement_style("import type { A } from './types.ts'"),
            EsImportStyle {
                quote: '\'',
                semicolon: false
            }
        );
        assert_eq!(
            statement_style("import { A } from \"./a\";"),
            EsImportStyle {
                quote: '"',
                semicolon: true
            }
        );
        assert_eq!(
            statement_style("import data from './d.json' with { type: 'json' }"),
            EsImportStyle {
                quote: '\'',
                semicolon: false
            }
        );
    }

    #[test]
    fn module_path_escaping() {
        assert_eq!(module_literal("a'\"\\b\n", '\''), "'a\\'\"\\\\b\\n'");
        assert_eq!(module_literal("a'\"\\b\n", '"'), "\"a'\\\"\\\\b\\n\"");
    }
}
