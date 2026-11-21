mod tests {
    use crate::bash_rewrite::catalog::ControlRole;
    use crate::bash_rewrite::dispatch::*;

    #[test]
    fn unregistered_rewrite_refuses_without_native_execution_or_mutation() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("notes.txt");
        std::fs::write(&file, "before\n").unwrap();
        let ctx = crate::test_context(
            crate::context::default_language_provider_factory(),
            crate::config::Config {
                project_root: Some(root.path().to_path_buf()),
                storage_dir: Some(root.path().join("storage")),
                experimental_bash_rewrite: true,
                ..crate::config::Config::default()
            },
        );
        // None is the caller's native-shell route; an accepted but unavailable
        // rewrite must instead return an error carrying the request id.
        let response = dispatch_for_request_with_handlers(
            "echo after >> notes.txt",
            "unregistered",
            None,
            &crate::host_tool_names::HostToolNames::default(),
            &ctx,
            None,
        )
        .expect("accepted rewrite must not fall back to bash");
        assert!(!response.success);
        assert_eq!(response.data["code"], "bash_rewrite_unavailable");
        assert_eq!(std::fs::read_to_string(file).unwrap(), "before\n");
        assert_eq!(response.id, "unregistered");
        assert!(route_record("unregistered").is_none());
    }

    #[test]
    fn dispatch_parses_once_even_when_every_rule_declines() {
        let root = tempfile::tempdir().unwrap();
        let ctx = crate::test_context(
            crate::context::default_language_provider_factory(),
            crate::config::Config {
                project_root: Some(root.path().to_path_buf()),
                storage_dir: Some(root.path().join("storage")),
                experimental_bash_rewrite: true,
                ..crate::config::Config::default()
            },
        );
        for command in ["printf hi", "printf $(date)"] {
            crate::bash_rewrite::parser::take_parse_calls();
            assert!(dispatch_for_request(
                command,
                "parse-once",
                None,
                &crate::host_tool_names::HostToolNames::default(),
                &ctx
            )
            .is_none());
            assert_eq!(
                crate::bash_rewrite::parser::take_parse_calls(),
                1,
                "{command}"
            );
        }
    }

    #[test]
    fn route_records_are_request_correlated_and_replaceable() {
        record_native(
            "route-test",
            ControlRole::Native,
            "dispatch.native.no_rule",
            "test",
        );
        assert_eq!(
            route_record("route-test").expect("route record").request_id,
            "route-test"
        );
        record_native(
            "route-test",
            ControlRole::Sandbox,
            "dispatch.native.sandbox",
            "test",
        );
        assert_eq!(
            route_record("route-test").expect("replacement").route,
            DispatchRoute::Native {
                role: "sandbox".to_string(),
                branch_id: "dispatch.native.sandbox".to_string(),
                reason: "test".to_string(),
            }
        );
        let _ = take_route_record("route-test");
    }

    #[test]
    fn audit_growth_route_records_are_bounded_without_losing_the_latest_pending_record() {
        const MAX_RECORDS: usize = 1024;
        let prefix = format!("route-growth-{}-", std::process::id());
        for index in 0..(MAX_RECORDS + 128) {
            record_native(
                &format!("{prefix}{index}"),
                ControlRole::Native,
                "dispatch.native.no_rule",
                "test",
            );
        }

        let retained = count_route_records_for_prefix(&prefix);
        assert!(retained <= MAX_RECORDS, "retained {retained} route records");
        let latest = format!("{prefix}{}", MAX_RECORDS + 127);
        assert!(
            route_record(&latest).is_some(),
            "latest record remains readable"
        );
        assert!(
            take_route_record(&latest).is_some(),
            "caller can consume its record"
        );
    }
}
