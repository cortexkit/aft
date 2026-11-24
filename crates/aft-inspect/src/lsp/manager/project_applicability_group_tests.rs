#[cfg(all(test, unix))]
mod project_applicability_tests {
    use super::*;

    #[test]
    fn runtime_note_presence_reports_only_raw_client_notes() {
        assert!(!LspManager::new().has_runtime_notes());
        let (_dir, mut manager, _config, ts) = fixture(false, false);
        assert!(!manager.has_runtime_notes());
        manager.clients.get_mut(&ts).unwrap().runtime_note = Some("selected SDK".into());
        assert!(manager.has_runtime_notes());
    }

    #[test]
    fn scoped_runtime_notes_preserve_selection_when_notes_exist() {
        let (_dir, mut manager, _config, ts) = fixture(false, false);
        manager.clients.get_mut(&ts).unwrap().runtime_note = Some("selected SDK".into());
        assert_eq!(
            manager.runtime_notes_for(&HashSet::from([ts])),
            vec!["selected SDK"]
        );
        assert!(manager.runtime_notes_for(&HashSet::new()).is_empty());
    }

    fn fixture(yaml_enabled: bool, go: bool) -> (tempfile::TempDir, LspManager, Config, ServerKey) {
        let dir = tempfile::tempdir().unwrap();
        let root = canonicalize_for_lsp(dir.path()).unwrap();
        std::fs::write(root.join("package.json"), "{}").unwrap();
        std::fs::write(root.join("main.ts"), "const x = 1;\n").unwrap();
        std::fs::write(root.join("settings.yaml"), "key: value\n").unwrap();
        if go {
            std::fs::write(root.join("go.mod"), "module example.test/app\n").unwrap();
            std::fs::write(root.join("main.go"), "package main\nfunc main() {}\n").unwrap();
        }
        let mut config = Config {
            project_root: Some(root.clone()),
            ..Config::default()
        };
        config.disabled_lsp.insert("biome".into());
        config.disabled_lsp.insert("oxlint".into());
        if !yaml_enabled {
            config.disabled_lsp.insert("yaml".into());
        }
        let mut manager = LspManager::new();
        manager.override_binary(ServerKind::TypeScript(None), PathBuf::from("/bin/sh"));
        manager.override_binary(ServerKind::Yaml, root.join("missing-yaml"));
        manager.override_binary(ServerKind::Go, root.join("missing-gopls"));
        let snapshot = manager
            .resolve_applicable_servers_for_root(&root, &config)
            .unwrap();
        let ts = snapshot
            .server_keys
            .iter()
            .find(|key| key.kind == ServerKind::TypeScript(None))
            .unwrap()
            .clone();
        manager.store_project_applicability(snapshot);
        // A live transport plus a stored empty report models an authoritative
        // producer without involving a real language server or SDK discovery.
        let client = LspClient::spawn_with_reclaim_root(
            ServerKind::TypeScript(None),
            ts.root.clone(),
            Path::new("/bin/sh"),
            &["-c".into(), "while read line; do :; done".into()],
            &HashMap::new(),
            manager.event_sender_for_test(),
            LspChildRegistry::new(),
            None,
            false,
        )
        .unwrap();
        manager.insert_client_for_test(client);
        manager
            .diagnostics
            .publish(ts.clone(), root.join("main.ts"), Vec::new());
        manager.certify_diagnostic_content_for_test(&root.join("main.ts"));
        (dir, manager, config, ts)
    }

    #[test]
    fn project_authority_ignores_disabled_and_undefined_producers() {
        let (_dir, manager, _config, _) = fixture(false, false);
        let (authoritative, gaps) = manager.project_diagnostics_authority();
        assert!(authoritative, "{gaps:?}");
        assert!(gaps.is_empty());
    }

    #[test]
    fn project_authority_masks_typescript_sdk_unavailable() {
        let (_dir, mut manager, _config, ts) = fixture(false, false);
        manager
            .clients
            .get_mut(&ts)
            .unwrap()
            .set_diagnostic_failure(Some("TypeScript SDK unavailable: no tsserver.js".into()));
        let (authoritative, gaps) = manager.project_diagnostics_authority();
        assert!(!authoritative);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0]["language"], "typescript");
        assert_eq!(gaps[0]["root"], ts.root.to_string_lossy().as_ref());
    }

    #[test]
    fn project_authority_masks_missing_yaml_binary() {
        let (_dir, manager, _config, _) = fixture(true, false);
        let (authoritative, gaps) = manager.project_diagnostics_authority();
        assert!(!authoritative);
        assert!(gaps.iter().any(|gap| gap["language"] == "yaml"));
    }

    #[test]
    fn scoped_walk_does_not_replace_project_missing_gopls() {
        let (dir, manager, config, _) = fixture(false, true);
        let before = manager.project_diagnostics_authority();
        assert!(!before.0);
        assert!(before.1.iter().any(|gap| gap["producer"] == "gopls"));
        let root = canonicalize_for_lsp(dir.path()).unwrap();
        let scoped = manager.classify_applicable_servers(
            walk_applicable_area(&root, Some(&[root.join("main.ts")]), &config, None).unwrap(),
            &config,
        );
        assert!(scoped
            .server_keys
            .iter()
            .all(|key| key.kind == ServerKind::TypeScript(None)));
        assert_eq!(manager.project_diagnostics_authority(), before);
    }
}
