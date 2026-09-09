use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub struct LintViolation {
    pub file: String,
    pub line: usize,
    pub rule: String,
    pub message: String,
}

/// Token opacity lint:
/// Fails any substring, split, parse, order, or range comparison of snapshot_generation
/// anywhere in A's fence. Equality (==, !=) is the only permitted operation.
pub fn lint_token_opacity(file: &str, content: &str) -> Result<(), Vec<LintViolation>> {
    let mut violations = Vec::new();

    for (line_idx, line) in content.lines().enumerate() {
        let line_num = line_idx + 1;
        let trimmed = line.trim();

        // Skip comments
        if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
            continue;
        }

        if line.contains("snapshot_generation") {
            // Check for forbidden operations on snapshot_generation

            // Substring / range: e.g. [..], .substring
            if line.contains(".substring(")
                || (line.contains("snapshot_generation")
                    && (line.contains("[..") || line.contains("..]")))
            {
                violations.push(LintViolation {
                    file: file.to_string(),
                    line: line_num,
                    rule: "token_opacity:substring_range".to_string(),
                    message: format!(
                        "substring or range slice on snapshot_generation is forbidden: {trimmed}"
                    ),
                });
            }

            // Split: e.g. .split(
            if line.contains(".split(") || line.contains(".split_whitespace(") {
                violations.push(LintViolation {
                    file: file.to_string(),
                    line: line_num,
                    rule: "token_opacity:split".to_string(),
                    message: format!("split on snapshot_generation is forbidden: {trimmed}"),
                });
            }

            // Parse: e.g. .parse::<
            if line.contains(".parse(") || line.contains(".parse::<") {
                violations.push(LintViolation {
                    file: file.to_string(),
                    line: line_num,
                    rule: "token_opacity:parse".to_string(),
                    message: format!("parse on snapshot_generation is forbidden: {trimmed}"),
                });
            }

            // Order comparison: <, >, <=, >= (excluding ==, !=, =>)
            for op in [" < ", " > ", " <= ", " >= "] {
                if line.contains(&format!("snapshot_generation{op}"))
                    || line.contains(&format!("{op}snapshot_generation"))
                    || line.contains(&format!("{op}*snapshot_generation"))
                {
                    violations.push(LintViolation {
                        file: file.to_string(),
                        line: line_num,
                        rule: "token_opacity:order".to_string(),
                        message: format!(
                            "order comparison ({op}) on snapshot_generation is forbidden: {trimmed}"
                        ),
                    });
                }
            }
        }
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

/// Cross-depth harness lint:
/// Fails any test comparing whole replies, trailers, lanes_exhausted, or lane_positions
/// across two different reached depths.
/// The allow-list across depths is exactly (ranked tuple, evidence descriptor) and the
/// derived confidence verdict.
pub fn lint_cross_depth_harness(file: &str, content: &str) -> Result<(), Vec<LintViolation>> {
    let mut violations = Vec::new();
    let mut in_cross_depth_context = false;

    for (line_idx, line) in content.lines().enumerate() {
        let line_num = line_idx + 1;
        let trimmed = line.trim();

        if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
            continue;
        }

        // Detect cross-depth test contexts or functions
        if line.contains("fn test_") || line.contains("fn ") {
            in_cross_depth_context = line.contains("cross_depth")
                || line.contains("different_reached_depths")
                || line.contains("across_depths")
                || line.contains("depth");
        }

        let is_cross_depth_line = in_cross_depth_context
            || line.contains("cross_depth")
            || line.contains("depth_200")
            || line.contains("depth_400")
            || line.contains("shallow")
            || line.contains("deep");

        // In a cross-depth context or on a cross-depth line
        if is_cross_depth_line {
            // Whole replies comparison across depths
            if (line.contains("assert_eq!") || line.contains("assert!"))
                && (line.contains("reply_200, reply_400")
                    || line.contains("reply_shallow, reply_deep")
                    || line.contains("shallow_reply, deep_reply")
                    || (line.contains("whole_reply") && !line.contains("rule")))
            {
                violations.push(LintViolation {
                    file: file.to_string(),
                    line: line_num,
                    rule: "cross_depth:whole_reply".to_string(),
                    message: format!(
                        "cross-depth comparison of whole replies is forbidden: {trimmed}"
                    ),
                });
            }

            // Trailer comparison
            if (line.contains("assert_eq!") || line.contains("=="))
                && line.contains(".trailer")
                && !line.contains("rule")
            {
                violations.push(LintViolation {
                    file: file.to_string(),
                    line: line_num,
                    rule: "cross_depth:trailer".to_string(),
                    message: format!("cross-depth comparison of trailers is forbidden: {trimmed}"),
                });
            }

            // lanes_exhausted comparison
            if (line.contains("assert_eq!") || line.contains("=="))
                && line.contains(".lanes_exhausted")
                && !line.contains("rule")
            {
                violations.push(LintViolation {
                    file: file.to_string(),
                    line: line_num,
                    rule: "cross_depth:lanes_exhausted".to_string(),
                    message: format!(
                        "cross-depth comparison of lanes_exhausted is forbidden: {trimmed}"
                    ),
                });
            }

            // lane_positions comparison
            if (line.contains("assert_eq!") || line.contains("=="))
                && line.contains(".lane_positions")
                && !line.contains("rule")
            {
                violations.push(LintViolation {
                    file: file.to_string(),
                    line: line_num,
                    rule: "cross_depth:lane_positions".to_string(),
                    message: format!(
                        "cross-depth comparison of lane_positions is forbidden: {trimmed}"
                    ),
                });
            }
        }
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

fn get_workspace_root() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    if manifest_dir.join("../../benchmarks").exists() {
        manifest_dir.join("../..").canonicalize().unwrap()
    } else {
        manifest_dir.canonicalize().unwrap()
    }
}

pub fn collect_fence_rust_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();

    // 1. Every *.rs under crates/aft/src/commands/semantic_search/ (direct and nested)
    let sem_search_dir = root.join("crates/aft/src/commands/semantic_search");
    if sem_search_dir.is_dir() {
        for entry in fs::read_dir(&sem_search_dir).expect("read_dir semantic_search") {
            let entry = entry.expect("read_dir entry");
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }

    // 2. Every crates/aft/tests/engine_*_test.rs
    let tests_dir = root.join("crates/aft/tests");
    if tests_dir.is_dir() {
        for entry in fs::read_dir(&tests_dir).expect("read_dir tests") {
            let entry = entry.expect("read_dir entry");
            let path = entry.path();
            if path.is_file() {
                let name = path.file_name().unwrap().to_string_lossy();
                if name.starts_with("engine_") && name.ends_with("_test.rs") {
                    files.push(path);
                }
            }
        }
    }

    files.sort();

    // Assert scanned set is non-empty
    assert!(!files.is_empty(), "scanned file set must not be empty");

    // Assert scanned set contains known files that must exist
    let file_names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();

    for known in [
        "mod.rs",
        "comparator.rs",
        "evidence_descriptor.rs",
        "generation_token.rs",
        "plan_table.rs",
        "engine_comparator_test.rs",
        "engine_plan_table_test.rs",
    ] {
        assert!(
            file_names.contains(&known.to_string()),
            "scanned files must contain {known}, found: {file_names:?}"
        );
    }

    files
}

pub fn collect_engine_test_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let tests_dir = root.join("crates/aft/tests");
    if tests_dir.is_dir() {
        for entry in fs::read_dir(&tests_dir).expect("read_dir tests") {
            let entry = entry.expect("read_dir entry");
            let path = entry.path();
            if path.is_file() {
                let name = path.file_name().unwrap().to_string_lossy();
                if name.starts_with("engine_") && name.ends_with("_test.rs") {
                    files.push(path);
                }
            }
        }
    }

    files.sort();
    assert!(!files.is_empty(), "engine test file set must not be empty");

    let file_names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();

    for known in ["engine_comparator_test.rs", "engine_plan_table_test.rs"] {
        assert!(
            file_names.contains(&known.to_string()),
            "engine test files must contain {known}, found: {file_names:?}"
        );
    }

    files
}

