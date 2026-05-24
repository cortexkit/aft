use std::fs;
use std::path::Path;

use aft::search_index::SearchIndex;
use aft::semantic_index::is_semantic_indexed_extension;

fn write_file(root: &Path, relative: &str, content: &str) -> std::path::PathBuf {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
    fs::write(&path, content).expect("write file");
    path
}

#[test]
fn zero_posting_trigrams_do_not_collapse_candidate_union() {
    let project = tempfile::tempdir().expect("create project");
    let path = write_file(project.path(), "src/lib.rs", "alpha_token");
    let mut index = SearchIndex::new();
    index.index_file(&path, b"alpha_token");

    let query = SearchIndex::query_trigrams_from_tokens(&["zzzzzz", "alpha_token"]);
    let ranked = index.lexical_rank(&query, None, 10);

    assert_eq!(ranked.len(), 1);
    assert_eq!(ranked[0].0, path);
}

#[test]
fn three_or_more_trigrams_use_rarest_three_and_cap_at_200() {
    let project = tempfile::tempdir().expect("create project");
    let mut index = SearchIndex::new();
    for i in 0..250 {
        let path = write_file(project.path(), &format!("src/file{i}.rs"), "abcdefghij");
        index.index_file(&path, b"abcdefghij");
    }

    let query = SearchIndex::query_trigrams_from_tokens(&["abcdefghij"]);
    let ranked = index.lexical_rank(&query, None, 1_000);

    assert_eq!(ranked.len(), 200);
}

#[test]
fn one_or_two_trigrams_use_all_remaining_and_cap_at_500() {
    let project = tempfile::tempdir().expect("create project");
    let mut index = SearchIndex::new();
    for i in 0..550 {
        let path = write_file(project.path(), &format!("src/file{i}.rs"), "abc");
        index.index_file(&path, b"abc");
    }

    let query = SearchIndex::query_trigrams_from_tokens(&["abc"]);
    let ranked = index.lexical_rank(&query, None, 1_000);

    assert_eq!(ranked.len(), 500);
}

#[test]
fn candidate_filter_excludes_non_semantic_file_extensions() {
    let project = tempfile::tempdir().expect("create project");
    let rust_path = write_file(project.path(), "src/lib.rs", "uniqueIdentifier");
    let readme_path = write_file(project.path(), "README.md", "uniqueIdentifier");
    let mut index = SearchIndex::new();
    index.index_file(&rust_path, b"uniqueIdentifier");
    index.index_file(&readme_path, b"uniqueIdentifier");

    let query = SearchIndex::query_trigrams_from_tokens(&["uniqueIdentifier"]);
    let ranked = index.lexical_rank(&query, Some(&is_semantic_indexed_extension), 10);

    assert!(ranked.iter().any(|(path, _)| path == &rust_path));
    assert!(!ranked.iter().any(|(path, _)| path == &readme_path));
}
