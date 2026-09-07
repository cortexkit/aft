use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use globset::{Glob, GlobSet, GlobSetBuilder};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tree_sitter::{Node, Parser, Tree};

use super::duplicates_classifier::{is_anonymizable, node_cost, AnonymizeAs};
use crate::cache_freshness;
use crate::inspect::entry_points::TOP_PREVIEW_ITEMS;
use crate::inspect::{
    FileContribution, InspectCategory, InspectJob, InspectResult, InspectScanSuccess,
};
use crate::parser::{detect_language, grammar_for, LangId};

const LOWER_BOUND: u32 = 20;
const MAX_COST: u32 = 7_000;
/// Minimum source-line span for a REPORTED duplicate group: every occurrence
/// must cover at least this many lines. The cost bound alone admits 3-4 line
/// fragments (a dense call with member chains clears cost 20 easily), and
/// field evidence shows those are idiomatic guards/setup blocks nobody would
/// extract — report noise, not debt. SonarQube's duplicate-block convention
/// (10 lines / ~100 tokens) is the calibration. Applied at the verdict layer
/// so cached per-file fragment facts stay span-agnostic.
const MIN_SPAN_LINES: u32 = 10;
// Defensive recursion bound for `collect_fragments`. Hand-written code nests
// only tens of levels deep, but minified bundles, generated code, and very long
// operator/promise chains can produce trees thousands of nodes deep. The inspect
// rayon pool uses bounded worker stacks, so unbounded AST recursion here can
// overflow the stack and SIGABRT the entire bridge. Past this depth we stop
// descending and treat the node as an opaque leaf — duplicate detection inside
// such pathological nesting is noise anyway. Kept well under the pool's stack
// budget (see dispatch.rs stack_size).
const MAX_FRAGMENT_DEPTH: u32 = 1_500;
const MAX_GROUP_ITEMS: usize = 100;
const VARIABLE_SENTINEL: &str = "_var";
const FIELD_SENTINEL: &str = "_field";
const EXPECTED_DUPLICATE_MARKER: &str = "aft:expected-duplicate";

thread_local! {
    static DUPLICATES_PARSERS: RefCell<HashMap<LangId, Parser>> = RefCell::new(HashMap::new());
}

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
    line_count: u32,
    expected_duplicate: bool,
    generated: bool,
    fragments: Vec<DuplicateFragment>,
}

#[derive(Debug, Clone)]
struct FragmentOccurrence {
    file: Rc<str>,
    start_line: u32,
    end_line: u32,
    cost: u32,
    generated: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize)]
struct DuplicateGroup {
    files: Vec<String>,
    cost: u32,
    duplicated_lines: u32,
    sample_file: String,
    sample_start_line: u32,
    sample_end_line: u32,
    #[serde(skip_serializing_if = "is_false")]
    generated: bool,
}

#[cfg(test)]
#[derive(Debug, Clone, Deserialize)]
struct DuplicateContribution {
    file: String,
    #[serde(default)]
    line_count: u32,
    #[serde(default)]
    expected_duplicate: bool,
    #[serde(default)]
    generated: bool,
    fragments: Vec<DuplicateFragment>,
}

#[derive(Debug, Clone, Copy)]
struct DuplicateContributionRef<'a> {
    file: &'a str,
    line_count: u32,
    expected_duplicate: bool,
    generated: bool,
    fragments: &'a [Value],
}

#[derive(Debug, Clone, Copy)]
struct DuplicateFragmentRef<'a> {
    hash: &'a str,
    start_line: u32,
    end_line: u32,
    cost: u32,
}

#[derive(Debug)]
struct ExpectedMirrorPair {
    left: GlobSet,
    right: GlobSet,
}

#[derive(Debug, Default)]
struct SuppressionStats {
    mirror_groups: usize,
    marker_groups: usize,
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

    let aggregate = aggregate_file_scans(
        &file_scans,
        skipped_languages_from_file_scans(&file_scans),
        &job.config.inspect.duplicates.expected_mirrors,
    );
    let contributions = file_scans
        .iter()
        .map(file_scan_to_contribution)
        .collect::<Vec<_>>();
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
            line_count: 0,
            expected_duplicate: false,
            generated: crate::inspect::generated::is_generated_file(Path::new(""), path),
            fragments: Vec::new(),
        });
    };

    if !is_supported_language(lang) {
        return Ok(FileScan {
            path: path.to_path_buf(),
            display_path,
            language_skipped: Some(language_name(lang)),
            freshness,
            line_count: 0,
            expected_duplicate: false,
            generated: crate::inspect::generated::is_generated_file(Path::new(""), path),
            fragments: Vec::new(),
        });
    }

    let source = fs::read_to_string(path)
        .map_err(|error| format!("read failed for {}: {error}", path.display()))?;
    let line_count = source_line_count(&source);
    let expected_duplicate = source.contains(EXPECTED_DUPLICATE_MARKER);
    let generated = crate::inspect::generated::is_generated_file_from_source(path, &source);
    let tree = parse_source(path, lang, &source)?;
    let mut fragments = Vec::new();
    let mut hash_scratch = Vec::new();
    collect_fragments(
        tree.root_node(),
        &source,
        lang,
        &mut fragments,
        &mut hash_scratch,
        0,
    );
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
        line_count,
        expected_duplicate,
        generated,
        fragments,
    })
}

fn parse_source(path: &Path, lang: LangId, source: &str) -> Result<Tree, String> {
    DUPLICATES_PARSERS.with(|parsers| {
        let mut parsers = parsers.borrow_mut();
        if let std::collections::hash_map::Entry::Vacant(entry) = parsers.entry(lang) {
            let grammar = grammar_for(lang);
            let mut parser = Parser::new();
            parser
                .set_language(&grammar)
                .map_err(|error| format!("grammar init failed for {:?}: {error}", lang))?;
            entry.insert(parser);
        }

        parsers
            .get_mut(&lang)
            .expect("parser inserted for language")
            .parse(source, None)
            .ok_or_else(|| format!("tree-sitter parse returned None for {}", path.display()))
    })
}