#[test]
fn test_token_opacity_lint_passes_on_slice_fence() {
    let root = get_workspace_root();
    let files = collect_fence_rust_files(&root);

    for file_path in files {
        // Read directly with no existence skip
        let content = fs::read_to_string(&file_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", file_path.display()));
        let file_str = file_path.display().to_string();
        if let Err(violations) = lint_token_opacity(&file_str, &content) {
            panic!(
                "Token opacity violations found in {}:\n{:#?}",
                file_str, violations
            );
        }
    }
}

#[test]
fn test_token_opacity_lint_mutation_red_forbidden_ops() {
    // 1. Split
    let split_op = "split";
    let split_snippet =
        format!("let parts = snapshot_generation.{split_op}(':').collect::<Vec<_>>();");
    let res = lint_token_opacity("test_split.rs", &split_snippet);
    assert!(res.is_err(), "split on snapshot_generation must fail lint");
    assert_eq!(res.unwrap_err()[0].rule, "token_opacity:split");

    // 2. Substring / range
    let slice_snippet = format!("let sub = &snapshot_generation[{}..8];", "");
    let res = lint_token_opacity("test_slice.rs", &slice_snippet);
    assert!(
        res.is_err(),
        "range slice on snapshot_generation must fail lint"
    );
    assert_eq!(res.unwrap_err()[0].rule, "token_opacity:substring_range");

    // 3. Parse
    let parse_op = "parse::<u64>";
    let parse_snippet = format!("let gen_num: u64 = snapshot_generation.{parse_op}().unwrap();");
    let res = lint_token_opacity("test_parse.rs", &parse_snippet);
    assert!(res.is_err(), "parse on snapshot_generation must fail lint");
    assert_eq!(res.unwrap_err()[0].rule, "token_opacity:parse");

    // 4. Order comparison
    let lt_op = "<";
    let order_snippet = format!("if snapshot_generation {lt_op} previous_generation {{ }}");
    let res = lint_token_opacity("test_order.rs", &order_snippet);
    assert!(
        res.is_err(),
        "order comparison on snapshot_generation must fail lint"
    );
    assert_eq!(res.unwrap_err()[0].rule, "token_opacity:order");

    // 5. Equality is permitted
    let eq_snippet = r#"
        if snapshot_generation == expected_generation {
            println!("matches");
        }
    "#;
    assert!(
        lint_token_opacity("test_eq.rs", eq_snippet).is_ok(),
        "equality on snapshot_generation must be permitted"
    );
}

#[test]
fn test_cross_depth_harness_lint_passes_on_slice_fence() {
    let root = get_workspace_root();
    let test_files = collect_engine_test_files(&root);

    for file_path in test_files {
        // Read directly with no existence skip
        let content = fs::read_to_string(&file_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", file_path.display()));
        let file_str = file_path.display().to_string();
        if let Err(violations) = lint_cross_depth_harness(&file_str, &content) {
            panic!(
                "Cross-depth harness lint violations in {}:\n{:#?}",
                file_str, violations
            );
        }
    }
}

#[test]
fn test_cross_depth_harness_lint_mutation_red_forbidden_assertions() {
    // 1. Comparing whole replies across depths
    let reply_word = "reply";
    let whole_reply_snippet = format!(
        "fn test_cross_depth_stability() {{ let shallow_{reply_word} = 1; let deep_{reply_word} = 2; assert_eq!(shallow_{reply_word}, deep_{reply_word}); }}"
    );
    let res = lint_cross_depth_harness("test_replies.rs", &whole_reply_snippet);
    assert!(
        res.is_err(),
        "comparing whole replies across depths must fail lint"
    );
    assert_eq!(res.unwrap_err()[0].rule, "cross_depth:whole_reply");

    // 2. Comparing trailers across depths
    let trailer_word = "trailer";
    let trailer_snippet = format!(
        "fn test_cross_depth_trailer() {{ assert_eq!(depth_200.{trailer_word}, depth_400.{trailer_word}); }}"
    );
    let res = lint_cross_depth_harness("test_trailer.rs", &trailer_snippet);
    assert!(
        res.is_err(),
        "comparing trailers across depths must fail lint"
    );
    assert_eq!(res.unwrap_err()[0].rule, "cross_depth:trailer");

    // 3. Comparing lanes_exhausted across depths
    let exh_word = "lanes_exhausted";
    let exhausted_snippet = format!(
        "fn test_cross_depth_exhaustion() {{ assert_eq!(depth_200.{exh_word}, depth_400.{exh_word}); }}"
    );
    let res = lint_cross_depth_harness("test_exhausted.rs", &exhausted_snippet);
    assert!(
        res.is_err(),
        "comparing lanes_exhausted across depths must fail lint"
    );
    assert_eq!(res.unwrap_err()[0].rule, "cross_depth:lanes_exhausted");

    // 4. Comparing lane_positions across depths
    let pos_word = "lane_positions";
    let positions_snippet = format!(
        "fn test_cross_depth_positions() {{ assert_eq!(depth_200.{pos_word}, depth_400.{pos_word}); }}"
    );
    let res = lint_cross_depth_harness("test_positions.rs", &positions_snippet);
    assert!(
        res.is_err(),
        "comparing lane_positions across depths must fail lint"
    );
    assert_eq!(res.unwrap_err()[0].rule, "cross_depth:lane_positions");

    // 5. Allowed: comparing ranked tuple, evidence descriptor, and confidence
    let allowed_snippet = r#"
        fn test_cross_depth_stability_units() {
            assert_eq!(depth_200.ranked_tuple, depth_400.ranked_tuple);
            assert_eq!(depth_200.evidence_descriptor, depth_400.evidence_descriptor);
            assert_eq!(depth_200.confidence, depth_400.confidence);
        }
    "#;
    assert!(
        lint_cross_depth_harness("test_allowed.rs", allowed_snippet).is_ok(),
        "comparing stability units and confidence must be permitted"
    );
}
