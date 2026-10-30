use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tree_sitter::Node;

use super::source_text::{read_source_text, write_skipped_files, SourceText, NOT_VALID_UTF8};
use crate::cache_freshness::{self, FileFreshness};
use crate::inspect::{
    FileContribution, InspectCategory, InspectJob, InspectResult, InspectScanSuccess,
};
use crate::parser::{detect_language, parse_source_with_cached_parser, LangId};

pub(crate) const COMPLEXITY_THRESHOLD: u32 = 10;
const DRILL_DOWN_LIMIT: usize = 100;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
struct FunctionComplexity {
    #[serde(rename = "function")]
    name: String,
    line: u32,
    complexity: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ComplexityContribution {
    file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(default)]
    functions: Vec<FunctionComplexity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parse_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    language_skipped: Option<String>,
    /// Set when the file could not be analyzed at all (its bytes are not
    /// valid UTF-8). The aggregate names it in `skipped_files`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    skipped_reason: Option<String>,
}

#[derive(Debug, Clone)]
struct FileComplexityScan {
    path: PathBuf,
    freshness: FileFreshness,
    contribution: ComplexityContribution,
}

#[derive(Debug, Clone, Copy)]
struct DecisionSpec {
    if_nodes: &'static [&'static str],
    loop_nodes: &'static [&'static str],
    match_nodes: &'static [&'static str],
    match_arm_nodes: &'static [&'static str],
    catch_nodes: &'static [&'static str],
    ternary_nodes: &'static [&'static str],
}

#[derive(Debug, Clone, Copy)]
struct LanguageSpec {
    function_nodes: &'static [&'static str],
    decisions: DecisionSpec,
}

const EMPTY: &[&str] = &[];
const C_LIKE: DecisionSpec = DecisionSpec {
    if_nodes: &["if_statement"],
    loop_nodes: &[
        "for_statement",
        "for_in_statement",
        "while_statement",
        "do_statement",
    ],
    match_nodes: &["switch_statement"],
    match_arm_nodes: &[
        "switch_case",
        "switch_default",
        "case_statement",
        "default_statement",
    ],
    catch_nodes: &["catch_clause"],
    ternary_nodes: &["ternary_expression", "conditional_expression"],
};
const RUST: DecisionSpec = DecisionSpec {
    if_nodes: &["if_expression"],
    loop_nodes: &["for_expression", "while_expression", "loop_expression"],
    match_nodes: &["match_expression"],
    match_arm_nodes: &["match_arm"],
    catch_nodes: EMPTY,
    ternary_nodes: EMPTY,
};
const PYTHON: DecisionSpec = DecisionSpec {
    if_nodes: &["if_statement", "elif_clause"],
    loop_nodes: &["for_statement", "while_statement"],
    match_nodes: &["match_statement"],
    match_arm_nodes: &["case_clause"],
    catch_nodes: &["except_clause"],
    ternary_nodes: &["conditional_expression"],
};
const GO: DecisionSpec = DecisionSpec {
    if_nodes: &["if_statement"],
    loop_nodes: &["for_statement"],
    match_nodes: &["expression_switch_statement", "type_switch_statement"],
    match_arm_nodes: &[
        "expression_case",
        "type_case",
        "default_case",
        "communication_case",
    ],
    catch_nodes: EMPTY,
    ternary_nodes: EMPTY,
};
const JAVA: DecisionSpec = DecisionSpec {
    if_nodes: &["if_statement"],
    loop_nodes: &[
        "for_statement",
        "enhanced_for_statement",
        "while_statement",
        "do_statement",
    ],
    match_nodes: &["switch_expression", "switch_statement"],
    match_arm_nodes: &["switch_label"],
    catch_nodes: &["catch_clause"],
    ternary_nodes: &["ternary_expression"],
};
const KOTLIN: DecisionSpec = DecisionSpec {
    if_nodes: &["if_expression"],
    loop_nodes: &["for_statement", "while_statement", "do_while_statement"],
    match_nodes: &["when_expression"],
    match_arm_nodes: &["when_entry"],
    catch_nodes: &["catch_block"],
    ternary_nodes: EMPTY,
};
const SWIFT: DecisionSpec = DecisionSpec {
    if_nodes: &["if_statement", "guard_statement"],
    loop_nodes: &["for_statement", "while_statement", "repeat_while_statement"],
    match_nodes: &["switch_statement"],
    match_arm_nodes: &["switch_entry"],
    catch_nodes: &["catch_clause"],
    ternary_nodes: &["ternary_expression"],
};
const CSHARP: DecisionSpec = DecisionSpec {
    if_nodes: &["if_statement"],
    loop_nodes: &[
        "for_statement",
        "for_each_statement",
        "while_statement",
        "do_statement",
    ],
    match_nodes: &["switch_statement", "switch_expression"],
    match_arm_nodes: &["switch_section", "switch_expression_arm"],
    catch_nodes: &["catch_clause"],
    ternary_nodes: &["conditional_expression"],
};
const RUBY: DecisionSpec = DecisionSpec {
    if_nodes: &["if", "elsif", "unless"],
    loop_nodes: &["for", "while", "until"],
    match_nodes: &["case"],
    match_arm_nodes: &["when", "else"],
    catch_nodes: &["rescue"],
    ternary_nodes: &["conditional"],
};
const PHP: DecisionSpec = DecisionSpec {
    if_nodes: &["if_statement", "else_if_clause"],
    loop_nodes: &[
        "for_statement",
        "foreach_statement",
        "while_statement",
        "do_statement",
    ],
    match_nodes: &["switch_statement", "match_expression"],
    match_arm_nodes: &[
        "case_statement",
        "default_statement",
        "match_conditional_expression",
    ],
    catch_nodes: &["catch_clause"],
    ternary_nodes: &["conditional_expression"],
};
const GENERIC: DecisionSpec = DecisionSpec {
    if_nodes: &["if_statement", "if_expression"],
    loop_nodes: &["for_statement", "while_statement", "loop_expression"],
    match_nodes: &["switch_statement", "match_expression"],
    match_arm_nodes: &["case_clause", "case_statement", "match_arm", "switch_case"],
    catch_nodes: &["catch_clause", "except_clause"],
    ternary_nodes: &["ternary_expression", "conditional_expression"],
};

