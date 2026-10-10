use super::*;
use crate::config_resolve::{resolve_config_for_harness, ConfigTier};
use crate::harness::Harness;
use serde_json::json;

#[test]
fn inspect_cleanup_doctor_maps_idle_and_drops_inert_keys() {
    for (old, expected) in [(1, 5), (10, 10), (2000, 1440)] {
        let doc = json!({"idle":{"root_ttl_minutes":20,"lsp_ttl_minutes":old},"inspect":{"tier2_soft_deadline_ms":50,"max_drill_down_items":20,"enabled":false},"harnesses":{"pi":{"idle":{"lsp_ttl_minutes":old}}}}).to_string();
        let migrated = migrate_config_text(&doc, FixTier::User).unwrap();
        assert!(migrated.changed);
        let value: Value =
            serde_json::from_str(&crate::jsonc::strip_jsonc(&migrated.text)).unwrap();
        assert_eq!(value["lsp"]["idle_minutes"], expected);
        assert_eq!(value["idle"], json!({"root_ttl_minutes":20}));
        assert_eq!(value["inspect"], json!({"enabled":false}));
        assert_eq!(value["harnesses"]["pi"]["lsp"]["idle_minutes"], expected);
        assert!(
            !migrate_config_text(&migrated.text, FixTier::User)
                .unwrap()
                .changed
        );
    }
    let migrated = migrate_config_text(
        r#"{"idle":{"lsp_ttl_minutes":10},"lsp":{"idle_minutes":"never"}}"#,
        FixTier::User,
    )
    .unwrap();
    let value: Value = serde_json::from_str(&migrated.text).unwrap();
    assert_eq!(value["lsp"]["idle_minutes"], "never", "canonical key wins");
    // A whole number written as a float migrates as that number, matching
    // the load-time translation in both languages.
    let migrated =
        migrate_config_text(r#"{"idle":{"lsp_ttl_minutes":12.0}}"#, FixTier::User).unwrap();
    let value: Value = serde_json::from_str(&migrated.text).unwrap();
    assert_eq!(value["lsp"]["idle_minutes"], 12);
}

fn resolved(doc: &str, tier: &str, harness: Option<&Harness>) -> Value {
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
    let result = resolve_config_for_harness(&tiers, harness);
    assert!(result.errors.is_empty(), "{doc}: {:?}", result.errors);
    let config = result.config;
    json!({
        "disabled_tools": config.disabled_tools,
        "indexes": serde_json::to_value(config.indexes).unwrap(),
        "github": serde_json::to_value(&config.github).unwrap(),
        "backup": config.backup.enabled,
        "bash": config.bash.enabled,
        "bash_features": [config.experimental_bash_rewrite, config.experimental_bash_compress, config.experimental_bash_background],
        "lsp_ty": config.experimental_lsp_ty,
    })
}

/// Fixing a file must preserve the values ordinary loading derives from its
/// retired keys, and the fixed file must use none of them any more.
fn assert_fix_preserves_intent(doc: &str, tier: FixTier) -> Migration {
    let tier_name = if tier == FixTier::User {
        "user"
    } else {
        "project"
    };
    let migration = migrate_config_text(doc, tier).unwrap();
    assert!(migration.changed, "{doc} needed migration");
    for harness in [None, Some(Harness::Opencode), Some(Harness::Pi)] {
        let before = resolved(doc, tier_name, harness.as_ref());
        let after = resolved(&migration.text, tier_name, harness.as_ref());
        assert_eq!(before, after, "{doc}\n=>\n{}", migration.text);
    }
    let value: Value = serde_json::from_str(&crate::jsonc::strip_jsonc(&migration.text)).unwrap();
    let mut fixed = value.as_object().unwrap().clone();
    let document_tier = if tier == FixTier::User {
        feature_config::DocumentTier::User
    } else {
        feature_config::DocumentTier::Project
    };
    let check = feature_config::translate_document(&mut fixed, document_tier);
    assert!(!check.legacy_input, "{:?}", check.retired_keys);
    migration
}

#[test]
fn legacy_registration_and_index_keys_become_canonical() {
    for doc in [
        r#"{"tool_surface": "recommended"}"#,
        r#"{"tool_surface": "all"}"#,
        r#"{"tool_surface": "minimal", "disabled_tools": []}"#,
        r#"{"hoist_builtin_tools": false}"#,
        r#"{"hoist_builtin_tools": false, "bash": false, "backup": {"enabled": false}}"#,
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
        // A false runtime gate is a current key that only switches its
        // behaviour off, so there is no registration choice to record.
        r#"{"backup": {"enabled": false}}"#,
        r#"{"bash": false, "inspect": {"enabled": false}}"#,
    ] {
        let migration = migrate_config_text(doc, FixTier::User).unwrap();
        assert!(!migration.changed, "{doc}");
        assert_eq!(migration.text, doc);
    }
}

#[test]
fn retired_github_aliases_are_repaired() {
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
    let after = resolved(&migration.text, "user", None);
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
        (
            r#"{"hoist_builtin_tools": false, "bash": false}"#,
            json!([]),
        ),
        // A false gate beside a retired key still records no disable.
        (
            r#"{"search_index": false, "inspect": {"enabled": false}}"#,
            Value::Null,
        ),
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
            let result = resolve_config_for_harness(&tiers, None);
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

#[test]
fn experimental_keys_doctor_preserves_opt_in_defaults_and_precedence() {
    for tier in [FixTier::User, FixTier::Project] {
        for (input, expected) in [
            (
                json!({"experimental_lsp_ty": true}),
                json!({"experimental": {"lsp_ty": true}}),
            ),
            (
                json!({"experimental_bash_rewrite": true}),
                json!({"bash": {"rewrite": true, "compress": false, "background": false}}),
            ),
            (
                json!({"experimental_bash_compress": true}),
                json!({"bash": {"rewrite": false, "compress": true, "background": false}}),
            ),
            (
                json!({"experimental_bash_background": true}),
                json!({"bash": {"rewrite": false, "compress": false, "background": true}}),
            ),
            (
                json!({"experimental_bash_rewrite": true, "experimental": {"bash": {"rewrite": false, "compress": true}}}),
                json!({"bash": {"rewrite": false, "compress": true, "background": false}}),
            ),
            (
                json!({"experimental_bash_rewrite": true, "bash": {"compress": true}}),
                json!({"bash": {"compress": true}}),
            ),
            (
                json!({"experimental_lsp_ty": true, "experimental": {"lsp_ty": false}}),
                json!({"experimental": {"lsp_ty": false}}),
            ),
            (
                json!({"experimental": {"bash": {"rewrite": true, "long_running_reminder_enabled": false, "long_running_reminder_interval_ms": 1000}}}),
                json!({"bash": {"rewrite": true, "compress": false, "background": false, "long_running_reminder_enabled": false, "long_running_reminder_interval_ms": 1000}}),
            ),
            (json!({"experimental": {"bash": true}}), json!({})),
            (
                json!({"harnesses": {"pi": {"experimental_bash_background": true}, "opencode": {"experimental_lsp_ty": true}}}),
                json!({"harnesses": {"pi": {"bash": {"rewrite": false, "compress": false, "background": true}}, "opencode": {"experimental": {"lsp_ty": true}}}}),
            ),
        ] {
            let migration = assert_fix_preserves_intent(&input.to_string(), tier);
            let value: Value = serde_json::from_str(&migration.text).unwrap();
            assert_eq!(value, expected, "{input}");
            assert!(!migrate_config_text(&migration.text, tier).unwrap().changed);
        }
        let tuning_only = r#"{"experimental":{"bash":{"long_running_reminder_enabled":false}}}"#;
        assert!(!migrate_config_text(tuning_only, tier).unwrap().changed);
    }
}

#[test]
fn experimental_keys_auto_migrate_user_once_via_doctor_mapping() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.jsonc");
    let input = "{\n  // keep the opt-in settings\n  \"experimental_lsp_ty\": true,\n  \"experimental_bash_rewrite\": true,\n  \"experimental_bash_compress\": false,\n  \"experimental_bash_background\": true,\n  \"harnesses\": {\"pi\": {\"experimental_bash_background\": false}}\n}\n";
    std::fs::write(&path, input).unwrap();
    let outcome = auto_migrate_user_config(&path).expect("experimental keys need migration");
    let UserConfigMigration::Migrated { backup, notice, .. } = outcome else {
        panic!("expected a rewrite, got {outcome:?}");
    };
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), input);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("// keep the opt-in settings"));
    let value: Value = serde_json::from_str(&crate::jsonc::strip_jsonc(&text)).unwrap();
    assert_eq!(
        value["bash"],
        json!({"rewrite": true, "compress": false, "background": true})
    );
    assert_eq!(value["experimental"], json!({"lsp_ty": true}));
    assert_eq!(
        value["harnesses"]["pi"]["bash"],
        json!({"rewrite": false, "compress": false, "background": false})
    );
    assert!(!value
        .as_object()
        .unwrap()
        .contains_key("experimental_lsp_ty"));
    assert_eq!(
        text,
        migrate_config_text(input, FixTier::User).unwrap().text
    );
    assert!(notice.contains("experimental_lsp_ty"));
    assert!(notice.contains("experimental_bash_rewrite"));
    assert_eq!(auto_migrate_user_config(&path), None);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    assert_eq!(backups(dir.path()).len(), 1);
}

