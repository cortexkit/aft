use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use aft::config::Config;
use aft::inspect::scanners::duplicates::run_duplicates_scan;
use aft::inspect::{InspectCategory, InspectJob, InspectScanSuccess, JobKey};
use aft::parser::SymbolCache;
use serde_json::Value;

fn write_file(root: &Path, relative: &str, contents: &str) -> PathBuf {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("file has parent")).expect("create parent");
    fs::write(&path, contents).expect("write fixture");
    path
}

fn fixture_root() -> (tempfile::TempDir, PathBuf) {
    let temp_dir = crate::test_helpers::tempdir();
    let root = temp_dir.path().join("project");
    fs::create_dir_all(&root).expect("create project");
    (temp_dir, root)
}

fn duplicates_job(root: &Path) -> InspectJob {
    let mut scope_files = aft::callgraph::walk_project_files(root).collect::<Vec<_>>();
    scope_files.sort();
    let config = Config {
        project_root: Some(root.to_path_buf()),
        ..Config::default()
    };

    InspectJob {
        job_id: 1,
        key: JobKey::for_project_category(InspectCategory::Duplicates),
        category: InspectCategory::Duplicates,
        scope_files,
        project_root: root.to_path_buf(),
        inspect_dir: root.join(".aft-cache").join("inspect"),
        config: Arc::new(config),
        symbol_cache: Arc::new(RwLock::new(SymbolCache::new())),
        inspect_writer: true,
        callgraph_writer: true,
        callgraph_snapshot: None,
    }
}

fn run_scan(root: &Path) -> InspectScanSuccess {
    let job = duplicates_job(root);
    run_duplicates_scan(&job)
        .outcome
        .expect("duplicates scan should succeed")
}

fn expected_duplicate_flag_for(success: &InspectScanSuccess, relative: &str) -> bool {
    success
        .contributions
        .iter()
        .find(|contribution| contribution.file_path.ends_with(relative))
        .unwrap_or_else(|| panic!("missing contribution for {relative}"))
        .contribution["expected_duplicate"]
        .as_bool()
        .expect("expected_duplicate is a bool")
}

fn fragments_for<'a>(success: &'a InspectScanSuccess, relative: &str) -> &'a Vec<Value> {
    success
        .contributions
        .iter()
        .find(|contribution| contribution.file_path.ends_with(relative))
        .unwrap_or_else(|| panic!("missing contribution for {relative}"))
        .contribution["fragments"]
        .as_array()
        .expect("fragments are an array")
}

fn has_group_with_files(aggregate: &Value, left: &str, right: &str) -> bool {
    let left_prefix = format!("{left}:");
    let right_prefix = format!("{right}:");
    aggregate["items"]
        .as_array()
        .expect("items are an array")
        .iter()
        .any(|item| {
            let files = item["files"].as_array().expect("group files are an array");
            files
                .iter()
                .filter_map(|file| file.as_str())
                .any(|file| file.starts_with(&left_prefix))
                && files
                    .iter()
                    .filter_map(|file| file.as_str())
                    .any(|file| file.starts_with(&right_prefix))
        })
}

fn assert_no_groups(aggregate: &Value) {
    assert_eq!(aggregate["total_groups"], 0, "aggregate: {aggregate:?}");
    assert!(
        aggregate["items"]
            .as_array()
            .expect("items are an array")
            .is_empty(),
        "aggregate: {aggregate:?}"
    );
}

fn duplicate_fixture_source() -> &'static str {
    r#"
export function calculate(input: number) {
  const first = input + 1;
  const second = first + 2;
  const third = second + first;
  const fourth = third + 3;
  const fifth = fourth + third;
  const sixth = fifth + second;
  const seventh = sixth + fifth;
  return seventh + third;
}
"#
}

#[test]
fn inspect_duplicates_empty_project_reports_no_groups() {
    let (_temp_dir, root) = fixture_root();

    let success = run_scan(&root);

    assert_eq!(success.aggregate["total_groups"], 0);
    assert_eq!(success.aggregate["scanned_files"], 0);
    assert!(success.contributions.is_empty());
}

#[test]
fn inspect_duplicates_identical_anonymized_ast_reports_group() {
    let (_temp_dir, root) = fixture_root();
    let source = duplicate_fixture_source();
    write_file(&root, "src/foo.ts", source);
    write_file(&root, "src/bar.ts", source);

    let success = run_scan(&root);

    assert!(success.aggregate["total_groups"].as_u64().unwrap() > 0);
    assert!(
        has_group_with_files(&success.aggregate, "src/foo.ts", "src/bar.ts"),
        "aggregate: {:?}",
        success.aggregate
    );
}

