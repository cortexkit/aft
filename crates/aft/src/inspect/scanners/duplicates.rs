use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tree_sitter::{Node, Parser, Tree};

use super::duplicates_classifier::{is_anonymizable, node_cost, AnonymizeAs};
use crate::cache_freshness;
use crate::inspect::{
    FileContribution, InspectCategory, InspectJob, InspectResult, InspectScanSuccess,
};
use crate::parser::{detect_language, grammar_for, LangId};

const LOWER_BOUND: u32 = 20;
const MAX_COST: u32 = 7_000;
const MAX_GROUP_ITEMS: usize = 100;
const VARIABLE_SENTINEL: &str = "_var";
const FIELD_SENTINEL: &str = "_field";

#[derive(Debug, Clone, Deserialize, Serialize)]
struct DuplicateFragment {
    hash: String,
    start_line: u32,
    end_line: u32,
    cost: u32,
}

#[derive(Debug)]
struct FileScan {
    path: PathBuf,
    display_path: String,
    language_skipped: Option<&'static str>,
    freshness: cache_freshness::FileFreshness,
    fragments: Vec<DuplicateFragment>,
}

#[derive(Debug, Clone)]
struct FragmentOccurrence {
    file: String,
    start_line: u32,
    end_line: u32,
    cost: u32,
}

#[derive(Debug, Clone, Serialize)]
struct DuplicateGroup {
    files: Vec<String>,
    cost: u32,
    sample_file: String,
    sample_start_line: u32,
    sample_end_line: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct DuplicateContribution {
    file: String,
    fragments: Vec<DuplicateFragment>,
}

#[derive(Debug, Clone, Copy)]
struct NodeDigest {
    hash: blake3::Hash,
    cost: u32,
}

pub fn run_duplicates_scan(job: &InspectJob) -> InspectResult {
    let started = Instant::now();
    let file_scans = job
        .scope_files
        .par_iter()
        .map(|path| scan_file(job, path))
        .collect::<Result<Vec<_>, _>>();

    let file_scans = match file_scans {
        Ok(file_scans) => file_scans,
        Err(message) => return InspectResult::failed(job, message, started.elapsed()),
    };

    let contributions = file_scans
        .iter()
        .map(file_scan_to_contribution)
        .collect::<Vec<_>>();
    let aggregate = aggregate_duplicate_contributions(
        &contributions,
        skipped_languages_from_file_scans(&file_scans),
    );
    let scanned_files = file_scans
        .iter()
        .map(|scan| scan.path.clone())
        .collect::<Vec<_>>();

    let success = InspectScanSuccess {
        scanned_files,
        contributions,
        aggregate,
    };
    InspectResult::success(job, success, started.elapsed())
}

fn scan_file(job: &InspectJob, path: &Path) -> Result<FileScan, String> {
    let display_path = display_path(&job.project_root, path);
    let freshness = cache_freshness::collect(path)
        .map_err(|error| format!("freshness failed for {}: {error}", path.display()))?;
    let Some(lang) = detect_language(path) else {
        return Ok(FileScan {
            path: path.to_path_buf(),
            display_path,
            language_skipped: Some("unknown"),
            freshness,
            fragments: Vec::new(),
        });
    };

    if !is_supported_language(lang) {
        return Ok(FileScan {
            path: path.to_path_buf(),
            display_path,
            language_skipped: Some(language_name(lang)),
            freshness,
            fragments: Vec::new(),
        });
    }

    let source = fs::read_to_string(path)
        .map_err(|error| format!("read failed for {}: {error}", path.display()))?;
    let tree = parse_source(path, lang, &source)?;
    let mut fragments = Vec::new();
    collect_fragments(tree.root_node(), &source, lang, &mut fragments);
    fragments.sort_by(|left, right| {
        left.start_line
            .cmp(&right.start_line)
            .then(left.end_line.cmp(&right.end_line))
            .then(left.hash.cmp(&right.hash))
    });

    Ok(FileScan {
        path: path.to_path_buf(),
        display_path,
        language_skipped: None,
        freshness,
        fragments,
    })
}

fn parse_source(path: &Path, lang: LangId, source: &str) -> Result<Tree, String> {
    let grammar = grammar_for(lang);
    let mut parser = Parser::new();
    parser
        .set_language(&grammar)
        .map_err(|error| format!("grammar init failed for {:?}: {error}", lang))?;
    parser
        .parse(source, None)
        .ok_or_else(|| format!("tree-sitter parse returned None for {}", path.display()))
}

fn collect_fragments(
    node: Node<'_>,
    source: &str,
    lang: LangId,
    fragments: &mut Vec<DuplicateFragment>,
) -> NodeDigest {
    let mut cost = node_cost(lang, node.kind());
    let mut child_hashes = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_extra() {
            continue;
        }
        let child_digest = collect_fragments(child, source, lang, fragments);
        cost = cost.saturating_add(child_digest.cost);
        child_hashes.push(child_digest.hash);
    }

