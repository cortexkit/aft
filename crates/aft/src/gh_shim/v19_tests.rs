use super::*;

fn manifest() -> Manifest {
    let manifest: Manifest = serde_json::from_str(include_str!(
        "../../tests/fixtures/gh_shim/v19-manifest.json"
    ))
    .unwrap();
    manifest.validate().unwrap();
    manifest
}

fn v18_manifest() -> Manifest {
    let manifest: Manifest = serde_json::from_str(include_str!(
        "../../tests/fixtures/gh_shim/v18-manifest.json"
    ))
    .unwrap();
    manifest.validate().unwrap();
    manifest
}

#[test]
fn v19_payload_parses_additions_and_preserves_v18_bindings() {
    let current = manifest();
    let previous = v18_manifest();
    assert_eq!(current.manifest_version, 19);
    assert_eq!(previous.manifest_version, 18);

    let governed = [
        (&["pr", "ready", "5"][..], "pr ready"),
        (&["pr", "ready", "5", "--undo"][..], "pr ready"),
    ];
    for (raw, tuple) in governed {
        assert!(matches!(
            classify(&args(raw), &current, "linux"),
            Classification::Governed { tuple: found, .. } if found == tuple
        ));
        assert!(matches!(
            classify(&args(raw), &previous, "linux"),
            Classification::Unclassified
        ));
    }

    let admin = [
        (&["repo", "create", "cortexkit/new-repo"][..], "repo create"),
        (
            &["repo", "rename", "new-name", "--repo", "cortexkit/aft"][..],
            "repo rename",
        ),
        (
            &["workflow", "disable", "ci.yml", "--repo", "cortexkit/aft"][..],
            "workflow disable",
        ),
        (
            &["workflow", "enable", "ci.yml", "--repo", "cortexkit/aft"][..],
            "workflow enable",
        ),
        (
            &[
                "api",
                "-X",
                "PUT",
                "repos/cortexkit/aft/private-vulnerability-reporting",
            ][..],
            "api:PUT:/repos/*/*/private-vulnerability-reporting",
        ),
        (
            &[
                "api",
                "-X",
                "DELETE",
                "/repos/cortexkit/aft/private-vulnerability-reporting",
            ][..],
            "api:DELETE:/repos/*/*/private-vulnerability-reporting",
        ),
    ];
    for (raw, tuple) in admin {
        assert!(matches!(
            classify(&args(raw), &current, "linux"),
            Classification::Admin { tuple: found } if found == tuple
        ));
        assert!(matches!(
            classify(&args(raw), &previous, "linux"),
            Classification::Unclassified
        ));
    }

    assert_eq!(
        current
            .bindings
            .get("cortexkit/common-auth")
            .map(String::as_str),
        Some("agent_0f4d7713e64e7cc4")
    );
    assert_eq!(current.bindings.len(), previous.bindings.len() + 1);
    for (repository, agent_id) in &previous.bindings {
        assert_eq!(current.bindings.get(repository), Some(agent_id));
    }
}

fn args(raw: &[&str]) -> Vec<OsString> {
    raw.iter().map(OsString::from).collect()
}

fn rung() -> RungRecord {
    RungDetermination::r3(
        1_700_000_000,
        19,
        &RungRecordProvenance {
            image_path: "/test/aft".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            repo_key: "cortexkit/aft".into(),
        },
    )
    .record
}

#[test]
fn v19_pr_ready_is_versioned_bot_speech() {
    let manifest = manifest();
    manifest.validate().unwrap();
    for platform in ["macos", "linux"] {
        for raw in [vec!["pr", "ready", "7"], vec!["pr", "ready", "7", "--undo"]] {
            assert!(matches!(
                classify(&args(&raw), &manifest, platform),
                Classification::Governed { tuple, .. } if tuple == "pr ready"
            ));
            let mut old = manifest.clone();
            old.manifest_version = 18;
            assert!(matches!(
                classify(&args(&raw), &old, platform),
                Classification::Unclassified
            ));
            let mut undeclared = manifest.clone();
            undeclared
                .tiers
                .get_mut(&Tier::Governed)
                .unwrap()
                .retain(|row| row.tuple() != "pr ready");
            assert!(matches!(
                classify(&args(&raw), &undeclared, platform),
                Classification::Unclassified
            ));
        }
    }
}

