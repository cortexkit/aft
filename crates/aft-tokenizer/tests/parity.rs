use aft_tokenizer::count_tokens;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct CorpusEntry {
    input: String,
    expected: usize,
}

#[test]
fn snapshot_corpus_matches_ai_tokenizer_claude_counts() {
    let corpus: Vec<CorpusEntry> = serde_json::from_str(include_str!("snapshot_corpus.json"))
        .expect("snapshot corpus should be valid JSON");
    assert!(corpus.len() >= 30, "snapshot corpus should be substantial");

    for (idx, entry) in corpus.iter().enumerate() {
        assert_eq!(
            count_tokens(&entry.input),
            entry.expected,
            "corpus entry #{idx} failed for input {:?}",
            entry.input
        );
    }
}
