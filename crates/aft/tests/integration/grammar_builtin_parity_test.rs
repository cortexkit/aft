use std::path::Path;

use aft::callgraph_store::join::CallgraphBlob;
use aft::parser::FileParser;

#[test]
fn grammar_builtin_parity_against_slice_base() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grammars");
    let inputs: Vec<serde_json::Value> = serde_json::from_slice(
        &std::fs::read(fixtures.join("builtin-parity-inputs.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(inputs.len(), 33);
    let root = tempfile::tempdir().unwrap();
    let mut answers = serde_json::Map::new();
    for input in inputs {
        let source = input["source"].as_str().unwrap();
        let language = input["language"].as_str().unwrap();
        let path = root.path().join(input["file"].as_str().unwrap());
        std::fs::write(&path, source).unwrap();
        let outline = FileParser::new().extract_symbols(&path).unwrap();
        let graph = CallgraphBlob::extract(source, language, "builtin-parity").unwrap();
        answers.insert(language.into(), serde_json::json!({
            "outline": outline,
            "callgraph": serde_json::from_slice::<serde_json::Value>(&graph.to_bytes().unwrap()).unwrap(),
        }));
    }
    let bytes = serde_json::to_vec_pretty(&answers).unwrap();
    let oracle = fixtures.join("builtin-parity.json");
    assert_eq!(
        bytes,
        std::fs::read(&oracle).unwrap(),
        "built-in outline or callgraph differs from the slice base"
    );
}
