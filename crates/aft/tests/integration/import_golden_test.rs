//! Golden-parity characterization corpus for import operations — the Stream M
//! migration safety net (plan: imports-refactor-expansion-v2.md, council finding #1).
//!
//! Captures the CURRENT add/remove/organize output for the existing import
//! languages (TS/JS/TSX, Python, Rust, Go) byte-for-byte, BEFORE the
//! `ImportStatement -> ImportForm` / `ImportSyntax` refactor. The refactor must
//! keep every golden here unchanged — that is the proof the type redesign did
//! not silently regress a working language (e.g. Rust's `pub`-in-`default_import`
//! organize merge, or Go's alias-in-`default_import`).
//!
//! Workflow:
//!   - First capture / intentional update: `UPDATE_GOLDEN=1 cargo test -p \
//!     agent-file-tools --test integration import_golden_corpus`
//!   - Normal run / CI gate: asserts each scenario's final file content matches
//!     its committed `.golden` fixture exactly.
//!
//! Each scenario applies a deterministic op sequence to a small input file and
//! snapshots the resulting file content. One behavior per scenario keeps drift
//! diagnosis precise.

use super::helpers::{cargo_manifest_dir, AftProcess};
use std::fs;
use std::path::PathBuf;

/// A single import operation in a scenario's sequence.
enum Op {
    Add {
        module: &'static str,
        names: &'static [&'static str],
        default_import: Option<&'static str>,
        type_only: bool,
    },
    /// Add with the structured schema fields (ES/Solidity namespace + alias,
    /// Java/C# `modifiers`, PHP `import_kind`) — exercises the new schema params
    /// end-to-end through the command path.
    AddForm {
        module: &'static str,
        names: &'static [&'static str],
        namespace: Option<&'static str>,
        alias: Option<&'static str>,
        modifiers: &'static [&'static str],
        import_kind: Option<&'static str>,
    },
    AddNamespace {
        module: &'static str,
        names: &'static [&'static str],
        default_import: Option<&'static str>,
        namespace: &'static str,
        type_only: bool,
    },
    Remove {
        module: &'static str,
        /// `Some(name)` removes one named import; `None` removes the whole statement.
        name: Option<&'static str>,
    },
    Organize,
}

struct Scenario {
    /// Golden key — also the `.golden` fixture filename stem.
    name: &'static str,
    /// File extension that drives language detection (`ts`, `js`, `py`, `rs`, `go`).
    ext: &'static str,
    /// Initial source written to the input file.
    input: &'static str,
    /// Ops applied in order; the final file content is the captured golden.
    ops: &'static [Op],
}

fn golden_dir() -> PathBuf {
    cargo_manifest_dir().join("tests/integration/fixtures/import_golden")
}

/// Apply a scenario's op sequence to a fresh temp file and return the final content.
fn run_scenario(aft: &mut AftProcess, scenario: &Scenario) -> String {
    let dir = crate::test_helpers::tempdir();
    let file = dir.path().join(format!("input.{}", scenario.ext));
    fs::write(&file, scenario.input).expect("write input");
    let file_str = file.display().to_string();

    for (idx, op) in scenario.ops.iter().enumerate() {
        let mut params = match op {
            Op::Add {
                module,
                names,
                default_import,
                type_only,
            } => {
                let mut p = serde_json::json!({
                    "id": format!("{}-{}", scenario.name, idx),
                    "command": "add_import",
                    "file": file_str,
                    "module": module,
                });
                if !names.is_empty() {
                    p["names"] = serde_json::json!(names);
                }
                if let Some(def) = default_import {
                    p["default_import"] = serde_json::json!(def);
                }
                if *type_only {
                    p["type_only"] = serde_json::json!(true);
                }
                p
            }
            Op::AddForm {
                module,
                names,
                namespace,
                alias,
                modifiers,
                import_kind,
            } => {
                let mut p = serde_json::json!({
                    "id": format!("{}-{}", scenario.name, idx),
                    "command": "add_import",
                    "file": file_str,
                    "module": module,
                });
                if !names.is_empty() {
                    p["names"] = serde_json::json!(names);
                }
                if let Some(ns) = namespace {
                    p["namespace"] = serde_json::json!(ns);
                }
                if let Some(al) = alias {
                    p["alias"] = serde_json::json!(al);
                }
                if !modifiers.is_empty() {
                    p["modifiers"] = serde_json::json!(modifiers);
                }
                if let Some(kind) = import_kind {
                    p["import_kind"] = serde_json::json!(kind);
                }
                p
            }
            Op::AddNamespace {
                module,
                names,
                default_import,
                namespace,
                type_only,
            } => {
                let mut p = serde_json::json!({
                    "id": format!("{}-{}", scenario.name, idx),
                    "command": "add_import",
                    "file": file_str,
                    "module": module,
                    "namespace": namespace,
                });
                if !names.is_empty() {
                    p["names"] = serde_json::json!(names);
                }
                if let Some(def) = default_import {
                    p["default_import"] = serde_json::json!(def);
                }
                if *type_only {
                    p["type_only"] = serde_json::json!(true);
                }
                p
            }
            Op::Remove { module, name } => {
                let mut p = serde_json::json!({
                    "id": format!("{}-{}", scenario.name, idx),
                    "command": "remove_import",
                    "file": file_str,
                    "module": module,
                });
                if let Some(n) = name {
                    p["name"] = serde_json::json!(n);
                }
                p
            }
            Op::Organize => serde_json::json!({
                "id": format!("{}-{}", scenario.name, idx),
                "command": "organize_imports",
                "file": file_str,
            }),
        };
        params["validate"] = serde_json::json!("syntax");

        let resp = aft.send(&serde_json::to_string(&params).unwrap());
        // A hard error means the scenario itself is malformed — fail loudly so we
        // never bake a broken op into a golden. No-op successes (removed:false,
        // duplicate add) are legitimate and captured via the final content.
        assert_ne!(
            resp["success"], false,
            "scenario '{}' op {} returned an error: {resp:?}",
            scenario.name, idx
        );
    }

    fs::read_to_string(&file).expect("read final content")
}