#[test]
fn v19_pr_ready_undo_reaches_the_daemon_wire() {
    let manifest = manifest();
    let canonical = &manifest.canonicalization["pr ready"];
    for undo in [false, true] {
        let mut raw = vec!["pr", "ready", "7", "--repo", "cortexkit/aft"];
        if undo {
            raw.push("--undo");
        }
        let request = canonicalize_governed(&args(&raw), "pr ready", canonical, 19).unwrap();
        let mutation = GithubReadMutation::from_governed_request(&request).unwrap();
        assert_eq!(mutation.resource_kind, GithubReadResourceKind::PullRequest);
        assert_eq!(mutation.resource_number, 7);
        let wire = governed_wire_request(&rung(), "alfonso-aft", request);
        assert_eq!(wire["verb"], "pr ready");
        assert_eq!(wire["number"], "7");
        assert_eq!(wire["repository"], "cortexkit/aft");
        assert_eq!(wire["undo"], undo);
        assert!(wire.get("action").is_none());
        assert!(wire.get("body").is_none());
    }
    for suffix in [
        vec!["--undo", "--undo"],
        vec!["--undo=false"],
        vec!["--comment", "not a ready flag"],
        vec!["--delete-branch"],
    ] {
        let mut raw = vec!["pr", "ready", "7", "--repo", "cortexkit/aft"];
        raw.extend(suffix);
        assert!(canonicalize_governed(&args(&raw), "pr ready", canonical, 19).is_err());
    }
}

struct BypassEnv(&'static str, Option<OsString>);

impl BypassEnv {
    fn set(value: Option<&str>) -> Self {
        Self::set_key("GH_SHIM_BYPASS", value)
    }

    fn set_key(key: &'static str, value: Option<&str>) -> Self {
        let previous = std::env::var_os(key);
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
        Self(key, previous)
    }
}

impl Drop for BypassEnv {
    fn drop(&mut self) {
        match &self.1 {
            Some(value) => std::env::set_var(self.0, value),
            None => std::env::remove_var(self.0),
        }
    }
}

fn fake_rename_preflight(args: &[String]) -> Result<String, String> {
    let expected = [
        "repo",
        "view",
        "cortexkit/aft",
        "--json",
        "nameWithOwner,defaultBranchRef,url",
    ]
    .map(str::to_owned);
    assert_eq!(args, expected);
    Err("injected gh preflight runner".into())
}

fn check_admin(raw: &[&str], tuple: &str) {
    let _lock = crate::test_env::process_env_lock();
    let _bypass = BypassEnv::set(None);
    let manifest = manifest();
    manifest.validate().unwrap();
    let args = args(raw);
    for platform in ["macos", "linux"] {
        assert!(matches!(classify(&args, &manifest, platform),
            Classification::Admin { tuple: found } if found == tuple));
        let mut old = manifest.clone();
        old.manifest_version = 18;
        assert!(matches!(
            classify(&args, &old, platform),
            Classification::Unclassified
        ));
    }
    let mut undeclared = manifest.clone();
    if tuple.starts_with("api:") {
        undeclared.api_rules.retain(|rule| api_tuple(rule) != tuple);
    } else {
        undeclared
            .tiers
            .get_mut(&Tier::Admin)
            .unwrap()
            .retain(|row| row.tuple() != tuple);
    }
    assert!(matches!(
        classify(&args, &undeclared, "linux"),
        Classification::Unclassified
    ));
    let directory = tempfile::tempdir().unwrap();
    let paths = StatePaths::from_root(directory.path().to_path_buf());
    let binding = AgentBinding {
        repo: "cortexkit/aft".into(),
        agent_id: "alfonso-aft".into(),
    };
    let dispatch = || {
        let classification = classify(&args, &manifest, "linux");
        let run_upstream = |forwarded: &[OsString]| {
            assert_eq!(forwarded, args);
            let (records, error) = read_bypass_audit(&paths);
            assert!(error.is_none());
            let records = records.unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0]["tuple"], tuple);
            73
        };
        if tuple == "repo create" {
            let target = TargetRepository::from_invocation(&args);
            let write = unbound_write(&args, &manifest, "linux", &target, directory.path())
                .expect("a new repository has no bot binding");
            assert!(matches!(write.target, WriteTarget::NotARepository { .. }));
            dispatch_unbound_write(
                &args,
                &write,
                &manifest.detectors,
                &paths,
                1_700_000_000,
                run_upstream,
            )
        } else {
            dispatch_r3(
                &args,
                classification,
                &manifest,
                &paths,
                &rung(),
                &binding,
                1_700_000_000,
                run_upstream,
            )
        }
    };
    assert_eq!(dispatch(), REFUSAL_EXIT_STATUS);
    assert!(!paths.bypass_audit.exists());
    let _operator = BypassEnv::set(Some("operator"));
    // This classifier test has no upstream gh; the runtime fixtures exercise
    // the rename preflight, while this row verifies its explicit override.
    let _rename_override = (tuple == "repo rename")
        .then(|| BypassEnv::set_key("GH_SHIM_RENAME_PUBLISHED", Some("allow")));
    let result = if tuple == "repo rename" {
        // Keep a broken injection from accidentally finding and invoking the
        // developer's real gh while this test verifies the fake runner.
        let path = directory.path().to_string_lossy().into_owned();
        let _path = BypassEnv::set_key("PATH", Some(&path));
        let (result, calls) =
            rename_guard::with_test_preflight_runner(fake_rename_preflight, dispatch);
        assert_eq!(calls, 1, "the injected gh preflight runner must be called");
        result
    } else {
        dispatch()
    };
    assert_eq!(result, 73);
}