fn collect_fragments(
    node: Node<'_>,
    source: &str,
    lang: LangId,
    fragments: &mut Vec<DuplicateFragment>,
    hash_scratch: &mut Vec<blake3::Hash>,
    depth: u32,
) -> NodeDigest {
    let mut cost = node_cost(lang, node.kind());
    // Stop descending past MAX_FRAGMENT_DEPTH to avoid overflowing the bounded
    // inspect worker stack on pathologically deep trees (minified bundles,
    // generated code, long chains). Hash the node as an opaque leaf so the
    // parent digest stays deterministic; we simply do not emit fragments from
    // the truncated subtree.
    if depth >= MAX_FRAGMENT_DEPTH {
        let leaf = leaf_hash_text(node, source);
        return NodeDigest {
            hash: hash_node(node.kind(), &[], Some(leaf)),
            cost,
        };
    }
    let child_start = hash_scratch.len();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_extra() {
            continue;
        }
        let child_digest =
            collect_fragments(child, source, lang, fragments, hash_scratch, depth + 1);
        cost = cost.saturating_add(child_digest.cost);
        hash_scratch.push(child_digest.hash);
    }

    let child_hashes = &hash_scratch[child_start..];
    let leaf_text = if child_hashes.is_empty() {
        Some(leaf_hash_text(node, source))
    } else {
        None
    };
    let hash = hash_node(node.kind(), child_hashes, leaf_text);
    hash_scratch.truncate(child_start);

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

fn leaf_hash_text<'source>(node: Node<'_>, source: &'source str) -> &'source str {
    match anonymize_leaf(node) {
        AnonymizeAs::Variable => VARIABLE_SENTINEL,
        AnonymizeAs::Field => FIELD_SENTINEL,
        // Anonymous tree-sitter leaves are usually fixed tokens whose kind is
        // their source text. Reuse the kind only after comparing the source
        // bytes, so generated/external zero-width tokens keep the old hash.
        AnonymizeAs::None if !node.is_named() => {
            let source_text = source
                .as_bytes()
                .get(node.start_byte()..node.end_byte())
                .unwrap_or_default();
            if source_text == node.kind().as_bytes() {
                node.kind()
            } else {
                node.utf8_text(source.as_bytes()).unwrap_or("")
            }
        }
        AnonymizeAs::None => node.utf8_text(source.as_bytes()).unwrap_or(""),
    }
}

fn anonymize_leaf(node: Node<'_>) -> AnonymizeAs {
    // Most leaves (punctuation, keywords, literals, type identifiers) are not
    // anonymizable. Avoid parent/field callee checks unless the kind can be
    // anonymized in the first place.
    let anonymize_as = is_anonymizable(node.kind());
    if anonymize_as == AnonymizeAs::None || node.kind() == "type_identifier" {
        return AnonymizeAs::None;
    }
    if is_method_or_callee_name(node) {
        AnonymizeAs::None
    } else {
        anonymize_as
    }
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
    let mut contribution = json!({
        "file": scan.display_path,
        "line_count": scan.line_count,
        "expected_duplicate": scan.expected_duplicate,
        "fragments": scan.fragments,
    });
    if scan.generated {
        contribution["generated"] = json!(true);
    }
    FileContribution::new(
        InspectCategory::Duplicates,
        scan.path.clone(),
        scan.freshness,
        contribution,
    )
}

#[cfg(test)]
pub(crate) fn aggregate_duplicate_contributions(
    contributions: &[FileContribution],
    languages_skipped: Vec<String>,
) -> serde_json::Value {
    aggregate_duplicate_contributions_with_limit(
        contributions,
        languages_skipped,
        Some(MAX_GROUP_ITEMS),
        &[],
    )
}

pub(crate) fn aggregate_duplicate_contributions_with_limit(
    contributions: &[FileContribution],
    languages_skipped: Vec<String>,
    drill_down_limit: Option<usize>,
    expected_mirrors: &[[String; 2]],
) -> serde_json::Value {
    let parsed = contributions
        .iter()
        .filter_map(|contribution| duplicate_contribution_ref(&contribution.contribution))
        .collect::<Vec<_>>();
    let mut fragment_counts = HashMap::<&str, usize>::new();
    for scan in &parsed {
        for fragment in scan.fragments {
            let fragment = duplicate_fragment_ref(fragment)
                .expect("duplicate contribution fragments were validated while parsing");
            *fragment_counts.entry(fragment.hash).or_default() += 1;
        }
    }

    let mut by_hash = BTreeMap::<String, Vec<FragmentOccurrence>>::new();
    let mut line_counts = BTreeMap::<String, u32>::new();
    let mut marker_files = BTreeSet::<String>::new();
    for scan in &parsed {
        line_counts.insert(scan.file.to_string(), scan.line_count);
        if scan.expected_duplicate {
            marker_files.insert(scan.file.to_string());
        }
        let file = Rc::<str>::from(scan.file);
        let generated = scan.generated
            || crate::inspect::generated::path_has_generated_shape(Path::new(scan.file));
        for fragment in scan.fragments {
            let fragment = duplicate_fragment_ref(fragment)
                .expect("duplicate contribution fragments were validated while parsing");
            if fragment_counts
                .get(fragment.hash)
                .copied()
                .unwrap_or_default()
                < 2
            {
                continue;
            }
            push_occurrence(
                &mut by_hash,
                fragment.hash,
                FragmentOccurrence {
                    file: Rc::clone(&file),
                    start_line: fragment.start_line,
                    end_line: fragment.end_line,
                    cost: fragment.cost,
                    generated,
                },
            );
        }
    }

    aggregate_duplicate_occurrences(
        by_hash,
        parsed.len(),
        line_counts.values().copied().map(u64::from).sum(),
        marker_files,
        languages_skipped,
        drill_down_limit,
        expected_mirrors,
    )
}

