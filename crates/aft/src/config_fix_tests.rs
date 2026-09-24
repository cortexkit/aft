use super::*;
use crate::config_resolve::{resolve_config_for_harness_with_phase, ConfigTier};
use crate::harness::Harness;
use serde_json::json;

fn resolved(doc: &str, tier: &str, harness: Option<&Harness>, phase: PolicyPhase) -> Value {
    let mut tiers = Vec::new();
    if tier == "project" {
        tiers.push(ConfigTier {
            tier: "user".to_string(),
            source: "user".to_string(),
            doc: "{}".to_string(),
        });
    }
    tiers.push(ConfigTier {
        tier: tier.to_string(),
        source: tier.to_string(),
        doc: doc.to_string(),
    });
    let result = resolve_config_for_harness_with_phase(&tiers, harness, phase);
    assert!(result.errors.is_empty(), "{doc}: {:?}", result.errors);
    let config = result.config;
    json!({
        "disabled_tools": config.disabled_tools,
        "indexes": serde_json::to_value(config.indexes).unwrap(),
        "github": serde_json::to_value(&config.github).unwrap(),
        "backup": config.backup.enabled,
        "bash": config.bash.enabled,
    })
}

/// Fixing a file must preserve the values ordinary loading derives from its
/// retired keys while it still accepts them (`PolicyPhase::Window`), and the
/// fixed file must load once those keys are rejected (`PolicyPhase::Rejecting`).
fn assert_fix_preserves_intent(doc: &str, tier: FixTier) -> Migration {
    let tier_name = if tier == FixTier::User {
        "user"
    } else {
        "project"
    };
    let migration = migrate_config_text(doc, tier).unwrap();
    assert!(migration.changed, "{doc} needed migration");
    for harness in [None, Some(Harness::Opencode), Some(Harness::Pi)] {
        let before = resolved(doc, tier_name, harness.as_ref(), PolicyPhase::Window);
        let after = resolved(
            &migration.text,
            tier_name,
            harness.as_ref(),
            PolicyPhase::Rejecting,
        );
        assert_eq!(before, after, "{doc}\n=>\n{}", migration.text);
    }
    let value: Value = serde_json::from_str(&crate::jsonc::strip_jsonc(&migration.text)).unwrap();
    let mut rejected = value.as_object().unwrap().clone();
    let document_tier = if tier == FixTier::User {
        feature_config::DocumentTier::User
    } else {
        feature_config::DocumentTier::Project
    };
    let check =
        feature_config::translate_document(&mut rejected, PolicyPhase::Rejecting, document_tier);
    assert!(check.errors.is_empty(), "{:?}", check.errors);
    migration
}

#[test]
fn legacy_registration_and_index_keys_become_canonical() {
    for doc in [
        r#"{"tool_surface": "recommended"}"#,
        r#"{"tool_surface": "all"}"#,
        r#"{"tool_surface": "minimal", "disabled_tools": []}"#,
        r#"{"hoist_builtin_tools": false}"#,
        r#"{"backup": {"enabled": false}}"#,
        r#"{"bash": false, "inspect": {"enabled": false}}"#,
        r#"{"enabled": false}"#,
        r#"{"disabled_tools": ["aft_glob", "aft_bash", "aft_zoom"]}"#,
        r#"{"search_index": false, "experimental_semantic_search": false, "callgraph_store": true}"#,
        r#"{"semantic_search": true, "experimental_semantic_search": false, "indexes": {"trigram": false}}"#,
        r#"{"github": {"enabled": false, "write": true}}"#,
        r#"{"github": {"enabled": true}}"#,
        r#"{"harnesses": {"pi": {"tool_surface": "minimal", "search_index": false}, "opencode": {"disabled_tools": ["aft_read"]}}}"#,
    ] {
        let migration = assert_fix_preserves_intent(doc, FixTier::User);
        for (retired, _) in RETIRED_PATHS {
            let value: Value = serde_json::from_str(&migration.text).unwrap();
            let pointer = format!("/{}", retired.replace('.', "/"));
            assert!(
                value.pointer(&pointer).is_none(),
                "{retired} left in {}",
                migration.text
            );
        }
    }
}