const RETIRED_USER_FILE: &str =
    "{\n  // my settings\n  \"search_index\": false,\n  \"hoist_builtin_tools\": false\n}\n";

fn backups(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.to_string_lossy().contains(".bak-"))
        .collect();
    found.sort();
    found
}

/// The user file is rewritten with the doctor migration, keeps its comment,
/// leaves the old text in a backup, and a second load changes nothing.
#[test]
fn auto_migration_rewrites_the_user_file_once_and_keeps_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.jsonc");
    std::fs::write(&path, RETIRED_USER_FILE).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    let outcome = auto_migrate_user_config(&path).expect("migrated");
    let UserConfigMigration::Migrated { backup, notice, .. } = &outcome else {
        panic!("expected a rewrite, got {outcome:?}");
    };
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        text,
        migrate_config_text(RETIRED_USER_FILE, FixTier::User)
            .unwrap()
            .text
    );
    assert!(text.contains("// my settings"), "{text}");
    assert!(retired_key_translation(&text).is_none(), "{text}");
    assert_eq!(std::fs::read_to_string(backup).unwrap(), RETIRED_USER_FILE);
    // The backup sits next to the canonical file, and macOS's temp dir is a
    // symlink (/var -> /private/var), so compare canonical paths.
    let canonical = |paths: Vec<std::path::PathBuf>| -> Vec<std::path::PathBuf> {
        paths
            .into_iter()
            .map(|path| std::fs::canonicalize(path).unwrap())
            .collect()
    };
    assert_eq!(
        canonical(backups(dir.path())),
        canonical(vec![backup.clone()])
    );
    assert!(
        notice.contains("hoist_builtin_tools, search_index"),
        "{notice}"
    );
    assert!(notice.contains(&backup.display().to_string()), "{notice}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the rewrite keeps the file mode");
    }

    assert_eq!(
        auto_migrate_user_config(&path),
        None,
        "second run is a no-op"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    assert_eq!(backups(dir.path()).len(), 1);
    assert!(
        !dir.path().join(".aft.jsonc.migrate.lock").exists(),
        "the lock is released"
    );
}