#[test]
fn inspect_duplicates_variable_names_are_anonymized() {
    let (_temp_dir, root) = fixture_root();
    write_file(
        &root,
        "src/apple.ts",
        r#"
export function alpha(input: number) {
  const apple = input + 1;
  const banana = apple + 2;
  const cherry = banana + apple;
  const date = cherry + 3;
  const elderberry = date + cherry;
  const fig = elderberry + banana;
  const grape = fig + elderberry;
  return grape + cherry;
}
"#,
    );
    write_file(
        &root,
        "src/orange.ts",
        r#"
export function beta(source: number) {
  const one = source + 1;
  const two = one + 2;
  const three = two + one;
  const four = three + 3;
  const five = four + three;
  const six = five + two;
  const seven = six + five;
  return seven + three;
}
"#,
    );

    let success = run_scan(&root);

    assert!(
        has_group_with_files(&success.aggregate, "src/apple.ts", "src/orange.ts"),
        "aggregate: {:?}",
        success.aggregate
    );
}

#[test]
fn inspect_duplicates_method_names_are_distinguished() {
    let (_temp_dir, root) = fixture_root();
    write_file(
        &root,
        "src/first.ts",
        r#"
export function run(target: Service) {
  target.first(target);
  target.first(target);
  target.first(target);
  target.first(target);
  target.first(target);
  target.first(target);
  target.first(target);
  return target.first(target);
}
"#,
    );
    write_file(
        &root,
        "src/second.ts",
        r#"
export function run(target: Service) {
  target.second(target);
  target.second(target);
  target.second(target);
  target.second(target);
  target.second(target);
  target.second(target);
  target.second(target);
  return target.second(target);
}
"#,
    );

    let success = run_scan(&root);

    assert_no_groups(&success.aggregate);
}

#[test]
fn inspect_duplicates_literals_are_distinguished() {
    let (_temp_dir, root) = fixture_root();
    write_file(
        &root,
        "src/low.ts",
        r#"
export function values() {
  const a = 11;
  const b = a + 12;
  const c = b + 13;
  const d = c + 14;
  const e = d + 15;
  const f = e + 16;
  const g = f + 17;
  return g + 18;
}
"#,
    );
    write_file(
        &root,
        "src/high.ts",
        r#"
export function values() {
  const a = 21;
  const b = a + 22;
  const c = b + 23;
  const d = c + 24;
  const e = d + 25;
  const f = e + 26;
  const g = f + 27;
  return g + 28;
}
"#,
    );

    let success = run_scan(&root);

    assert_no_groups(&success.aggregate);
}

#[test]
fn inspect_duplicates_fragment_below_lower_bound_is_not_indexed() {
    let (_temp_dir, root) = fixture_root();
    write_file(&root, "src/tiny.ts", "const tiny = 1;\n");

    let success = run_scan(&root);

    assert!(fragments_for(&success, "src/tiny.ts").is_empty());
    assert_no_groups(&success.aggregate);
}

#[test]
fn inspect_duplicates_fragment_above_max_cost_is_not_indexed() {
    let (_temp_dir, root) = fixture_root();
    let mut source = String::new();
    for index in 0..2_000 {
        source.push_str(&format!("const value{index} = {index};\n"));
    }
    write_file(&root, "src/huge.ts", &source);

    let success = run_scan(&root);

    assert!(fragments_for(&success, "src/huge.ts")
        .iter()
        .all(|fragment| fragment[3].as_u64().unwrap() <= 7_000));
    assert_no_groups(&success.aggregate);
}

#[test]
fn inspect_duplicates_unsupported_language_contributes_empty_fragments() {
    let (_temp_dir, root) = fixture_root();
    write_file(&root, "scripts/run.bash", "echo hello\n");

    let success = run_scan(&root);

    assert!(fragments_for(&success, "scripts/run.bash").is_empty());
    assert_eq!(success.aggregate["total_groups"], 0);
    assert!(success.aggregate["languages_skipped"]
        .as_array()
        .expect("languages_skipped is an array")
        .iter()
        .any(|language| language == "bash"));
}

#[test]
fn inspect_duplicates_expected_duplicate_marker_suppresses_file_groups() {
    let (_temp_dir, root) = fixture_root();
    let mirror = duplicate_fixture_source();
    let source = format!(
        "// aft:expected-duplicate -- fixture source intentionally duplicates the calculate function from another file.\n{mirror}",
    );
    write_file(&root, "src/marked.ts", &source);
    write_file(&root, "src/plain.ts", mirror);

    let success = run_scan(&root);

    assert!(expected_duplicate_flag_for(&success, "src/marked.ts"));
    assert_eq!(
        success.aggregate["total_groups"], 0,
        "{:?}",
        success.aggregate
    );
    assert_eq!(success.aggregate["marker_suppressed_groups"], 1);
}