#[test]
fn v19_workflow_toggle_rows_are_reviewed_only_at_v19() {
    let manifest = manifest();
    manifest.validate().unwrap();
    let rows = [
        (
            "workflow disable",
            vec!["workflow", "disable", "ci.yml", "--repo", "cortexkit/aft"],
        ),
        (
            "workflow disable",
            vec!["workflow", "disable", "12345", "-R", "cortexkit/aft"],
        ),
        (
            "workflow enable",
            vec!["workflow", "enable", "ci.yml", "--repo", "cortexkit/aft"],
        ),
        (
            "workflow enable",
            vec!["workflow", "enable", "12345", "-R", "cortexkit/aft"],
        ),
    ];
    for (tuple, raw) in &rows {
        for platform in ["macos", "linux"] {
            assert!(matches!(
                classify(&args(raw), &manifest, platform),
                Classification::Admin { tuple: found } if found == *tuple
            ));
            // The same declaration on a v18 manifest is not reviewed yet, so it
            // refuses as undeclared rather than running as administration.
            let mut previous = manifest.clone();
            previous.manifest_version = 18;
            assert!(matches!(
                classify(&args(raw), &previous, platform),
                Classification::Unclassified
            ));
        }
    }
    let mut undeclared = manifest.clone();
    undeclared
        .tiers
        .get_mut(&Tier::Admin)
        .unwrap()
        .retain(|row| !row.tuple().starts_with("workflow "));
    for (_, raw) in &rows {
        assert!(matches!(
            classify(&args(raw), &undeclared, "linux"),
            Classification::Unclassified
        ));
    }
    // Adding these two rows must not move the neighbours: `workflow run`
    // stays an admin tuple and `workflow view`/`workflow list` stay reads.
    assert!(matches!(
        classify(&args(&["workflow", "run", "ci.yml"]), &manifest, "linux"),
        Classification::Admin { tuple } if tuple == "workflow run"
    ));
    for raw in [vec!["workflow", "view", "ci.yml"], vec!["workflow", "list"]] {
        assert!(matches!(
            classify(&args(&raw), &manifest, "linux"),
            Classification::Mechanical
        ));
    }
}

#[test]
fn v19_workflow_toggle_requires_the_audited_operator_bypass() {
    let _lock = crate::test_env::process_env_lock();
    let manifest = manifest();
    manifest.validate().unwrap();
    let binding = AgentBinding {
        repo: "cortexkit/aft".into(),
        agent_id: "alfonso-aft".into(),
    };
    for (tuple, raw) in [
        (
            "workflow disable",
            vec!["workflow", "disable", "ci.yml", "--repo", "cortexkit/aft"],
        ),
        (
            "workflow enable",
            vec!["workflow", "enable", "ci.yml", "--repo", "cortexkit/aft"],
        ),
    ] {
        let argv = args(&raw);
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        {
            let _bypass = BypassEnv::set(None);
            assert_eq!(
                dispatch_r3(
                    &argv,
                    classify(&argv, &manifest, "linux"),
                    &manifest,
                    &paths,
                    &rung(),
                    &binding,
                    1_700_000_000,
                    |_| panic!("{tuple} reached upstream without the operator bypass"),
                ),
                REFUSAL_EXIT_STATUS
            );
            assert!(!paths.bypass_audit.exists());
        }
        let _bypass = BypassEnv::set(Some("operator"));
        assert_eq!(
            dispatch_r3(
                &argv,
                classify(&argv, &manifest, "linux"),
                &manifest,
                &paths,
                &rung(),
                &binding,
                1_700_000_000,
                |forwarded| {
                    assert_eq!(forwarded, argv);
                    let (records, error) = read_bypass_audit(&paths);
                    assert!(error.is_none());
                    let records = records.unwrap();
                    assert_eq!(records.len(), 1);
                    assert_eq!(records[0]["tuple"], tuple);
                    assert_eq!(records[0]["repository"], "cortexkit/aft");
                    73
                },
            ),
            73
        );
    }
}

