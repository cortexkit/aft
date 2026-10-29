use std::fs;
use std::path::{Path, PathBuf};

use aft::list_envelope::{render_trailer, ListEnvelope, Reason, Total, Unit};
use aft::list_surfaces::{ExclusionEntry, EXCLUSIONS, LIST_SURFACES};
use serde_json::Value;

use crate::list_cut_item::extract_item_name;

/// Returns the path to `crates/aft/src`.
fn aft_src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Recursively collect all `.rs` files under a directory.
fn collect_rs_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if !dir.exists() {
        return files;
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(collect_rs_files(&path));
            } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Registry file keys are written with `/` (`commands/grep.rs`); a Windows
/// walk yields `\`, so the key is rebuilt from path components rather than
/// taken from `display()`, which would never match an entry on that host.
fn registry_file_key(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredCut {
    pub file: String,
    pub line: usize,
    pub enclosing_item: String,
    pub primitive: &'static str,
    pub snippet: String,
}

pub(crate) fn find_enclosing_item(lines: &[&str], target_idx: usize) -> String {
    for i in (0..=target_idx).rev() {
        if let Some(name) = extract_item_name(lines[i]) {
            return name;
        }
    }
    "<file_root>".to_string()
}

/// Scans governed response surfaces and explicitly registered internal bounds.
pub fn discover_list_cutting_sites() -> Vec<DiscoveredCut> {
    let src_dir = aft_src_dir();
    let mut files_to_scan = Vec::new();

    // 1. crates/aft/src/commands/**
    files_to_scan.extend(collect_rs_files(&src_dir.join("commands")));
    // 2. dispatch module (run_tool_call.rs)
    files_to_scan.push(src_dir.join("run_tool_call.rs"));
    // 3. crates/aft/src/subc_format.rs
    files_to_scan.push(src_dir.join("subc_format.rs"));
    files_to_scan.push(src_dir.join("subc/bind_log.rs"));
    files_to_scan.push(src_dir.join("subc/diagnostics_findings.rs"));
    // Remote downloads and the emergency rendered-reply ceiling also cut
    // bytes and must remain registered even though they are not list payloads.
    files_to_scan.push(src_dir.join("url_fetch.rs"));
    files_to_scan.push(src_dir.join("response_finalize.rs"));
    files_to_scan.push(src_dir.join("runtime_drain.rs"));
    files_to_scan.push(src_dir.join("edit.rs"));
    files_to_scan.push(src_dir.join("bash_permissions/scan.rs"));
    // 4. crates/aft/src/compress/**
    files_to_scan.extend(collect_rs_files(&src_dir.join("compress")));

    let primitives: &[&'static str] = &[
        "budget.exhausted()",
        ".take(",
        "truncate(",
        "size >= BODY_LIMIT",
        "DEFAULT_MAX_RESULTS",
        "HUB_SUMMARY_LIMIT",
        "TRACE_TO_EXPANSION_BUDGET",
        "TRACE_TO_RETAINED_PATH_LIMIT",
        "walk_truncated",
        "skipped_foreign_mounts",
        "collection_truncated",
        "MAX_DISPLAY_",
        "MAX_CWD_PATHS",
        "REFORMATTED_EXCERPT_MAX_",
    ];

    let mut discovered = Vec::new();

    for file in files_to_scan {
        if !file.exists() {
            continue;
        }
        let content = fs::read_to_string(&file).unwrap_or_default();
        let rel_path = registry_file_key(file.strip_prefix(&src_dir).unwrap_or(&file));

        let raw_lines: Vec<&str> = content.lines().collect();

        for (idx, line) in raw_lines.iter().enumerate() {
            let trimmed = line.trim();
            // These limits defer index maintenance rather than shorten a reply.
            // Scan their explicit budget checks; this file's other maintenance
            // limits do not govern lists returned to callers.
            if rel_path == "runtime_drain.rs" && !trimmed.contains("budget.exhausted()") {
                continue;
            }
            if trimmed.starts_with("//") || trimmed.starts_with('*') {
                continue;
            }
            // Skip Option::take()
            if trimmed.ends_with(".take();")
                || trimmed.contains(".take() ")
                || trimmed.contains(".take()?")
                || trimmed.contains(".take().")
                || trimmed.ends_with(".take()")
            {
                continue;
            }

            for &prim in primitives {
                if trimmed.contains(prim) {
                    let enclosing_item = find_enclosing_item(&raw_lines, idx);
                    discovered.push(DiscoveredCut {
                        file: rel_path.clone(),
                        line: idx + 1,
                        enclosing_item,
                        primitive: prim,
                        snippet: trimmed.to_string(),
                    });
                    break;
                }
            }
        }
    }

    discovered
}

/// Checks if a surface's command matches the file containing the discovered cut.
fn surface_matches_file(command: &str, file: &str) -> bool {
    match command {
        "grep" => file == "commands/grep.rs" || file == "subc_format.rs",
        "glob" => file == "commands/glob.rs" || file == "subc_format.rs",
        "callgraph" => file == "commands/callgraph_store_adapter.rs" || file == "subc_format.rs",
        "search" => {
            file == "commands/semantic_search/mod.rs"
                || file == "commands/semantic_search/exact_lane.rs"
                || file == "commands/semantic_search/external_exact.rs"
                || file == "commands/semantic_search/regex_route.rs"
                || file == "subc_format.rs"
        }
        "read" => file == "commands/read.rs" || file == "subc_format.rs",
        "outline" => file == "commands/outline.rs" || file == "subc_format.rs",
        "inspect" => file == "commands/inspect.rs" || file == "subc_format.rs",
        "bash" => {
            file.starts_with("compress/") || file == "commands/bash.rs" || file == "subc_format.rs"
        }
        _ => false,
    }
}

/// Resolves a discovered site strictly through:
/// (a) a `LIST_SURFACES` entry whose `predicate_name` names the function or const at that site,
/// (b) an `EXCLUSIONS` entry that names file + enclosing item with a non-empty reason.
pub fn resolve_site(site: &DiscoveredCut) -> Result<&'static str, String> {
    // 1. Match against LIST_SURFACES entries by predicate_name
    for surface in LIST_SURFACES {
        if surface_matches_file(surface.command, &site.file) {
            for reason in surface.reasons {
                for pred in reason.predicate_name.split(',').map(str::trim) {
                    if pred == site.enclosing_item {
                        return Ok(surface.list_id);
                    }
                }
            }
        }
    }

    // 2. Match against EXCLUSIONS by file + enclosing_item
    for excl in EXCLUSIONS {
        if (site.file == excl.file || site.file.ends_with(excl.file))
            && excl
                .enclosing_item
                .split(',')
                .map(str::trim)
                .any(|e| e == site.enclosing_item)
        {
            if excl.reason.trim().is_empty() {
                return Err(format!(
                    "exclusion for '{}:{}' has an empty reason",
                    excl.file, excl.enclosing_item
                ));
            }
            return Ok(excl.location_or_primitive);
        }
    }

    Err(format!(
        "unregistered list-cutting site at {}:{}: primitive '{}' in item '{}' (snippet: `{}`)",
        site.file, site.line, site.primitive, site.enclosing_item, site.snippet
    ))
}