/// Return a deliberately explicit grammar table. Languages omitted from this
/// table are reported as skipped rather than guessed from similarly named nodes.
fn language_spec(language: LangId) -> Option<LanguageSpec> {
    let spec = match language {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript => LanguageSpec {
            function_nodes: &[
                "function_declaration",
                "function_expression",
                "generator_function",
                "arrow_function",
                "method_definition",
            ],
            decisions: C_LIKE,
        },
        LangId::Python => LanguageSpec {
            function_nodes: &["function_definition", "lambda"],
            decisions: PYTHON,
        },
        LangId::Rust => LanguageSpec {
            function_nodes: &["function_item"],
            decisions: RUST,
        },
        LangId::Go => LanguageSpec {
            function_nodes: &["function_declaration", "method_declaration", "func_literal"],
            decisions: GO,
        },
        LangId::Java => LanguageSpec {
            function_nodes: &[
                "method_declaration",
                "constructor_declaration",
                "lambda_expression",
            ],
            decisions: JAVA,
        },
        LangId::Kotlin => LanguageSpec {
            function_nodes: &[
                "function_declaration",
                "anonymous_function",
                "lambda_literal",
            ],
            decisions: KOTLIN,
        },
        LangId::Swift => LanguageSpec {
            function_nodes: &["function_declaration", "closure_expression"],
            decisions: SWIFT,
        },
        LangId::C => LanguageSpec {
            function_nodes: &["function_definition"],
            decisions: C_LIKE,
        },
        LangId::Cpp | LangId::Cuda | LangId::Metal => LanguageSpec {
            function_nodes: &["function_definition", "lambda_expression"],
            decisions: C_LIKE,
        },
        LangId::CSharp => LanguageSpec {
            function_nodes: &[
                "method_declaration",
                "constructor_declaration",
                "local_function_statement",
                "lambda_expression",
                "anonymous_method_expression",
            ],
            decisions: CSHARP,
        },
        LangId::Ruby => LanguageSpec {
            function_nodes: &["method", "singleton_method", "lambda"],
            decisions: RUBY,
        },
        LangId::Php => LanguageSpec {
            function_nodes: &[
                "function_definition",
                "method_declaration",
                "anonymous_function",
                "arrow_function",
            ],
            decisions: PHP,
        },
        // The remaining function grammars are explicitly listed, but their
        // decision names use the conservative common-node fallback until each
        // grammar has fixture coverage. Unsupported document/data grammars stay
        // absent and therefore cannot fabricate a complexity finding.
        LangId::Zig => LanguageSpec {
            function_nodes: &["function_declaration"],
            decisions: GENERIC,
        },
        LangId::Bash => LanguageSpec {
            function_nodes: &["function_definition"],
            decisions: GENERIC,
        },
        LangId::Solidity => LanguageSpec {
            function_nodes: &[
                "function_definition",
                "constructor_definition",
                "modifier_definition",
            ],
            decisions: GENERIC,
        },
        LangId::Scala => LanguageSpec {
            function_nodes: &["function_definition"],
            decisions: GENERIC,
        },
        LangId::Lua => LanguageSpec {
            function_nodes: &["function_declaration", "function_definition"],
            decisions: GENERIC,
        },
        LangId::Perl => LanguageSpec {
            function_nodes: &[
                "subroutine_declaration_statement",
                "method_declaration_statement",
            ],
            decisions: GENERIC,
        },
        LangId::R => LanguageSpec {
            function_nodes: &["function_definition"],
            decisions: GENERIC,
        },
        LangId::Groovy => LanguageSpec {
            function_nodes: &["method_definition", "closure"],
            decisions: GENERIC,
        },
        LangId::ObjC => LanguageSpec {
            function_nodes: &["function_definition", "method_definition"],
            decisions: C_LIKE,
        },
        LangId::Html
        | LangId::Markdown
        | LangId::Scss
        | LangId::Vue
        | LangId::Json
        | LangId::Yaml
        | LangId::Toml
        | LangId::Pascal => return None,
    };
    Some(spec)
}

