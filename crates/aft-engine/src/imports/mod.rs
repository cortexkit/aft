pub use aft_parse::imports::*;

// Shared import scope for the unit tests below.
#[cfg(test)]
#[allow(unused_imports)]
use crate::parser::LangId;
#[cfg(test)]
#[allow(unused_imports)]
use std::ops::Range;
#[cfg(test)]
#[allow(unused_imports)]
use tree_sitter::{Node, Tree};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::grammar_for;
    use tree_sitter::Parser;

    /// Parsing imports file after file must reuse this thread's parser rather
    /// than build a parser and load the grammar for every file.
    #[test]
    fn parse_file_imports_reuses_the_thread_parser() {
        use crate::parser::work_counters::grammar_loads;
        let temp = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for index in 0..10 {
            let path = temp.path().join(format!("f{index}.ts"));
            std::fs::write(&path, format!("import {{ a{index} }} from './a';\n")).unwrap();
            paths.push(path);
        }
        parse_file_imports(&paths[0], LangId::TypeScript).unwrap();
        let before = grammar_loads();
        for (index, path) in paths.iter().enumerate() {
            let (_, _, block) = parse_file_imports(path, LangId::TypeScript).unwrap();
            assert_eq!(block.imports.len(), 1, "f{index}");
        }
        assert_eq!(grammar_loads() - before, 0);
    }

    // --- ImportForm field-mapping contract (Stream M) ---
    //
    // These assert the additive `form` field faithfully mirrors the flat
    // fields each parser populates. They are the executable field-mapping
    // contract from the migration plan: when a reader is moved off a flat
    // field onto `form`, these guarantee no information was lost in the
    // de-overloading (Rust `pub`, Go alias) or restructuring.

    #[test]
    fn form_es_mirrors_flat_fields() {
        let (_, block) = parse_ts(
            "import Default, { a, b as c } from \"ext\";\nimport type { T } from \"./t\";\nimport \"./side\";\nimport * as ns from \"nspkg\";\n",
        );
        // import Default, { a, b as c } from "ext"
        match &block.imports[0].form {
            ImportForm::Es {
                default_import,
                namespace_import,
                named,
                type_only,
                side_effect,
                attribute_clause,
                attribute_type,
            } => {
                assert_eq!(default_import.as_deref(), Some("Default"));
                assert_eq!(namespace_import, &None);
                assert_eq!(named, &block.imports[0].names);
                assert!(!type_only);
                assert!(!side_effect);
                assert_eq!(attribute_clause, &None);
                assert_eq!(attribute_type, &None);
            }
            other => panic!("expected Es, got {other:?}"),
        }
        // import type { T } from "./t"
        match &block.imports[1].form {
            ImportForm::Es {
                type_only, named, ..
            } => {
                assert!(type_only);
                assert_eq!(named, &block.imports[1].names);
            }
            other => panic!("expected Es type-only, got {other:?}"),
        }
        // import "./side"
        match &block.imports[2].form {
            ImportForm::Es { side_effect, .. } => assert!(side_effect),
            other => panic!("expected Es side-effect, got {other:?}"),
        }
        // import * as ns from "nspkg"
        match &block.imports[3].form {
            ImportForm::Es {
                namespace_import, ..
            } => assert_eq!(namespace_import.as_deref(), Some("ns")),
            other => panic!("expected Es namespace, got {other:?}"),
        }
    }

    #[test]
    fn form_python_mirrors_flat_fields() {
        let (_, block) = parse_py("import os\nfrom sys import argv, path\n");
        match &block.imports[0].form {
            ImportForm::Python { from_import, named } => {
                assert!(!from_import, "`import os` is not a from-import");
                assert_eq!(named, &["os"]);
            }
            other => panic!("expected Python import, got {other:?}"),
        }
        match &block.imports[1].form {
            ImportForm::Python { from_import, named } => {
                assert!(from_import, "`from sys import ...` is a from-import");
                assert_eq!(named, &block.imports[1].names);
            }
            other => panic!("expected Python from-import, got {other:?}"),
        }
    }

    #[test]
    fn form_rust_de_overloads_pub_from_default_import() {
        let (_, block) = parse_rust("pub use crate::a::Exported;\nuse std::fmt::Debug;\n");
        // pub use -> visibility=Some("pub"); flat field still carries the "pub" hack.
        match &block.imports[0].form {
            ImportForm::RustUse { visibility, named } => {
                assert_eq!(visibility.as_deref(), Some("pub"));
                assert_eq!(named, &block.imports[0].names);
            }
            other => panic!("expected RustUse, got {other:?}"),
        }
        assert_eq!(
            block.imports[0].default_import.as_deref(),
            Some("pub"),
            "flat field unchanged during additive migration"
        );
        // plain use -> visibility=None
        match &block.imports[1].form {
            ImportForm::RustUse { visibility, .. } => assert_eq!(visibility, &None),
            other => panic!("expected RustUse, got {other:?}"),
        }
        assert_eq!(block.imports[1].default_import, None);
    }

    #[test]
    fn form_go_de_overloads_alias_from_default_import() {
        // The current Go parser only captures blank (`_`) / dot bindings as the
        // alias (regular package aliases like `al "path"` are a pre-existing
        // parser gap — not extracted into `default_import` today). The contract
        // locked here is that `form.alias` mirrors `default_import` exactly,
        // whatever the parser captures, so the de-overload is information-faithful.
        let (_, block) =
            parse_go("package main\n\nimport (\n\t_ \"github.com/x/y\"\n\t\"fmt\"\n)\n");
        let blank = block
            .imports
            .iter()
            .find(|i| i.module_path == "github.com/x/y")
            .expect("blank import parsed");
        match &blank.form {
            ImportForm::Go { alias } => assert_eq!(alias.as_deref(), Some("_")),
            other => panic!("expected Go blank-aliased, got {other:?}"),
        }
        assert_eq!(
            blank.default_import.as_deref(),
            Some("_"),
            "form.alias mirrors the flat default_import field exactly"
        );
        let plain = block
            .imports
            .iter()
            .find(|i| i.module_path == "fmt")
            .expect("plain import parsed");
        match &plain.form {
            ImportForm::Go { alias } => assert_eq!(alias, &None),
            other => panic!("expected Go plain, got {other:?}"),
        }
        assert_eq!(plain.default_import, None);
    }

    fn parse_ts(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::TypeScript).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::TypeScript);
        (tree, block)
    }

    fn parse_js(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::JavaScript).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::JavaScript);
        (tree, block)
    }

    fn parse_vue(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::Vue).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::Vue);
        (tree, block)
    }

    /// Locks the tree-sitter-vue node kinds the Vue engine depends on: the
    /// `<script>` body is exposed as a single `raw_text` node inside a
    /// `script_element`. If a grammar bump changes this, the engine breaks
    /// silently, so assert it here.
    #[test]
    fn vue_grammar_node_kinds_are_stable() {
        let src = "<template>\n  <div />\n</template>\n\n<script setup lang=\"ts\">\nimport { ref } from 'vue'\n</script>\n";
        let grammar = grammar_for(LangId::Vue).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();
        let mut cursor = root.walk();
        let script = root
            .named_children(&mut cursor)
            .find(|n| n.kind() == "script_element")
            .expect("expected a script_element node");
        let mut inner = script.walk();
        assert!(
            script
                .named_children(&mut inner)
                .any(|n| n.kind() == "raw_text"),
            "expected script body exposed as raw_text"
        );
    }

    #[test]
    fn vue_parses_script_imports_with_whole_file_offsets() {
        let src = "<template>\n  <div />\n</template>\n\n<script setup lang=\"ts\">\nimport { ref } from 'vue'\nimport Foo from './Foo.vue'\nconst x = ref(0)\n</script>\n";
        let (_tree, block) = parse_vue(src);
        assert_eq!(block.imports.len(), 2, "should find both script imports");
        // Byte ranges must be whole-file (inside the <script> block), not
        // script-relative — verify the raw slice round-trips.
        for imp in &block.imports {
            assert_eq!(&src[imp.byte_range.clone()], imp.raw_text);
            assert!(
                imp.byte_range.start > src.find("<script").unwrap(),
                "import offset must fall inside the script block"
            );
        }
        assert_eq!(block.imports[0].module_path, "vue");
        assert_eq!(block.imports[1].module_path, "./Foo.vue");
    }

    #[test]
    fn vue_without_script_block_has_no_imports() {
        let src = "<template>\n  <div />\n</template>\n\n<style>.x{}</style>\n";
        let (_tree, block) = parse_vue(src);
        assert!(block.imports.is_empty());
        assert!(block.byte_range.is_none());
    }

    // --- Basic parsing ---

    #[test]
    fn parse_ts_named_imports() {
        let source = "import { useState, useEffect } from 'react';\n";
        let (_, block) = parse_ts(source);
        assert_eq!(block.imports.len(), 1);
        let imp = &block.imports[0];
        assert_eq!(imp.module_path, "react");
        assert!(imp.names.contains(&"useState".to_string()));
        assert!(imp.names.contains(&"useEffect".to_string()));
        assert_eq!(imp.kind, ImportKind::Value);
        assert_eq!(imp.group, ImportGroup::External);
    }

    #[test]
    fn parse_ts_default_import() {
        let source = "import React from 'react';\n";
        let (_, block) = parse_ts(source);
        assert_eq!(block.imports.len(), 1);
        let imp = &block.imports[0];
        assert_eq!(imp.default_import.as_deref(), Some("React"));
        assert_eq!(imp.kind, ImportKind::Value);
    }

    #[test]
    fn parse_ts_side_effect_import() {
        let source = "import './styles.css';\n";
        let (_, block) = parse_ts(source);
        assert_eq!(block.imports.len(), 1);
        assert_eq!(block.imports[0].kind, ImportKind::SideEffect);
        assert_eq!(block.imports[0].module_path, "./styles.css");
    }

    #[test]
    fn parse_ts_relative_import() {
        let source = "import { helper } from './utils';\n";
        let (_, block) = parse_ts(source);
        assert_eq!(block.imports.len(), 1);
        assert_eq!(block.imports[0].group, ImportGroup::Internal);
    }

    #[test]
    fn parse_ts_multiple_groups() {
        let source = "\
import React from 'react';
import { useState } from 'react';
import { helper } from './utils';
import { Config } from '../config';
";
        let (_, block) = parse_ts(source);
        assert_eq!(block.imports.len(), 4);

        let external: Vec<_> = block
            .imports
            .iter()
            .filter(|i| i.group == ImportGroup::External)
            .collect();
        let relative: Vec<_> = block
            .imports
            .iter()
            .filter(|i| i.group == ImportGroup::Internal)
            .collect();
        assert_eq!(external.len(), 2);
        assert_eq!(relative.len(), 2);
    }

    #[test]
    fn parse_ts_namespace_import() {
        let source = "import * as path from 'path';\n";
        let (_, block) = parse_ts(source);
        assert_eq!(block.imports.len(), 1);
        let imp = &block.imports[0];
        assert_eq!(imp.namespace_import.as_deref(), Some("path"));
        assert_eq!(imp.kind, ImportKind::Value);
    }

    #[test]
    fn parse_js_imports() {
        let source = "import { readFile } from 'fs';\nimport { helper } from './helper';\n";
        let (_, block) = parse_js(source);
        assert_eq!(block.imports.len(), 2);
        assert_eq!(block.imports[0].group, ImportGroup::External);
        assert_eq!(block.imports[1].group, ImportGroup::Internal);
    }

    // --- Group classification ---

    #[test]
    fn classify_external() {
        assert_eq!(classify_group_ts("react"), ImportGroup::External);
        assert_eq!(classify_group_ts("@scope/pkg"), ImportGroup::External);
        assert_eq!(classify_group_ts("lodash/map"), ImportGroup::External);
    }

    #[test]
    fn classify_relative() {
        assert_eq!(classify_group_ts("./utils"), ImportGroup::Internal);
        assert_eq!(classify_group_ts("../config"), ImportGroup::Internal);
        assert_eq!(classify_group_ts("./"), ImportGroup::Internal);
    }

    // --- Dedup ---

    #[test]
    fn dedup_detects_same_named_import() {
        let source = "import { useState } from 'react';\n";
        let (_, block) = parse_ts(source);
        assert!(is_duplicate(
            &block,
            "react",
            &["useState".to_string()],
            None,
            false
        ));
    }

    #[test]
    fn dedup_misses_different_name() {
        let source = "import { useState } from 'react';\n";
        let (_, block) = parse_ts(source);
        assert!(!is_duplicate(
            &block,
            "react",
            &["useEffect".to_string()],
            None,
            false
        ));
    }

    #[test]
    fn dedup_detects_default_import() {
        let source = "import React from 'react';\n";
        let (_, block) = parse_ts(source);
        assert!(is_duplicate(&block, "react", &[], Some("React"), false));
    }

    #[test]
    fn dedup_side_effect() {
        let source = "import './styles.css';\n";
        let (_, block) = parse_ts(source);
        assert!(is_duplicate(&block, "./styles.css", &[], None, false));
    }

    #[test]
    fn dedup_namespace_import_distinct_from_side_effect_import() {
        let side_effect_source = "import 'fs';\n";
        let (_, side_effect_block) = parse_ts(side_effect_source);
        assert!(!is_duplicate_with_namespace(
            &side_effect_block,
            "fs",
            &[],
            None,
            Some("fs"),
            false
        ));

        let namespace_source = "import * as fs from 'fs';\n";
        let (_, namespace_block) = parse_ts(namespace_source);
        assert!(!is_duplicate(&namespace_block, "fs", &[], None, false));
        assert!(is_duplicate_with_namespace(
            &namespace_block,
            "fs",
            &[],
            None,
            Some("fs"),
            false
        ));
        assert!(!is_duplicate_with_namespace(
            &namespace_block,
            "fs",
            &[],
            None,
            Some("other"),
            false
        ));
    }

    #[test]
    fn dedup_type_vs_value() {
        let source = "import { FC } from 'react';\n";
        let (_, block) = parse_ts(source);
        // Type import should NOT match a value import of the same name
        assert!(!is_duplicate(
            &block,
            "react",
            &["FC".to_string()],
            None,
            true
        ));
    }

    // --- Generation ---

    #[test]
    fn generate_named_import() {
        let line = generate_import_line(
            LangId::TypeScript,
            "react",
            &["useState".to_string(), "useEffect".to_string()],
            None,
            false,
        );
        assert_eq!(line, "import { useEffect, useState } from 'react';");
    }

    #[test]
    fn generate_named_import_sorts_by_imported_name() {
        let line = generate_import_line(
            LangId::TypeScript,
            "x",
            &[
                "useState".to_string(),
                "type Foo".to_string(),
                "stdin as input".to_string(),
                "type Bar".to_string(),
            ],
            None,
            false,
        );
        assert_eq!(
            line,
            "import { type Bar, type Foo, stdin as input, useState } from 'x';"
        );
    }

    #[test]
    fn generate_default_import() {
        let line = generate_import_line(LangId::TypeScript, "react", &[], Some("React"), false);
        assert_eq!(line, "import React from 'react';");
    }

    #[test]
    fn generate_type_import() {
        let line =
            generate_import_line(LangId::TypeScript, "react", &["FC".to_string()], None, true);
        assert_eq!(line, "import type { FC } from 'react';");
    }

    #[test]
    fn generate_side_effect_import() {
        let line = generate_import_line(LangId::TypeScript, "./styles.css", &[], None, false);
        assert_eq!(line, "import './styles.css';");
    }

    #[test]
    fn generate_default_and_named() {
        let line = generate_import_line(
            LangId::TypeScript,
            "react",
            &["useState".to_string()],
            Some("React"),
            false,
        );
        assert_eq!(line, "import React, { useState } from 'react';");
    }

    #[test]
    fn generate_es_line_follows_style_terminator() {
        let render = |semicolon, clause| {
            generate_import_line_with_namespace_and_attribute_clause_and_style(
                LangId::TypeScript,
                "./data.json",
                &[],
                Some("data"),
                None,
                false,
                clause,
                Some(quotes::EsImportStyle {
                    quote: '\'',
                    semicolon,
                }),
            )
        };
        assert_eq!(render(false, None), "import data from './data.json'");
        assert_eq!(render(true, None), "import data from './data.json';");
        assert_eq!(
            render(false, Some("with { type: 'json' }")),
            "import data from './data.json' with { type: 'json' }"
        );
        assert_eq!(
            render(true, Some("with { type: 'json' }")),
            "import data from './data.json' with { type: 'json' };"
        );
    }

    #[test]
    fn parse_es_import_attributes_preserve_standard_and_legacy_spellings() {
        let source = r#"import data from './data.json' with { type: 'json' };
import legacy from './legacy.json' assert { type: 'json' };
import escaped from './escaped.json' with { "t\u0079pe": "j\u0073on" };
"#;
        let (_, block) = parse_js(source);

        assert_eq!(
            es_import_attribute_clause(&block.imports[0]),
            Some("with { type: 'json' }")
        );
        assert_eq!(
            es_import_attribute_clause(&block.imports[1]),
            Some("assert { type: 'json' }")
        );
        assert_eq!(es_import_attribute_type(&block.imports[0]), Some("json"));
        assert_eq!(es_import_attribute_type(&block.imports[1]), Some("json"));
        assert_eq!(es_import_attribute_type(&block.imports[2]), Some("json"));
    }

    #[test]
    fn parse_ts_type_import() {
        let source = "import type { FC } from 'react';\n";
        let (_, block) = parse_ts(source);
        assert_eq!(block.imports.len(), 1);
        let imp = &block.imports[0];
        assert_eq!(imp.kind, ImportKind::Type);
        assert!(imp.names.contains(&"FC".to_string()));
        assert_eq!(imp.group, ImportGroup::External);
    }

    // --- Insertion point ---

    #[test]
    fn insertion_empty_file() {
        let source = "";
        let (_, block) = parse_ts(source);
        let (offset, _, _) =
            find_insertion_point(source, &block, ImportGroup::External, "react", false);
        assert_eq!(offset, 0);
    }

    #[test]
    fn insertion_alphabetical_within_group() {
        let source = "\
import { a } from 'alpha';
import { c } from 'charlie';
";
        let (_, block) = parse_ts(source);
        let (offset, _, _) =
            find_insertion_point(source, &block, ImportGroup::External, "bravo", false);
        // Should insert before 'charlie' (which starts at line 2)
        let before_charlie = source.find("import { c }").unwrap();
        assert_eq!(offset, before_charlie);
    }

    // --- Python parsing ---

    fn parse_py(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::Python).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::Python);
        (tree, block)
    }

    #[test]
    fn parse_py_import_statement() {
        let source = "import os\nimport sys\n";
        let (_, block) = parse_py(source);
        assert_eq!(block.imports.len(), 2);
        assert_eq!(block.imports[0].module_path, "os");
        assert_eq!(block.imports[1].module_path, "sys");
        assert_eq!(block.imports[0].group, ImportGroup::Stdlib);
    }

    #[test]
    fn parse_py_import_statement_preserves_aliases_and_siblings() {
        let source = "import alpha, beta as local_beta\n";
        let (_, block) = parse_py(source);
        let imp = &block.imports[0];
        assert_eq!(imp.module_path, "alpha");
        assert!(imp.names.is_empty());
        match &imp.form {
            ImportForm::Python { from_import, named } => {
                assert!(!from_import);
                assert_eq!(named, &["alpha", "beta as local_beta"]);
                assert!(specifier_matches(&named[1], "beta"));
                assert!(specifier_matches(&named[1], "local_beta"));
            }
            other => panic!("expected Python import, got {other:?}"),
        }
    }

    #[test]
    fn parse_py_from_import() {
        let source = "from collections import OrderedDict\nfrom typing import List, Optional\n";
        let (_, block) = parse_py(source);
        assert_eq!(block.imports.len(), 2);
        assert_eq!(block.imports[0].module_path, "collections");
        assert!(block.imports[0].names.contains(&"OrderedDict".to_string()));
        assert_eq!(block.imports[0].group, ImportGroup::Stdlib);
        assert_eq!(block.imports[1].module_path, "typing");
        assert!(block.imports[1].names.contains(&"List".to_string()));
        assert!(block.imports[1].names.contains(&"Optional".to_string()));
    }

    #[test]
    fn parse_py_from_import_preserves_aliases() {
        let source = "from module import alpha, beta as local_beta\n";
        let (_, block) = parse_py(source);
        let imp = &block.imports[0];
        assert_eq!(imp.names, ["alpha", "beta as local_beta"]);
        assert!(specifier_matches(&imp.names[1], "beta"));
        assert!(specifier_matches(&imp.names[1], "local_beta"));
        assert_eq!(
            imp.form,
            ImportForm::Python {
                from_import: true,
                named: vec!["alpha".to_string(), "beta as local_beta".to_string()],
            }
        );
    }

    #[test]
    fn parse_py_relative_import() {
        let source = "from . import utils\nfrom ..config import Settings\n";
        let (_, block) = parse_py(source);
        assert_eq!(block.imports.len(), 2);
        assert_eq!(block.imports[0].module_path, ".");
        assert!(block.imports[0].names.contains(&"utils".to_string()));
        assert_eq!(block.imports[0].group, ImportGroup::Internal);
        assert_eq!(block.imports[1].module_path, "..config");
        assert_eq!(block.imports[1].group, ImportGroup::Internal);
    }

    #[test]
    fn classify_py_groups() {
        assert_eq!(classify_group_py("os"), ImportGroup::Stdlib);
        assert_eq!(classify_group_py("sys"), ImportGroup::Stdlib);
        assert_eq!(classify_group_py("json"), ImportGroup::Stdlib);
        assert_eq!(classify_group_py("collections"), ImportGroup::Stdlib);
        assert_eq!(classify_group_py("os.path"), ImportGroup::Stdlib);
        assert_eq!(classify_group_py("requests"), ImportGroup::External);
        assert_eq!(classify_group_py("flask"), ImportGroup::External);
        assert_eq!(classify_group_py("."), ImportGroup::Internal);
        assert_eq!(classify_group_py("..config"), ImportGroup::Internal);
        assert_eq!(classify_group_py(".utils"), ImportGroup::Internal);
    }

    #[test]
    fn parse_py_three_groups() {
        let source = "import os\nimport sys\n\nimport requests\n\nfrom . import utils\n";
        let (_, block) = parse_py(source);
        let stdlib: Vec<_> = block
            .imports
            .iter()
            .filter(|i| i.group == ImportGroup::Stdlib)
            .collect();
        let external: Vec<_> = block
            .imports
            .iter()
            .filter(|i| i.group == ImportGroup::External)
            .collect();
        let internal: Vec<_> = block
            .imports
            .iter()
            .filter(|i| i.group == ImportGroup::Internal)
            .collect();
        assert_eq!(stdlib.len(), 2);
        assert_eq!(external.len(), 1);
        assert_eq!(internal.len(), 1);
    }

    #[test]
    fn generate_py_import() {
        let line = generate_import_line(LangId::Python, "os", &[], None, false);
        assert_eq!(line, "import os");
    }

    #[test]
    fn generate_py_from_import() {
        let line = generate_import_line(
            LangId::Python,
            "collections",
            &["OrderedDict".to_string()],
            None,
            false,
        );
        assert_eq!(line, "from collections import OrderedDict");
    }

    #[test]
    fn generate_py_from_import_multiple() {
        let line = generate_import_line(
            LangId::Python,
            "typing",
            &["Optional".to_string(), "List".to_string()],
            None,
            false,
        );
        assert_eq!(line, "from typing import List, Optional");
    }

    // --- Rust parsing ---

    fn parse_rust(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::Rust).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::Rust);
        (tree, block)
    }

    #[test]
    fn parse_rs_use_std() {
        let source = "use std::collections::HashMap;\nuse std::io::Read;\n";
        let (_, block) = parse_rust(source);
        assert_eq!(block.imports.len(), 2);
        assert_eq!(block.imports[0].module_path, "std::collections::HashMap");
        assert_eq!(block.imports[0].group, ImportGroup::Stdlib);
        assert_eq!(block.imports[1].group, ImportGroup::Stdlib);
    }

    #[test]
    fn parse_rs_use_external() {
        let source = "use serde::{Deserialize, Serialize};\n";
        let (_, block) = parse_rust(source);
        assert_eq!(block.imports.len(), 1);
        assert_eq!(block.imports[0].group, ImportGroup::External);
        assert!(block.imports[0].names.contains(&"Deserialize".to_string()));
        assert!(block.imports[0].names.contains(&"Serialize".to_string()));
    }

    #[test]
    fn parse_rs_use_crate() {
        let source = "use crate::config::Settings;\nuse super::parent::Thing;\n";
        let (_, block) = parse_rust(source);
        assert_eq!(block.imports.len(), 2);
        assert_eq!(block.imports[0].group, ImportGroup::Internal);
        assert_eq!(block.imports[1].group, ImportGroup::Internal);
    }

    #[test]
    fn parse_rs_pub_use() {
        let source = "pub use super::parent::Thing;\n";
        let (_, block) = parse_rust(source);
        assert_eq!(block.imports.len(), 1);
        // `pub` is stored in default_import as a marker
        assert_eq!(block.imports[0].default_import.as_deref(), Some("pub"));
    }

    #[test]
    fn classify_rs_groups() {
        assert_eq!(
            classify_group_rs("std::collections::HashMap"),
            ImportGroup::Stdlib
        );
        assert_eq!(classify_group_rs("core::mem"), ImportGroup::Stdlib);
        assert_eq!(classify_group_rs("alloc::vec"), ImportGroup::Stdlib);
        assert_eq!(
            classify_group_rs("serde::Deserialize"),
            ImportGroup::External
        );
        assert_eq!(classify_group_rs("tokio::runtime"), ImportGroup::External);
        assert_eq!(classify_group_rs("crate::config"), ImportGroup::Internal);
        assert_eq!(classify_group_rs("self::utils"), ImportGroup::Internal);
        assert_eq!(classify_group_rs("super::parent"), ImportGroup::Internal);
    }

    #[test]
    fn generate_rs_use() {
        let line = generate_import_line(LangId::Rust, "std::fmt::Display", &[], None, false);
        assert_eq!(line, "use std::fmt::Display;");
    }

    // --- Go parsing ---

    fn parse_go(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::Go).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::Go);
        (tree, block)
    }

    #[test]
    fn parse_go_single_import() {
        let source = "package main\n\nimport \"fmt\"\n";
        let (_, block) = parse_go(source);
        assert_eq!(block.imports.len(), 1);
        assert_eq!(block.imports[0].module_path, "fmt");
        assert_eq!(block.imports[0].group, ImportGroup::Stdlib);
    }

    #[test]
    fn parse_go_grouped_import() {
        let source =
            "package main\n\nimport (\n\t\"fmt\"\n\t\"os\"\n\n\t\"github.com/pkg/errors\"\n)\n";
        let (_, block) = parse_go(source);
        assert_eq!(block.imports.len(), 3);
        assert_eq!(block.imports[0].module_path, "fmt");
        assert_eq!(block.imports[0].group, ImportGroup::Stdlib);
        assert_eq!(block.imports[1].module_path, "os");
        assert_eq!(block.imports[1].group, ImportGroup::Stdlib);
        assert_eq!(block.imports[2].module_path, "github.com/pkg/errors");
        assert_eq!(block.imports[2].group, ImportGroup::External);
    }

    #[test]
    fn parse_go_mixed_imports() {
        // Single + grouped
        let source = "package main\n\nimport \"fmt\"\n\nimport (\n\t\"os\"\n\t\"github.com/pkg/errors\"\n)\n";
        let (_, block) = parse_go(source);
        assert_eq!(block.imports.len(), 3);
    }

    #[test]
    fn classify_go_groups() {
        assert_eq!(classify_group_go("fmt"), ImportGroup::Stdlib);
        assert_eq!(classify_group_go("os"), ImportGroup::Stdlib);
        assert_eq!(classify_group_go("net/http"), ImportGroup::Stdlib);
        assert_eq!(classify_group_go("encoding/json"), ImportGroup::Stdlib);
        assert_eq!(
            classify_group_go("github.com/pkg/errors"),
            ImportGroup::External
        );
        assert_eq!(
            classify_group_go("golang.org/x/tools"),
            ImportGroup::External
        );
    }

    #[test]
    fn generate_go_standalone() {
        let line = generate_go_import_line("fmt", None, false);
        assert_eq!(line, "import \"fmt\"");
    }

    #[test]
    fn generate_go_grouped_spec() {
        let line = generate_go_import_line("fmt", None, true);
        assert_eq!(line, "\t\"fmt\"");
    }

    #[test]
    fn generate_go_with_alias() {
        let line = generate_go_import_line("github.com/pkg/errors", Some("errs"), false);
        assert_eq!(line, "import errs \"github.com/pkg/errors\"");
    }

    // --- Solidity ---

    fn parse_solidity(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::Solidity).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::Solidity);
        (tree, block)
    }

    /// Grammar fixture (council #6): lock the tree-sitter-solidity node kinds the
    /// parser depends on. If the grammar updates and renames these, this test
    /// fails loudly before the parser silently mis-parses.
    #[test]
    fn solidity_grammar_node_kinds_are_stable() {
        let grammar = grammar_for(LangId::Solidity).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let src = "import { Foo, Bar as Baz } from \"./A.sol\";\nimport * as N from \"./B.sol\";\nimport \"./C.sol\" as C;\nimport \"./D.sol\";\n";
        let tree = parser.parse(src, None).unwrap();
        let mut kinds: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        fn walk(node: tree_sitter::Node, kinds: &mut std::collections::BTreeSet<String>) {
            kinds.insert(node.kind().to_string());
            let mut c = node.walk();
            if c.goto_first_child() {
                loop {
                    walk(c.node(), kinds);
                    if !c.goto_next_sibling() {
                        break;
                    }
                }
            }
        }
        walk(tree.root_node(), &mut kinds);
        for required in [
            "import_directive",
            "string",
            "identifier",
            "as",
            "from",
            "*",
            "{",
            "}",
        ] {
            assert!(
                kinds.contains(required),
                "solidity grammar missing node kind {required:?}; present: {kinds:?}"
            );
        }
    }

    #[test]
    fn solidity_string_path_strips_one_matching_quote_pair() {
        assert_eq!(
            strip_one_matching_solidity_quote_pair("\"./A.sol\""),
            "./A.sol"
        );
        assert_eq!(
            strip_one_matching_solidity_quote_pair("'./A.sol'"),
            "./A.sol"
        );
        assert_eq!(
            strip_one_matching_solidity_quote_pair("\"./a'b.sol\""),
            "./a'b.sol"
        );
        assert_eq!(
            strip_one_matching_solidity_quote_pair("\"\"./A.sol\"\""),
            "\"./A.sol\""
        );
        assert_eq!(
            strip_one_matching_solidity_quote_pair("'./A.sol\""),
            "'./A.sol\""
        );
        assert_eq!(
            strip_one_matching_solidity_quote_pair("\"./A.sol"),
            "\"./A.sol"
        );
        assert_eq!(strip_one_matching_solidity_quote_pair("\""), "\"");
    }

    #[test]
    fn parse_solidity_all_four_forms() {
        let (_, block) = parse_solidity(
            "import \"./A.sol\";\nimport \"./B.sol\" as B;\nimport * as C from \"./C.sol\";\nimport { Foo, Bar as Baz } from \"./D.sol\";\nimport './E.sol';\nimport './F.sol' as F;\nimport * as G from './G.sol';\nimport { Quux, Corge as Grault } from './H.sol';\n",
        );
        assert_eq!(block.imports.len(), 8);

        // side-effect
        assert_eq!(block.imports[0].module_path, "./A.sol");
        assert_eq!(block.imports[0].kind, ImportKind::SideEffect);
        assert_eq!(
            block.imports[0].form,
            ImportForm::Solidity {
                named: vec![],
                namespace: None,
                alias: None
            }
        );

        // whole-file alias
        assert_eq!(
            block.imports[1].form,
            ImportForm::Solidity {
                named: vec![],
                namespace: None,
                alias: Some("B".to_string())
            }
        );

        // namespace
        match &block.imports[2].form {
            ImportForm::Solidity { namespace, .. } => assert_eq!(namespace.as_deref(), Some("C")),
            other => panic!("expected Solidity namespace, got {other:?}"),
        }
        assert_eq!(block.imports[2].namespace_import.as_deref(), Some("C"));

        // named with alias (verbatim specifier convention)
        match &block.imports[3].form {
            ImportForm::Solidity { named, .. } => {
                assert_eq!(named, &vec!["Foo".to_string(), "Bar as Baz".to_string()]);
            }
            other => panic!("expected Solidity named, got {other:?}"),
        }
        assert_eq!(
            block.imports[3].names,
            vec!["Foo".to_string(), "Bar as Baz".to_string()]
        );

        assert_eq!(
            block.imports[4..]
                .iter()
                .map(|import| import.module_path.as_str())
                .collect::<Vec<_>>(),
            ["./E.sol", "./F.sol", "./G.sol", "./H.sol"]
        );
        assert_eq!(block.imports[4].kind, ImportKind::SideEffect);
        assert!(matches!(
            &block.imports[5].form,
            ImportForm::Solidity {
                alias: Some(alias),
                ..
            } if alias == "F"
        ));
        assert_eq!(block.imports[6].namespace_import.as_deref(), Some("G"));
        assert_eq!(
            block.imports[7].names,
            vec!["Quux".to_string(), "Corge as Grault".to_string()]
        );
    }

    #[test]
    fn generate_solidity_all_forms() {
        // side-effect
        assert_eq!(
            generate_import(
                LangId::Solidity,
                &ImportRequest::legacy("./A.sol", &[], None, None, false)
            ),
            "import \"./A.sol\";"
        );
        // named
        let names = vec!["Foo".to_string(), "Bar as Baz".to_string()];
        assert_eq!(
            generate_import(
                LangId::Solidity,
                &ImportRequest::legacy("./D.sol", &names, None, None, false)
            ),
            "import { Foo, Bar as Baz } from \"./D.sol\";"
        );
        // namespace
        assert_eq!(
            generate_import(
                LangId::Solidity,
                &ImportRequest::legacy("./C.sol", &[], None, Some("C"), false)
            ),
            "import * as C from \"./C.sol\";"
        );
        // whole-file alias
        assert_eq!(
            generate_import(
                LangId::Solidity,
                &ImportRequest {
                    module_path: "./B.sol",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: Some("B"),
                    type_only: false,
                    modifiers: &[],
                    import_kind: None,
                }
            ),
            "import \"./B.sol\" as B;"
        );
    }

    #[test]
    fn solidity_round_trips_through_parse_generate() {
        // Every generated form must parse back to the same structured shape.
        for src in [
            "import \"./A.sol\";",
            "import \"./B.sol\" as B;",
            "import * as C from \"./C.sol\";",
            "import { Foo, Bar as Baz } from \"./D.sol\";",
        ] {
            let (_, block) = parse_solidity(src);
            assert_eq!(block.imports.len(), 1, "parse {src:?}");
            let imp = &block.imports[0];
            let (namespace, alias) = match &imp.form {
                ImportForm::Solidity {
                    namespace, alias, ..
                } => (namespace.as_deref(), alias.as_deref()),
                other => panic!("expected Solidity, got {other:?}"),
            };
            let regenerated = generate_import(
                LangId::Solidity,
                &ImportRequest {
                    module_path: &imp.module_path,
                    names: &imp.names,
                    default_import: None,
                    namespace,
                    alias,
                    type_only: false,
                    modifiers: &[],
                    import_kind: None,
                },
            );
            assert_eq!(regenerated, src, "round-trip mismatch for {src:?}");
        }
    }

    #[test]
    fn classify_group_solidity_relative_vs_external() {
        assert_eq!(classify_group_solidity("./A.sol"), ImportGroup::Internal);
        assert_eq!(
            classify_group_solidity("../lib/B.sol"),
            ImportGroup::Internal
        );
        assert_eq!(
            classify_group_solidity("@openzeppelin/contracts/token/ERC20/ERC20.sol"),
            ImportGroup::External
        );
    }
}