    let leaf_text = if child_hashes.is_empty() {
        Some(leaf_hash_text(node, source))
    } else {
        None
    };
    let hash = hash_node(node.kind(), &child_hashes, leaf_text.as_deref());

    if node.is_named() && (LOWER_BOUND..=MAX_COST).contains(&cost) {
        fragments.push(DuplicateFragment {
            hash: hash.to_hex().to_string(),
            start_line: node.start_position().row as u32 + 1,
            end_line: node.end_position().row as u32 + 1,
            cost,
        });
    }

    NodeDigest { hash, cost }
}

fn leaf_hash_text(node: Node<'_>, source: &str) -> String {
    match anonymize_leaf(node) {
        AnonymizeAs::Variable => VARIABLE_SENTINEL.to_string(),
        AnonymizeAs::Field => FIELD_SENTINEL.to_string(),
        AnonymizeAs::None => node.utf8_text(source.as_bytes()).unwrap_or("").to_string(),
    }
}

fn anonymize_leaf(node: Node<'_>) -> AnonymizeAs {
    if is_method_or_callee_name(node) || node.kind() == "type_identifier" {
        return AnonymizeAs::None;
    }
    is_anonymizable(node.kind())
}

fn is_method_or_callee_name(node: Node<'_>) -> bool {
    if is_direct_callee(node) || is_member_call_name(node) {
        return true;
    }

    let Some(parent) = node.parent() else {
        return false;
    };
    matches!(
        parent.kind(),
        "method_definition" | "method_declaration" | "method_signature" | "method_invocation"
    ) && is_name_field(parent, node)
}

fn is_direct_callee(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    matches!(parent.kind(), "call_expression" | "call" | "function_call")
        && is_field_node(parent, "function", node)
}

fn is_member_call_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !matches!(
        parent.kind(),
        "member_expression" | "field_expression" | "selector_expression" | "attribute"
    ) || !(is_field_node(parent, "property", node)
        || is_field_node(parent, "field", node)
        || is_field_node(parent, "attribute", node))
    {
        return false;
    }

    let Some(grandparent) = parent.parent() else {
        return false;
    };
    matches!(
        grandparent.kind(),
        "call_expression" | "call" | "function_call"
    ) && is_field_node(grandparent, "function", parent)
}

fn is_name_field(parent: Node<'_>, node: Node<'_>) -> bool {
    is_field_node(parent, "name", node)
        || is_field_node(parent, "property", node)
        || is_field_node(parent, "field", node)
}

fn is_field_node(parent: Node<'_>, field_name: &str, node: Node<'_>) -> bool {
    parent
        .child_by_field_name(field_name)
        .is_some_and(|field_node| same_node(field_node, node))
}

fn same_node(left: Node<'_>, right: Node<'_>) -> bool {
    left.kind() == right.kind()
        && left.start_byte() == right.start_byte()
        && left.end_byte() == right.end_byte()
}