/// Several loaders migrating at once converge on one rewrite, one backup and
/// one notice.
#[test]
fn concurrent_auto_migrations_converge() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.jsonc");
    std::fs::write(&path, RETIRED_USER_FILE).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                auto_migrate_user_config(&path)
            })
        })
        .collect();
    let outcomes: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    let migrated = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, Some(UserConfigMigration::Migrated { .. })))
        .count();
    assert_eq!(migrated, 1, "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .all(|outcome| !matches!(outcome, Some(UserConfigMigration::NotMigrated { .. }))),
        "{outcomes:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        migrate_config_text(RETIRED_USER_FILE, FixTier::User)
            .unwrap()
            .text
    );
    assert_eq!(backups(dir.path()).len(), 1);
}

/// A read-only user file is never rewritten; the caller translates it in
/// memory and the notice says why the file was left alone.
#[test]
fn a_read_only_user_file_is_left_alone_with_a_notice() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.jsonc");
    std::fs::write(&path, RETIRED_USER_FILE).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&path, permissions).unwrap();

    let outcome = auto_migrate_user_config(&path).expect("reported");
    let UserConfigMigration::NotMigrated { reason, notice, .. } = &outcome else {
        panic!("expected no rewrite, got {outcome:?}");
    };
    assert_eq!(reason, "the file is read-only");
    assert!(
        notice.contains("applied their current equivalents"),
        "{notice}"
    );
    assert!(notice.contains("doctor --fix"), "{notice}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), RETIRED_USER_FILE);
    assert!(backups(dir.path()).is_empty());

    // Loading the same text still resolves, translated in memory.
    let result = resolve_config_for_harness(
        &[ConfigTier {
            tier: "user".to_string(),
            source: path.display().to_string(),
            doc: RETIRED_USER_FILE.to_string(),
        }],
        None,
    );
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(!result.config.indexes.trigram);

    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(&path, permissions).unwrap();
}