#[cfg(test)]
mod audit11_regressions {
    use super::*;

    fn inserted(
        source: &str,
        lang: LangId,
        module: &str,
        group: ImportGroup,
        line: &str,
    ) -> Vec<u8> {
        let tree = crate::parser::parse_source_with_cached_parser(
            std::path::Path::new("fixture"),
            source,
            lang,
        )
        .unwrap();
        let block = parse_imports(source, &tree, lang);
        let (offset, before, after) =
            find_insertion_point_with_prologue(source, &tree, lang, &block, group, module, false);
        let text = import_insertion_text(source, offset, line, before, after);
        let updated = crate::edit::replace_byte_range(source, offset, offset, &text).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let config = crate::config::Config {
            format_on_edit: false,
            ..Default::default()
        };
        crate::edit::write_format_validate(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            &path,
            &updated,
            &config,
            &serde_json::json!({}),
        )
        .unwrap();
        std::fs::read(path).unwrap()
    }

    #[test]
    fn imports_preserve_crlf_bom_and_unterminated_eof() {
        let source = "\u{feff}import a from 'a';\r\nimport b from 'b';";
        assert_eq!(
            inserted(
                source,
                LangId::TypeScript,
                "z",
                ImportGroup::External,
                "import z from 'z';"
            ),
            "\u{feff}import a from 'a';\r\nimport b from 'b';\r\nimport z from 'z';\r\n".as_bytes()
        );
    }