#[test]
fn v19_workflow_toggle_refuses_modelled_flags_and_leaves_others_to_upstream_gh() {
    let _lock = crate::test_env::process_env_lock();
    let manifest = manifest();
    manifest.validate().unwrap();
    // The shim models no flags for these rows, exactly as for `run cancel`:
    // argv goes to upstream `gh` unchanged, and its own parser is what refuses a
    // flag the verb does not have. The flags the shim refuses for every verb
    // (deleting or creating the authenticated user's last comment) still refuse
    // here, before the manifest or the bypass are consulted.
    for flag in ["--delete-last", "--create-if-none", "--edit-last"] {
        for verb in ["disable", "enable"] {
            assert!(
                matches!(
                    classify(
                        &args(&["workflow", verb, "ci.yml", flag]),
                        &manifest,
                        "linux"
                    ),
                    Classification::Unclassified
                ),
                "{flag} on workflow {verb}"
            );
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let paths = StatePaths::from_root(directory.path().to_path_buf());
    let binding = AgentBinding {
        repo: "cortexkit/aft".into(),
        agent_id: "alfonso-aft".into(),
    };
    let _bypass = BypassEnv::set(Some("operator"));
    let argv = args(&[
        "workflow",
        "disable",
        "ci.yml",
        "--bogus",
        "--repo",
        "cortexkit/aft",
    ]);
    assert!(matches!(
        classify(&argv, &manifest, "linux"),
        Classification::Admin { tuple } if tuple == "workflow disable"
    ));
    assert_eq!(
        dispatch_r3(
            &argv,
            classify(&argv, &manifest, "linux"),
            &manifest,
            &paths,
            &rung(),
            &binding,
            1_700_000_000,
            |forwarded| {
                // Byte-identical forwarding is what lets upstream `gh` refuse
                // `--bogus` itself: a shim that dropped or rewrote the flag
                // would change what the operator asked for.
                assert_eq!(forwarded, argv);
                let records = read_bypass_audit(&paths).0.unwrap();
                assert_eq!(records.len(), 1);
                assert_eq!(records[0]["tuple"], "workflow disable");
                UPSTREAM_FAILURE_EXIT_STATUS
            },
        ),
        UPSTREAM_FAILURE_EXIT_STATUS
    );
}

#[test]
fn v19_repo_rename_requires_operator_bypass() {
    check_admin(
        &["repo", "rename", "new-name", "--repo", "cortexkit/aft"],
        "repo rename",
    );
}

#[test]
fn v19_repo_create_requires_operator_bypass_even_without_a_binding() {
    check_admin(
        &["repo", "create", "cortexkit/new-repo", "--private"],
        "repo create",
    );
}

#[test]
fn v19_private_reporting_put_requires_operator_bypass() {
    check_admin(
        &[
            "api",
            "-X",
            "PUT",
            "repos/cortexkit/aft/private-vulnerability-reporting",
        ],
        "api:PUT:/repos/*/*/private-vulnerability-reporting",
    );
}

#[test]
fn v19_private_reporting_delete_requires_operator_bypass() {
    check_admin(
        &[
            "api",
            "-X",
            "DELETE",
            "/repos/cortexkit/aft/private-vulnerability-reporting",
        ],
        "api:DELETE:/repos/*/*/private-vulnerability-reporting",
    );
}

#[test]
fn v19_private_reporting_put_rejects_other_paths() {
    let manifest = manifest();
    for path in [
        "repos/cortexkit/aft/private-vulnerability-reporting/extra",
        "repos/cortexkit/aft/private-vulnerability-reporting-extra",
        "repos/cortexkit/aft/other-setting",
        "repos/cortexkit/aft/extra/private-vulnerability-reporting",
        "repos/cortexkit//private-vulnerability-reporting",
    ] {
        assert!(
            matches!(
                classify(&args(&["api", "-X", "PUT", path]), &manifest, "linux"),
                Classification::Unclassified
            ),
            "{path}"
        );
    }
}

#[test]
fn v19_private_reporting_rejects_post_but_keeps_get_mechanical() {
    let manifest = manifest();
    let endpoint = "repos/cortexkit/aft/private-vulnerability-reporting";
    for method in ["POST", "PATCH"] {
        assert!(matches!(
            classify(&args(&["api", "-X", method, endpoint]), &manifest, "linux"),
            Classification::Unclassified
        ));
        let mut wrong_method = manifest.clone();
        wrong_method
            .api_rules
            .iter_mut()
            .find(|rule| rule.method == "PUT" && rule.path_glob == PRIVATE_REPORTING_PATH_GLOB)
            .unwrap()
            .method = method.into();
        assert!(matches!(
            classify(
                &args(&["api", "-X", method, endpoint]),
                &wrong_method,
                "linux"
            ),
            Classification::Unclassified
        ));
    }
    assert!(matches!(
        classify(&args(&["api", endpoint]), &manifest, "linux"),
        Classification::Mechanical
    ));
}

#[test]
fn v19_pr_ready_refuses_an_unbound_repository() {
    let _lock = crate::test_env::process_env_lock();
    let _bypass = BypassEnv::set(None);
    let manifest = manifest();
    let directory = tempfile::tempdir().unwrap();
    let paths = StatePaths::from_root(directory.path().to_path_buf());
    for verb in ["ready", "close", "reopen"] {
        let args = args(&["pr", verb, "7", "--repo", "outside/unbound"]);
        let target = TargetRepository::from_invocation(&args);
        assert!(governing_binding(
            &classify(&args, &manifest, "linux"),
            &manifest,
            &target,
            directory.path()
        )
        .is_none());
        let write = unbound_write(&args, &manifest, "linux", &target, directory.path()).unwrap();
        assert_eq!(
            dispatch_unbound_write(
                &args,
                &write,
                &manifest.detectors,
                &paths,
                1_700_000_000,
                |_| panic!("unbound bot speech must not reach upstream")
            ),
            REFUSAL_EXIT_STATUS
        );
    }
    assert!(!paths.bypass_audit.exists());
}

#[test]
fn v19_pr_ready_without_an_agent_session_never_falls_back_to_upstream() {
    let _lock = crate::test_env::process_env_lock();
    let manifest = manifest();
    let directory = tempfile::tempdir().unwrap();
    let paths = StatePaths::from_root(directory.path().to_path_buf());
    let binding = AgentBinding {
        repo: "cortexkit/aft".into(),
        agent_id: "alfonso-aft".into(),
    };
    let relay = RelayContext {
        worker_broker: None,
        connection_file: None,
        ticket: None,
        transient_delays: [Duration::ZERO; 2],
        relay_budget: Duration::from_secs(1),
    };
    for bypass in [None, Some("operator")] {
        let _bypass = BypassEnv::set(bypass);
        let args = args(&["pr", "ready", "7", "--undo", "--repo", "cortexkit/aft"]);
        let classification = classify(&args, &manifest, "linux");
        assert_eq!(
            dispatch_r3_with_relay_at(
                &args,
                classification,
                &manifest,
                &paths,
                &rung(),
                &binding,
                1_700_000_000,
                |_| panic!("bot speech must not use operator credentials"),
                &relay,
                directory.path()
            ),
            REFUSAL_EXIT_STATUS
        );
        let request = canonicalize_governed(
            &args,
            "pr ready",
            &manifest.canonicalization["pr ready"],
            19,
        )
        .unwrap();
        assert!(matches!(
            route_governed(
                &paths,
                &rung(),
                &binding,
                request,
                1_700_000_000,
                &manifest,
                &relay
            ),
            RouteOutcome::NoAgentSession
        ));
    }
}

#[test]
fn v19_pr_ready_rejects_a_different_signed_canonicalization() {
    let manifest = manifest();
    let args = args(&["pr", "ready", "7", "--repo", "cortexkit/aft"]);
    let mut canonical = manifest.canonicalization["pr ready"].clone();
    canonical.body_fields.clear();
    assert!(canonicalize_governed(&args, "pr ready", &canonical, 19).is_err());
}