#[test]
fn an_empty_generated_base_list_is_persisted() {
    let migration = assert_fix_preserves_intent(r#"{"tool_surface": "all"}"#, FixTier::User);
    let value: Value = serde_json::from_str(&migration.text).unwrap();
    assert_eq!(value, json!({"disabled_tools": []}));
}

#[test]
fn comments_and_unrelated_keys_survive_a_fix() {
    let doc = "{\n  // pick the fast path\n  \"edit_mode\": \"hashline\",\n  /* old */\n  \"search_index\": false,\n  \"harnesses\": {\n    // pi only\n    \"pi\": {\"hoist_builtin_tools\": false}\n  }\n}\n";
    let migration = assert_fix_preserves_intent(doc, FixTier::User);
    for kept in [
        "// pick the fast path",
        "/* old */",
        "// pi only",
        "\"edit_mode\": \"hashline\"",
    ] {
        assert!(
            migration.text.contains(kept),
            "{kept} missing from {}",
            migration.text
        );
    }
}

#[test]
fn files_without_legacy_input_are_left_byte_identical() {
    for doc in [
        "{\n  // nothing to do\n  \"disabled_tools\": [\"aft_zoom\"],\n  \"indexes\": {\"semantic\": false}\n}\n",
        "{}",
        r#"{"gh_shim": {"binary_path": "/opt/aft"}}"#,
    ] {
        let migration = migrate_config_text(doc, FixTier::User).unwrap();
        assert!(!migration.changed, "{doc}");
        assert_eq!(migration.text, doc);
    }
}

#[test]
fn retired_github_aliases_are_repaired_only_here() {
    let migration = migrate_config_text(
        r#"{"gh_read": {"enabled": true}, "gh_shim": {"enabled": false, "binary_path": "/opt/aft"}}"#,
        FixTier::User,
    )
    .unwrap();
    let value: Value = serde_json::from_str(&migration.text).unwrap();
    assert_eq!(
        value,
        json!({"gh_shim": {"binary_path": "/opt/aft"}, "github": {"read": true, "shim": false}})
    );
    let after = resolved(&migration.text, "user", None, PolicyPhase::Rejecting);
    assert_eq!(
        after["github"],
        json!({"shim": false, "read": true, "write": false})
    );

    // Canonical leaves win over the aliases; conflicts are reported.
    let migration = migrate_config_text(
        r#"{"github": {"read": false}, "gh_read": {"enabled": true}, "gh_shim": {"enabled": true}}"#,
        FixTier::User,
    )
    .unwrap();
    let value: Value = serde_json::from_str(&migration.text).unwrap();
    assert_eq!(value, json!({"github": {"read": false, "shim": true}}));
    assert!(migration
        .notes
        .iter()
        .any(|note| note.contains("gh_read.enabled=true ignored")));

    // The master switch's generated value outranks the older alias.
    let migration = migrate_config_text(
        r#"{"github": {"enabled": false}, "gh_shim": {"enabled": true}}"#,
        FixTier::User,
    )
    .unwrap();
    let value: Value = serde_json::from_str(&migration.text).unwrap();
    assert_eq!(
        value,
        json!({"github": {"read": false, "write": false, "shim": false}})
    );
    assert!(migration
        .notes
        .iter()
        .any(|note| note.contains("gh_shim.enabled=true ignored")));
}

#[test]
fn project_fixes_never_materialize_protected_disables() {
    let migration = assert_fix_preserves_intent(r#"{"enabled": false}"#, FixTier::Project);
    let value: Value = serde_json::from_str(&migration.text).unwrap();
    let list: Vec<&str> = value["disabled_tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for protected in feature_config::HOST_TOOL_NAMES
        .iter()
        .chain(["aft_safety"].iter())
    {
        assert!(!list.contains(protected), "{protected}");
        assert!(migration
            .notes
            .iter()
            .any(|note| note.contains(&format!("disable {protected};"))));
    }
    assert!(list.contains(&"aft_zoom"));

    let migration =
        assert_fix_preserves_intent(r#"{"hoist_builtin_tools": false}"#, FixTier::Project);
    let value: Value = serde_json::from_str(&migration.text).unwrap();
    assert_eq!(value, json!({"disabled_tools": []}));
}

/// The move/delete default belongs to the user base: fixing a project file
/// writes only the names its own legacy keys imply, so a user who enabled
/// move and delete keeps them.
#[test]
fn project_fixes_never_materialize_the_default_disables() {
    for (doc, want) in [
        (r#"{"hoist_builtin_tools": false}"#, json!([])),
        (r#"{"backup": {"enabled": false}}"#, json!([])),
        (
            r#"{"bash": false}"#,
            json!(["bash_kill", "bash_status", "bash_watch", "bash_write"]),
        ),
        (r#"{"inspect": {"enabled": false}}"#, json!(["aft_inspect"])),
    ] {
        let migration = migrate_config_text(doc, FixTier::Project).unwrap();
        let value: Value = serde_json::from_str(&migration.text).unwrap();
        assert_eq!(value["disabled_tools"], want, "{doc}");
        for text in [doc, migration.text.as_str()] {
            let tiers = [
                ConfigTier {
                    tier: "user".to_string(),
                    source: "user".to_string(),
                    doc: r#"{"disabled_tools": []}"#.to_string(),
                },
                ConfigTier {
                    tier: "project".to_string(),
                    source: "project".to_string(),
                    doc: text.to_string(),
                },
            ];
            let result = resolve_config_for_harness_with_phase(&tiers, None, PolicyPhase::Window);
            assert!(result.errors.is_empty());
            for kept in ["aft_move", "aft_delete"] {
                assert!(
                    !result.config.disabled_tools.iter().any(|name| name == kept),
                    "{text}: {kept} must stay registered"
                );
            }
        }
    }
}

#[test]
fn a_multi_file_run_keeps_successful_repairs_when_another_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("user.jsonc");
    let bad = dir.path().join("project.jsonc");
    std::fs::write(&good, r#"{"search_index": false}"#).unwrap();
    std::fs::write(&bad, r#"{"search_index": false"#).unwrap();
    let outcomes = fix_files(&[
        (good.clone(), FixTier::User),
        (bad.clone(), FixTier::Project),
    ]);
    assert_eq!(outcomes[0].status, "rewritten");
    assert_eq!(outcomes[1].status, "failed");
    let fixed: Value = serde_json::from_str(&std::fs::read_to_string(&good).unwrap()).unwrap();
    assert_eq!(fixed, json!({"indexes": {"trigram": false}}));
    assert_eq!(
        std::fs::read_to_string(&bad).unwrap(),
        r#"{"search_index": false"#
    );
}

#[test]
fn without_a_project_file_only_the_user_file_is_a_target() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("aft.jsonc");
    std::fs::write(&user, "{}").unwrap();
    let cwd = dir.path().join("elsewhere");
    std::fs::create_dir_all(&cwd).unwrap();
    assert_eq!(
        fix_targets(Some(&user), &cwd),
        vec![(user.clone(), FixTier::User)]
    );
    std::fs::create_dir_all(cwd.join(".cortexkit")).unwrap();
    std::fs::write(cwd.join(".cortexkit/aft.jsonc"), "{}").unwrap();
    assert_eq!(fix_targets(None, &cwd).len(), 1);
    assert_eq!(fix_targets(None, &cwd)[0].1, FixTier::Project);
}