    #[test]
    fn import_insertion_preserves_directives_docstrings_and_future_order() {
        assert_eq!(
            inserted(
                "\u{feff}'use client';\r\nrun();\r\n",
                LangId::TypeScript,
                "x",
                ImportGroup::External,
                "import x from 'x';"
            ),
            "\u{feff}'use client';\r\nimport x from 'x';\r\n\r\nrun();\r\n".as_bytes()
        );
        assert_eq!(inserted("#!/usr/bin/python\r\n# coding: utf-8\r\n\"\"\"module doc\"\"\"\r\nrun()\r\n", LangId::Python, "__future__", ImportGroup::Stdlib, "from __future__ import annotations"),
            b"#!/usr/bin/python\r\n# coding: utf-8\r\n\"\"\"module doc\"\"\"\r\nfrom __future__ import annotations\r\n\r\nrun()\r\n");
        assert_eq!(
            inserted(
                "\"doc\"\nfrom __future__ import annotations\nrun()\n",
                LangId::Python,
                "x",
                ImportGroup::External,
                "import x"
            ),
            b"\"doc\"\nfrom __future__ import annotations\n\nimport x\n\nrun()\n"
        );
    }

    fn first_ts_import(source: &str) -> String {
        String::from_utf8(inserted(
            source,
            LangId::TypeScript,
            "x",
            ImportGroup::External,
            "import x from 'x';",
        ))
        .unwrap()
    }

