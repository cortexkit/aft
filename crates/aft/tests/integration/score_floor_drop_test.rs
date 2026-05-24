use std::fs;

use aft::semantic_index::SemanticIndex;

#[test]
fn zero_score_tail_can_surface_without_evicting_positive_hits() {
    let project = tempfile::tempdir().expect("create project dir");
    let source_file = project.path().join("src/lib.rs");
    fs::create_dir_all(source_file.parent().expect("source parent")).expect("create src dir");
    fs::write(
        &source_file,
        "pub fn positive_one() {\n    let _ = 1;\n}\npub fn positive_two() {\n    let _ = 2;\n}\npub fn zero_one() {\n    let _ = 0;\n}\npub fn zero_two() {\n    let _ = 0;\n}\n",
    )
    .expect("write source");

    let files = vec![source_file];
    let mut embed = |texts: Vec<String>| {
        Ok::<Vec<Vec<f32>>, String>(
            texts
                .into_iter()
                .map(|text| {
                    if text.contains("positive_one") || text.contains("positive_two") {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.0, 1.0]
                    }
                })
                .collect(),
        )
    };
    let index = SemanticIndex::build(project.path(), &files, &mut embed, 16).expect("build index");

    let results = index.search(&[1.0, 0.0], 3);

    assert_eq!(
        results.len(),
        3,
        "zero-score tail result should be retained"
    );
    let positive_names: Vec<&str> = results
        .iter()
        .filter(|result| result.score > 0.0)
        .map(|result| result.name.as_str())
        .collect();
    assert_eq!(positive_names, vec!["positive_one", "positive_two"]);
    assert!(
        results[2].score.abs() <= f32::EPSILON,
        "zero-score noise can appear in the tail: {results:?}"
    );
    assert_eq!(results[2].source, "semantic");
}