#[test]
fn auto_migration_ignores_current_missing_and_unparsable_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.jsonc");
    assert_eq!(auto_migrate_user_config(&path), None);
    for text in [
        "{\n  \"indexes\": {\"semantic\": false},\n  \"bash\": false\n}\n",
        "{ \"search_index\": false",
    ] {
        std::fs::write(&path, text).unwrap();
        assert_eq!(auto_migrate_user_config(&path), None, "{text}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }
    assert!(backups(dir.path()).is_empty());
}

/// A loader that finds another process's fresh lock waits, then leaves the
/// file to that process; a lock left behind by a crash is cleared.
#[test]
fn a_held_lock_defers_and_a_stale_lock_is_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.jsonc");
    std::fs::write(&path, RETIRED_USER_FILE).unwrap();
    let lock = dir.path().join(".aft.jsonc.migrate.lock");
    std::fs::write(&lock, "").unwrap();
    assert_eq!(auto_migrate_user_config(&path), None);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), RETIRED_USER_FILE);

    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    std::fs::File::options()
        .write(true)
        .open(&lock)
        .unwrap()
        .set_modified(old)
        .unwrap();
    assert!(matches!(
        auto_migrate_user_config(&path),
        Some(UserConfigMigration::Migrated { .. })
    ));
    assert!(!lock.exists());
}

/// A symlinked user file (for example from a dotfiles checkout) is rewritten
/// at its target, so the link itself survives.
#[cfg(unix)]
#[test]
fn a_symlinked_user_file_is_rewritten_at_its_target() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("dotfiles-aft.jsonc");
    std::fs::write(&real, RETIRED_USER_FILE).unwrap();
    let link_dir = dir.path().join("config");
    std::fs::create_dir_all(&link_dir).unwrap();
    let link = link_dir.join("aft.jsonc");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    assert!(matches!(
        auto_migrate_user_config(&link),
        Some(UserConfigMigration::Migrated { .. })
    ));
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(retired_key_translation(&std::fs::read_to_string(&real).unwrap()).is_none());
}

/// A debug build (a test run, or the `target/debug/aft` a test spawns) that
/// inherited the operator's real HOME must not rewrite the operator's own
/// config file: it translates in memory and says why. The account's config
/// directory comes from the storage fence's test seam, so this test never
/// looks at the real one. `AFT_ALLOW_PRODUCTION_MIGRATION=1` opts back in.
#[cfg(debug_assertions)]
#[test]
fn a_debug_build_never_rewrites_a_user_file_in_the_account_config_dir() {
    let fixture = tempfile::tempdir().unwrap();
    let config_dir = fixture.path().join("account/.config/cortexkit");
    std::fs::create_dir_all(&config_dir).unwrap();
    let path = config_dir.join("aft.jsonc");
    std::fs::write(&path, RETIRED_USER_FILE).unwrap();
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();

    let outcome = crate::production_storage::with_test_account(&config_dir, false, || {
        auto_migrate_user_config(&path)
    })
    .expect("reported");
    let UserConfigMigration::NotMigrated { reason, notice, .. } = &outcome else {
        panic!("expected no rewrite, got {outcome:?}");
    };
    assert!(reason.contains(crate::production_storage::CODE), "{reason}");
    assert!(
        reason.contains("AFT_ALLOW_PRODUCTION_MIGRATION=1"),
        "{reason}"
    );
    assert!(
        notice.contains("applied their current equivalents"),
        "{notice}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), RETIRED_USER_FILE);
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        modified
    );
    assert_eq!(
        std::fs::read_dir(&config_dir).unwrap().count(),
        1,
        "no backup, lock or temporary file may be created"
    );

    let opted_in = crate::production_storage::with_test_account(&config_dir, true, || {
        auto_migrate_user_config(&path)
    });
    assert!(
        matches!(opted_in, Some(UserConfigMigration::Migrated { .. })),
        "{opted_in:?}"
    );
}