pub fn run_complexity_scan(job: &InspectJob) -> InspectResult {
    let started = Instant::now();
    let scans = job
        .scope_files
        .par_iter()
        .map(|path| scan_file(&job.project_root, path))
        .collect::<Result<Vec<_>, _>>();
    let scans = match scans {
        // A file that vanished mid-scan yields no scan, as if it was deleted.
        Ok(scans) => scans.into_iter().flatten().collect::<Vec<_>>(),
        Err(message) => return InspectResult::failed(job, message, started.elapsed()),
    };

    let aggregate = aggregate_file_scans(&job.project_root, &scans, Some(DRILL_DOWN_LIMIT));
    let scanned_files = scans.iter().map(|scan| scan.path.clone()).collect();
    let contributions = scans
        .iter()
        .map(file_scan_to_contribution)
        .collect::<Vec<_>>();
    InspectResult::success(
        job,
        InspectScanSuccess {
            scanned_files,
            contributions,
            aggregate,
        },
        started.elapsed(),
    )
}

pub(crate) fn aggregate_complexity_contributions_with_limit(
    project_root: &Path,
    contributions: &[FileContribution],
    drill_down_limit: Option<usize>,
) -> Value {
    let scans = contributions
        .iter()
        .filter_map(|contribution| {
            serde_json::from_value::<ComplexityContribution>(contribution.contribution.clone()).ok()
        })
        .collect::<Vec<_>>();
    aggregate_contributions(project_root, &scans, drill_down_limit)
}

/// Scan one scope file. `Ok(None)` means the file vanished between the walk
/// and this scan and is treated as deleted. A file that is not valid UTF-8 is
/// a per-file skip; other read failures keep failing the scan as before.
fn scan_file(project_root: &Path, path: &Path) -> Result<Option<FileComplexityScan>, String> {
    let freshness = match cache_freshness::collect(path) {
        Ok(freshness) => freshness,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("freshness failed for {}: {error}", path.display())),
    };
    // Only languages with a complexity table are read: a data or document
    // file (JSON, YAML, Markdown, ...) can never contribute, so its bytes,
    // whatever their encoding, are never opened as source.
    let analyzable =
        detect_language(path).is_some_and(|language| language_spec(language).is_some());
    let contribution = if analyzable {
        match read_source_text(path) {
            Ok(SourceText::Text(source)) => scan_source(project_root, path, &source),
            Ok(SourceText::NotUtf8) => unreadable_contribution(project_root, path, NOT_VALID_UTF8),
            Ok(SourceText::Vanished) => return Ok(None),
            Err(error) => return Err(format!("read failed for {}: {error}", path.display())),
        }
    } else {
        // `scan_source` reports the language as skipped before it looks at
        // the source text, so no text is needed here.
        scan_source(project_root, path, "")
    };
    Ok(Some(FileComplexityScan {
        path: path.to_path_buf(),
        freshness,
        contribution,
    }))
}