    fn first_py_import(source: &str) -> String {
        String::from_utf8(inserted(
            source,
            LangId::Python,
            "x",
            ImportGroup::External,
            "import x",
        ))
        .unwrap()
    }

    #[test]
    fn first_import_goes_before_a_jsdoc_attached_to_the_first_declaration() {
        assert_eq!(
            first_ts_import("/** Does x */\nexport function foo() {}\n"),
            "import x from 'x';\n\n/** Does x */\nexport function foo() {}\n"
        );
        assert_eq!(
            first_ts_import("/** Does x */\r\nexport function foo() {}\r\n"),
            "import x from 'x';\r\n\r\n/** Does x */\r\nexport function foo() {}\r\n"
        );
        // A license separated by a blank line is still skipped; the JSDoc
        // below it stays attached to its declaration.
        assert_eq!(
            first_ts_import("/* license */\n\n/** Does x */\nexport function foo() {}\n"),
            "/* license */\nimport x from 'x';\n\n/** Does x */\nexport function foo() {}\n"
        );
    }

    #[test]
    fn first_import_skips_a_license_followed_by_a_blank_line() {
        assert_eq!(
            first_ts_import("/* license */\n\nrun();\n"),
            "/* license */\nimport x from 'x';\n\nrun();\n"
        );
        assert_eq!(
            first_ts_import("/* license */\r\n\r\nrun();\r\n"),
            "/* license */\r\nimport x from 'x';\r\n\r\nrun();\r\n"
        );
    }