fn duplicate_contribution_ref(value: &Value) -> Option<DuplicateContributionRef<'_>> {
    let object = value.as_object()?;
    let fragments = object.get("fragments")?.as_array()?;
    if !fragments
        .iter()
        .all(|fragment| duplicate_fragment_ref(fragment).is_some())
    {
        return None;
    }
    Some(DuplicateContributionRef {
        file: object.get("file")?.as_str()?,
        line_count: optional_u32(object.get("line_count"))?,
        expected_duplicate: optional_bool(object.get("expected_duplicate"))?,
        generated: optional_bool(object.get("generated"))?,
        fragments,
    })
}

fn duplicate_fragment_ref(value: &Value) -> Option<DuplicateFragmentRef<'_>> {
    let object = value.as_object()?;
    Some(DuplicateFragmentRef {
        hash: object.get("hash")?.as_str()?,
        start_line: required_u32(object.get("start_line")?)?,
        end_line: required_u32(object.get("end_line")?)?,
        cost: required_u32(object.get("cost")?)?,
    })
}

fn optional_u32(value: Option<&Value>) -> Option<u32> {
    value.map_or(Some(0), required_u32)
}

fn required_u32(value: &Value) -> Option<u32> {
    u32::try_from(value.as_u64()?).ok()
}

fn optional_bool(value: Option<&Value>) -> Option<bool> {
    value.map_or(Some(false), Value::as_bool)
}

#[cfg(test)]
fn aggregate_duplicate_contributions_with_clone_for_test(
    contributions: &[FileContribution],
    languages_skipped: Vec<String>,
    drill_down_limit: Option<usize>,
    expected_mirrors: &[[String; 2]],
) -> Value {
    let parsed = contributions
        .iter()
        .filter_map(|contribution| {
            serde_json::from_value::<DuplicateContribution>(contribution.contribution.clone()).ok()
        })
        .collect::<Vec<_>>();
    let mut by_hash = BTreeMap::<String, Vec<FragmentOccurrence>>::new();
    let mut line_counts = BTreeMap::<String, u32>::new();
    let mut marker_files = BTreeSet::<String>::new();

    for scan in &parsed {
        line_counts.insert(scan.file.clone(), scan.line_count);
        if scan.expected_duplicate {
            marker_files.insert(scan.file.clone());
        }
        let file = Rc::<str>::from(scan.file.as_str());
        for fragment in &scan.fragments {
            push_occurrence(
                &mut by_hash,
                fragment.hash.as_str(),
                FragmentOccurrence {
                    file: Rc::clone(&file),
                    start_line: fragment.start_line,
                    end_line: fragment.end_line,
                    cost: fragment.cost,
                    generated: scan.generated
                        || crate::inspect::generated::path_has_generated_shape(Path::new(
                            &scan.file,
                        )),
                },
            );
        }
    }

    aggregate_duplicate_occurrences(
        by_hash,
        parsed.len(),
        line_counts.values().copied().map(u64::from).sum(),
        marker_files,
        languages_skipped,
        drill_down_limit,
        expected_mirrors,
    )
}

fn aggregate_file_scans(
    file_scans: &[FileScan],
    languages_skipped: Vec<String>,
    expected_mirrors: &[[String; 2]],
) -> serde_json::Value {
    aggregate_file_scans_with_limit(
        file_scans,
        languages_skipped,
        Some(MAX_GROUP_ITEMS),
        expected_mirrors,
    )
}

fn aggregate_file_scans_with_limit(
    file_scans: &[FileScan],
    languages_skipped: Vec<String>,
    drill_down_limit: Option<usize>,
    expected_mirrors: &[[String; 2]],
) -> serde_json::Value {
    let mut by_hash = BTreeMap::<String, Vec<FragmentOccurrence>>::new();
    let mut marker_files = BTreeSet::<String>::new();

    for scan in file_scans {
        if scan.expected_duplicate {
            marker_files.insert(scan.display_path.clone());
        }
        let file = Rc::<str>::from(scan.display_path.as_str());
        for fragment in &scan.fragments {
            push_occurrence(
                &mut by_hash,
                fragment.hash.as_str(),
                FragmentOccurrence {
                    file: Rc::clone(&file),
                    start_line: fragment.start_line,
                    end_line: fragment.end_line,
                    cost: fragment.cost,
                    generated: scan.generated,
                },
            );
        }
    }

    aggregate_duplicate_occurrences(
        by_hash,
        file_scans.len(),
        file_scans
            .iter()
            .map(|scan| u64::from(scan.line_count))
            .sum(),
        marker_files,
        languages_skipped,
        drill_down_limit,
        expected_mirrors,
    )
}

fn occurrence_span_lines(occurrence: &FragmentOccurrence) -> u32 {
    occurrence
        .end_line
        .saturating_sub(occurrence.start_line)
        .saturating_add(1)
}

fn push_occurrence(
    by_hash: &mut BTreeMap<String, Vec<FragmentOccurrence>>,
    hash: &str,
    occurrence: FragmentOccurrence,
) {
    if let Some(occurrences) = by_hash.get_mut(hash) {
        occurrences.push(occurrence);
    } else {
        by_hash.insert(hash.to_string(), vec![occurrence]);
    }
}