fn unreadable_contribution(
    project_root: &Path,
    path: &Path,
    reason: &str,
) -> ComplexityContribution {
    ComplexityContribution {
        file: display_path(project_root, path),
        language: detect_language(path)
            .map(|language| crate::inspect::job::language_name(language).to_string()),
        functions: Vec::new(),
        parse_error: None,
        language_skipped: None,
        skipped_reason: Some(reason.to_string()),
    }
}

fn scan_source(project_root: &Path, path: &Path, source: &str) -> ComplexityContribution {
    let file = display_path(project_root, path);
    let Some(language) = detect_language(path) else {
        return skipped_contribution(file, "unknown");
    };
    let Some(spec) = language_spec(language) else {
        return skipped_contribution(file, crate::inspect::job::language_name(language));
    };
    let language_name = crate::inspect::job::language_name(language).to_string();
    let tree = match parse_source_with_cached_parser(path, source, language) {
        Ok(tree) => tree,
        Err(error) => {
            return ComplexityContribution {
                file,
                language: Some(language_name),
                functions: Vec::new(),
                parse_error: Some(error.to_string()),
                language_skipped: None,
                skipped_reason: None,
            };
        }
    };

    let mut functions = collect_functions(tree.root_node(), source, spec);
    functions.sort_by(|left, right| {
        left.line
            .cmp(&right.line)
            .then_with(|| left.name.cmp(&right.name))
    });
    ComplexityContribution {
        file,
        language: Some(language_name),
        functions,
        parse_error: tree
            .root_node()
            .has_error()
            .then(|| "tree-sitter parse contains syntax errors".to_string()),
        language_skipped: None,
        skipped_reason: None,
    }
}

fn skipped_contribution(file: String, language: &str) -> ComplexityContribution {
    ComplexityContribution {
        file,
        language: None,
        functions: Vec::new(),
        parse_error: None,
        language_skipped: Some(language.to_string()),
        skipped_reason: None,
    }
}

fn collect_functions(root: Node<'_>, source: &str, spec: LanguageSpec) -> Vec<FunctionComplexity> {
    let mut functions = Vec::new();
    // Carry the innermost function with each pending node so nested functions take
    // ownership of their subtree without forcing a second walk of every function.
    let mut pending = vec![(root, None)];
    while let Some((node, enclosing_function)) = pending.pop() {
        let function = if is_function_node(node, spec) {
            if node.has_error() {
                // An invalid function cannot be scored reliably. Independent
                // functions elsewhere in the file still have valid subtrees.
                push_children_with_state(node, None, &mut pending);
                continue;
            }
            let function = functions.len();
            functions.push(FunctionComplexity {
                name: function_name(node, source),
                line: node.start_position().row as u32 + 1,
                complexity: 1,
            });
            Some(function)
        } else {
            enclosing_function
        };

        if let Some(function) = function {
            let increment = decision_complexity(node, source, spec);
            functions[function].complexity =
                functions[function].complexity.saturating_add(increment);
        }
        push_children_with_state(node, function, &mut pending);
    }
    functions
}

fn decision_complexity(node: Node<'_>, source: &str, spec: LanguageSpec) -> u32 {
    let kind = node.kind();
    if spec.decisions.if_nodes.contains(&kind) || spec.decisions.loop_nodes.contains(&kind) {
        1u32.saturating_add(short_circuits_in_condition(node, source, spec))
    } else if spec.decisions.match_nodes.contains(&kind) {
        match_arms_beyond_first(node, spec)
    } else {
        u32::from(
            spec.decisions.catch_nodes.contains(&kind)
                || spec.decisions.ternary_nodes.contains(&kind),
        )
    }
}

