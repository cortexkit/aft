use super::*;

fn production_json() -> serde_json::Value {
    serde_json::from_slice(include_bytes!("../manifest.json")).unwrap()
}

fn load(json: &serde_json::Value, production: bool) -> Result<ManifestList, String> {
    ManifestList::load(&serde_json::to_vec(json).unwrap(), production)
}

#[test]
fn grammar_production_manifest_is_valid() {
    assert!(!PRODUCTION_MANIFEST.grammars.is_empty());
}

#[test]
fn grammar_manifest_rejects_duplicate_languages() {
    let mut json = production_json();
    let mut second = json["grammars"][0].clone();
    second["extensions"] = serde_json::json!(["test-only"]);
    json["grammars"].as_array_mut().unwrap().push(second);
    assert_eq!(
        load(&json, false).err().unwrap(),
        "duplicate grammar language: luau"
    );
}

#[test]
fn grammar_manifest_rejects_shared_extensions() {
    let mut json = production_json();
    let mut second = json["grammars"][0].clone();
    second["language"] = "test-only".into();
    json["grammars"].as_array_mut().unwrap().push(second);
    assert_eq!(
        load(&json, false).err().unwrap(),
        "duplicate grammar extension: luau"
    );
}

#[test]
fn grammar_manifest_rejects_natively_linked_extensions() {
    let mut json = production_json();
    json["grammars"][0]["extensions"] = serde_json::json!(["h"]);
    assert_eq!(
        load(&json, false).err().unwrap(),
        "grammar extension is already linked: h"
    );
}

#[test]
fn grammar_manifest_requires_extractors_only_in_production() {
    let mut json = production_json();
    json["grammars"][0]["language"] = "test-only".into();
    assert_eq!(
        load(&json, true).err().unwrap(),
        "no extractor for grammar: test-only"
    );
    assert_eq!(
        load(&json, false).unwrap().grammars[0].language,
        "test-only"
    );
}
