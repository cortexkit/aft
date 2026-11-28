pub use aft_parse::imports::c::*;

// Shared import scope for the unit tests below.
#[cfg(test)]
#[allow(unused_imports)]
use super::{
    import_byte_range, ImportBlock, ImportForm, ImportGroup, ImportKind, ImportRequest,
    ImportStatement, ImportSyntax,
};
#[cfg(test)]
#[allow(unused_imports)]
use tree_sitter::{Node, Tree};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imports::{generate_import, parse_imports};
    use crate::parser::{grammar_for, LangId};
    use std::collections::BTreeSet;
    use tree_sitter::Parser;

    fn parse_c_like(source: &str, lang: LangId) -> (Tree, ImportBlock) {
        let grammar = grammar_for(lang).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, lang);
        (tree, block)
    }

    fn structured(import: &ImportStatement) -> Option<&str> {
        match &import.form {
            ImportForm::Structured {
                named,
                namespace,
                alias,
                modifiers,
                import_kind,
            } => {
                assert!(named.is_empty());
                assert!(namespace.is_none());
                assert!(alias.is_none());
                assert!(modifiers.is_empty());
                import_kind.as_deref()
            }
            other => panic!("expected C/C++ Structured import, got {other:?}"),
        }
    }

    /// Grammar fixture: lock the exact tree-sitter-c and tree-sitter-cpp node
    /// kinds this parser depends on. Both grammars currently emit
    /// `preproc_include`, with `system_lib_string` for `<...>` headers and
    /// `string_literal` / `string_content` for `"..."` headers.
    #[test]
    fn c_include_grammar_node_kinds_are_stable() {
        for (lang, source) in [
            (
                LangId::C,
                "#include <stdio.h>\n#include \"local.h\"\nint main(void) { return 0; }\n",
            ),
            (
                LangId::Cpp,
                "#include <vector>\n#include \"foo.hpp\"\nint main() { return 0; }\n",
            ),
        ] {
            let (tree, _) = parse_c_like(source, lang);
            assert!(!tree.root_node().has_error(), "parse errors for {lang:?}");

            let mut kinds = BTreeSet::new();
            fn walk(node: Node, kinds: &mut BTreeSet<String>) {
                kinds.insert(node.kind().to_string());
                let mut cursor = node.walk();
                if cursor.goto_first_child() {
                    loop {
                        walk(cursor.node(), kinds);
                        if !cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
            }
            walk(tree.root_node(), &mut kinds);

            for required in [
                "translation_unit",
                "preproc_include",
                "#include",
                "system_lib_string",
                "string_literal",
                "string_content",
            ] {
                assert!(
                    kinds.contains(required),
                    "{lang:?} grammar missing node kind {required:?}; present: {kinds:?}"
                );
            }
        }
    }

    #[test]
    fn parse_c_and_cpp_includes_preserves_delimiters() {
        for (lang, source, system_path, local_path) in [
            (
                LangId::C,
                "#include <stdio.h>\n#include \"project/header.h\"\nint main(void) { return 0; }\n",
                "stdio.h",
                "project/header.h",
            ),
            (
                LangId::Cpp,
                "#include <vector>\n#include \"foo/bar.hpp\"\nint main() { return 0; }\n",
                "vector",
                "foo/bar.hpp",
            ),
        ] {
            let (_, block) = parse_c_like(source, lang);
            assert_eq!(block.imports.len(), 2, "parse imports for {lang:?}");
            assert_c_include(
                &block.imports[0],
                system_path,
                C_INCLUDE_SYSTEM_KIND,
                ImportGroup::Stdlib,
            );
            assert_c_include(
                &block.imports[1],
                local_path,
                C_INCLUDE_LOCAL_KIND,
                ImportGroup::External,
            );
        }
    }

    fn assert_c_include(
        imp: &ImportStatement,
        module_path: &str,
        import_kind: &str,
        group: ImportGroup,
    ) {
        assert_eq!(imp.module_path, module_path);
        assert_eq!(imp.names, Vec::<String>::new());
        assert_eq!(imp.default_import.as_deref(), Some(import_kind));
        assert_eq!(imp.namespace_import, None);
        assert_eq!(imp.kind, ImportKind::SideEffect);
        assert_eq!(imp.group, group);
        assert_eq!(structured(imp), Some(import_kind));
    }

    #[test]
    fn generate_c_and_cpp_supported_forms() {
        for lang in [LangId::C, LangId::Cpp] {
            assert_eq!(
                generate_import(
                    lang,
                    &ImportRequest {
                        module_path: "stdio.h",
                        names: &[],
                        default_import: None,
                        namespace: None,
                        alias: None,
                        type_only: false,
                        modifiers: &[],
                        import_kind: Some(C_INCLUDE_SYSTEM_KIND),
                    },
                ),
                "#include <stdio.h>"
            );
            assert_eq!(
                generate_import(
                    lang,
                    &ImportRequest {
                        module_path: "project/header.h",
                        names: &[],
                        default_import: None,
                        namespace: None,
                        alias: None,
                        type_only: false,
                        modifiers: &[],
                        import_kind: Some(C_INCLUDE_LOCAL_KIND),
                    },
                ),
                "#include \"project/header.h\""
            );
        }
    }

    #[test]
    fn generate_c_preserves_organized_flat_markers() {
        for (marker, expected) in [
            (C_INCLUDE_SYSTEM_KIND, "#include <stdio.h>"),
            (C_INCLUDE_LOCAL_KIND, "#include \"stdio.h\""),
        ] {
            assert_eq!(
                generate_import(
                    LangId::C,
                    &ImportRequest::legacy("stdio.h", &[], Some(marker), None, false),
                ),
                expected
            );
        }
    }

    #[test]
    fn c_and_cpp_round_trip_through_parse_generate() {
        for (lang, samples) in [
            (
                LangId::C,
                ["#include <stdio.h>", "#include \"project/header.h\""],
            ),
            (LangId::Cpp, ["#include <vector>", "#include \"foo.hpp\""]),
        ] {
            for src in samples {
                let (_, block) = parse_c_like(src, lang);
                assert_eq!(block.imports.len(), 1, "parse {src:?} for {lang:?}");
                let imp = &block.imports[0];
                let import_kind = structured(imp);
                let regenerated = generate_import(
                    lang,
                    &ImportRequest {
                        module_path: &imp.module_path,
                        names: &imp.names,
                        default_import: imp.default_import.as_deref(),
                        namespace: None,
                        alias: None,
                        type_only: false,
                        modifiers: &[],
                        import_kind,
                    },
                );
                assert_eq!(regenerated, src, "round-trip mismatch for {src:?}");
            }
        }
    }

    #[test]
    fn normalize_include_module_strips_delimiters_and_infers_kind() {
        // Angle/system delimiter.
        assert_eq!(
            normalize_include_module("<vector>"),
            ("vector".to_string(), Some(C_INCLUDE_SYSTEM_KIND))
        );
        // Quote/local delimiter.
        assert_eq!(
            normalize_include_module("\"foo/bar.h\""),
            ("foo/bar.h".to_string(), Some(C_INCLUDE_LOCAL_KIND))
        );
        // Surrounding whitespace tolerated.
        assert_eq!(
            normalize_include_module("  <stdio.h>  "),
            ("stdio.h".to_string(), Some(C_INCLUDE_SYSTEM_KIND))
        );
        // Bare header: unchanged, no inferred kind (caller defaults to system).
        assert_eq!(
            normalize_include_module("string"),
            ("string".to_string(), None)
        );
    }
}