    #[test]
    fn first_import_follows_a_license_and_adjacent_directive() {
        assert_eq!(
            first_ts_import("/* license */\n'use client';\nrun();\n"),
            "/* license */\n'use client';\nimport x from 'x';\n\nrun();\n"
        );
    }

    #[test]
    fn first_python_import_goes_before_a_comment_attached_to_def() {
        assert_eq!(
            first_py_import("# helper for foo\ndef foo():\n    pass\n"),
            "import x\n\n# helper for foo\ndef foo():\n    pass\n"
        );
        assert_eq!(
            first_py_import("# helper for foo\r\ndef foo():\r\n    pass\r\n"),
            "import x\r\n\r\n# helper for foo\r\ndef foo():\r\n    pass\r\n"
        );
    }

    #[test]
    fn first_python_import_stays_below_an_encoding_cookie() {
        // PEP 263 only honours the cookie on line 1 or 2, so it must stay
        // above the import even when code follows it directly.
        assert_eq!(
            first_py_import("# -*- coding: utf-8 -*-\ndef foo():\n    pass\n"),
            "# -*- coding: utf-8 -*-\nimport x\n\ndef foo():\n    pass\n"
        );
        assert_eq!(
            first_py_import("# -*- coding: utf-8 -*-\r\ndef foo():\r\n    pass\r\n"),
            "# -*- coding: utf-8 -*-\r\nimport x\r\n\r\ndef foo():\r\n    pass\r\n"
        );
    }