fn short_circuits_in_condition(node: Node<'_>, source: &str, spec: LanguageSpec) -> u32 {
    ["condition", "predicate", "guard"]
        .into_iter()
        .filter_map(|field| node.child_by_field_name(field))
        .map(|condition| short_circuits_in_subtree(condition, source, spec))
        .sum()
}

fn short_circuits_in_subtree(root: Node<'_>, source: &str, spec: LanguageSpec) -> u32 {
    let mut count = 0u32;
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if !same_node(node, root) && is_function_node(node, spec) {
            continue;
        }
        count = count.saturating_add(direct_short_circuit_operator_count(node, source));
        push_children(node, &mut pending);
    }
    count
}

fn direct_short_circuit_operator_count(node: Node<'_>, source: &str) -> u32 {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|child| !child.is_named())
        .filter_map(|child| child.utf8_text(source.as_bytes()).ok())
        .filter(|token| matches!(*token, "&&" | "||" | "and" | "or"))
        .count() as u32
}

fn match_arms_beyond_first(root: Node<'_>, spec: LanguageSpec) -> u32 {
    let mut arms = 0u32;
    let mut pending = Vec::new();
    push_children(root, &mut pending);
    while let Some(node) = pending.pop() {
        if is_function_node(node, spec) || spec.decisions.match_nodes.contains(&node.kind()) {
            continue;
        }
        if spec.decisions.match_arm_nodes.contains(&node.kind()) {
            arms = arms.saturating_add(1);
            continue;
        }
        push_children(node, &mut pending);
    }
    arms.saturating_sub(1)
}

fn function_name(node: Node<'_>, source: &str) -> String {
    if let Some(name) = node.child_by_field_name("name") {
        return node_text(name, source).to_string();
    }
    let mut current = node.parent();
    while let Some(parent) = current {
        if matches!(
            parent.kind(),
            "variable_declarator" | "assignment_expression"
        ) {
            for field in ["name", "left"] {
                if let Some(name) = parent.child_by_field_name(field) {
                    return node_text(name, source).to_string();
                }
            }
        }
        current = parent.parent();
    }
    "<anonymous>".to_string()
}

fn aggregate_file_scans(
    project_root: &Path,
    scans: &[FileComplexityScan],
    drill_down_limit: Option<usize>,
) -> Value {
    let contributions = scans
        .iter()
        .map(|scan| scan.contribution.clone())
        .collect::<Vec<_>>();
    aggregate_contributions(project_root, &contributions, drill_down_limit)
}

fn aggregate_contributions(
    project_root: &Path,
    contributions: &[ComplexityContribution],
    drill_down_limit: Option<usize>,
) -> Value {
    let mut hotspots = Vec::new();
    let mut parse_errors = Vec::new();
    let mut skipped_files = Vec::new();
    let mut languages_skipped = BTreeSet::new();
    for contribution in contributions {
        if let Some(reason) = &contribution.skipped_reason {
            skipped_files.push((contribution.file.clone(), reason.clone()));
        }
        if let Some(error) = &contribution.parse_error {
            parse_errors.push(json!({ "file": contribution.file, "message": error }));
        }
        if let Some(language) = &contribution.language_skipped {
            languages_skipped.insert(language.clone());
        }
        let language = contribution.language.as_deref().unwrap_or("unknown");
        for function in &contribution.functions {
            if function.complexity < COMPLEXITY_THRESHOLD {
                continue;
            }
            hotspots.push(json!({
                "file": contribution.file,
                "function": function.name,
                "line": function.line,
                "complexity": function.complexity,
                "language": language,
            }));
        }
    }

    let worst = hotspots.iter().cloned().max_by(|left, right| {
        left["complexity"]
            .as_u64()
            .cmp(&right["complexity"].as_u64())
            .then_with(|| right["file"].as_str().cmp(&left["file"].as_str()))
            .then_with(|| right["function"].as_str().cmp(&left["function"].as_str()))
            .then_with(|| right["line"].as_u64().cmp(&left["line"].as_u64()))
    });
    hotspots.sort_by(|left, right| {
        right["complexity"]
            .as_u64()
            .cmp(&left["complexity"].as_u64())
            .then_with(|| left["file"].as_str().cmp(&right["file"].as_str()))
            .then_with(|| left["function"].as_str().cmp(&right["function"].as_str()))
            .then_with(|| left["line"].as_u64().cmp(&right["line"].as_u64()))
    });
    let count = hotspots.len();
    let roles = crate::inspect::entry_points::resolve_project_roles(project_root);
    let items =
        crate::inspect::entry_points::rank_and_truncate_items(hotspots, &roles, drill_down_limit);
    let limit = drill_down_limit.unwrap_or(usize::MAX);
    let mut aggregate = json!({
        "count": count,
        "threshold": COMPLEXITY_THRESHOLD,
        "worst": worst,
        "items": items,
        "drill_down_capped": count > limit,
        "scanned_files": contributions.len(),
        "languages_skipped": languages_skipped.into_iter().collect::<Vec<_>>(),
        "complete": parse_errors.is_empty(),
    });
    if !parse_errors.is_empty() {
        aggregate["parse_errors"] = Value::Array(parse_errors);
    }
    write_skipped_files(&mut aggregate, skipped_files, drill_down_limit);
    aggregate
}