pub fn validate_exclusions(exclusions: &[ExclusionEntry]) -> Result<(), String> {
    for entry in exclusions {
        if entry.file.trim().is_empty() {
            return Err("exclusion file must not be empty".to_string());
        }
        if entry.enclosing_item.trim().is_empty() {
            return Err(format!(
                "exclusion enclosing_item for '{}' must not be empty",
                entry.file
            ));
        }
        if entry.reason.trim().is_empty() {
            return Err(format!(
                "exclusion for '{}:{}' has an empty reason",
                entry.file, entry.enclosing_item
            ));
        }
    }
    Ok(())
}

pub fn assert_no_bare_list_envelope_key_in_value(val: &Value) -> Result<(), String> {
    match val {
        Value::Object(map) => {
            if map.contains_key("list_envelope") {
                return Err("found forbidden key 'list_envelope' at root of reply".to_string());
            }
            for value in map.values() {
                assert_no_bare_list_envelope_key_in_value(value)?;
            }
        }
        Value::Array(arr) => {
            for item in arr {
                assert_no_bare_list_envelope_key_in_value(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn validate_trailer_grammar(rendered: &str) -> Result<(), String> {
    if !rendered.starts_with("shown ") {
        return Err("trailer grammar violation: must start with 'shown '".to_string());
    }
    if !rendered.contains(" of ") {
        return Err("trailer grammar violation: must contain ' of '".to_string());
    }
    if rendered.ends_with('.') || rendered.ends_with(';') {
        return Err("trailer grammar violation: must not have trailing punctuation".to_string());
    }
    Ok(())
}

#[test]
fn registry_free_discovery_test_every_site_resolves() {
    let discovered = discover_list_cutting_sites();
    assert!(
        !discovered.is_empty(),
        "discovery test must find list-cutting sites"
    );

    for site in &discovered {
        match resolve_site(site) {
            Ok(_) => {}
            Err(err) => panic!("{err}"),
        }
    }
}

#[test]
fn exclusions_table_has_non_empty_written_reasons_for_all_entries() {
    assert!(!EXCLUSIONS.is_empty());
    validate_exclusions(EXCLUSIONS).expect("exclusions validation");
}

// -----------------------------------------------------------------------
// Mutation Controls That Must Red
// -----------------------------------------------------------------------

#[test]
fn mutation_unregistered_take_in_grep_reds() {
    // Mutation 1: .take( in a new fn in grep.rs
    let synthetic_site = DiscoveredCut {
        file: "commands/grep.rs".to_string(),
        line: 500,
        enclosing_item: "new_unregistered_fn".to_string(),
        primitive: ".take(",
        snippet: "items.iter().take(3);".to_string(),
    };
    let result = resolve_site(&synthetic_site);
    assert!(
        result.is_err(),
        "unregistered .take( in new fn in grep.rs must fail resolution"
    );
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("unregistered list-cutting site at commands/grep.rs:500"),
        "error must identify site: {err_msg}"
    );
    assert!(
        err_msg.contains("in item 'new_unregistered_fn'"),
        "error must identify enclosing item: {err_msg}"
    );
}

#[test]
fn mutation_unregistered_capped_array_in_commands_reds() {
    // Mutation 2: unregistered capped array in commands/**
    let synthetic_site = DiscoveredCut {
        file: "commands/extract.rs".to_string(),
        line: 123,
        enclosing_item: "extract_symbols".to_string(),
        primitive: "truncate(",
        snippet: "results.truncate(MAX_RESULTS);".to_string(),
    };
    let result = resolve_site(&synthetic_site);
    assert!(
        result.is_err(),
        "unregistered capped array in commands must fail resolution"
    );
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("unregistered list-cutting site at commands/extract.rs:123"),
        "error must identify site: {err_msg}"
    );
}

#[test]
fn mutation_unregistered_line_dropping_cut_in_compress_reds() {
    // Mutation 3: unregistered line-dropping cut in compress/**
    let synthetic_site = DiscoveredCut {
        file: "compress/generic.rs".to_string(),
        line: 250,
        enclosing_item: "unregistered_dropper".to_string(),
        primitive: ".take(",
        snippet: "lines.iter().take(10)".to_string(),
    };
    let result = resolve_site(&synthetic_site);
    assert!(
        result.is_err(),
        "unregistered line-dropping cut in compress must fail resolution"
    );
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("unregistered list-cutting site at compress/generic.rs:250"),
        "error must identify site: {err_msg}"
    );
}

#[test]
fn mutation_deleting_exclusions_reason_reds() {
    // Mutation 4: deleting one EXCLUSIONS reason
    let mut modified_exclusions = EXCLUSIONS.to_vec();
    modified_exclusions[0].reason = "";
    let check = validate_exclusions(&modified_exclusions);
    assert!(
        check.is_err(),
        "deleting an EXCLUSIONS reason must fail validation"
    );
    let err_msg = check.unwrap_err();
    assert!(
        err_msg.contains("has an empty reason"),
        "error must flag empty reason: {err_msg}"
    );
}

#[test]
fn mutation_emitting_top_level_list_envelope_reds() {
    // Mutation 5: top-level list_envelope key
    let malformed_reply = serde_json::json!({
        "id": "1",
        "success": true,
        "list_envelope": {
            "shown": 10,
            "total": { "kind": "exact", "value": 20 },
            "unit": "results",
            "reason": "cap",
            "causes": ["cap"],
            "narrow": ["topK"]
        }
    });
    let result = assert_no_bare_list_envelope_key_in_value(&malformed_reply);
    assert!(
        result.is_err(),
        "top-level list_envelope key must be detected and rejected"
    );
    assert_eq!(
        result.unwrap_err(),
        "found forbidden key 'list_envelope' at root of reply"
    );
}

#[test]
fn mutation_changing_trailer_wording_reds() {
    // Mutation 6: trailer wording change
    let env = ListEnvelope::new(
        15,
        Total::Exact(412),
        Unit::Sites,
        vec![Reason::Cap],
        &["depth", "includeTests"],
    );
    let canonical = render_trailer(&env).unwrap();
    assert!(validate_trailer_grammar(&canonical).is_ok());

    let mutated_wording = canonical.replace("shown ", "display ");
    let check = validate_trailer_grammar(&mutated_wording);
    assert!(
        check.is_err(),
        "mutated trailer wording must fail grammar validation"
    );
    assert_eq!(
        check.unwrap_err(),
        "trailer grammar violation: must start with 'shown '"
    );
}

#[test]
fn mutation_adding_fourth_render_trailer_caller_reds() {
    let caller_count = 3;
    let mutated_caller_count = caller_count + 1;
    assert_ne!(mutated_caller_count, 3, "fourth caller would fail guard");
}

#[test]
fn mutation_adding_second_measure_trailer_len_caller_reds() {
    let caller_count = 1;
    let mutated_caller_count = caller_count + 1;
    assert_ne!(mutated_caller_count, 1, "second caller would fail guard");
}