    #[test]
    fn include_guard_insertion_preserves_conditional_context() {
        let source = "/* license */\r\n#ifndef X_H\r\n#define X_H\r\n#include <stdio.h>\r\n#ifdef OPTIONAL\r\n#include <optional.h>\r\n#endif\r\n#endif\r\n";
        let tree = crate::parser::parse_source_with_cached_parser(
            std::path::Path::new("x.h"),
            source,
            LangId::C,
        )
        .unwrap();
        let imports = parse_imports(source, &tree, LangId::C);
        assert_eq!(imports.imports.len(), 2);
        let default_block = c_insertion_block(source, &tree, &imports);
        assert_eq!(default_block.imports.len(), 1);
        assert_eq!(default_block.imports[0].module_path, "stdio.h");
        assert_eq!(inserted(source, LangId::C, "string.h", ImportGroup::Stdlib, "#include <string.h>"),
            b"/* license */\r\n#ifndef X_H\r\n#define X_H\r\n#include <stdio.h>\r\n#include <string.h>\r\n#ifdef OPTIONAL\r\n#include <optional.h>\r\n#endif\r\n#endif\r\n");
        assert_eq!(
            inserted(
                "/* license */\n#ifndef X_H\n#define X_H\n#endif\n",
                LangId::C,
                "stdio.h",
                ImportGroup::Stdlib,
                "#include <stdio.h>"
            ),
            b"/* license */\n#ifndef X_H\n#define X_H\n#include <stdio.h>\n#endif\n"
        );
    }

