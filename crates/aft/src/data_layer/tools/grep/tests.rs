//! The data layer's grep and glob against today's index, on small
//! checkouts built in temporary folders.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use aft_store::ids::{Digest128, FamilyId};

use super::ViewScope;
use crate::data_layer::checkout::{Checkout, CheckoutView, DataLayer};
use crate::pattern_compile::{compile, CompileOpts, CompileResult, CompiledPattern};
use crate::search_index::{
    decompose_grep_pattern, sort_paths_by_cached_mtime_desc, GrepResult, GrepScanOrder,
    PathFilters, SearchIndex, SearchIndexSnapshot,
};

/// A checkout under construction: its root, its store, and the next
/// modification time to give a file, so files order by when they were
/// written whatever the filesystem's time resolution.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    store: PathBuf,
    clock: SystemTime,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap().join("checkout");
        let store = dir.path().join("store");
        std::fs::create_dir_all(&root).unwrap();
        Self {
            _dir: dir,
            root,
            store,
            clock: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        }
    }

    fn write(&mut self, path: &str, bytes: &[u8]) -> PathBuf {
        let absolute = self.root.join(path);
        std::fs::create_dir_all(absolute.parent().unwrap()).unwrap();
        std::fs::write(&absolute, bytes).unwrap();
        self.clock += Duration::from_secs(10);
        filetime::set_file_mtime(&absolute, filetime::FileTime::from_system_time(self.clock))
            .unwrap();
        absolute
    }

    fn bind(&self) -> Checkout {
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let family = FamilyId::from_digest(Digest128::of(b"grep-tool-tests"));
        let checkout = Checkout::bind(layer, &self.store, family, &self.root).unwrap();
        checkout.build().unwrap();
        checkout
    }
}

fn compiled(pattern: &str, case_insensitive: bool) -> CompiledPattern {
    match compile(
        pattern,
        CompileOpts {
            case_insensitive,
            ..CompileOpts::default()
        },
    ) {
        CompileResult::Ok(compiled) => compiled,
        other => panic!("{pattern:?} does not compile: {other:?}"),
    }
}

/// The same grep from today's index and from the view, as the grep tool
/// prints them, with the view's candidate count.
fn grep_both(
    today: &SearchIndexSnapshot,
    view: &CheckoutView,
    root: &Path,
    scope: &Path,
    pattern: &str,
    case_insensitive: bool,
) -> (String, String, usize) {
    let pattern = compiled(pattern, case_insensitive);
    let query = decompose_grep_pattern(&pattern);
    let filters = PathFilters::default();
    let search = |snapshot: &SearchIndexSnapshot| -> GrepResult {
        snapshot
            .search_grep_profiled_with_filters_and_query_in_order(
                &pattern,
                &query,
                &filters,
                scope,
                200,
                None,
                GrepScanOrder::Path,
            )
            .unwrap()
            .0
    };
    let render = |result: &GrepResult| {
        format!(
            "{}\nmissing on disk: {}",
            crate::commands::grep::format_grep_text(result, root),
            result.missing_on_disk
        )
    };
    let candidates = ViewScope::of(view.clone(), scope)
        .expect("the scope is inside the checkout")
        .candidates(&pattern)
        .unwrap();
    (
        render(&search(today)),
        render(&search(&candidates)),
        candidates.file_entries().len(),
    )
}

/// A checkout path as the grep tool prints it: with the platform's
/// separators, as today's index prints it.
fn printed(path: &str) -> String {
    path.replace('/', std::path::MAIN_SEPARATOR_STR)
}

/// The same glob from today's index and from the view: the files in the
/// order the glob tool prints them, newest first.
fn glob_both(
    today: &SearchIndexSnapshot,
    view: &CheckoutView,
    root: &Path,
    scope: &Path,
    pattern: &str,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let listed = |snapshot: &SearchIndexSnapshot| {
        let (mut files, _, _) = snapshot.glob_profiled(pattern, scope, false);
        let mtimes = files
            .iter()
            .filter_map(|path| {
                snapshot
                    .file_modified(path)
                    .map(|time| (path.clone(), time))
            })
            .collect();
        sort_paths_by_cached_mtime_desc(&mut files, root, &mtimes);
        files
    };
    let listing = ViewScope::of(view.clone(), scope)
        .expect("the scope is inside the checkout")
        .listing();
    (listed(today), listed(&listing))
}

const PATTERNS: &[(&str, bool)] = &[
    ("alpha", false),
    ("ALPHA", true),
    ("(?i)alpha", false),
    ("beta|alpha", false),
    (r"\bfn\s+\w+", false),
    ("TODO", false),
    (".", false),
    ("nothing matches this", false),
];

const GLOBS: &[&str] = &["**/*", "**/*.rs", "*.md", "src/**", "*", "**/c.ts"];