fn aggregate_duplicate_occurrences(
    by_hash: BTreeMap<String, Vec<FragmentOccurrence>>,
    scanned_files: usize,
    total_analyzed_lines: u64,
    marker_files: BTreeSet<String>,
    languages_skipped: Vec<String>,
    drill_down_limit: Option<usize>,
    expected_mirrors: &[[String; 2]],
) -> serde_json::Value {
    // Collapse nested/overlapping duplicate fragments. tree-sitter records a
    // duplicate hash for EVERY named subtree, so one duplicated block emits the
    // outer node PLUS every nested descendant as a separate group — inflating
    // the count by an order of magnitude (e.g. one shared file → dozens of
    // overlapping groups). A duplicate fragment whose every occurrence is
    // spatially enclosed by a LARGER duplicate fragment in the same file is
    // redundant: its duplication is already implied by the enclosing block.
    // Keep only "maximal" groups — those with at least one occurrence that is
    // NOT enclosed by any other duplicate fragment. A group that is sometimes
    // standalone (a shared idiom duplicated widely on its own AND nested inside
    // a larger dup elsewhere) is preserved, because that standalone occurrence
    // is unenclosed.
    let surfaced_hashes = surfaced_duplicate_hashes(&by_hash);

    let expected_mirrors = compile_expected_mirrors(expected_mirrors);
    let mut suppression = SuppressionStats::default();
    let mut groups = by_hash
        .iter()
        .filter(|(hash, occurrences)| {
            occurrences.len() >= 2
                && surfaced_hashes.contains(hash.as_str())
                && occurrences
                    .iter()
                    .all(|occurrence| occurrence_span_lines(occurrence) >= MIN_SPAN_LINES)
        })
        .filter_map(|(_, occurrences)| {
            let mut occurrences = occurrences.clone();
            occurrences.sort_by(|left, right| {
                left.file
                    .cmp(&right.file)
                    .then(left.start_line.cmp(&right.start_line))
                    .then(left.end_line.cmp(&right.end_line))
            });
            let group = occurrences_to_group(&occurrences);
            if expected_mirror_suppresses_group(&group, &expected_mirrors) {
                suppression.mirror_groups += 1;
                return None;
            }
            if marker_suppresses_group(&group, &marker_files) {
                suppression.marker_groups += 1;
                return None;
            }
            Some(group)
        })
        .collect::<Vec<_>>();

    groups.sort_by(|left, right| {
        left.generated
            .cmp(&right.generated)
            .then(right.cost.cmp(&left.cost))
            .then(left.sample_file.cmp(&right.sample_file))
            .then(left.sample_start_line.cmp(&right.sample_start_line))
    });

    let groups_count = groups.len();
    let headline_groups = groups
        .iter()
        .filter(|group| !group.generated)
        .cloned()
        .collect::<Vec<_>>();
    let generated_groups = groups
        .iter()
        .filter(|group| group.generated)
        .cloned()
        .collect::<Vec<_>>();
    let count = headline_groups.len();
    let generated_count = generated_groups.len();
    let duplicate_stats = duplicate_line_stats(&headline_groups);
    let all_duplicate_stats = duplicate_line_stats(&groups);
    let generated_duplicate_stats = duplicate_line_stats(&generated_groups);
    let duplicated_percent =
        duplicate_percent(duplicate_stats.duplicated_lines, total_analyzed_lines);
    let drill_down_capped = drill_down_limit.is_some_and(|limit| groups_count > limit);
    let generated_drill_down_capped = drill_down_limit.is_some_and(|limit| generated_count > limit);
    let generated_items = match drill_down_limit {
        Some(limit) => generated_groups.into_iter().take(limit).collect::<Vec<_>>(),
        None => generated_groups,
    };
    let items = match drill_down_limit {
        Some(limit) => groups.into_iter().take(limit).collect::<Vec<_>>(),
        None => groups,
    };

    // Cost-ranked top-N preview for the summary view (groups are already sorted
    // by cost). One `aft_inspect` call surfaces the biggest duplications without
    // a drill-down.
    let top = items
        .iter()
        .take(TOP_PREVIEW_ITEMS)
        .map(duplicate_top_item)
        .collect::<Vec<_>>();
    let generated_top = generated_items
        .iter()
        .take(TOP_PREVIEW_ITEMS)
        .map(duplicate_top_item)
        .collect::<Vec<_>>();

    json!({
        "count": count,
        "generated_count": generated_count,
        "total_count": groups_count,
        "total_groups": groups_count,
        "groups_count": groups_count,
        "duplicated_lines": duplicate_stats.duplicated_lines,
        "duplicated_percent": duplicated_percent,
        "duplicated_file_count": duplicate_stats.file_count,
        "generated_duplicated_lines": generated_duplicate_stats.duplicated_lines,
        "generated_duplicated_file_count": generated_duplicate_stats.file_count,
        "total_duplicated_lines": all_duplicate_stats.duplicated_lines,
        "total_duplicated_file_count": all_duplicate_stats.file_count,
        "total_analyzed_lines": total_analyzed_lines,
        "suppressed_groups": suppression.mirror_groups + suppression.marker_groups,
        "mirror_suppressed_groups": suppression.mirror_groups,
        "marker_suppressed_groups": suppression.marker_groups,
        "items": items,
        "top": top,
        "generated_items": generated_items,
        "generated_top": generated_top,
        "drill_down_capped": drill_down_capped,
        "generated_drill_down_capped": generated_drill_down_capped,
        "scanned_files": scanned_files,
        "languages_skipped": languages_skipped,
    })
}