fn file_scan_to_contribution(scan: &FileComplexityScan) -> FileContribution {
    FileContribution::new(
        InspectCategory::Complexity,
        scan.path.clone(),
        scan.freshness,
        serde_json::to_value(&scan.contribution).expect("complexity contribution serializes"),
    )
}

fn display_path(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn is_function_node(node: Node<'_>, spec: LanguageSpec) -> bool {
    spec.function_nodes.contains(&node.kind())
}

fn node_text<'source>(node: Node<'_>, source: &'source str) -> &'source str {
    node.utf8_text(source.as_bytes()).unwrap_or("")
}

fn same_node(left: Node<'_>, right: Node<'_>) -> bool {
    left.kind() == right.kind()
        && left.start_byte() == right.start_byte()
        && left.end_byte() == right.end_byte()
}

fn push_children<'tree>(node: Node<'tree>, pending: &mut Vec<Node<'tree>>) {
    let first_child = pending.len();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            pending.push(cursor.node());
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        pending[first_child..].reverse();
    }
}

fn push_children_with_state<'tree, T: Copy>(
    node: Node<'tree>,
    state: T,
    pending: &mut Vec<(Node<'tree>, T)>,
) {
    let first_child = pending.len();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            pending.push((cursor.node(), state));
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        pending[first_child..].reverse();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture_scan(name: &str, source: &str) -> ComplexityContribution {
        scan_source(Path::new("fixtures"), Path::new(name), source)
    }

    #[test]
    fn audit05_identifier_is_not_short_circuit_operator() {
        let scan = fixture_scan(
            "field.ts",
            "function f(value: {or: boolean}) { if (value.or) { return 1; } return 0; }",
        );
        assert!(scan.parse_error.is_none());
        assert_eq!(scan.functions[0].complexity, 2);
    }

    #[test]
    fn audit05_parse_error_keeps_valid_nested_functions() {
        let scan = fixture_scan("nested.ts", "function broken() { function intact(flag: boolean) { if (flag) return 1; } const = 1; }");
        assert!(scan.parse_error.is_some());
        assert_eq!(scan.functions.len(), 1);
        assert_eq!(scan.functions[0].name, "intact");
        assert_eq!(scan.functions[0].complexity, 2);
    }

    #[test]
    fn audit05_parse_error_keeps_valid_functions() {
        let scan = fixture_scan(
            "partial.ts",
            "function good(flag: boolean) { if (flag) { return 1; } return 0; }\nfunction bad( {",
        );
        assert!(scan.parse_error.is_some());
        assert_eq!(scan.functions.len(), 1);
        assert_eq!(scan.functions[0].name, "good");
        assert_eq!(scan.functions[0].complexity, 2);
    }

    fn collect_functions_two_pass_reference(
        root: Node<'_>,
        source: &str,
        spec: LanguageSpec,
    ) -> Vec<FunctionComplexity> {
        let mut functions = Vec::new();
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            if is_function_node(node, spec) {
                functions.push(FunctionComplexity {
                    name: function_name(node, source),
                    line: node.start_position().row as u32 + 1,
                    complexity: function_complexity_reference(node, source, spec),
                });
            }
            push_children(node, &mut pending);
        }
        functions
    }

    fn function_complexity_reference(function: Node<'_>, source: &str, spec: LanguageSpec) -> u32 {
        let mut complexity = 1u32;
        let mut pending = vec![function];
        while let Some(node) = pending.pop() {
            if !same_node(node, function) && is_function_node(node, spec) {
                continue;
            }
            complexity = complexity.saturating_add(decision_complexity(node, source, spec));
            push_children(node, &mut pending);
        }
        complexity
    }

    #[test]
    fn single_pass_walk_is_byte_exact_with_two_pass_reference() {
        let cases = [
            (
                "shaped.rs",
                include_str!("../../../tests/fixtures/inspect_complexity/shaped.rs"),
            ),
            (
                "shaped.ts",
                include_str!("../../../tests/fixtures/inspect_complexity/shaped.ts"),
            ),
            (
                "shaped.py",
                include_str!("../../../tests/fixtures/inspect_complexity/shaped.py"),
            ),
            (
                "shaped.go",
                include_str!("../../../tests/fixtures/inspect_complexity/shaped.go"),
            ),
            (
                "nested.ts",
                "function outer(flag: boolean) { if (flag && ready()) { const inner = () => { while (flag) {} }; inner(); } return flag ? 1 : 0; }",
            ),
        ];

        for (name, source) in cases {
            let path = Path::new(name);
            let language = detect_language(path).expect("fixture language");
            let spec = language_spec(language).expect("fixture language spec");
            let tree =
                parse_source_with_cached_parser(path, source, language).expect("parse fixture");
            let actual = collect_functions(tree.root_node(), source, spec);
            let expected = collect_functions_two_pass_reference(tree.root_node(), source, spec);
            assert_eq!(
                serde_json::to_vec(&actual).expect("serialize optimized output"),
                serde_json::to_vec(&expected).expect("serialize reference output"),
                "{name}"
            );
        }
    }

    #[test]
    fn known_language_fixtures_have_hand_derived_complexity() {
        let cases = [
            (
                "shaped.rs",
                include_str!("../../../tests/fixtures/inspect_complexity/shaped.rs"),
                6,
                // Rust: base 1 + if 1 + && 1 + for 1 + (three match arms - 1) 2.
                "Rust has no ternary expression",
            ),
            (
                "shaped.ts",
                include_str!("../../../tests/fixtures/inspect_complexity/shaped.ts"),
                7,
                // TS: base 1 + if 1 + && 1 + for 1 + (three switch arms - 1) 2 + ternary 1.
                "TypeScript decision count",
            ),
            (
                "shaped.py",
                include_str!("../../../tests/fixtures/inspect_complexity/shaped.py"),
                7,
                // Python: base 1 + if 1 + and 1 + for 1 + (three match arms - 1) 2 + conditional expression 1.
                // External oracle: radon 6.x computes the same fixture as complexity 7 (cross-checked 2026-08-27).
                "Python decision count",
            ),
            (
                "shaped.go",
                include_str!("../../../tests/fixtures/inspect_complexity/shaped.go"),
                6,
                // Go: base 1 + if 1 + && 1 + for 1 + (three switch arms - 1) 2.
                "Go has no ternary expression",
            ),
        ];

        for (name, source, expected, derivation) in cases {
            let scan = fixture_scan(name, source);
            assert_eq!(scan.functions.len(), 1, "{name}: {derivation}");
            assert_eq!(
                scan.functions[0].complexity, expected,
                "{name}: {derivation}"
            );
        }
    }

    #[test]
    fn threshold_includes_ten_and_excludes_nine() {
        let root = tempfile::tempdir().expect("project");
        let contributions = vec![
            test_contribution(root.path(), "src/nine.rs", "nine", 9),
            test_contribution(root.path(), "src/ten.rs", "ten", 10),
        ];

        let aggregate =
            aggregate_complexity_contributions_with_limit(root.path(), &contributions, Some(100));
        assert_eq!(aggregate["count"], 1);
        assert_eq!(aggregate["items"][0]["function"], "ten");
        assert_eq!(aggregate["items"][0]["complexity"], 10);
    }

    #[test]
    fn product_hotspots_rank_above_higher_complexity_test_hotspots() {
        let root = tempfile::tempdir().expect("project");
        let contributions = vec![
            test_contribution(root.path(), "tests/hotspot.rs", "test_hotspot", 99),
            test_contribution(root.path(), "src/hotspot.rs", "product_hotspot", 10),
        ];

        let aggregate =
            aggregate_complexity_contributions_with_limit(root.path(), &contributions, Some(100));
        assert_eq!(aggregate["items"][0]["function"], "product_hotspot");
        assert_eq!(aggregate["items"][1]["function"], "test_hotspot");
        assert_eq!(aggregate["worst"]["function"], "test_hotspot");
    }

    #[test]
    fn absent_language_table_entry_is_an_honest_zero_finding_gap() {
        let source = include_str!("../../../tests/fixtures/inspect_complexity/unsupported.pas");
        let scan = fixture_scan("unsupported.pas", source);

        assert!(scan.functions.is_empty());
        assert_eq!(scan.language_skipped.as_deref(), Some("pascal"));
        assert!(scan.language.is_none());

        let aggregate = aggregate_contributions(Path::new("fixtures"), &[scan], Some(100));
        assert_eq!(aggregate["count"], 0);
        assert!(aggregate["items"].as_array().is_some_and(Vec::is_empty));
        assert_eq!(aggregate["languages_skipped"], json!(["pascal"]));
    }

    #[test]
    fn non_utf8_source_is_a_named_skip_and_non_utf8_data_file_is_never_read() {
        let dir = tempfile::tempdir().expect("project");
        let root = dir.path();
        // GBK bytes: valid JSON grammar, invalid UTF-8.
        let bad_json = root.join("data/bad.json");
        let bad_ts = root.join("src/bad.ts");
        let good_ts = root.join("src/good.ts");
        fs::create_dir_all(root.join("data")).expect("data dir");
        fs::create_dir_all(root.join("src")).expect("src dir");
        fs::write(&bad_json, b"{\"k\":\"\xb9\xe6\xce\"}").expect("write bad json");
        fs::write(&bad_ts, b"export const k = \"\xb9\xe6\xce\";\n").expect("write bad ts");
        fs::write(
            &good_ts,
            "export function f(a: boolean) { return a ? 1 : 0; }\n",
        )
        .expect("write good ts");

        let json_scan = scan_file(root, &bad_json)
            .expect("data file scan succeeds")
            .expect("data file exists");
        assert_eq!(
            json_scan.contribution.language_skipped.as_deref(),
            Some("json")
        );
        assert!(json_scan.contribution.skipped_reason.is_none());
        assert_eq!(
            super::super::source_text::source_read_count_for_debug(&bad_json),
            0,
            "complexity has no table for JSON, so it must never read its bytes"
        );

        let bad_scan = scan_file(root, &bad_ts)
            .expect("a non-UTF-8 source must not fail the scan")
            .expect("source exists");
        assert_eq!(
            bad_scan.contribution.skipped_reason.as_deref(),
            Some(NOT_VALID_UTF8)
        );
        assert!(bad_scan.contribution.functions.is_empty());
        let good_scan = scan_file(root, &good_ts)
            .expect("good scan")
            .expect("good exists");
        assert_eq!(good_scan.contribution.functions.len(), 1);

        let aggregate = aggregate_contributions(
            root,
            &[
                json_scan.contribution,
                bad_scan.contribution,
                good_scan.contribution,
            ],
            Some(100),
        );
        assert_eq!(aggregate["complete"], json!(false));
        assert_eq!(
            aggregate["skipped_files"],
            json!([{ "file": "src/bad.ts", "reason": NOT_VALID_UTF8 }])
        );
        assert_eq!(aggregate["skipped_files_count"], json!(1));

        assert!(
            scan_file(root, &root.join("src/vanished.ts"))
                .expect("a vanished file is not a failure")
                .is_none(),
            "a file removed after the walk is treated as deleted"
        );
    }

    fn test_contribution(
        root: &Path,
        relative: &str,
        name: &str,
        complexity: u32,
    ) -> FileContribution {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(&path, "fn fixture() {}\n").expect("write fixture");
        FileContribution::new(
            InspectCategory::Complexity,
            path.clone(),
            cache_freshness::collect(&path).expect("freshness"),
            json!({
                "file": relative,
                "language": "rust",
                "functions": [{
                    "function": name,
                    "line": 1,
                    "complexity": complexity,
                }],
            }),
        )
    }
}