fn scenarios() -> Vec<Scenario> {
    vec![
        // ---- TypeScript (ES engine) ----
        Scenario {
            name: "ts_add_named_merges_into_existing",
            ext: "ts",
            input: "import { useState } from \"react\";\n\nexport const x = 1;\n",
            ops: &[Op::Add {
                module: "react",
                names: &["useEffect"],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "ts_add_external_sorts_vs_internal",
            ext: "ts",
            input: "import { local } from \"./local\";\n\nexport const x = 1;\n",
            ops: &[Op::Add {
                module: "lodash",
                names: &["debounce"],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "ts_add_type_only",
            ext: "ts",
            input: "export const x = 1;\n",
            ops: &[Op::Add {
                module: "./types",
                names: &["Foo"],
                default_import: None,
                type_only: true,
            }],
        },
        Scenario {
            name: "ts_add_default",
            ext: "ts",
            input: "export const x = 1;\n",
            ops: &[Op::Add {
                module: "react",
                names: &[],
                default_import: Some("React"),
                type_only: false,
            }],
        },
        Scenario {
            name: "ts_add_named_with_alias",
            ext: "ts",
            input: "export const x = 1;\n",
            ops: &[Op::Add {
                module: "./util",
                names: &["original as renamed"],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "ts_remove_one_named",
            ext: "ts",
            input: "import { a, b, c } from \"mod\";\n\nexport const x = 1;\n",
            ops: &[Op::Remove {
                module: "mod",
                name: Some("b"),
            }],
        },
        Scenario {
            name: "ts_remove_entire",
            ext: "ts",
            input: "import { a } from \"mod\";\nimport { keep } from \"other\";\n\nexport const x = 1;\n",
            ops: &[Op::Remove {
                module: "mod",
                name: None,
            }],
        },
        Scenario {
            name: "ts_organize_mixed",
            ext: "ts",
            input: "import { z } from \"./z\";\nimport { a } from \"axios\";\nimport \"./side-effect\";\nimport { b } from \"./b\";\nimport { y } from \"zod\";\n\nexport const x = 1;\n",
            ops: &[Op::Organize],
        },
        Scenario {
            name: "ts_organize_preserves_side_effect_position",
            ext: "ts",
            input: "import { init } from \"./a\";\nimport \"./b\";\n\nexport const x = init;\n",
            ops: &[Op::Organize],
        },
        Scenario {
            name: "ts_organize_default_namespace",
            ext: "ts",
            input: "import React, * as NS from \"react\";\n\nexport const x = React;\n",
            ops: &[Op::Organize],
        },
        Scenario {
            name: "ts_remove_default_from_namespace",
            ext: "ts",
            input: "import React, * as NS from \"react\";\n\nexport const x = NS;\n",
            ops: &[Op::Remove {
                module: "react",
                name: Some("React"),
            }],
        },
        Scenario {
            name: "ts_add_default_namespace_dedupes_exact",
            ext: "ts",
            input: "import Foo, * as NS from \"mod\";\n\nexport const x = Foo || NS;\n",
            ops: &[Op::AddNamespace {
                module: "mod",
                names: &[],
                default_import: Some("Foo"),
                namespace: "NS",
                type_only: false,
            }],
        },
        Scenario {
            name: "ts_add_default_namespace_extends_plain_default",
            ext: "ts",
            input: "import Foo from \"mod\";\n\nexport const x = Foo;\n",
            ops: &[Op::AddNamespace {
                module: "mod",
                names: &[],
                default_import: Some("Foo"),
                namespace: "NS",
                type_only: false,
            }],
        },
        Scenario {
            name: "ts_remove_namespace_from_default_namespace",
            ext: "ts",
            input: "import Foo, * as NS from \"./mod\";\n\nexport const x = Foo;\n",
            ops: &[Op::Remove {
                module: "./mod",
                name: Some("NS"),
            }],
        },
        // ---- JavaScript (shares the ES engine — proves the JS dispatch path) ----
        Scenario {
            name: "js_add_named",
            ext: "js",
            input: "import { existing } from \"pkg\";\n\nexport const x = 1;\n",
            ops: &[Op::Add {
                module: "pkg",
                names: &["added"],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "js_organize_mixed",
            ext: "js",
            input: "import { b } from \"./b\";\nimport { a } from \"alpha\";\n\nexport const x = 1;\n",
            ops: &[Op::Organize],
        },
        // ---- Python ----
        Scenario {
            name: "py_add_from_import",
            ext: "py",
            input: "x = 1\n",
            ops: &[Op::Add {
                module: "os",
                names: &["path"],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "py_add_module_import",
            ext: "py",
            input: "x = 1\n",
            ops: &[Op::Add {
                module: "sys",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "py_remove_name",
            ext: "py",
            input: "from os import path, sep\n\nx = 1\n",
            ops: &[Op::Remove {
                module: "os",
                name: Some("sep"),
            }],
        },
        Scenario {
            name: "py_remove_name_preserves_aliased_sibling",
            ext: "py",
            input: "from m import a, b as c\n\nvalue = c\n",
            ops: &[Op::Remove {
                module: "m",
                name: Some("a"),
            }],
        },
        Scenario {
            name: "py_remove_alias_by_local_name",
            ext: "py",
            input: "from m import a, b as c\n\nvalue = a\n",
            ops: &[Op::Remove {
                module: "m",
                name: Some("c"),
            }],
        },
        Scenario {
            name: "py_remove_plain_module_preserves_aliased_sibling",
            ext: "py",
            input: "import a, b as c\n\nvalue = c\n",
            ops: &[Op::Remove {
                module: "a",
                name: None,
            }],
        },
        Scenario {
            name: "py_add_merge_preserves_aliased_sibling",
            ext: "py",
            input: "from m import a, b as c\n\nvalue = c\n",
            ops: &[Op::Add {
                module: "m",
                names: &["d"],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "py_organize_grouped",
            ext: "py",
            input: "from . import local\nimport requests\nimport os\n\nx = 1\n",
            ops: &[Op::Organize],
        },
        // Multi-module `import os, sys` must keep BOTH modules (the structured
        // path used to drop `sys`). Raw-preserve keeps the line intact.
        Scenario {
            name: "py_organize_multi_module_no_data_loss",
            ext: "py",
            input: "import os, sys\nimport abc\n\nx = 1\n",
            ops: &[Op::Organize],
        },
        Scenario {
            name: "py_organize_alias_aware_dedup",
            ext: "py",
            input: "from m import a\nfrom m import a, b as c\n\nvalue = (a, c)\n",
            ops: &[Op::Organize],
        },
        Scenario {
            name: "py_organize_preserves_inter_import_comment",
            ext: "py",
            input: "import os\n# stdlib boundary\nimport sys\n\nx = 1\n",
            ops: &[Op::Organize],
        },
        // ---- Rust (the gnarliest: pub-in-default_import + use-tree merge) ----
        Scenario {
            name: "rs_add_use",
            ext: "rs",
            input: "fn main() {}\n",
            ops: &[Op::Add {
                module: "std::collections::HashMap",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "rs_add_named_use",
            ext: "rs",
            input: "fn main() {}\n",
            ops: &[Op::Add {
                module: "std::collections",
                names: &["HashMap", "HashSet"],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "rs_organize_merges_use_tree",
            ext: "rs",
            input: "use std::collections::HashMap;\nuse std::collections::BTreeMap;\n\nfn main() {}\n",
            ops: &[Op::Organize],
        },
        Scenario {
            name: "rs_organize_pub_use_preserved",
            ext: "rs",
            input: "pub use crate::a::Exported;\nuse crate::a::Internal;\n\nfn main() {}\n",
            ops: &[Op::Organize],
        },
        // Restricted visibilities must NOT be widened to bare `pub` on organize,
        // and must not merge across visibilities.
        Scenario {
            name: "rs_organize_restricted_visibility_preserved",
            ext: "rs",
            input: "pub(crate) use crate::a::CrateOnly;\npub(super) use crate::a::SuperOnly;\npub use crate::a::Public;\nuse crate::a::Private;\n\nfn main() {}\n",
            ops: &[Op::Organize],
        },
        Scenario {
            name: "rs_remove_use",
            ext: "rs",
            input: "use std::collections::HashMap;\nuse std::fmt::Debug;\n\nfn main() {}\n",
            ops: &[Op::Remove {
                module: "std::fmt::Debug",
                name: None,
            }],
        },
        // ---- Go (alias-in-default_import + grouped block) ----
        Scenario {
            name: "go_add_path",
            ext: "go",
            input: "package main\n\nfunc main() {}\n",
            ops: &[Op::Add {
                module: "fmt",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "go_organize_grouped",
            ext: "go",
            input: "package main\n\nimport (\n\t\"github.com/x/y\"\n\t\"fmt\"\n)\n\nfunc main() {}\n",
            ops: &[Op::Organize],
        },
        Scenario {
            name: "go_organize_mixed_grouped_and_standalone",
            ext: "go",
            input: "package main\n\nimport \"fmt\"\nimport (\n\t\"github.com/z/z\"\n)\n\nfunc main() {}\n",
            ops: &[Op::Organize],
        },
        Scenario {
            name: "go_add_standalone_in_mixed_imports",
            ext: "go",
            input: "package main\n\nimport \"fmt\"\nimport (\n\t\"github.com/z/z\"\n)\n\nfunc main() {}\n",
            ops: &[Op::Add {
                module: "os",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "go_remove_path",
            ext: "go",
            input: "package main\n\nimport (\n\t\"fmt\"\n\t\"os\"\n)\n\nfunc main() {}\n",
            ops: &[Op::Remove {
                module: "os",
                name: None,
            }],
        },
        // ---- Solidity (Phase 1: first new engine — captures NEW behavior) ----
        Scenario {
            name: "sol_add_named",
            ext: "sol",
            input: "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.0;\n\ncontract C {}\n",
            ops: &[Op::Add {
                module: "./Token.sol",
                names: &["ERC20", "IERC20 as IToken"],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "sol_add_side_effect",
            ext: "sol",
            input: "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.0;\n\ncontract C {}\n",
            ops: &[Op::Add {
                module: "@openzeppelin/contracts/utils/Context.sol",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "sol_add_spdx_only_keeps_license_first",
            ext: "sol",
            input: r#"// SPDX-License-Identifier: MIT
contract C {}
"#,
            ops: &[Op::Add {
                module: "./Dep.sol",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "sol_remove_named",
            ext: "sol",
            input: "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.0;\n\nimport { A, B } from \"./Lib.sol\";\n\ncontract C {}\n",
            ops: &[Op::Remove {
                module: "./Lib.sol",
                name: Some("B"),
            }],
        },
        Scenario {
            name: "sol_remove_entire",
            ext: "sol",
            input: "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.0;\n\nimport \"./Unused.sol\";\nimport { Keep } from \"./Keep.sol\";\n\ncontract C {}\n",
            ops: &[Op::Remove {
                module: "./Unused.sol",
                name: None,
            }],
        },
        Scenario {
            name: "sol_organize_named",
            ext: "sol",
            input: "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.0;\n\nimport { Z } from \"./Z.sol\";\nimport { A } from \"@openzeppelin/A.sol\";\n\ncontract C {}\n",
            ops: &[Op::Organize],
        },
        Scenario {
            name: "sol_add_namespace",
            ext: "sol",
            input: "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.0;\n\ncontract C {}\n",
            ops: &[Op::AddForm {
                module: "./Math.sol",
                names: &[],
                namespace: Some("Math"),
                alias: None,
                modifiers: &[],
                import_kind: None,
            }],
        },
        Scenario {
            name: "sol_add_whole_file_alias",
            ext: "sol",
            input: "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.0;\n\ncontract C {}\n",
            ops: &[Op::AddForm {
                module: "./Utils.sol",
                names: &[],
                namespace: None,
                alias: Some("Utils"),
                modifiers: &[],
                import_kind: None,
            }],
        },
        Scenario {
            name: "sol_add_alias_not_deduped_by_side_effect",
            ext: "sol",
            input: r#"// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import "./X.sol";

contract C {}
"#,
            ops: &[Op::AddForm {
                module: "./X.sol",
                names: &[],
                namespace: None,
                alias: Some("X"),
                modifiers: &[],
                import_kind: None,
            }],
        },
        // ---- Java (Phase 1 structured modifiers: static + wildcard) ----
        Scenario {
            name: "java_add_plain",
            ext: "java",
            input: "package com.example;\n\nimport java.io.File;\n\nclass C {}\n",
            ops: &[Op::Add {
                module: "java.util.List",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "java_add_static",
            ext: "java",
            input: "package com.example;\n\nimport java.util.List;\n\nclass C {}\n",
            ops: &[Op::AddForm {
                module: "java.util.Collections.emptyList",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &["static"],
                import_kind: None,
            }],
        },
        Scenario {
            name: "java_add_static_member_dedupes_existing",
            ext: "java",
            input: "package com.example;\n\nimport static java.util.Collections.emptyList;\n\nclass C {}\n",
            ops: &[Op::AddForm {
                module: "java.util.Collections",
                names: &["emptyList"],
                namespace: None,
                alias: None,
                modifiers: &["static"],
                import_kind: None,
            }],
        },
        Scenario {
            name: "java_add_wildcard",
            ext: "java",
            input: "package com.example;\n\nimport java.io.File;\n\nclass C {}\n",
            ops: &[Op::AddForm {
                module: "java.util",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &["wildcard"],
                import_kind: None,
            }],
        },
        // Multiple static members from one class: Java has no grouped import, so
        // each member must get its own `import static` statement. Regression for
        // the bug where >1 name fell through to the invalid `import static
        // java.util.Collections;` (dropping every member).
        Scenario {
            name: "java_add_static_multiple_members",
            ext: "java",
            input: "package com.example;\n\nimport java.util.List;\n\nclass C {}\n",
            ops: &[Op::AddForm {
                module: "java.util.Collections",
                names: &["emptyList", "singletonList"],
                namespace: None,
                alias: None,
                modifiers: &["static"],
                import_kind: None,
            }],
        },
        Scenario {
            name: "java_add_remove_static_member_round_trip",
            ext: "java",
            input: "package com.example;\n\nimport java.util.List;\n\nclass C {}\n",
            ops: &[
                Op::AddForm {
                    module: "java.util.Collections",
                    names: &["emptyList"],
                    namespace: None,
                    alias: None,
                    modifiers: &["static"],
                    import_kind: None,
                },
                Op::Remove {
                    module: "java.util.Collections",
                    name: Some("emptyList"),
                },
            ],
        },
        Scenario {
            name: "java_remove_static_member_keeps_sibling",
            ext: "java",
            input: "package com.example;\n\nimport static java.util.Collections.emptyList;\nimport static java.util.Collections.singletonList;\nimport java.util.List;\n\nclass C {}\n",
            ops: &[Op::Remove {
                module: "java.util.Collections",
                name: Some("emptyList"),
            }],
        },
        Scenario {
            name: "java_remove_static_member_keeps_wildcard",
            ext: "java",
            input: "package com.example;\n\nimport static java.util.Collections.*;\nimport static java.util.Collections.emptyList;\n\nclass C {}\n",
            ops: &[Op::Remove {
                module: "java.util.Collections",
                name: Some("emptyList"),
            }],
        },
        Scenario {
            name: "java_remove_type_removes_static_members_and_exact_import",
            ext: "java",
            input: "package com.example;\n\nimport static java.util.Collections.emptyList;\nimport static java.util.Collections.singletonList;\nimport java.util.Collections;\nimport java.util.List;\n\nclass C {}\n",
            ops: &[Op::Remove {
                module: "java.util.Collections",
                name: None,
            }],
        },
        Scenario {
            name: "java_add_static_wildcard",
            ext: "java",
            input: "package com.example;\n\nimport java.util.List;\n\nclass C {}\n",
            ops: &[Op::AddForm {
                module: "java.util.Arrays",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &["static", "wildcard"],
                import_kind: None,
            }],
        },
        Scenario {
            name: "java_remove_plain",
            ext: "java",
            input: "package com.example;\n\nimport java.io.File;\nimport java.util.List;\n\nclass C {}\n",
            ops: &[Op::Remove {
                module: "java.io.File",
                name: None,
            }],
        },
        Scenario {
            name: "java_organize_preserves_static_and_wildcard",
            ext: "java",
            input: r#"package com.example;

import org.example.Zed;
import static java.util.Collections.emptyList;
import java.util.*;
import static java.util.Arrays.*;
import java.io.File;

class C {}
"#,
            ops: &[Op::Organize],
        },
        Scenario {
            name: "java_organize_dedupes_whitespace_variant",
            ext: "java",
            input: "package com.example;\n\nimport java.util.List;\nimport   java.util.List;\n\nclass C {}\n",
            ops: &[Op::Organize],
        },
        // ---- Kotlin (structured alias + wildcard imports) ----
        Scenario {
            name: "kt_add_plain_sorted",
            ext: "kt",
            input: "package com.example\n\nimport com.zeta.Z\n\nfun main() {}\n",
            ops: &[Op::Add {
                module: "com.alpha.A",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "kt_add_wildcard",
            ext: "kt",
            input: "package com.example\n\nimport kotlin.collections.List\n\nfun main() {}\n",
            ops: &[Op::AddForm {
                module: "kotlin.math",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &["wildcard"],
                import_kind: None,
            }],
        },
        Scenario {
            name: "kt_add_alias",
            ext: "kt",
            input: "package com.example\n\nimport com.example.Existing\n\nfun main() {}\n",
            ops: &[Op::AddForm {
                module: "com.example.Original",
                names: &[],
                namespace: None,
                alias: Some("Renamed"),
                modifiers: &[],
                import_kind: None,
            }],
        },
        Scenario {
            name: "kt_remove_import",
            ext: "kt",
            input: "package com.example\n\nimport com.example.Keep\nimport com.example.Unused\n\nfun main() {}\n",
            ops: &[Op::Remove {
                module: "com.example.Unused",
                name: None,
            }],
        },
        Scenario {
            name: "kt_organize_mixed",
            ext: "kt",
            input: "package com.example\n\nimport com.zeta.Z as Last\nimport kotlin.math.*\nimport com.alpha.A\n\nfun main() {}\n",
            ops: &[Op::Organize],
        },
        Scenario {
            name: "csharp_add_plain_using",
            ext: "cs",
            input: "namespace App;\n\nclass C {}\n",
            ops: &[Op::Add {
                module: "System",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "csharp_add_static_using",
            ext: "cs",
            input: "namespace App;\n\nclass C {}\n",
            ops: &[Op::AddForm {
                module: "System.Math",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &["static"],
                import_kind: None,
            }],
        },
        Scenario {
            name: "csharp_add_alias_using",
            ext: "cs",
            input: "namespace App;\n\nclass C {}\n",
            ops: &[Op::AddForm {
                module: "System.Console",
                names: &[],
                namespace: None,
                alias: Some("Con"),
                modifiers: &[],
                import_kind: None,
            }],
        },
        Scenario {
            name: "csharp_add_global_using",
            ext: "cs",
            input: "namespace App;\n\nclass C {}\n",
            ops: &[Op::AddForm {
                module: "System",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &["global"],
                import_kind: None,
            }],
        },
        Scenario {
            name: "csharp_add_global_static_using",
            ext: "cs",
            input: "namespace App;\n\nclass C {}\n",
            ops: &[Op::AddForm {
                module: "System.Math",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &["global", "static"],
                import_kind: None,
            }],
        },
        Scenario {
            name: "csharp_add_with_in_namespace_comment",
            ext: "cs",
            input: "namespace App {\nusing System;\n// keep using section note\nusing System.Text;\n\nclass C {}\n}\n",
            ops: &[Op::Add {
                module: "System.Collections",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "csharp_remove_with_in_namespace_comment",
            ext: "cs",
            input: "namespace App {\nusing System;\n// keep using section note\nusing System.Text;\nusing System.Xml;\n\nclass C {}\n}\n",
            ops: &[Op::Remove {
                module: "System.Text",
                name: None,
            }],
        },
        Scenario {
            name: "csharp_remove_using",
            ext: "cs",
            input: "using System;\nusing System.Text;\n\nnamespace App;\n\nclass C {}\n",
            ops: &[Op::Remove {
                module: "System.Text",
                name: None,
            }],
        },
        Scenario {
            name: "csharp_organize_preserves_global_static_alias",
            ext: "cs",
            input: r#"using Newtonsoft.Json;
global using System;
using static System.Math;
using Con = System.Console;
global using static System.Linq.Enumerable;

namespace App;

class C {}
"#,
            ops: &[Op::Organize],
        },
        Scenario {
            name: "php_add_plain_use",
            ext: "php",
            input: "<?php\n\nnamespace Demo;\n\nuse App\\Existing;\n\nclass C {}\n",
            ops: &[Op::Add {
                module: "App\\Foo",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "php_add_fully_qualified_use",
            ext: "php",
            input: "<?php\n\nclass C {}\n",
            ops: &[Op::Add {
                module: "\\Vendor\\Pkg\\Thing",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "php_add_alias",
            ext: "php",
            input: "<?php\n\nnamespace Demo;\n\nuse App\\Existing;\n\nclass C {}\n",
            ops: &[Op::AddForm {
                module: "App\\Foo",
                names: &[],
                namespace: None,
                alias: Some("Bar"),
                modifiers: &[],
                import_kind: None,
            }],
        },
        Scenario {
            name: "php_add_function",
            ext: "php",
            input: "<?php\n\nnamespace Demo;\n\nuse App\\Existing;\n\nclass C {}\n",
            ops: &[Op::AddForm {
                module: "App\\helper",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &[],
                import_kind: Some("function"),
            }],
        },
        Scenario {
            name: "php_add_const",
            ext: "php",
            input: "<?php\n\nnamespace Demo;\n\nuse App\\Existing;\n\nclass C {}\n",
            ops: &[Op::AddForm {
                module: "App\\VERSION",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &[],
                import_kind: Some("const"),
            }],
        },
        Scenario {
            name: "php_add_with_in_namespace_comment",
            ext: "php",
            input: "<?php\n\nnamespace Demo;\n\nuse App\\Alpha;\n// keep use section note\nuse App\\Zulu;\n\nclass C {}\n",
            ops: &[Op::Add {
                module: "App\\Beta",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "php_remove_with_in_namespace_comment",
            ext: "php",
            input: "<?php\n\nnamespace Demo;\n\nuse App\\Alpha;\n// keep use section note\nuse App\\Unused;\nuse App\\Zulu;\n\nclass C {}\n",
            ops: &[Op::Remove {
                module: "App\\Unused",
                name: None,
            }],
        },
        Scenario {
            name: "php_remove_entire",
            ext: "php",
            input: "<?php\n\nnamespace Demo;\n\nuse App\\Unused;\nuse App\\Keep;\n\nclass C {}\n",
            ops: &[Op::Remove {
                module: "App\\Unused",
                name: None,
            }],
        },
        Scenario {
            name: "php_remove_comma_separated_middle",
            ext: "php",
            input: "<?php\n\nnamespace Demo;\n\nuse App\\First, App\\Unused as Old, App\\Keep;\n\nclass C { public Keep $keep; }\n",
            ops: &[Op::Remove {
                module: "App\\Unused",
                name: None,
            }],
        },
        Scenario {
            name: "php_organize_preserves_grouped_and_kind_uses",
            ext: "php",
            input: r#"<?php

namespace Demo;

use Zed\Last;
use App\{Grouped, Other as Alias};
use function App\helper;
use App\Alpha;
use const App\VERSION;

class C {}
"#,
            ops: &[Op::Organize],
        },
        // ---- Scala (Scala 2/3 import forms: wildcard, selectors, rename, given) ----
        Scenario {
            name: "scala_add_single",
            ext: "scala",
            input: "object Main { val x = 1 }\n",
            ops: &[Op::Add {
                module: "scala.collection.mutable.ListBuffer",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "scala_add_wildcard",
            ext: "scala",
            input: "object Main { val x = 1 }\n",
            ops: &[Op::AddForm {
                module: "cats.syntax.all",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &["wildcard"],
                import_kind: None,
            }],
        },
        Scenario {
            name: "scala_add_selector",
            ext: "scala",
            input: "object Main { val x = 1 }\n",
            ops: &[Op::Add {
                module: "cats.effect",
                names: &["IO", "Resource"],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "scala_add_rename",
            ext: "scala",
            input: "object Main { val x = 1 }\n",
            ops: &[Op::Add {
                module: "scala.concurrent",
                names: &["ExecutionContext as EC"],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "scala_add_given",
            ext: "scala",
            input: "object Main { val x = 1 }\n",
            ops: &[Op::AddForm {
                module: "cats.effect.kernel",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &[],
                import_kind: Some("given"),
            }],
        },
        Scenario {
            name: "scala_remove_selector_name",
            ext: "scala",
            input: "import cats.effect.{IO, Resource}\n\nobject Main { val x = 1 }\n",
            ops: &[Op::Remove {
                module: "cats.effect",
                name: Some("Resource"),
            }],
        },
        Scenario {
            name: "scala_organize_mixed",
            ext: "scala",
            input: "import cats.syntax.all._\nimport scala.util.Try\nimport cats.effect.{Resource, IO}\nimport cats.syntax.all.*\nimport cats.effect.kernel.given\n\nobject Main { val x = 1 }\n",
            ops: &[Op::Organize],
        },
        // Regression: organize must NOT rewrite Scala 2 syntax into Scala 3.
        // `import a.b._` must stay `_` (not become `.*`) and `{C => D}` must keep
        // the arrow (not become `{C as D}`) — both would be syntax errors in Scala 2.
        Scenario {
            name: "scala_organize_scala2_preserves_syntax",
            ext: "scala",
            input: "import scala.collection.mutable._\nimport java.util.{List => JList, Map => JMap}\n\nobject Main { val x = 1 }\n",
            ops: &[Op::Organize],
        },
        Scenario {
            name: "scala_add_preserves_existing_scala2_imports",
            ext: "scala",
            input: r#"import a.{b => c}
import a._

object Main { val x = 1 }
"#,
            ops: &[Op::Add {
                module: "z.Y",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "scala_add_uses_existing_scala2_dialect",
            ext: "scala",
            input: r#"import a._

object Main { val x = 1 }
"#,
            ops: &[
                Op::AddForm {
                    module: "cats.syntax.all",
                    names: &[],
                    namespace: None,
                    alias: None,
                    modifiers: &["wildcard"],
                    import_kind: None,
                },
                Op::Add {
                    module: "scala.concurrent",
                    names: &["ExecutionContext as EC"],
                    default_import: None,
                    type_only: false,
                },
            ],
        },
        Scenario {
            name: "scala_add_uses_brace_wildcard_scala2_dialect",
            ext: "scala",
            input: r#"import a.{_}

object Main { val x = 1 }
"#,
            ops: &[Op::AddForm {
                module: "cats.syntax.all",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &["wildcard"],
                import_kind: None,
            }],
        },
        Scenario {
            name: "scala_remove_selector_preserves_scala2_arrow",
            ext: "scala",
            input: r#"import a.{b => c, d}
import a._

object Main { val x = 1 }
"#,
            ops: &[Op::Remove {
                module: "a",
                name: Some("d"),
            }],
        },
        // ---- Swift (structured modifiers + kind imports) ----
        Scenario {
            name: "swift_add_plain",
            ext: "swift",
            input: "struct App {}\n",
            ops: &[Op::Add {
                module: "Foundation",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "swift_add_testable",
            ext: "swift",
            input: "import Foundation\nstruct App {}\n",
            ops: &[Op::AddForm {
                module: "MyApp",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &["@testable"],
                import_kind: None,
            }],
        },
        Scenario {
            name: "swift_add_duplicate_testable_is_noop",
            ext: "swift",
            input: r#"@testable import MyApp
struct App {}
"#,
            ops: &[Op::AddForm {
                module: "MyApp",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &["@testable"],
                import_kind: None,
            }],
        },
        Scenario {
            name: "swift_add_struct_import",
            ext: "swift",
            input: "import Foundation\nstruct App {}\n",
            ops: &[Op::AddForm {
                module: "Foo.Bar",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &[],
                import_kind: Some("struct"),
            }],
        },
        Scenario {
            name: "swift_remove_import",
            ext: "swift",
            input: "import Foundation\n@testable import MyApp\nimport struct Foo.Bar\n\nstruct App {}\n",
            ops: &[Op::Remove {
                module: "MyApp",
                name: None,
            }],
        },
        Scenario {
            name: "swift_organize_mixed",
            ext: "swift",
            input: "import Foo.Zed\n@testable import MyApp\nimport struct Foo.Bar\nimport Foundation\nimport UIKit.UIView\n\nstruct App {}\n",
            ops: &[Op::Organize],
        },
        // ---- Ruby (require-style method-call imports) ----
        Scenario {
            name: "ruby_add_require",
            ext: "rb",
            input: "puts 'hi'\n",
            ops: &[Op::Add {
                module: "json",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "ruby_add_require_not_deduped_by_load",
            ext: "rb",
            input: r#"load 'boot.rb'

puts 'hi'
"#,
            ops: &[Op::Add {
                module: "boot.rb",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "ruby_add_after_shebang",
            ext: "rb",
            input: r#"#!/usr/bin/env ruby
puts 'hi'
"#,
            ops: &[Op::Add {
                module: "json",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "ruby_add_require_relative",
            ext: "rb",
            input: "puts 'hi'\n",
            ops: &[Op::AddForm {
                module: "../lib/helper",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &[],
                import_kind: Some("require_relative"),
            }],
        },
        Scenario {
            name: "ruby_remove_require_relative",
            ext: "rb",
            input: "require 'json'\nrequire_relative '../unused'\nload 'boot.rb'\n\nputs 'hi'\n",
            ops: &[Op::Remove {
                module: "../unused",
                name: None,
            }],
        },
        Scenario {
            name: "ruby_organize_preserves_require_kinds",
            ext: "rb",
            input: "load \"boot.rb\"\nrequire_relative '../helper'\nrequire \"json\"\n\nputs 'hi'\n",
            ops: &[Op::Organize],
        },
        // ---- Lua (require-style imports: local binding + bare require) ----
        Scenario {
            name: "lua_add_local_require",
            ext: "lua",
            input: "print('ready')\n",
            ops: &[Op::Add {
                module: "pkg.foo",
                names: &[],
                default_import: Some("foo"),
                type_only: false,
            }],
        },
        Scenario {
            name: "lua_add_bare_require",
            ext: "lua",
            input: "print('ready')\n",
            ops: &[Op::Add {
                module: "side.effect",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "lua_remove_require",
            ext: "lua",
            input: "local unused = require(\"pkg.unused\")\nlocal keep = require(\"pkg.keep\")\n\nreturn keep\n",
            ops: &[Op::Remove {
                module: "pkg.unused",
                name: None,
            }],
        },
        Scenario {
            name: "lua_organize_requires",
            ext: "lua",
            input: "local zeta = require(\"zeta\")\nrequire(\"boot\")\nlocal alpha = require(\"alpha\")\n\nreturn alpha, zeta\n",
            ops: &[Op::Organize],
        },
        // ---- Header-prologue insertion (add to a header-only file with no
        // existing imports must land AFTER the package/namespace header, not at
        // offset 0 which would be invalid code). ----
        Scenario {
            name: "java_add_into_header_only",
            ext: "java",
            input: "package com.example;\n\nclass C {}\n",
            ops: &[Op::Add {
                module: "java.io.File",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "kotlin_add_into_header_only",
            ext: "kt",
            input: "package com.example\n\nfun main() {}\n",
            ops: &[Op::Add {
                module: "kotlin.collections.List",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "php_add_into_header_only",
            ext: "php",
            input: "<?php\n\nnamespace Demo;\n\nclass C {}\n",
            ops: &[Op::Add {
                module: "App\\Service",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "php_add_into_semicolon_namespace",
            ext: "php",
            input: "<?php\n\nnamespace Demo;\n\nclass C {}\n",
            ops: &[Op::Add {
                module: "App\\Repository",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "php_add_into_braced_namespace",
            ext: "php",
            input: "<?php\n\nnamespace Demo {\nclass C {}\n}\n",
            ops: &[Op::Add {
                module: "App\\Service",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        // ---- Perl (use/require/no statements with preserved raw args) ----
        Scenario {
            name: "perl_add_use",
            ext: "pl",
            input: "print \"hi\\n\";\n",
            ops: &[Op::Add {
                module: "Foo::Bar",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "perl_add_use_qw_args",
            ext: "pl",
            input: "print \"hi\\n\";\n",
            ops: &[Op::AddForm {
                module: "Foo",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &["qw(a b)"],
                import_kind: Some("use"),
            }],
        },
        Scenario {
            name: "perl_remove_require",
            ext: "pl",
            input: "require Foo::Unused;\nuse Foo::Keep;\n\nprint \"hi\\n\";\n",
            ops: &[Op::Remove {
                module: "Foo::Unused",
                name: None,
            }],
        },
        Scenario {
            name: "perl_organize_mixed_preserves_kinds_and_args",
            ext: "pm",
            input: "require Foo::Runtime;\nuse Foo qw(a b);\nno strict 'refs';\nuse parent -norequire, 'Base';\nuse Foo::Plain;\n\n1;\n",
            ops: &[Op::Organize],
        },
        // ---- C (preprocessor #include: system angle + local quote) ----
        Scenario {
            name: "c_add_system_include",
            ext: "c",
            input: "int main(void) { return 0; }\n",
            ops: &[Op::AddForm {
                module: "stdio.h",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &[],
                import_kind: Some("system"),
            }],
        },
        Scenario {
            name: "c_add_local_include",
            ext: "h",
            input: "void f(void);\n",
            ops: &[Op::AddForm {
                module: "project/config.h",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &[],
                import_kind: Some("local"),
            }],
        },
        Scenario {
            name: "c_add_local_same_header_as_system_not_duplicate",
            ext: "c",
            input: r#"#include <config.h>

int main(void) { return 0; }
"#,
            ops: &[Op::AddForm {
                module: "\"config.h\"",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &[],
                import_kind: None,
            }],
        },
        // Agents naturally pass includes WITH the delimiter. The engine must
        // strip it and infer the kind rather than double-wrapping (which would
        // generate `#include <<string>>` and silently roll back).
        Scenario {
            name: "cpp_add_system_include_delimited",
            ext: "cpp",
            input: "#include <vector>\n\nint main() { return 0; }\n",
            ops: &[Op::AddForm {
                module: "<string>",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &[],
                import_kind: None,
            }],
        },
        Scenario {
            name: "cpp_add_local_include_delimited",
            ext: "cpp",
            input: "#include <vector>\n\nint main() { return 0; }\n",
            ops: &[Op::AddForm {
                module: "\"widget.hpp\"",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &[],
                import_kind: None,
            }],
        },
        // A local include whose name sorts alphabetically BEFORE the existing
        // system include must still land AFTER it (grouped by include kind, not
        // alphabetically), contiguous with no blank line between — matching
        // organize. This is the case the delimited test above missed by alpha
        // accident (`widget` > `vector`).
        Scenario {
            name: "cpp_add_local_sorts_before_system",
            ext: "cpp",
            input: "#include <vector>\n\nint main() { return 0; }\n",
            ops: &[Op::AddForm {
                module: "\"aaa.hpp\"",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &[],
                import_kind: None,
            }],
        },
        Scenario {
            name: "c_remove_include",
            ext: "c",
            input: "#include <stdio.h>\n#include \"unused.h\"\n#include \"keep.h\"\n\nint main(void) { return 0; }\n",
            ops: &[Op::Remove {
                module: "unused.h",
                name: None,
            }],
        },
        Scenario {
            name: "c_remove_preserves_remaining_include_delimiters",
            ext: "c",
            input: r#"#include <stdio.h>
#include "keep.h"
#include <stdlib.h>

int main(void) { return 0; }
"#,
            ops: &[Op::Remove {
                module: "stdio.h",
                name: None,
            }],
        },
        Scenario {
            name: "c_remove_system_include_keeps_local_same_path",
            ext: "c",
            input: r#"#include <config.h>
#include "config.h"

int main(void) { return 0; }
"#,
            ops: &[Op::Remove {
                module: "<config.h>",
                name: None,
            }],
        },
        Scenario {
            // Organize must not reorder these. `#include` is a textual paste
            // whose expansion depends on preprocessor state at its position,
            // so sorting can change what the program compiles to. The golden
            // keeps source order; the blank lines it also records are cosmetic
            // and are the price of routing every include through the barrier
            // path. If a future golden for this scenario shows the system
            // headers hoisted above the quoted ones, that is the bug returning,
            // not a formatting change.
            name: "c_organize_mixed_includes",
            ext: "h",
            input: "#include \"z_local.h\"\n#include <stdio.h>\n#include \"a_local.h\"\n#include <stdlib.h>\n\nvoid f(void);\n",
            ops: &[Op::Organize],
        },
        // ---- C++ (same #include engine as C) ----
        Scenario {
            name: "cpp_add_system_include",
            ext: "cpp",
            input: "int main() { return 0; }\n",
            ops: &[Op::AddForm {
                module: "vector",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &[],
                import_kind: Some("system"),
            }],
        },
        Scenario {
            name: "cpp_add_local_include",
            ext: "hpp",
            input: "void f();\n",
            ops: &[Op::AddForm {
                module: "project/widget.hpp",
                names: &[],
                namespace: None,
                alias: None,
                modifiers: &[],
                import_kind: Some("local"),
            }],
        },
        Scenario {
            name: "cpp_remove_include",
            ext: "cpp",
            input: "#include <vector>\n#include \"unused.hpp\"\n#include \"keep.hpp\"\n\nint main() { return 0; }\n",
            ops: &[Op::Remove {
                module: "unused.hpp",
                name: None,
            }],
        },
        Scenario {
            name: "cpp_remove_delimited_system_include",
            ext: "cpp",
            input: r#"#include <vector>
#include "keep.hpp"

int main() { return 0; }
"#,
            ops: &[Op::Remove {
                module: "<vector>",
                name: None,
            }],
        },
        Scenario {
            name: "cpp_organize_mixed_includes",
            ext: "hpp",
            input: "#include \"z_widget.hpp\"\n#include <vector>\n#include \"a_widget.hpp\"\n#include <string>\n\nvoid f();\n",
            ops: &[Op::Organize],
        },
        // ---- Vue SFC (imports live inside the <script> block; the engine
        // re-parses the script body as TS and remaps offsets to whole-file). ----
        Scenario {
            name: "vue_add_into_existing_script",
            ext: "vue",
            input: "<template>\n  <div />\n</template>\n\n<script setup lang=\"ts\">\nimport { ref } from 'vue'\nconst x = ref(0)\n</script>\n",
            ops: &[Op::Add {
                module: "./Foo.vue",
                names: &[],
                default_import: Some("Foo"),
                type_only: false,
            }],
        },
        Scenario {
            name: "vue_add_into_empty_script",
            ext: "vue",
            input: "<template>\n  <div />\n</template>\n\n<script setup lang=\"ts\">\n</script>\n",
            ops: &[Op::Add {
                module: "vue",
                names: &["ref"],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "vue_remove_from_script",
            ext: "vue",
            input: "<template>\n  <div />\n</template>\n\n<script setup lang=\"ts\">\nimport { ref } from 'vue'\nimport Foo from './Foo.vue'\nconst x = ref(0)\n</script>\n",
            ops: &[Op::Remove {
                module: "./Foo.vue",
                name: None,
            }],
        },
        Scenario {
            name: "vue_remove_default_from_namespace",
            ext: "vue",
            input: "<template>\n  <div />\n</template>\n\n<script setup lang=\"ts\">\nimport React, * as NS from 'react'\nconst x = NS\n</script>\n",
            ops: &[Op::Remove {
                module: "react",
                name: Some("React"),
            }],
        },
        Scenario {
            name: "vue_organize_script",
            ext: "vue",
            input: "<template>\n  <div />\n</template>\n\n<script setup lang=\"ts\">\nimport Foo from './Foo.vue'\nimport { ref } from 'vue'\nconst x = ref(0)\n</script>\n",
            ops: &[Op::Organize],
        },
        Scenario {
            name: "vue_organize_default_namespace",
            ext: "vue",
            input: "<template>\n  <div />\n</template>\n\n<script setup lang=\"ts\">\nimport React, * as NS from 'react'\nconst x = React\n</script>\n",
            ops: &[Op::Organize],
        },
    ]
}

/// Golden-parity gate for the import engines. See module docs.
#[test]
fn import_golden_corpus() {
    let scenarios = scenarios();
    let update = std::env::var("UPDATE_GOLDEN").is_ok();
    let dir = golden_dir();
    if update {
        fs::create_dir_all(&dir).expect("create golden dir");
    }

    let mut aft = AftProcess::spawn();
    let mut drift: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();

    for scenario in &scenarios {
        let actual = run_scenario(&mut aft, scenario);
        let golden_path = dir.join(format!("{}.golden", scenario.name));

        if update {
            fs::write(&golden_path, &actual).expect("write golden");
            continue;
        }

        match fs::read_to_string(&golden_path) {
            Ok(expected) if expected.replace('\r', "") == actual.replace('\r', "") => {}
            Ok(expected) => drift.push(format!(
                "\n=== DRIFT: {} ===\n--- expected (golden) ---\n{}\n--- actual ---\n{}",
                scenario.name, expected, actual
            )),
            Err(_) => missing.push(scenario.name.to_string()),
        }
    }

    if update {
        return;
    }

    assert!(
        missing.is_empty(),
        "missing golden fixtures (run with UPDATE_GOLDEN=1 to capture): {missing:?}"
    );
    assert!(
        drift.is_empty(),
        "import golden parity drift in {} scenario(s):{}",
        drift.len(),
        drift.join("")
    );
}