/// Hashes of duplicate fragments that are "maximal" — i.e. have at least one
/// occurrence NOT spatially enclosed by another duplicate fragment in the same
/// file. A fragment whose every occurrence is enclosed by a larger duplicate
/// fragment is collapsed away (its duplication is implied by the enclosing
/// block). tree-sitter node spans nest properly, so within a file a stack of
/// currently-open intervals yields each fragment's enclosing ancestors.
fn surfaced_duplicate_hashes(
    by_hash: &BTreeMap<String, Vec<FragmentOccurrence>>,
) -> BTreeSet<&str> {
    // Per-file intervals, restricted to fragments whose hash is actually
    // duplicated (>= 2 occurrences project-wide). Only such fragments can
    // subsume — and only such fragments are eligible to surface.
    let mut by_file: BTreeMap<&str, Vec<(u32, u32, &str)>> = BTreeMap::new();
    for (hash, occurrences) in by_hash {
        if occurrences.len() < 2 {
            continue;
        }
        for occ in occurrences {
            by_file.entry(occ.file.as_ref()).or_default().push((
                occ.start_line,
                occ.end_line,
                hash.as_str(),
            ));
        }
    }

    let mut surfaced = BTreeSet::new();
    for intervals in by_file.values_mut() {
        // Enclosing-first: a parent (smaller start, then larger end) precedes
        // its descendants. Equal spans (a wrapper node and its sole child) sort
        // adjacently; the first surfaces and the rest are treated as enclosed,
        // collapsing the pair to one region.
        intervals.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        let mut open: Vec<(u32, u32)> = Vec::new();
        for &(start, end, hash) in intervals.iter() {
            while let Some(&(_, top_end)) = open.last() {
                if top_end < start {
                    open.pop();
                } else {
                    break;
                }
            }
            // Unenclosed by any larger duplicate fragment in this file → this
            // occurrence is maximal here, so the group surfaces.
            if open.is_empty() {
                surfaced.insert(hash);
            }
            open.push((start, end));
        }
    }

    surfaced
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

fn duplicate_top_item(group: &DuplicateGroup) -> Value {
    let mut item = json!({
        "files": group.files,
        "cost": group.cost,
        "duplicated_lines": group.duplicated_lines,
    });
    if group.generated {
        item["generated"] = json!(true);
    }
    item
}

fn occurrences_to_group(occurrences: &[FragmentOccurrence]) -> DuplicateGroup {
    let sample = &occurrences[0];
    DuplicateGroup {
        files: occurrences
            .iter()
            .map(|occurrence| {
                format!(
                    "{}:{}-{}",
                    occurrence.file.as_ref(),
                    occurrence.start_line,
                    occurrence.end_line
                )
            })
            .collect(),
        cost: sample.cost,
        duplicated_lines: occurrences.iter().map(occurrence_line_count).sum(),
        sample_file: sample.file.to_string(),
        sample_start_line: sample.start_line,
        sample_end_line: sample.end_line,
        generated: occurrences.iter().all(|occurrence| occurrence.generated),
    }
}

#[derive(Debug, Default)]
struct DuplicateLineStats {
    duplicated_lines: u64,
    file_count: usize,
}

fn duplicate_line_stats(groups: &[DuplicateGroup]) -> DuplicateLineStats {
    let mut by_file = BTreeMap::<String, Vec<(u32, u32)>>::new();
    for group in groups {
        for occurrence in &group.files {
            let Some((file, start, end)) = parse_duplicate_occurrence(occurrence) else {
                continue;
            };
            by_file
                .entry(file.to_string())
                .or_default()
                .push((start, end));
        }
    }

    let file_count = by_file.len();
    let duplicated_lines = by_file
        .values_mut()
        .map(|intervals| merged_interval_line_count(intervals))
        .sum();

    DuplicateLineStats {
        duplicated_lines,
        file_count,
    }
}

fn merged_interval_line_count(intervals: &mut [(u32, u32)]) -> u64 {
    if intervals.is_empty() {
        return 0;
    }

    intervals.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
    let mut total = 0u64;
    let (mut current_start, mut current_end) = intervals[0];
    for &(start, end) in &intervals[1..] {
        if start <= current_end.saturating_add(1) {
            current_end = current_end.max(end);
        } else {
            total += inclusive_line_count(current_start, current_end);
            current_start = start;
            current_end = end;
        }
    }
    total + inclusive_line_count(current_start, current_end)
}

fn inclusive_line_count(start_line: u32, end_line: u32) -> u64 {
    u64::from(end_line.saturating_sub(start_line).saturating_add(1))
}

fn occurrence_line_count(occurrence: &FragmentOccurrence) -> u32 {
    occurrence
        .end_line
        .saturating_sub(occurrence.start_line)
        .saturating_add(1)
}

fn parse_duplicate_occurrence(value: &str) -> Option<(&str, u32, u32)> {
    let (file, range) = value.rsplit_once(':')?;
    let (start, end) = range.split_once('-')?;
    if !start.chars().all(|char| char.is_ascii_digit())
        || !end.chars().all(|char| char.is_ascii_digit())
    {
        return None;
    }

    Some((file, start.parse().ok()?, end.parse().ok()?))
}

fn duplicate_percent(duplicated_lines: u64, total_analyzed_lines: u64) -> f64 {
    if total_analyzed_lines == 0 {
        0.0
    } else {
        (duplicated_lines as f64 * 100.0) / total_analyzed_lines as f64
    }
}

fn source_line_count(source: &str) -> u32 {
    source.lines().count().try_into().unwrap_or(u32::MAX)
}

fn compile_expected_mirrors(expected_mirrors: &[[String; 2]]) -> Vec<ExpectedMirrorPair> {
    expected_mirrors
        .iter()
        .filter_map(|[left, right]| {
            Some(ExpectedMirrorPair {
                left: compile_single_glob(left)?,
                right: compile_single_glob(right)?,
            })
        })
        .collect()
}

fn compile_single_glob(pattern: &str) -> Option<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    builder.add(Glob::new(pattern).ok()?);
    builder.build().ok()
}