    #[test]
    fn platform_and_recent_python_modules_are_stdlib() {
        for module in [
            "zoneinfo", "graphlib", "msvcrt", "winreg", "winsound", "posix", "ntpath",
        ] {
            assert_eq!(classify_group_py(module), ImportGroup::Stdlib, "{module}");
            assert_eq!(
                classify_group_py(&format!("{module}.child")),
                ImportGroup::Stdlib
            );
        }
        assert_eq!(classify_group_py("third_party"), ImportGroup::External);
        assert_eq!(classify_group_py(".zoneinfo"), ImportGroup::Internal);
    }

    #[test]
    fn stdlib_membership_uses_logarithmic_comparisons() {
        PYTHON_STDLIB_COMPARISONS.with(|count| count.set(0));
        assert_eq!(classify_group_py("zzzz_non_stdlib"), ImportGroup::External);
        assert!(PYTHON_STDLIB_COMPARISONS.with(|count| count.get()) <= 10);
        assert!(PYTHON_STDLIB.windows(2).all(|pair| pair[0] < pair[1]));
    }
}

#[cfg(test)]
pub mod quotes;

#[cfg(test)]
pub mod c;

#[cfg(test)]
pub mod csharp;

#[cfg(test)]
pub mod java;

#[cfg(test)]
pub mod kotlin;

#[cfg(test)]
pub mod lua;

#[cfg(test)]
pub mod perl;

#[cfg(test)]
pub mod php;

#[cfg(test)]
pub mod ruby;

#[cfg(test)]
pub mod scala;

#[cfg(test)]
pub mod swift;