fn a_small_checkout(fixture: &mut Fixture) {
    fixture.write("src/a.rs", b"fn alpha() { beta(); }\n// TODO: alpha\n");
    fixture.write("src/b.rs", b"fn beta() {}\n");
    fixture.write("src/nested/c.ts", b"export const alpha = 1;\n");
    fixture.write("docs/readme.md", b"Alpha and beta\n");
    fixture.write("blob.dat", b"\0\x01alpha\0");
    let mut big = "filler line without the word\n".repeat(40_000).into_bytes();
    big.extend_from_slice(b"alpha at the end\n");
    fixture.write("big.txt", &big);
    fixture.write(".gitignore", b"ignored.txt\n");
    fixture.write("ignored.txt", b"alpha\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink("src/a.rs", fixture.root.join("link.rs")).unwrap();
}

fn compare(fixture: &Fixture, today: &SearchIndexSnapshot, view: &CheckoutView, scopes: &[&str]) {
    let root = &fixture.root;
    for scope in scopes {
        let scope = root.join(scope);
        for (pattern, case_insensitive) in PATTERNS {
            let (today, data, _) = grep_both(today, view, root, &scope, pattern, *case_insensitive);
            assert_eq!(
                today,
                data,
                "grep {pattern:?} (case-insensitive: {case_insensitive}) in {}",
                scope.display()
            );
        }
        for pattern in GLOBS {
            let (today, data) = glob_both(today, view, root, &scope, pattern);
            assert_eq!(today, data, "glob {pattern:?} in {}", scope.display());
        }
    }
}

#[test]
fn a_view_answers_grep_and_glob_as_todays_index_does() {
    let mut fixture = Fixture::new();
    a_small_checkout(&mut fixture);
    let today = SearchIndex::build(&fixture.root).snapshot();
    let checkout = fixture.bind();
    let view = checkout.view().unwrap();
    compare(
        &fixture,
        &today,
        &view,
        &["", "src", "src/nested", "docs", "src/a.rs"],
    );

    // The view narrows the files a selective pattern reads, and still
    // reads every file whose content it holds no trigrams for.
    let (_, data, candidates) =
        grep_both(&today, &view, &fixture.root, &fixture.root, "TODO", false);
    assert!(
        data.contains(&format!("{}\n2: // TODO: alpha", printed("src/a.rs"))),
        "{data}"
    );
    assert_eq!(candidates, 3, "src/a.rs, and the binary and oversize files");

    // Today's index never lists a symlink, and neither does the view.
    let (today_files, data_files) = glob_both(&today, &view, &fixture.root, &fixture.root, "**/*");
    assert!(!data_files.contains(&fixture.root.join("link.rs")));
    assert!(data_files.contains(&fixture.root.join("big.txt")));
    assert_eq!(today_files, data_files);
}

#[test]
fn build_folders_are_left_out_as_today_and_a_file_named_build_is_kept() {
    let mut fixture = Fixture::new();
    fixture.write("src/a.rs", b"fn alpha() {}\n");
    for folder in [
        "build",
        "dist",
        "node_modules/pkg",
        "target/debug",
        "venv",
        ".venv/lib",
        "__pycache__",
        ".tox",
        "src/build",
    ] {
        fixture.write(&format!("{folder}/out.rs"), b"fn alpha() {}\n");
    }
    fixture.write("tools/build", b"alpha, a file named build\n");
    let today = SearchIndex::build(&fixture.root).snapshot();
    let checkout = fixture.bind();
    let view = checkout.view().unwrap();
    compare(&fixture, &today, &view, &["", "src", "tools"]);

    let (_, data) = glob_both(&today, &view, &fixture.root, &fixture.root, "**/*");
    let listed = data
        .iter()
        .map(|path| path.strip_prefix(&fixture.root).unwrap().to_path_buf())
        .collect::<Vec<_>>();
    assert_eq!(
        listed,
        vec![PathBuf::from("tools/build"), PathBuf::from("src/a.rs")]
    );
    let (_, data, _) = grep_both(&today, &view, &fixture.root, &fixture.root, "alpha", false);
    assert!(
        data.starts_with(&format!(
            "{}\n1: alpha, a file named build\n\n{}\n1: fn alpha() {{}}\n\nFound 2 match across 2 file",
            printed("tools/build"),
            printed("src/a.rs")
        )),
        "{data}"
    );
}

#[test]
fn a_root_outside_the_checkout_is_not_a_scope_of_its_view() {
    let mut fixture = Fixture::new();
    a_small_checkout(&mut fixture);
    let checkout = fixture.bind();
    let view = checkout.view().unwrap();
    let outside = fixture.root.parent().unwrap().join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    assert!(ViewScope::of(view.clone(), &outside).is_none());
    let scope = ViewScope::of(view, &fixture.root.join("src")).unwrap();
    assert!(!scope.is_whole_checkout());
    assert!(scope.has_files());
}