fn hash_node(kind: &str, child_hashes: &[blake3::Hash], leaf_text: Option<&str>) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"aft-duplicates-v1");
    hash_bytes(&mut hasher, kind.as_bytes());
    hasher.update(&(child_hashes.len() as u64).to_le_bytes());
    for child_hash in child_hashes {
        hasher.update(child_hash.as_bytes());
    }
    match leaf_text {
        Some(text) => {
            hasher.update(&[1]);
            hash_bytes(&mut hasher, text.as_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    };
    hasher.finalize()
}

fn hash_bytes(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn file_scan_to_contribution(scan: &FileScan) -> FileContribution {
    FileContribution::new(
        InspectCategory::Duplicates,
        scan.path.clone(),
        scan.freshness,
        json!({
            "file": scan.display_path,
            "fragments": scan.fragments,
        }),
    )
}

pub(crate) fn aggregate_duplicate_contributions(
    contributions: &[FileContribution],
    languages_skipped: Vec<String>,
) -> serde_json::Value {
    aggregate_duplicate_contributions_with_limit(
        contributions,
        languages_skipped,
        Some(MAX_GROUP_ITEMS),
    )
}

pub(crate) fn aggregate_duplicate_contributions_with_limit(
    contributions: &[FileContribution],
    languages_skipped: Vec<String>,
    drill_down_limit: Option<usize>,
) -> serde_json::Value {
    let parsed = contributions
        .iter()
        .filter_map(|contribution| {
            serde_json::from_value::<DuplicateContribution>(contribution.contribution.clone()).ok()
        })
        .collect::<Vec<_>>();
    let mut by_hash = BTreeMap::<String, Vec<FragmentOccurrence>>::new();

    for scan in &parsed {
        for fragment in &scan.fragments {
            by_hash
                .entry(fragment.hash.clone())
                .or_default()
                .push(FragmentOccurrence {
                    file: scan.file.clone(),
                    start_line: fragment.start_line,
                    end_line: fragment.end_line,
                    cost: fragment.cost,
                });
        }
    }

    let mut groups = by_hash
        .into_values()
        .filter_map(|mut occurrences| {
            occurrences.sort_by(|left, right| {
                left.file
                    .cmp(&right.file)
                    .then(left.start_line.cmp(&right.start_line))
                    .then(left.end_line.cmp(&right.end_line))
            });
            if occurrences.len() < 2 {
                return None;
            }
            Some(occurrences_to_group(&occurrences))
        })
        .collect::<Vec<_>>();

    groups.sort_by(|left, right| {
        right
            .cost
            .cmp(&left.cost)
            .then(left.sample_file.cmp(&right.sample_file))
            .then(left.sample_start_line.cmp(&right.sample_start_line))
    });

    let groups_count = groups.len();
    let drill_down_capped = drill_down_limit.is_some_and(|limit| groups_count > limit);
    let items = match drill_down_limit {
        Some(limit) => groups.into_iter().take(limit).collect::<Vec<_>>(),
        None => groups,
    };

    json!({
        "count": groups_count,
        "total_groups": groups_count,
        "groups_count": groups_count,
        "items": items,
        "drill_down_capped": drill_down_capped,
        "scanned_files": parsed.len(),
        "languages_skipped": languages_skipped,
    })
}

fn skipped_languages_from_file_scans(file_scans: &[FileScan]) -> Vec<String> {
    file_scans
        .iter()
        .filter_map(|scan| scan.language_skipped)
        .map(ToString::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn occurrences_to_group(occurrences: &[FragmentOccurrence]) -> DuplicateGroup {
    let sample = &occurrences[0];
    DuplicateGroup {
        files: occurrences
            .iter()
            .map(|occurrence| {
                format!(
                    "{}:{}-{}",
                    occurrence.file, occurrence.start_line, occurrence.end_line
                )
            })
            .collect(),
        cost: sample.cost,
        sample_file: sample.file.clone(),
        sample_start_line: sample.start_line,
        sample_end_line: sample.end_line,
    }
}

fn display_path(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn is_supported_language(lang: LangId) -> bool {
    !matches!(
        lang,
        LangId::Bash
            | LangId::Html
            | LangId::Json
            | LangId::Scala
            | LangId::Solidity
            | LangId::Vue
            | LangId::Markdown
            | LangId::Java
            | LangId::Ruby
            | LangId::Kotlin
            | LangId::Swift
            | LangId::Php
            | LangId::Lua
            | LangId::Perl
    )
}

fn language_name(lang: LangId) -> &'static str {
    match lang {
        LangId::TypeScript => "typescript",
        LangId::Tsx => "tsx",
        LangId::JavaScript => "javascript",
        LangId::Python => "python",
        LangId::Rust => "rust",
        LangId::Go => "go",
        LangId::C => "c",
        LangId::Cpp => "cpp",
        LangId::Zig => "zig",
        LangId::CSharp => "csharp",
        LangId::Bash => "bash",
        LangId::Html => "html",
        LangId::Markdown => "markdown",
        LangId::Yaml => "yaml",
        LangId::Solidity => "solidity",
        LangId::Vue => "vue",
        LangId::Json => "json",
        LangId::Scala => "scala",
        LangId::Java => "java",
        LangId::Ruby => "ruby",
        LangId::Kotlin => "kotlin",
        LangId::Swift => "swift",
        LangId::Php => "php",
        LangId::Lua => "lua",
        LangId::Perl => "perl",
    }
}
