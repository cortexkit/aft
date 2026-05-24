use std::fs;

use aft::semantic_index::SemanticIndex;

#[test]
fn exported_symbol_boost_ranks_export_above_equivalent_private_symbol() {
    let project = tempfile::tempdir().expect("create project dir");
    let private_file = project.path().join("src/private.rs");
    let public_file = project.path().join("src/public.rs");
    fs::create_dir_all(private_file.parent().expect("source parent")).expect("create src dir");
    fs::write(&private_file, "fn target() -> bool {\n    true\n}\n").expect("write private source");
    fs::write(&public_file, "pub fn target() -> bool {\n    true\n}\n")
        .expect("write public source");

    let files = vec![private_file, public_file];
    let mut embed =
        |texts: Vec<String>| Ok::<Vec<Vec<f32>>, String>(vec![vec![1.0, 0.0]; texts.len()]);
    let index = SemanticIndex::build(project.path(), &files, &mut embed, 16).expect("build index");

    let results = index.search(&[1.0, 0.0], 2);

    assert_eq!(results.len(), 2);
    assert!(
        results[0].exported,
        "exported symbol should sort first: {results:?}"
    );
    assert_eq!(results[0].source, "semantic");
    assert!(
        results[0].score > results[1].score,
        "boost must apply before sort/take and produce a higher score: {results:?}"
    );
}
