use std::fs;

use aft::search_index::{extract_trigrams, lexical_score, SearchIndex};

fn indexed_file(content: &str) -> (tempfile::TempDir, SearchIndex, u32) {
    let project = tempfile::tempdir().expect("create project");
    let path = project.path().join("src/lib.rs");
    fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
    fs::write(&path, content).expect("write file");

    let mut index = SearchIndex::new();
    index.index_file(&path, content.as_bytes());
    let file_id = *index.path_to_id.get(&path).expect("file id");
    (project, index, file_id)
}

#[test]
fn empty_trigrams_score_zero() {
    let (_project, index, file_id) = indexed_file("abcdef");
    assert_eq!(lexical_score(&index, &[], file_id), 0.0);
}

#[test]
fn all_zero_posting_trigrams_score_zero() {
    let (_project, index, file_id) = indexed_file("abcdef");
    let query = SearchIndex::query_trigrams_from_tokens(&["zzzzzz"]);
    assert_eq!(lexical_score(&index, &query, file_id), 0.0);
}

#[test]
fn single_match_uses_length_normalization() {
    let (_project, index, file_id) = indexed_file("abcdef");
    let query = SearchIndex::query_trigrams_from_tokens(&["abc"]);
    let expected = 1.0 / (1.0 + 4.0_f32.ln());
    assert!((lexical_score(&index, &query, file_id) - expected).abs() < 0.0001);
}

#[test]
fn unknown_file_scores_zero() {
    let (_project, index, _file_id) = indexed_file("abcdef");
    let query = SearchIndex::query_trigrams_from_tokens(&["abc"]);
    assert_eq!(lexical_score(&index, &query, 999), 0.0);
}

#[test]
fn query_trigram_helper_dedupes_repeated_offsets() {
    let (_project, index, file_id) = indexed_file("abcabc");
    let raw = extract_trigrams("abcabc".as_bytes())
        .into_iter()
        .map(|(trigram, _, _)| trigram)
        .collect::<Vec<_>>();
    let deduped = SearchIndex::query_trigrams_from_tokens(&["abcabc"]);

    assert!(lexical_score(&index, &raw, file_id) > lexical_score(&index, &deduped, file_id));
    let expected = 3.0 / (1.0 + 3.0_f32.ln());
    assert!((lexical_score(&index, &deduped, file_id) - expected).abs() < 0.0001);
}

#[test]
fn empty_file_trigram_entry_uses_defensive_guard() {
    let (_project, mut index, file_id) = indexed_file("abcdef");
    index.file_trigrams.insert(file_id, Vec::new());
    let query = SearchIndex::query_trigrams_from_tokens(&["abc"]);

    assert_eq!(lexical_score(&index, &query, file_id), 1.0);
}