fn expected_mirror_suppresses_group(
    group: &DuplicateGroup,
    expected_mirrors: &[ExpectedMirrorPair],
) -> bool {
    if expected_mirrors.is_empty() {
        return false;
    }
    let files = group_files(group);
    expected_mirrors.iter().any(|pair| {
        let mut has_left = false;
        let mut has_right = false;
        for file in &files {
            let left = pair.left.is_match(file.as_str());
            let right = pair.right.is_match(file.as_str());
            if !left && !right {
                return false;
            }
            has_left |= left;
            has_right |= right;
        }
        has_left && has_right
    })
}

fn marker_suppresses_group(group: &DuplicateGroup, marker_files: &BTreeSet<String>) -> bool {
    !marker_files.is_empty()
        && group_files(group)
            .iter()
            .any(|file| marker_files.contains(file.as_str()))
}

fn group_files(group: &DuplicateGroup) -> BTreeSet<String> {
    group
        .files
        .iter()
        .map(|occurrence| display_file_from_occurrence(occurrence).to_string())
        .collect()
}

fn display_file_from_occurrence(value: &str) -> &str {
    parse_duplicate_occurrence(value)
        .map(|(file, _, _)| file)
        .unwrap_or(value)
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
            | LangId::Scss
            | LangId::Vue
            | LangId::Markdown
            | LangId::Java
            | LangId::Ruby
            | LangId::Kotlin
            | LangId::Swift
            | LangId::Php
            | LangId::Lua
            | LangId::Perl
            | LangId::Pascal
            | LangId::R
            | LangId::Groovy
            | LangId::ObjC
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
        LangId::Cuda => "cuda",
        LangId::Metal => "metal",
        LangId::Zig => "zig",
        LangId::CSharp => "csharp",
        LangId::Bash => "bash",
        LangId::Html => "html",
        LangId::Markdown => "markdown",
        LangId::Yaml => "yaml",
        LangId::Solidity => "solidity",
        LangId::Scss => "scss",
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
        LangId::Pascal => "pascal",
        LangId::R => "r",
        LangId::Groovy => "groovy",
        LangId::ObjC => "objc",
        LangId::Toml => "toml",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_freshness::FileFreshness;
    use serde_json::json;

    fn freshness() -> FileFreshness {
        FileFreshness {
            mtime: std::time::SystemTime::UNIX_EPOCH,
            size: 0,
            content_hash: blake3::hash(b""),
        }
    }

    /// Build a duplicates FileContribution from (start, end, cost, hash) fragments.
    fn contribution(file: &str, fragments: &[(u32, u32, u32, &str)]) -> FileContribution {
        contribution_with_generated(file, 100, false, false, fragments)
    }

    fn contribution_with_options(
        file: &str,
        line_count: u32,
        expected_duplicate: bool,
        fragments: &[(u32, u32, u32, &str)],
    ) -> FileContribution {
        contribution_with_generated(file, line_count, expected_duplicate, false, fragments)
    }

    fn contribution_with_generated(
        file: &str,
        line_count: u32,
        expected_duplicate: bool,
        generated: bool,
        fragments: &[(u32, u32, u32, &str)],
    ) -> FileContribution {
        let frag_json = fragments
            .iter()
            .map(|(start, end, cost, hash)| {
                json!({
                    "hash": hash,
                    "start_line": start,
                    "end_line": end,
                    "cost": cost,
                })
            })
            .collect::<Vec<_>>();
        let mut contribution = json!({
            "file": file,
            "line_count": line_count,
            "expected_duplicate": expected_duplicate,
            "fragments": frag_json,
        });
        if generated {
            contribution["generated"] = json!(true);
        }
        FileContribution::new(InspectCategory::Duplicates, file, freshness(), contribution)
    }

    fn group_count(aggregate: &serde_json::Value) -> u64 {
        aggregate["count"].as_u64().unwrap()
    }

    fn unique_fragment_contributions(fragment_count: usize) -> Vec<FileContribution> {
        let fragments = (0..fragment_count)
            .map(|index| {
                json!({
                    "hash": format!("unique-{index}"),
                    "start_line": index as u32 + 1,
                    "end_line": index as u32 + 10,
                    "cost": 100,
                })
            })
            .collect::<Vec<_>>();
        vec![FileContribution::new(
            InspectCategory::Duplicates,
            "src/unique.ts",
            freshness(),
            json!({
                "file": "src/unique.ts",
                "line_count": 2_000,
                "expected_duplicate": false,
                "fragments": fragments,
            }),
        )]
    }

    #[test]
    fn borrowed_rollup_is_byte_identical_to_clone_rollup() {
        let contributions = vec![
            contribution(
                "src/a.ts",
                &[
                    (1, 20, 900, "shared"),
                    (30, 39, 200, "kept"),
                    (50, 60, 100, "single"),
                ],
            ),
            contribution("src/b.ts", &[(2, 21, 900, "shared"), (40, 49, 200, "kept")]),
            contribution_with_generated("gen/a.ts", 80, false, true, &[(1, 12, 700, "gen")]),
            contribution_with_generated("gen/b.ts", 80, false, true, &[(3, 14, 700, "gen")]),
            contribution("plugin/mirror.ts", &[(5, 16, 500, "mirror")]),
            contribution("pi-plugin/mirror.ts", &[(5, 16, 500, "mirror")]),
            contribution_with_options("src/marked.ts", 60, true, &[(10, 22, 300, "marked")]),
            contribution("src/plain.ts", &[(10, 22, 300, "marked")]),
        ];
        let expected_mirrors = vec![["plugin/**".to_string(), "pi-plugin/**".to_string()]];

        let before = aggregate_duplicate_contributions_with_clone_for_test(
            &contributions,
            vec!["markdown".to_string()],
            Some(3),
            &expected_mirrors,
        );
        let after = aggregate_duplicate_contributions_with_limit(
            &contributions,
            vec!["markdown".to_string()],
            Some(3),
            &expected_mirrors,
        );

        assert_eq!(
            serde_json::to_vec(&after).expect("serialize borrowed rollup"),
            serde_json::to_vec(&before).expect("serialize clone rollup")
        );
    }

    #[test]
    fn borrowed_rollup_allocations_do_not_scale_with_discarded_fragments() {
        let small = unique_fragment_contributions(8);
        let large = unique_fragment_contributions(1_024);
        let (_, small_allocations) = crate::test_allocations::count(|| {
            aggregate_duplicate_contributions_with_limit(&small, Vec::new(), Some(20), &[])
        });
        let (after, large_allocations) = crate::test_allocations::count(|| {
            aggregate_duplicate_contributions_with_limit(&large, Vec::new(), Some(20), &[])
        });
        let (before, clone_allocations) = crate::test_allocations::count(|| {
            aggregate_duplicate_contributions_with_clone_for_test(&large, Vec::new(), Some(20), &[])
        });

        assert_eq!(
            serde_json::to_vec(&after).expect("serialize borrowed rollup"),
            serde_json::to_vec(&before).expect("serialize clone rollup")
        );
        assert!(
            large_allocations <= small_allocations + 24,
            "discarded fragments added too many allocations: small={small_allocations}, large={large_allocations}"
        );
        assert!(
            clone_allocations > large_allocations + 2 * 1_024,
            "allocation seam is vacuous: clone={clone_allocations}, borrowed={large_allocations}"
        );
        eprintln!(
            "duplicates rollup allocations for 1,024 discarded fragments: clone={clone_allocations}, borrowed={large_allocations}"
        );
    }

    #[test]
    fn generated_duplicate_groups_sort_below_headline_and_keep_totals() {
        let aggregate = aggregate_duplicate_contributions_with_limit(
            &[
                contribution("src/a.ts", &[(1, 10, 10, "hand")]),
                contribution("src/b.ts", &[(1, 10, 10, "hand")]),
                contribution_with_generated("gen/a.ts", 100, false, true, &[(1, 12, 100, "gen")]),
                contribution_with_generated("gen/b.ts", 100, false, true, &[(1, 12, 100, "gen")]),
            ],
            Vec::new(),
            None,
            &[],
        );

        assert_eq!(aggregate["count"], 1, "{aggregate:#}");
        assert_eq!(aggregate["generated_count"], 1, "{aggregate:#}");
        assert_eq!(aggregate["total_groups"], 2, "{aggregate:#}");
        assert_eq!(aggregate["total_count"], 2, "{aggregate:#}");
        let items = aggregate["items"].as_array().expect("items");
        assert_eq!(items[0]["files"][0], "src/a.ts:1-10", "{items:#?}");
        assert_eq!(items[1]["generated"], true, "{items:#?}");
        assert_eq!(aggregate["top"][0]["files"][0], "src/a.ts:1-10");
        assert_eq!(aggregate["generated_top"][0]["files"][0], "gen/a.ts:1-12");
    }

    #[test]
    fn reported_duplicate_groups_require_each_occurrence_to_span_ten_lines() {
        let short = aggregate_duplicate_contributions(
            &[
                contribution("src/a.ts", &[(1, 9, 200, "dense-call-chain")]),
                contribution("src/b.ts", &[(1, 9, 200, "dense-call-chain")]),
            ],
            Vec::new(),
        );
        assert_eq!(short["total_groups"], 0, "aggregate: {short:#}");

        let long = aggregate_duplicate_contributions(
            &[
                contribution("src/a.ts", &[(1, 10, 200, "dense-call-chain")]),
                contribution("src/b.ts", &[(1, 10, 200, "dense-call-chain")]),
            ],
            Vec::new(),
        );
        assert_eq!(long["total_groups"], 1, "aggregate: {long:#}");
    }

    #[test]
    fn nested_fragments_of_one_duplicated_block_collapse_to_one_group() {
        // A duplicated block (hash "block", lines 1-20) whose nested child
        // (hash "child", lines 3-8) is ALSO duplicated across the same files.
        // Without collapse this is 2 groups; the child is enclosed by the block
        // in every occurrence, so it must collapse to 1 maximal group.
        let contributions = vec![
            contribution("src/a.ts", &[(1, 20, 1000, "block"), (3, 8, 400, "child")]),
            contribution("src/b.ts", &[(1, 20, 1000, "block"), (3, 8, 400, "child")]),
        ];

        let aggregate = aggregate_duplicate_contributions(&contributions, Vec::new());
        assert_eq!(group_count(&aggregate), 1, "aggregate: {aggregate:#}");
        assert_eq!(aggregate["items"][0]["cost"], 1000);
    }

    #[test]
    fn widely_duplicated_idiom_survives_even_when_nested_elsewhere() {
        // "idiom" is nested inside "block" in a.ts/b.ts, BUT also appears
        // standalone (unenclosed) in c.ts/d.ts. Its standalone occurrences are
        // maximal, so the idiom group must be preserved alongside the block.
        let contributions = vec![
            contribution("src/a.ts", &[(1, 20, 1000, "block"), (3, 12, 400, "idiom")]),
            contribution("src/b.ts", &[(1, 20, 1000, "block"), (3, 12, 400, "idiom")]),
            contribution("src/c.ts", &[(40, 49, 400, "idiom")]),
            contribution("src/d.ts", &[(70, 79, 400, "idiom")]),
        ];

        let aggregate = aggregate_duplicate_contributions(&contributions, Vec::new());
        assert_eq!(group_count(&aggregate), 2, "aggregate: {aggregate:#}");
    }

    #[test]
    fn disjoint_sibling_duplications_are_both_kept() {
        // Two unrelated duplicated blocks, side by side (no enclosure). Both
        // are maximal → both kept.
        let contributions = vec![
            contribution("src/a.ts", &[(1, 10, 300, "alpha"), (20, 30, 300, "beta")]),
            contribution("src/b.ts", &[(1, 10, 300, "alpha"), (20, 30, 300, "beta")]),
        ];

        let aggregate = aggregate_duplicate_contributions(&contributions, Vec::new());
        assert_eq!(group_count(&aggregate), 2, "aggregate: {aggregate:#}");
    }

    #[test]
    fn non_duplicated_fragment_does_not_subsume() {
        // The enclosing "block" appears only once (NOT duplicated), so it must
        // not subsume the genuinely-duplicated nested "child". The child is the
        // only real duplication and must surface.
        let contributions = vec![
            contribution(
                "src/a.ts",
                &[(1, 20, 1000, "uniqueA"), (3, 12, 400, "child")],
            ),
            contribution(
                "src/b.ts",
                &[(1, 20, 1000, "uniqueB"), (3, 12, 400, "child")],
            ),
        ];

        let aggregate = aggregate_duplicate_contributions(&contributions, Vec::new());
        assert_eq!(group_count(&aggregate), 1, "aggregate: {aggregate:#}");
        // The surfaced group is the child, not either unique block.
        assert_eq!(aggregate["items"][0]["cost"], 400);
    }

    #[test]
    fn framing_counts_unique_duplicated_lines_against_analyzed_lines() {
        let contributions = vec![
            contribution_with_options("src/a.ts", 20, false, &[(2, 11, 300, "block")]),
            contribution_with_options("src/b.ts", 20, false, &[(6, 15, 300, "block")]),
            contribution_with_options("src/c.ts", 10, false, &[]),
        ];

        let aggregate = aggregate_duplicate_contributions(&contributions, Vec::new());

        assert_eq!(aggregate["duplicated_lines"], 20);
        assert_eq!(aggregate["total_analyzed_lines"], 50);
        assert_eq!(aggregate["duplicated_percent"].as_f64(), Some(40.0));
        assert_eq!(aggregate["duplicated_file_count"], 2);
        assert_eq!(aggregate["total_groups"], 1);
    }

    #[test]
    fn expected_mirrors_suppress_only_groups_fully_straddling_pair() {
        let contributions = vec![
            contribution("plugin/a.ts", &[(1, 10, 300, "mirror")]),
            contribution("pi-plugin/a.ts", &[(1, 10, 300, "mirror")]),
            contribution("plugin/b.ts", &[(10, 19, 400, "within-left")]),
            contribution("plugin/c.ts", &[(10, 19, 400, "within-left")]),
            contribution("plugin/d.ts", &[(20, 29, 500, "has-neither")]),
            contribution("pi-plugin/d.ts", &[(20, 29, 500, "has-neither")]),
            contribution("other/d.ts", &[(20, 29, 500, "has-neither")]),
        ];
        let expected_mirrors = vec![["plugin/**".to_string(), "pi-plugin/**".to_string()]];

        let aggregate = aggregate_duplicate_contributions_with_limit(
            &contributions,
            Vec::new(),
            None,
            &expected_mirrors,
        );

        assert_eq!(aggregate["total_groups"], 2, "aggregate: {aggregate:#}");
        assert_eq!(aggregate["mirror_suppressed_groups"], 1);
        assert!(aggregate["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|file| file.as_str().unwrap().starts_with("plugin/b.ts:"))));
        assert!(aggregate["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|file| file.as_str().unwrap().starts_with("other/d.ts:"))));
    }

    #[test]
    fn expected_duplicate_marker_suppresses_groups_in_marked_files() {
        let contributions = vec![
            contribution_with_options("src/marked.ts", 20, true, &[(1, 10, 300, "block")]),
            contribution_with_options("src/plain.ts", 20, false, &[(1, 10, 300, "block")]),
        ];

        let aggregate = aggregate_duplicate_contributions(&contributions, Vec::new());

        assert_eq!(aggregate["total_groups"], 0, "aggregate: {aggregate:#}");
        assert_eq!(aggregate["marker_suppressed_groups"], 1);
        assert_eq!(aggregate["suppressed_groups"], 1);
    }

    #[test]
    fn deeply_nested_tree_does_not_overflow_stack() {
        // Regression for the inspect-thread stack overflow / SIGABRT: a
        // pathologically deep expression (here ~6000 nested parentheses, far
        // past MAX_FRAGMENT_DEPTH) must not recurse unbounded. Before the depth
        // guard this overflowed the bounded inspect worker stack and aborted the
        // whole process. We run it on the current (main) thread, which has a
        // larger stack than rayon workers, but the guard is what makes it safe
        // on the bounded pool. Reaching the assert at all proves no overflow.
        let depth = 6_000usize;
        let mut source = String::with_capacity(depth * 2 + 32);
        source.push_str("const x = ");
        for _ in 0..depth {
            source.push('(');
        }
        source.push('1');
        for _ in 0..depth {
            source.push(')');
        }
        source.push_str(";\n");

        let lang = LangId::TypeScript;
        let tree = parse_source(Path::new("deep.ts"), lang, &source).expect("parse deep source");
        let mut fragments = Vec::new();
        // Must return instead of overflowing. Run in a bounded-stack thread to
        // mirror the inspect pool and prove the guard (not just a big stack)
        // is doing the work.
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                let mut hash_scratch = Vec::new();
                collect_fragments(
                    tree.root_node(),
                    &source,
                    lang,
                    &mut fragments,
                    &mut hash_scratch,
                    0,
                );
            })
            .expect("spawn bounded-stack worker")
            .join()
            .expect("deep-tree scan must not overflow the bounded stack");
    }
}
